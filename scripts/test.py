#!/usr/bin/env python3
"""Run against a disposable PostgreSQL database (Docker, or a native test cluster)."""
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import uuid
from urllib.parse import quote

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)
environment = os.environ.copy()
database = "xxgate_test_" + uuid.uuid4().hex
command = ["cargo", "test", "--workspace", "--", "--include-ignored"] + sys.argv[1:]
native = environment.get("XXGATE_TEST_PG_BIN")
if native:
    # Never connect to an existing database or register a persistent service.
    binaries = Path(native).resolve()
    with tempfile.TemporaryDirectory(prefix="xxgate-test-pg-") as folder:
        data = Path(folder) / "data"
        subprocess.run([str(binaries / "initdb"), "-D", str(data), "-A", "trust", "--no-locale", "-E", "UTF8"], check=True, stdout=subprocess.DEVNULL)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        subprocess.run([str(binaries / "pg_ctl"), "-D", str(data), "-l", str(Path(folder) / "postgres.log"), "-o", f"-h 127.0.0.1 -p {port} -k {folder}", "-w", "start"], check=True, stdout=subprocess.DEVNULL)
        try:
            subprocess.run([str(binaries / "createdb"), "-h", "127.0.0.1", "-p", str(port), database], check=True)
            environment["TEST_DATABASE_URL"] = f"postgres://127.0.0.1:{port}/{database}"
            result = subprocess.run(command, env=environment)
        finally:
            subprocess.run([str(binaries / "pg_ctl"), "-D", str(data), "-m", "fast", "-w", "stop"], check=True, stdout=subprocess.DEVNULL)
else:
    compose = ["docker", "compose", "-p", "xxgate"]
    subprocess.run(compose + ["up", "-d", "--wait"], check=True)
    psql = compose + ["exec", "-T", "postgres", "psql", "-U", "xxgate", "-d", "postgres", "-v", "ON_ERROR_STOP=1", "-c"]
    subprocess.run(psql + [f'CREATE DATABASE "{database}"'], check=True, stdout=subprocess.DEVNULL)
    password = quote(environment.get("XXGATE_DB_PASSWORD", "xxgate-local-development"), safe="")
    port = environment.get("XXGATE_DB_PORT", "55439")
    environment["TEST_DATABASE_URL"] = f"postgres://xxgate:{password}@127.0.0.1:{port}/{database}"
    try:
        result = subprocess.run(command, env=environment)
    finally:
        subprocess.run(psql + [f'DROP DATABASE "{database}" WITH (FORCE)'], check=True, stdout=subprocess.DEVNULL)
sys.exit(result.returncode)
