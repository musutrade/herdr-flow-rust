"""Synthetic stdin only; never executes an installed/native hook or a model."""
import concurrent.futures
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'scripts/receive_native_hook.py'
SPEC = importlib.util.spec_from_file_location('receiver', SCRIPT)
receiver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(receiver)


def hermes(event='on_session_end', **extra):
    return dict(hook_event_name=event, tool_name=None, tool_input=None,
                session_id='sid', cwd='/PRIVATE', profile='default', extra=extra)


class ReceiverTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'inbox'

    def command(self, source='codex', root=None):
        return [sys.executable, '-I', '-B', str(SCRIPT), '--source', source,
                '--output', str(root or self.root)]

    def call(self, payload, source='codex', root=None):
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        p = subprocess.run(self.command(source, root), input=raw, capture_output=True, timeout=4)
        self.assertEqual(p.returncode, 0)
        self.assertEqual(p.stdout, b'')
        self.assertLessEqual(len(p.stderr), 80)
        self.assertNotIn(b'PRIVATE', p.stderr)
        self.assertNotIn(str(self.root).encode(), p.stderr)
        return p.stderr

    def records(self):
        return [json.loads(p.read_bytes()) for p in self.root.glob('*.json')]

    def test_sources_and_sensitive_content(self):
        bad = {'error_details': 'PRIVATE', 'last_assistant_message': 'PRIVATE',
               'transcript_path': '/PRIVATE', 'cwd': '/PRIVATE', 'request': 'PRIVATE'}
        self.assertFalse(self.call(dict(bad, hook_event_name='StopFailure', session_id='s',
                                       turn_id='forged', error='overloaded'), 'claude'))
        self.assertFalse(self.call(dict(bad, hook_event_name='Stop', session_id='s', turn_id='t')))
        for status in ('completed', 'failed', 'cancelled', 'invented'):
            self.assertFalse(self.call(dict(bad, hook_event_name='PostModelRequest',
                                           session_id='s', status=status, error='PRIVATE')))
        rows = self.records()
        self.assertEqual(len(rows), 6)
        claude = next(r for r in rows if r['source'] == 'claude')
        self.assertIsNone(claude['turn_id'])
        self.assertEqual(claude['error_class'], 'overloaded')
        self.assertEqual({r['reported_status'] for r in rows},
                         {'failed', 'stop_observed', 'request_completed', 'request_failed',
                          'request_cancelled', 'unknown'})
        for path in self.root.glob('*.json'):
            self.assertNotIn('PRIVATE', path.read_text())
            self.assertLessEqual(path.stat().st_size, 4096)
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.root.stat().st_mode), 0o700)

    def test_hermes_source_constructed_wire_not_native_trigger(self):
        for error in ({'message': 'PRIVATE'}, 'PRIVATE stringified'):
            self.assertFalse(self.call(hermes('api_request_error', turn_id='t', error=error,
                request={'message': 'PRIVATE'}, retry_count=999, max_retries=1), 'hermes'))
        self.assertFalse(self.call(hermes(turn_id='t', completed=True, failed=False,
                                         interrupted=False), 'hermes'))
        rows = self.records()
        self.assertEqual(sorted(r['reported_status'] for r in rows),
                         ['attempt_error', 'attempt_error', 'completed'])
        self.assertNotIn('PRIVATE', json.dumps(rows))

    def test_hermes_reduced_conflicts_and_wrong_envelope(self):
        for extra in ({}, {'turn_id': 't', 'completed': True},
                      {'turn_id': 't', 'completed': True, 'failed': True, 'interrupted': False},
                      {'turn_id': 't', 'completed': 1, 'failed': False, 'interrupted': False}):
            self.assertFalse(self.call(hermes(**extra), 'hermes'))
        p = hermes(turn_id='t', completed=True, failed=False, interrupted=False)
        p['session_id'] = ''
        self.assertFalse(self.call(p, 'hermes'))
        missing = hermes(); del missing['extra']
        self.assertFalse(self.call(missing, 'hermes'))
        for p in (dict(hermes(), extra=[]), dict(hermes(), turn_id='forged'),
                  dict(hermes(), completed=True), hermes(session_id='different')):
            self.assertTrue(self.call(p, 'hermes'))
        self.assertEqual({r['reported_status'] for r in self.records()}, {'unknown'})

    def test_missing_identity_unknown_error_and_duplicate_deliveries(self):
        p = dict(hook_event_name='StopFailure', error='PRIVATE')
        for _ in range(2):
            self.assertFalse(self.call(p, 'claude'))
        rows = self.records()
        self.assertEqual(len({r['receive_id'] for r in rows}), 2)
        self.assertTrue(all(r['session_id'] is None and r['error_class'] == 'unknown' for r in rows))

    def test_invalid_json_event_identity_and_oversize(self):
        for p in (b'{PRIVATE', b'x' * 65537, b'[]', b'{"hook_event_name":"Stop","extra":NaN}',
                  b'{"hook_event_name":"Stop","hook_event_name":"Stop"}',
                  {'hook_event_name': 'StopFailure'}, {'hook_event_name': 'Stop', 'session_id': '/PRIVATE'},
                  {'hook_event_name': []}):
            self.assertTrue(self.call(p))
        self.assertFalse(self.root.exists())

    def test_input_not_closed_is_bounded(self):
        start = time.monotonic()
        with subprocess.Popen(self.command(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE) as p:
            p.stdin.write(b'{'); p.stdin.flush()
            p.wait(timeout=3)
            self.assertEqual(p.returncode, 0)
            self.assertEqual(p.stdout.read(), b'')
            self.assertEqual(p.stderr.read(), b'native_hook:input_timeout\n')
        self.assertLess(time.monotonic() - start, 3)
        self.assertFalse(self.root.exists())

    def test_alias_permissions_and_storage_failure(self):
        base = Path(self.temp.name)
        real = base / 'real'; real.mkdir(mode=0o700)
        alias = base / 'alias'; alias.symlink_to(real, target_is_directory=True)
        p = {'hook_event_name': 'Stop'}
        for root in (alias, alias / 'child', base / 'missing' / 'child'):
            self.assertTrue(self.call(p, root=root))
        self.assertFalse(list(real.iterdir()))
        real.chmod(0o755)
        self.assertTrue(self.call(p, root=real))
        real.chmod(0o700)
        (real / '.lock').symlink_to(base / 'outside')
        self.assertTrue(self.call(p, root=real))
        self.assertFalse((base / 'outside').exists())

    def test_full_busy_and_parallel_receipts(self):
        payload = {'hook_event_name': 'Stop'}
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(lambda _: self.call(payload), range(16)))
        rows = self.records()
        self.assertEqual(len(rows), sum(not r for r in results))
        self.assertGreater(len(rows), 0)
        self.assertEqual(len({r['receive_id'] for r in rows}), len(rows))
        for i in range(256 - len(rows)):
            (self.root / ('occupied-' + str(i))).touch()
        self.assertEqual(self.call(payload), b'native_hook:output_full\n')
        import fcntl
        with (self.root / '.lock').open('rb') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertTrue(self.call(payload))

    def test_local_only_import_and_call_boundary(self):
        import ast
        tree = ast.parse(SCRIPT.read_text())
        imports = {n.names[0].name for n in ast.walk(tree) if isinstance(n, ast.Import)}
        self.assertEqual(imports, {'datetime', 'fcntl', 'json', 'os', 're', 'select',
                                   'stat', 'sys', 'time', 'uuid'})
        banned = {'system', 'popen', 'spawn', 'fork', 'execv', 'execve', 'getenv'}
        for node in ast.walk(tree):
            if isinstance(node, ast.Attribute):
                self.assertNotIn(node.attr, banned)


if __name__ == '__main__':
    unittest.main()
