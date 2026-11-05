#!/usr/bin/env python3
"""Stopped upgrade using a prebuilt image and an existing database.

This script never builds an image, dumps/restores a database, or removes a volume.
The caller must explicitly choose --without-backup. On failure after migration,
keep the old application stopped and repair forward; do not restart an old image.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import secrets
import subprocess
import time
import urllib.parse
import urllib.request


def run(args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def output(args):
    return subprocess.check_output(args, text=True)


def inspect(name):
    return json.loads(output(["docker", "inspect", name]))[0]


def dotenv(value):
    if "\n" in value or "\r" in value:
        raise ValueError("Environment values must not contain line breaks")
    return "'" + value.replace("'", "\\'") + "'"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--image-tag", required=True)
    parser.add_argument("--expected-image", required=True)
    parser.add_argument("--release-dir", required=True)
    parser.add_argument("--source-container", default="xxgate")
    parser.add_argument("--database-container", default="xxgate-postgres")
    parser.add_argument("--without-backup", action="store_true", required=True)
    args = parser.parse_args()
    folder = Path(args.release_dir).resolve()
    compose = folder / "compose.cluster.production.yaml"
    if not compose.is_file():
        raise RuntimeError("Stage the production Compose file first")
    source = inspect(args.source_container)
    if source["Config"]["Image"] != args.expected_image or not source["State"]["Running"]:
        raise RuntimeError("Source instance changed or is not running; inspect before switching")
    image = json.loads(output(["docker", "image", "inspect", "local/xxgate:" + args.image_tag]))[0]
    if image["Architecture"] != "amd64" or image["Os"] != "linux":
        raise RuntimeError("The release must be a prebuilt Linux amd64 image")
    environment = dict(item.split("=", 1) for item in source["Config"]["Env"] if "=" in item)
    database = urllib.parse.urlsplit(environment["DATABASE_URL"])
    if database.hostname != "xxgate-db" or database.path != "/xxgate":
        raise RuntimeError("Unexpected production database destination")
    networks = source["NetworkSettings"]["Networks"]
    if "xxgate-private" not in networks or "code-internal" not in networks:
        raise RuntimeError("Unexpected production networks")
    db = inspect(args.database_container)
    volumes = [m["Name"] for m in db["Mounts"] if m["Type"] == "volume"]
    if not volumes:
        raise RuntimeError("Database has no persistent volume")
    values = {
        "XXGATE_IMAGE_TAG": args.image_tag,
        "XXGATE_DB_PASSWORD": urllib.parse.unquote(database.password or ""),
        "XXGATE_MASTER_KEY": environment["XXGATE_MASTER_KEY"],
        "XXGATE_ADMIN_PASSWORD": environment["XXGATE_ADMIN_PASSWORD"],
        "XXGATE_CLUSTER_TOKEN": secrets.token_urlsafe(48),
        "XXGATE_HOST_PORT": "8797",
        "XXGATE_DATABASE_NETWORK": "xxgate-private",
        "XXGATE_SUB2API_NETWORK": "code-internal",
        "XXGATE_ENTRY_MEMORY": "512m",
        "XXGATE_WORKER_MEMORY": "1g",
        "XXGATE_WORKER_B_MEMORY": "512m",
    }
    env_file = folder / "cluster.env"
    descriptor = os.open(env_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as handle:
        handle.write("".join(k + "=" + dotenv(v) + "\n" for k, v in values.items()))
    command = ["docker", "compose", "--env-file", str(env_file), "-f", str(compose)]
    run(command + ["config", "--quiet"])

    def sql(query):
        return output(["docker", "exec", args.database_container, "psql", "-U", "xxgate", "-d", "xxgate", "-At", "-v", "ON_ERROR_STOP=1", "-c", query]).strip()

    def identity_fingerprint():
        return sql("SELECT json_build_object('accounts',(SELECT count(*) FROM accounts WHERE deleted_at IS NULL),'keys',(SELECT count(*) FROM api_keys WHERE deleted_at IS NULL),'account_ids',(SELECT md5(string_agg(id::text,',' ORDER BY id)) FROM accounts),'key_hashes',(SELECT md5(string_agg(id::text||secret_hash,',' ORDER BY id)) FROM api_keys))")

    record = {"image": "local/xxgate:" + args.image_tag, "previous_image": args.expected_image,
              "database_volumes": volumes, "backup": False, "account_placement": "all existing accounts remain on default"}
    record_path = folder / "upgrade-result.json"
    try:
        record["stopped_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        run(["docker", "update", "--restart=no", args.source_container], stdout=subprocess.DEVNULL)
        run(["docker", "stop", "--time", "40", args.source_container])
        record["before"] = json.loads(identity_fingerprint())
        # Removing the old alias prevents internal clients from resolving a stale backend.
        run(["docker", "network", "disconnect", "code-internal", args.source_container])
        run(command + ["run", "--rm", "--no-deps", "entry", "migrate"])
        run(command + ["up", "-d", "--no-build", "--wait", "--wait-timeout", "180"])
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        for attempt in range(90):
            try:
                with opener.open("http://127.0.0.1:8797/readyz", timeout=5) as response:
                    if response.status == 200:
                        break
            except Exception:
                time.sleep(1)
        else:
            raise RuntimeError("The new entry did not become ready")
        record["after"] = json.loads(identity_fingerprint())
        if record["after"] != record["before"]:
            raise RuntimeError("Account/Key identity verification failed")
        after_volumes = [m["Name"] for m in inspect(args.database_container)["Mounts"] if m["Type"] == "volume"]
        if after_volumes != volumes:
            raise RuntimeError("Database volume changed")
        record["schema_version"] = int(sql("SELECT max(version) FROM _sqlx_migrations"))
        record["account_nodes"] = sql("SELECT node_id||':'||count(*) FROM accounts WHERE deleted_at IS NULL GROUP BY node_id ORDER BY node_id")
        record["ready_at"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        record["status"] = "active"
    except Exception:
        record["status"] = "needs_forward_repair"
        record_path.write_text(json.dumps(record, indent=2) + "\n")
        print("Upgrade stopped. Keep the legacy image stopped; database volumes were retained.", flush=True)
        raise
    record_path.write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record, indent=2), flush=True)


if __name__ == "__main__":
    main()
