"""Exercise rollout safety gates without connecting to a Docker daemon."""
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('rollout', Path(__file__).resolve().parents[1]/'rollout-cluster.py')
rollout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rollout)


class RolloutTests(unittest.TestCase):
    def setup_release(self):
        self.config = {'networks': {'private': {'name': 'database'}, 'ingress': {'name': 'ingress'}}, 'services': {}}
        self.containers = {}
        self.handed = set()
        self.commands = []
        for service, node, role in [('entry','entry','router'), ('worker-a','default','worker'), ('worker-b','worker-b','worker')]:
            env = {'DATABASE_URL': 'postgres://test', 'XXGATE_MASTER_KEY': 'private-test', 'XXGATE_CLUSTER_TOKEN': 'private-token', 'XXGATE_ROLE': role, 'XXGATE_NODE_ID': node, 'XXGATE_NODE_URL': f'http://old-{service}:8787'}
            old = {'Id': f'old-{service}', 'Name': f'/old-{service}', 'Image': 'old-image', 'State': {'Running': True},
                   'Config': {'Env': [f'{k}={v}' for k,v in env.items()]},
                   'NetworkSettings': {'Networks': {'database': {'IPAddress': f'old-{service}', 'Aliases': [f'old-{service}']}, 'ingress': {'Aliases': []}}}}
            self.containers[old['Id']] = old
            new_env = dict(env, XXGATE_NODE_ID='entry-green' if service=='entry' else node,
                           XXGATE_NODE_URL=f'http://new-{service}:8787', XXGATE_WAIT_FOR_NODE_LOCK_SECS='3600')
            self.config['services'][service] = {'environment': new_env, 'image': 'local/xxgate:release', 'networks': {'private': {'aliases': [f'new-{service}']}}}
            new = copy.deepcopy(old)
            new.update(Id=f'new-{service}', Name=f'/new-{service}', Image='new-image')
            new['NetworkSettings']['Networks']['database']['IPAddress'] = f'new-{service}'
            self.containers[new['Id']] = new

    def output(self, command):
        if command[-3:] == ['config','--format','json']:
            return json.dumps(self.config)
        if command[:3] == ['docker','image','inspect']:
            return json.dumps([{'Id':'new-image','Architecture':'amd64','Os':'linux'}])
        if command[:3] == ['docker','network','inspect']:
            return json.dumps([{'Containers': {f'old-{s}':{} for s in self.config['services']}}])
        if command[-3:] == ['ps','-a','-q']:
            return ''
        if command[-4:-1] == ['ps','-a','-q']:
            return 'new-'+command[-1]
        raise AssertionError(command)

    def api(self, url, token, path='/internal/deployment', body=None):
        if body is not None:
            self.handed.add(url)
            self.assertIn('expected_successor_boot_id', body)
            return {'status':'draining'}
        return {'protocol':1,'phase':'forwarding' if url in self.handed else 'serving','boot_id':url}

    def execute(self, folder, fail_preflight=False):
        def run(command, **_):
            self.commands.append(command)
            if fail_preflight and command[-1] == 'preflight':
                raise subprocess.CalledProcessError(1, command)
        argv = ['rollout', '--env-file', str(Path(folder)/'green.env'), '--project', 'green',
                '--source-entry','old-entry','--source-worker-a','old-worker-a','--source-worker-b','old-worker-b',
                '--record', str(Path(folder)/'record.json')]
        with patch.object(sys,'argv',argv), patch.object(rollout,'output',side_effect=self.output), \
             patch.object(rollout,'inspect',side_effect=lambda name:self.containers[name]), \
             patch.object(rollout,'api',side_effect=self.api), patch.object(rollout,'wait_serving',return_value={'boot_id':'candidate','phase':'serving'}), \
             patch.object(rollout.subprocess,'run',side_effect=run), contextlib.redirect_stdout(io.StringIO()):
            rollout.main()

    def test_success_never_stops_builds_or_migrates(self):
        self.setup_release()
        with tempfile.TemporaryDirectory() as folder:
            self.execute(folder)
            record = json.loads((Path(folder)/'record.json').read_text())
            self.assertEqual(record['status'], 'ready_for_nginx_switch')
            self.assertEqual(len(record['workers']),2)
        up = next(c for c in self.commands if 'up' in c)
        self.assertIn('--no-build',up)
        self.assertIn('--no-deps',up)
        self.assertIn('never',up)
        for command in self.commands:
            self.assertFalse({'stop','down','build','migrate'} & set(command))

    def test_bad_credentials_fail_before_starting(self):
        self.setup_release()
        self.config['services']['worker-a']['environment']['XXGATE_MASTER_KEY'] = 'wrong'
        with tempfile.TemporaryDirectory() as folder, self.assertRaisesRegex(RuntimeError,'must be preserved'):
            self.execute(folder)
        self.assertFalse(self.commands)

    def test_schema_preflight_failure_does_not_start_or_handoff(self):
        self.setup_release()
        with tempfile.TemporaryDirectory() as folder, self.assertRaises(subprocess.CalledProcessError):
            self.execute(folder,fail_preflight=True)
        self.assertFalse(self.handed)
        self.assertFalse(any('up' in c for c in self.commands))

    def test_alias_collision_rejected_before_preflight(self):
        self.setup_release()
        self.config['services']['worker-a']['networks']['private']['aliases'] = ['old-worker-a']
        with tempfile.TemporaryDirectory() as folder, self.assertRaisesRegex(RuntimeError,'already exists'):
            self.execute(folder)
        self.assertFalse(self.commands)


if __name__ == '__main__':
    unittest.main()
