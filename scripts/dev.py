#!/usr/bin/env python3
"""Start a loopback-only development instance with persistent local credentials."""
import base64
import json
import os
from pathlib import Path
import secrets
import subprocess
from urllib.parse import quote

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)
environment = os.environ.copy()
directory = ROOT / ".xxgate"
directory.mkdir(mode=0o700, exist_ok=True)
config = directory / "local-env.json"
if not config.exists():
    password = environment.get("XXGATE_DB_PASSWORD", "xxgate-local-development")
    port = environment.get("XXGATE_DB_PORT", "55439")
    values = {
        "DATABASE_URL": f"postgres://xxgate:{quote(password, safe='')}@127.0.0.1:{port}/xxgate",
        "XXGATE_MASTER_KEY": base64.b64encode(secrets.token_bytes(32)).decode(),
        "XXGATE_ADMIN_PASSWORD": secrets.token_urlsafe(24),
        "XXGATE_BIND": "127.0.0.1:8787",
        "XXGATE_SECURE_COOKIES": "0",
    }
    descriptor = os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as handle:
        json.dump(values, handle, indent=2)
        handle.write("\n")
values = json.loads(config.read_text())
for key, value in values.items():
    environment.setdefault(key, value)
subprocess.run(["docker", "compose", "-p", "xxgate", "up", "-d", "--wait"], check=True)
print(f"Local administrator credentials: {config}", flush=True)
print(f"Open http://{environment['XXGATE_BIND']}", flush=True)
os.execvpe("cargo", ["cargo", "run", "-p", "xxgate-server"], environment)
