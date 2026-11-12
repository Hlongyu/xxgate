#!/usr/bin/env python3
"""Stage a prebuilt cluster release and hand off workers without stopping them.

Run on the Docker host after checksum verification and docker load. The old
release must support handoff protocol 1. No build, migration, deletion or forced
stop is performed. Nginx switching remains a separate, verified final step.
"""
import argparse
import datetime
import json
from pathlib import Path
import subprocess
import time
import urllib.request
import urllib.error


def output(command):
    return subprocess.check_output(command, text=True)


def inspect(container):
    return json.loads(output(['docker', 'inspect', container]))[0]


def environment(container):
    return dict(v.split('=', 1) for v in container['Config']['Env'] if '=' in v)


def endpoint(container, network):
    ip = container['NetworkSettings']['Networks'][network]['IPAddress']
    if not ip:
        raise RuntimeError('Container lacks a reachable database-network IP')
    return f'http://{ip}:8787'


def api(url, token, path='/internal/deployment', body=None):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    request = urllib.request.Request(url + path, headers={
        'x-xxgate-cluster-token': token, 'Content-Type': 'application/json',
    }, data=None if body is None else json.dumps(body).encode())
    with opener.open(request, timeout=5) as response:
        return json.load(response)


def wait_serving(url, token, timeout=90, phase="serving"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = api(url, token)
            if value.get('protocol') == 1 and value.get('phase') == phase:
                return value
        except (OSError, ValueError):
            pass
        time.sleep(0.5)
    raise RuntimeError('Candidate did not become ready; keep both releases running')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--env-file', required=True, type=Path)
    parser.add_argument('--compose-file', type=Path, default=Path(__file__).resolve().parent.parent / 'compose.cluster.production.yaml')
    parser.add_argument('--project', required=True, help='new, distinct Compose project')
    parser.add_argument('--source-entry', default='xxgate-cluster-entry-1')
    parser.add_argument('--source-worker-a', default='xxgate-cluster-worker-a-1')
    parser.add_argument('--source-worker-b', default='xxgate-cluster-worker-b-1')
    parser.add_argument('--drain-timeout', type=int, default=300)
    parser.add_argument('--record', required=True, type=Path)
    args = parser.parse_args()
    if not 1 <= args.drain_timeout <= 3600:
        parser.error('drain-timeout must be 1–3600 seconds')
    if args.record.exists():
        parser.error('Record exists; inspect that release before starting another rollout')
    command = ['docker', 'compose', '--env-file', str(args.env_file), '-p', args.project, '-f', str(args.compose_file)]
    config = json.loads(output(command + ['config', '--format', 'json']))
    if output(command + ['ps', '-a', '-q']).strip():
        raise RuntimeError('Target Compose project is not empty; refusing to recreate it')
    network = config['networks']['private']['name']
    sources = dict(zip(('entry', 'worker-a', 'worker-b'), (args.source_entry, args.source_worker_a, args.source_worker_b)))
    source_objects = {service: inspect(name) for service, name in sources.items()}
    record = {'project': args.project, 'sources': sources, 'status': 'checking', 'workers': {},
              'time': datetime.datetime.now(datetime.timezone.utc).isoformat()}
    token = None
    old_urls = set()
    candidate_urls = set()
    candidate_node_ids = set()
    for service, old in source_objects.items():
        if not old['State']['Running']:
            raise RuntimeError(f'{service} source is not running')
        old_env = environment(old)
        candidate = config['services'][service]
        new_env = candidate['environment']
        if candidate.get('build'):
            raise RuntimeError('Production rollout does not accept build configurations')
        for key in ('DATABASE_URL', 'XXGATE_MASTER_KEY', 'XXGATE_CLUSTER_TOKEN', 'XXGATE_ROLE'):
            if old_env.get(key) != new_env.get(key):
                raise RuntimeError(f'{service}: {key} must be preserved')
        token = old_env['XXGATE_CLUSTER_TOKEN']
        old_urls.add(old_env['XXGATE_NODE_URL'])
        candidate_urls.add(new_env['XXGATE_NODE_URL'])
        candidate_node_ids.add(new_env['XXGATE_NODE_ID'])
        if service == 'entry':
            if old_env['XXGATE_NODE_ID'] == new_env['XXGATE_NODE_ID']:
                raise RuntimeError('Candidate entry needs a separate node ID')
        elif (old_env['XXGATE_NODE_ID'] != new_env['XXGATE_NODE_ID']
              or int(new_env.get('XXGATE_WAIT_FOR_NODE_LOCK_SECS', '0')) < 2 * (args.drain_timeout + 120) + 60):
            raise RuntimeError('Candidate worker must preserve node ID and wait for ownership long enough')
        image = json.loads(output(['docker', 'image', 'inspect', candidate['image']]))[0]
        if image['Architecture'] != 'amd64' or image['Os'] != 'linux':
            raise RuntimeError('A locally built and loaded Linux amd64 image is required')
        current = api(endpoint(old, network), token)
        if current.get('protocol') != 1 or current.get('phase') != 'serving':
            raise RuntimeError('Source must be a serving release with handoff protocol 1')
        record[service] = {'old_boot_id': current['boot_id'], 'old_image': old['Image'], 'image': image['Id']}
    if old_urls & candidate_urls or len(candidate_urls) != 3 or len(candidate_node_ids) != 3:
        raise RuntimeError('Candidate URLs must be unique and different from all source URLs')
    # Look at every container on shared networks, including retained older
    # colors. A reused alias can make a healthy probe hit the wrong process.
    existing = {}
    for net_key in ('private', 'ingress'):
        real_network = config['networks'][net_key]['name']
        topology = json.loads(output(['docker', 'network', 'inspect', real_network]))[0]
        existing[real_network] = [inspect(cid) for cid in topology.get('Containers', {})]
    for candidate in config['services'].values():
        for net_key in ('private', 'ingress'):
            for alias in (candidate.get('networks', {}).get(net_key) or {}).get('aliases', []):
                real_network = config['networks'][net_key]['name']
                for container in existing[real_network]:
                    if alias in (container['NetworkSettings']['Networks'].get(real_network, {}).get('Aliases') or []):
                        raise RuntimeError(f'Candidate alias {alias} already exists on the shared network')
    subprocess.run(command + ['run', '--rm', '--no-deps', '--pull', 'never', 'entry', 'preflight'], check=True)

    def save():
        temporary = args.record.with_suffix('.tmp')
        temporary.write_text(json.dumps(record, indent=2) + '\n')
        temporary.chmod(0o600)
        temporary.replace(args.record)

    record['status'] = 'prepared'
    save()
    try:
        subprocess.run(command + ['up', '-d', '--no-deps', '--no-build', '--pull', 'never', 'entry', 'worker-a', 'worker-b'], check=True)
        candidate_containers = {s: inspect(output(command + ['ps', '-a', '-q', s]).strip()) for s in sources}
        for service, candidate in candidate_containers.items():
            if candidate['Image'] != record[service]['image']:
                raise RuntimeError('Candidate image changed since preflight')
            if service != 'entry':
                # Standby timeout must not become an endless restart loop.
                subprocess.run(['docker', 'update', '--restart=no', candidate['Id']], check=True, stdout=subprocess.DEVNULL)
        wait_serving(endpoint(candidate_containers['entry'], network), token)
        standbys = {s: wait_serving(endpoint(candidate_containers[s], network), token, phase='standby') for s in ('worker-a', 'worker-b')}
        record['status'] = 'handing_off'
        save()
        for service in ('worker-a', 'worker-b'):
            old_url = endpoint(source_objects[service], network)
            successor_url = config['services'][service]['environment']['XXGATE_NODE_URL']
            # Candidate remains alive but cannot dispatch until the lock closes.
            if not inspect(candidate_containers[service]['Id'])['State']['Running']:
                raise RuntimeError('Candidate exited before handoff; incumbent retained')
            api(old_url, token, '/internal/deployment/handoff', {
                'expected_boot_id': record[service]['old_boot_id'],
                'successor_url': successor_url, 'expected_successor_boot_id': standbys[service]['boot_id'],
                'drain_timeout_secs': args.drain_timeout,
            })
            deadline = time.monotonic() + args.drain_timeout + 120
            while time.monotonic() < deadline:
                state = api(old_url, token)
                if state['phase'] == 'forwarding':
                    break
                if state['phase'] in ('serving', 'failed'):
                    raise RuntimeError('Handoff aborted; inspect the recorded release, keep the source running')
                time.sleep(0.5)
            else:
                raise RuntimeError('Handoff is still pending; do not stop either process')
            ready = wait_serving(endpoint(candidate_containers[service], network), token)
            subprocess.run(['docker', 'update', '--restart=unless-stopped', candidate_containers[service]['Id']], check=True, stdout=subprocess.DEVNULL)
            subprocess.run(['docker', 'update', '--restart=no', source_objects[service]['Id']], check=True, stdout=subprocess.DEVNULL)
            record['workers'][service] = ready
            save()
        record['status'] = 'ready_for_nginx_switch'
        record['candidates'] = {s: c['Name'].lstrip('/') for s, c in candidate_containers.items()}
        save()
    except Exception:
        record['status'] = 'needs_inspection'
        save()
        raise
    print(json.dumps(record, indent=2))


if __name__ == '__main__':
    main()
