"""Real Nginx reload tests; run: python3 -B -m unittest discover -s deploy/tests."""
import argparse
import base64
import hashlib
import http.client
import http.server
import importlib.util
import json
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'bluegreen-switch.py'
spec = importlib.util.spec_from_file_location('bluegreen', SCRIPT)
bg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bg)


class Backend(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        if self.path == '/readyz':
            data = json.dumps({'status': 'ready', 'boot_id': self.server.color}).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        elif self.path == '/stream':
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(f'data: {self.server.color}\n\n'.encode())
            self.wfile.flush()
            self.server.release.wait(10)
            self.wfile.write(b'data: done\n\n')
            self.wfile.flush()
            self.close_connection = True
        elif self.path == '/ws':
            nonce = self.headers['Sec-WebSocket-Key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
            self.send_response(101)
            self.send_header('Upgrade', 'websocket')
            self.send_header('Connection', 'Upgrade')
            self.send_header('Sec-WebSocket-Accept', base64.b64encode(hashlib.sha1(nonce.encode()).digest()).decode())
            self.end_headers()
            value = self.server.color.encode()
            self.wfile.write(bytes([0x81, len(value)]) + value)
            self.wfile.flush()
            self.server.release.wait(10)
            self.wfile.write(b'\x81\x04done')
            self.wfile.flush()
            self.close_connection = True

    def log_message(self, *_):
        pass


def backend(color):
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Backend)
    server.color = color
    server.release = threading.Event()
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class SwitchTests(unittest.TestCase):
    def test_invalid_backend(self):
        for value in ('http://bad;host:80', 'http://a:0', 'http://a:99999', 'http://a:80/path', 'http://a:80\ninject', 'http://u@a:80', 'https://a:80'):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                bg.backend_server(value)

    @unittest.skipUnless(shutil.which('nginx'), 'requires local nginx')
    def test_reload_preserves_streams_and_detects_failed_activation(self):
        blue, green, unused = backend('blue'), backend('green'), backend('unused')
        old = f'http://127.0.0.1:{blue.server_port}'
        new = f'http://127.0.0.1:{green.server_port}'
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            include = root / 'active.inc'
            include.write_text(bg.backend_server(old)[1])
            with socket.socket() as reserve:
                reserve.bind(('127.0.0.1', 0))
                port = reserve.getsockname()[1]
            template = (SCRIPT.parent / 'nginx/xxgate-origin.conf').read_text()
            template = template.replace('/etc/nginx/xxgate-active-upstream.inc', str(include)).replace('192.0.2.20:8080', f'127.0.0.1:{port}').replace('192.0.2.10', '127.0.0.1')
            config = root / 'nginx.conf'
            config.write_text(f'pid {root}/nginx.pid;\nerror_log {root}/error.log;\nevents {{}}\nhttp {{ access_log off;\n{template}\n}}\n')
            command = [shutil.which('nginx'), '-p', folder, '-c', str(config)]
            subprocess.run(command + ['-t'], check=True, capture_output=True)
            subprocess.run(command, check=True, capture_output=True)
            streams = []
            try:
                stable = f'http://127.0.0.1:{port}'
                bg.wait_ready(stable, 'blue', 20, .05, 2)
                sse = http.client.HTTPConnection('127.0.0.1', port, timeout=5)
                streams.append(sse)
                sse.request('GET', '/stream')
                stream = sse.getresponse()
                self.assertEqual(stream.readline(), b'data: blue\n')
                self.assertEqual(stream.readline(), b'\n')
                ws = socket.create_connection(('127.0.0.1', port), timeout=5)
                streams.append(ws)
                ws.sendall(b'GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n')
                reader = ws.makefile('rb')
                self.assertIn(b'101', reader.readline())
                while reader.readline() != b'\r\n':
                    pass
                self.assertEqual(reader.read(6), b'\x81\x04blue')
                args = argparse.Namespace(upstream_file=include, state_file=root/'state.json',
                    expected_backend=old, backend=bg.backend_server(new), probe_url=stable,
                    attempts=30, interval=.05, timeout=2, nginx_test=command+['-t'], nginx_reload=command+['-s', 'reload'])
                result = bg.switch(args)
                self.assertEqual(result['status'], 'active')
                self.assertEqual(bg.ready(stable), 'green')
                blue.release.set()
                self.assertEqual(stream.readline(), b'data: done\n')
                self.assertEqual(reader.read(6), b'\x81\x04done')
                reader.close()
                # Merely returning zero from reload does NOT prove activation.
                args.expected_backend = new
                # Use a never-activated incarnation: an overlapping old Nginx
                # worker cannot accidentally satisfy the activation probe.
                args.backend = bg.backend_server(f'http://127.0.0.1:{unused.server_port}')
                args.nginx_reload = ['/usr/bin/true']
                args.attempts = 3
                with self.assertRaisesRegex(RuntimeError, 'incumbent restored and verified'):
                    bg.switch(args)
                self.assertEqual(include.read_text(), bg.backend_server(new)[1])
                self.assertEqual(bg.ready(stable), 'green')
                self.assertEqual(json.loads(args.state_file.read_text())['status'], 'rolled_back')
                args.expected_backend = old
                with self.assertRaisesRegex(RuntimeError, 'stale release'):
                    bg.switch(args)
            finally:
                blue.release.set()
                green.release.set()
                for stream in streams:
                    stream.close()
                subprocess.run(command+['-s', 'quit'], check=True, capture_output=True)
                blue.shutdown(); green.shutdown()
                blue.server_close(); green.server_close()
                unused.shutdown(); unused.server_close()


if __name__ == '__main__':
    unittest.main()
