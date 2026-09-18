#!/usr/bin/env python3
"""Run tests against a disposable database in the project's PostgreSQL container."""
import os
from pathlib import Path
import subprocess
import sys
import uuid
from urllib.parse import quote

ROOT = Path(__file__).resolve().parent.parent
os.chdir(ROOT)
COMPOSE = ["docker", "compose", "-p", "xxgate"]
subprocess.run(COMPOSE + ["up", "-d", "--wait"], check=True)
database = "xxgate_test_" + uuid.uuid4().hex
psql = COMPOSE + ["exec", "-T", "postgres", "psql", "-U", "xxgate", "-d", "postgres", "-v", "ON_ERROR_STOP=1", "-c"]
subprocess.run(psql + [f'CREATE DATABASE "{database}"'], check=True, stdout=subprocess.DEVNULL)
environment = os.environ.copy()
password = quote(environment.get("XXGATE_DB_PASSWORD", "xxgate-local-development"), safe="")
port = environment.get("XXGATE_DB_PORT", "55439")
environment["TEST_DATABASE_URL"] = f"postgres://xxgate:{password}@127.0.0.1:{port}/{database}"
try:
    result = subprocess.run(["cargo", "test", "--workspace", "--", "--include-ignored"] + sys.argv[1:], env=environment)
finally:
    subprocess.run(psql + [f'DROP DATABASE "{database}" WITH (FORCE)'], check=True, stdout=subprocess.DEVNULL)
sys.exit(result.returncode)
