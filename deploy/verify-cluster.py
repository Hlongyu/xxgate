#!/usr/bin/env python3
"""Verify the existing local entry and public proxy without a model generation."""
import argparse
import base64
import hashlib
import json
import secrets
import socket
import ssl
import subprocess
import urllib.parse
import urllib.request


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--entry-container", default="xxgate-cluster-entry-1")
    parser.add_argument("--public-url", required=True)
    args = parser.parse_args()
    entry = json.loads(subprocess.check_output(["docker", "inspect", args.entry_container]))[0]
    env = dict(v.split("=", 1) for v in entry["Config"]["Env"] if "=" in v)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    cookie = ""

    def admin(path, method="GET", data=None):
        nonlocal cookie
        headers = {"Content-Type": "application/json", "X-XXGate-CSRF": "1"}
        if cookie:
            headers["Cookie"] = cookie
        request = urllib.request.Request("http://127.0.0.1:8797/api/admin" + path,
                                         headers=headers, method=method,
                                         data=None if data is None else json.dumps(data).encode())
        with opener.open(request, timeout=20) as response:
            if response.headers.get("Set-Cookie"):
                cookie = response.headers["Set-Cookie"].split(";")[0]
            return {} if response.status == 204 else json.load(response)

    admin("/login", "POST", {"password": env["XXGATE_ADMIN_PASSWORD"]})
    nodes = admin("/nodes")["items"]
    assert {n["id"] for n in nodes} == {"entry", "default", "worker-b"}
    assert all(n["online"] for n in nodes)
    accounts = admin("/accounts")["items"]
    assert all(a["node_id"] == "default" for a in accounts)
    keys_before = len(admin("/keys")["items"])
    temporary = admin("/keys", "POST", {"name": "cluster-release-verification"})
    try:
        key = temporary["secret"]
        request = urllib.request.Request(args.public_url.rstrip("/") + "/v1/models",
                                         headers={"Authorization": "Bearer " + key})
        with opener.open(request, timeout=20) as response:
            models = json.load(response)["data"]
        assert models
        with opener.open(args.public_url.rstrip("/") + "/readyz", timeout=20) as response:
            assert response.status == 200
        url = urllib.parse.urlsplit(args.public_url)
        assert url.scheme == "https"
        nonce = base64.b64encode(secrets.token_bytes(16)).decode()
        context = ssl.create_default_context()
        with context.wrap_socket(socket.create_connection((url.hostname, url.port or 443), timeout=15), server_hostname=url.hostname) as connection:
            wire = ("GET /v1/responses HTTP/1.1\r\nHost: " + url.netloc + "\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + nonce + "\r\nAuthorization: Bearer " + key + "\r\n\r\n").encode()
            connection.sendall(wire)
            head = b""
            while b"\r\n\r\n" not in head and len(head) < 65536:
                chunk = connection.recv(4096)
                if not chunk:
                    break
                head += chunk
            assert head.startswith(b"HTTP/1.1 101"), head.split(b"\r\n", 1)[0]
            expected = base64.b64encode(hashlib.sha1((nonce + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
            assert expected.lower() in head.lower()
            connection.sendall(b"\x88\x80" + secrets.token_bytes(4))
        result = {"status": "passed", "public_ready": True, "public_models": len(models),
                  "public_websocket_upgrade": 101, "upstream_generations": 0,
                  "accounts": len(accounts), "existing_keys": keys_before,
                  "nodes": [{"id": n["id"], "online": n["online"], "accepting": n["accepting"]} for n in nodes]}
    finally:
        admin("/keys/" + temporary["key"]["id"], "DELETE")
        admin("/logout", "POST")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
