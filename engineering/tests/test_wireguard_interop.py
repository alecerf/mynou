"""The optional peer job must fail closed on incomplete or malformed evidence."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import wireguard_interop as interop


def record(names=interop.SCENARIOS):
    return '\n'.join(json.dumps(dict(scenario=name, outcome='pass', milliseconds=1,
                                    source='synthetic fixture', observed='observed'))
                     for name in names)


class EvidenceTests(unittest.TestCase):
    def test_requires_all_scenarios_once(self):
        self.assertEqual(len(interop.validate(record())), 13)
        for names in ((), interop.SCENARIOS[:-1], interop.SCENARIOS + ('extra',),
                      interop.SCENARIOS[:-1] + (interop.SCENARIOS[0],)):
            with self.subTest(names=names), self.assertRaises(ValueError):
                interop.validate(record(names))

    def test_rejects_malformed_or_failed_entries(self):
        entry = json.loads(record(('one',)))
        cases = ['[]', 'null', '{', record(('one',)).replace('"pass"', '"fail"'),
                 record(('one',)).replace('"outcome": "pass"', '"outcome":"fail","outcome":"pass"')]
        for key, value in (('milliseconds', True), ('milliseconds', -1), ('scenario', []),
                           ('source', 'x' * (interop.MAX_FIELD + 1))):
            cases.append(json.dumps(dict(entry, **{key: value})))
        cases.append(json.dumps(dict(entry, extra='unrecognized')))
        for text in cases:
            with self.subTest(text=text[:80]), self.assertRaises(ValueError):
                interop.validate(text, ('one',))

    def test_bounds_result_before_parsing(self):
        with self.assertRaises(ValueError):
            interop.validate(' ' * (interop.MAX_RECORD_BYTES + 1))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'record'
            path.write_bytes(b'x' * (interop.MAX_RECORD_BYTES + 1))
            with self.assertRaises(ValueError):
                interop.read_record(path)

    def test_diagnostic_run_cannot_look_like_gate_evidence(self):
        text = interop.summary({}, {}, interop.validate(record()), partial=True)
        self.assertIn('not evidence for the interoperability gate', text)
        self.assertIn('not universal', text)

    def test_pin_removal_is_rejected(self):
        dockerfile = interop.DOCKERFILE.read_text()
        self.assertIn('h1:whnFRl', interop.pins(dockerfile)['modules'][0])
        with self.assertRaises(ValueError):
            interop.pins(dockerfile.replace('@sha256:', ':'))

    def test_build_timeout_removes_the_build_container(self):
        calls = []

        def docker(*args, **kwargs):
            calls.append(args)
            if args[0] == 'run':
                raise subprocess.TimeoutExpired('docker', 1)
            return subprocess.CompletedProcess(args, 0, '', '')

        with tempfile.TemporaryDirectory() as directory, \
             patch.object(interop, 'ROOT', Path(directory)), \
             patch.object(interop, 'docker', side_effect=docker), \
             self.assertRaises(subprocess.TimeoutExpired):
            interop.build_test('rust', 'target', 'test-build')
        self.assertEqual(calls[-1], ('rm', '--force', '--volumes', 'test-build'))

    def test_test_timeout_removes_both_containers(self):
        calls = []

        def docker(*args, **kwargs):
            calls.append(args)
            if args[0] == 'run':
                raise subprocess.TimeoutExpired('docker', 1)
            return subprocess.CompletedProcess(args, 0, '', '')

        args = type('Args', (), dict(rust_image='rust', peer_image='peer', target_volume='target',
                                     scratch=None, scenario=None))()
        with patch.object(interop, 'build_test', return_value='/workspace/target/test'), \
             patch.object(interop, 'start_peer'), patch.object(interop, 'environment', return_value={}), \
             patch.object(interop, 'docker', side_effect=docker), \
             patch.dict(interop.os.environ, {}, clear=True), \
             patch('builtins.print'), self.assertRaises(SystemExit):
            interop.run(args)
        removed = [call for call in calls if call[0] == 'rm']
        self.assertEqual(len(removed), 2)
        self.assertTrue(removed[0][-1].startswith('mynou-wireguard-test-'))
        self.assertTrue(removed[1][-1].startswith('mynou-wireguard-peer-'))


if __name__ == '__main__':
    unittest.main()
