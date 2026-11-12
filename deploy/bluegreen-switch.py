#!/usr/bin/env python3
"""Switch an existing Nginx include and verify the running backend, with rollback.

This command never stops containers, builds images or migrates a database.
Both candidate and incumbent must implement /readyz with a boot_id.
"""
import argparse
import datetime
import fcntl
import ipaddress
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())


def command(value):
    result = shlex.split(value)
    if not result:
        raise argparse.ArgumentTypeError('command must not be empty')
    return result


def backend_server(value):
    try:
        parsed = urllib.parse.urlsplit(value)
        port = parsed.port
        host = parsed.hostname
        if (parsed.scheme != 'http' or not host or not port
                or parsed.username is not None or parsed.password is not None
                or parsed.query or parsed.fragment or parsed.path not in ('', '/')
                or any(c.isspace() for c in value)):
            raise ValueError('expected an HTTP origin with an explicit port')
        if ':' in host:
            if '%' in host:
                raise ValueError('scoped IPv6 hosts are not supported')
            host = f'[{ipaddress.IPv6Address(host)}]'
        elif not re.fullmatch(r'[A-Za-z0-9._-]+', host):
            raise ValueError('invalid host')
        return f'http://{host}:{port}', f'server {host}:{port};\n'
    except ValueError as error:
        raise argparse.ArgumentTypeError(str(error)) from error


def ready(url, timeout=5):
    req = urllib.request.Request(url.rstrip('/') + '/readyz', headers={'Connection': 'close'})
    with OPENER.open(req, timeout=timeout) as response:
        value = json.loads(response.read(65536))
    if value.get('status') != 'ready' or not value.get('boot_id'):
        raise RuntimeError('Backend lacks release identity in /readyz')
    return value['boot_id']


def wait_ready(url, expected, attempts, interval, timeout):
    consecutive = 0
    for attempt in range(attempts):
        try:
            if ready(url, timeout) == expected:
                consecutive += 1
                if consecutive >= 3:
                    return
            else:
                consecutive = 0
        except (OSError, ValueError, RuntimeError):
            consecutive = 0
        if attempt + 1 < attempts:
            time.sleep(interval)
    raise RuntimeError('Nginx did not serve the expected release identity')


def atomic_write(path, content, mode=0o644):
    descriptor, temporary = tempfile.mkstemp(prefix=f'.{path.name}.', dir=path.parent)
    try:
        with os.fdopen(descriptor, 'w') as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def switch(args):
    path = args.upstream_file.resolve(strict=True)
    state_path = args.state_file or path.with_suffix('.state.json')
    if state_path.resolve() == path:
        raise RuntimeError('State file must differ from the upstream include')
    # Serializes test/write/reload/probe/rollback, including concurrent releases.
    with path.with_suffix(path.suffix + '.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        previous = path.read_text()
        mode = path.stat().st_mode & 0o777
        old_server = backend_server(args.expected_backend)[1]
        servers = [line.strip() for line in previous.splitlines()
                   if line.strip() and not line.lstrip().startswith('#')]
        if servers != [old_server.strip()]:
            raise RuntimeError('Active upstream changed; refusing stale release')
        old_id = ready(args.expected_backend, args.timeout)
        candidate_id = ready(args.backend[0], args.timeout)
        if old_id == candidate_id:
            raise RuntimeError('Candidate resolves to the incumbent incarnation')
        wait_ready(args.probe_url, old_id, args.attempts, args.interval, args.timeout)
        state = {'previous': args.expected_backend, 'candidate': args.backend[0],
                 'previous_boot_id': old_id, 'candidate_boot_id': candidate_id,
                 'status': 'prepared', 'upstream_file': str(path),
                 'time': datetime.datetime.now(datetime.timezone.utc).isoformat()}
        atomic_write(state_path, json.dumps(state, indent=2) + '\n', 0o600)
        # Test the incumbent config too, before altering anything.
        subprocess.run(args.nginx_test, check=True, timeout=30)
        try:
            atomic_write(path, args.backend[1], mode)
            subprocess.run(args.nginx_test, check=True, timeout=30)
            subprocess.run(args.nginx_reload, check=True, timeout=30)
            # A successful `nginx -s reload` only acknowledges signal delivery.
            wait_ready(args.probe_url, candidate_id, args.attempts, args.interval, args.timeout)
        except Exception as error:
            atomic_write(path, previous, mode)
            try:
                subprocess.run(args.nginx_test, check=True, timeout=30)
                subprocess.run(args.nginx_reload, check=True, timeout=30)
                wait_ready(args.probe_url, old_id, args.attempts, args.interval, args.timeout)
                state['status'] = 'rolled_back'
            except Exception as rollback_error:
                state['status'] = 'rollback_unverified'
                atomic_write(state_path, json.dumps(state, indent=2) + '\n', 0o600)
                raise RuntimeError('Rollback could not be verified; keep both instances running') from rollback_error
            atomic_write(state_path, json.dumps(state, indent=2) + '\n', 0o600)
            raise RuntimeError('Switch failed; incumbent restored and verified') from error
        state.update(status='active', active=args.backend[0])
        atomic_write(state_path, json.dumps(state, indent=2) + '\n', 0o600)
        return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--backend', required=True, type=backend_server)
    parser.add_argument('--expected-backend', required=True)
    parser.add_argument('--probe-url', required=True, help='stable Nginx origin used by clients')
    parser.add_argument('--upstream-file', required=True, type=Path)
    parser.add_argument('--state-file', type=Path)
    parser.add_argument('--attempts', type=int, default=30)
    parser.add_argument('--interval', type=float, default=0.5)
    parser.add_argument('--timeout', type=float, default=5)
    parser.add_argument('--nginx-test', type=command, default=['nginx', '-t'])
    parser.add_argument('--nginx-reload', type=command, default=['nginx', '-s', 'reload'])
    args = parser.parse_args()
    if args.attempts < 3 or args.interval <= 0 or args.timeout <= 0:
        parser.error('Invalid polling limits')
    print(json.dumps(switch(args), indent=2))


if __name__ == '__main__':
    main()
