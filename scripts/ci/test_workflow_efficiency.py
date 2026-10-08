#!/usr/bin/env python3
"""Offline checks for preserved runtime gates, cleanup and cache attribution."""
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('runtime_gates', ROOT / 'scripts/ci/typescript-runtime-gates.py')
R = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(R)


class RuntimeGates(unittest.TestCase):
    def test_timeout_is_retained_without_retry_and_bun_still_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            calls = []
            def invoke(argv, **kwargs):
                calls.append(argv)
                if len(calls) == 1:
                    self.assertEqual(kwargs['timeout'], 600)
                    raise subprocess.TimeoutExpired(argv, kwargs['timeout'])
                self.assertEqual(kwargs['timeout'], 120)
                return subprocess.CompletedProcess(argv, 0)
            output = root / 'out.json'
            self.assertEqual(R.run(root, output, root, invoke), 1)
            self.assertEqual(len(calls), 5)
            evidence = json.loads(output.read_text())
            self.assertEqual(evidence['runtimes']['node'][0]['exit_code'], 124)
            self.assertEqual(len(evidence['runtimes']['bun']), 2)

    def test_hung_subprocess_is_terminated(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(subprocess.TimeoutExpired):
                R.invoke_bounded([sys.executable, '-c', 'import time; time.sleep(60)'],
                                 cwd=directory, env=os.environ, check=False, timeout=0.1)

    def test_all_candidate_and_promotion_jobs_have_finite_deadlines(self):
        for filename in ('linux.yml', 'release.yml'):
            workflow = (ROOT / '.forgejo/workflows' / filename).read_text()
            body = workflow.split('\njobs:\n', 1)[1]
            matches = list(re.finditer(r'^  ([\w-]+):$', body, re.M))
            self.assertTrue(matches)
            for index, match in enumerate(matches):
                block = body[match.end():matches[index + 1].start() if index + 1 < len(matches) else len(body)]
                timeout = re.search(r'^    timeout-minutes: (\d+)$', block, re.M)
                self.assertIsNotNone(timeout, match[1])
                self.assertTrue(1 <= int(timeout[1]) <= 90, match[1])

    def test_failure_keeps_bun_and_all_existing_node_gates(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stale = root / 'o7-vitest.json'
            stale.write_text('stale')
            calls = []
            def invoke(argv, **kwargs):
                calls.append(argv)
                self.assertFalse(stale.exists())
                self.assertIn('aster::lease::diagnostic=debug', kwargs['env']['ASTER_LOG'])
                self.assertIn('aster::coordination::diagnostic=debug', kwargs['env']['ASTER_LOG'])
                return subprocess.CompletedProcess(argv, 1 if len(calls) == 1 else 0)
            output = root / 'out.json'
            self.assertEqual(R.run(root, output, root, invoke), 1)
            self.assertEqual(len(calls), 5)
            self.assertEqual(calls[0][:5], ['npx', 'vitest', 'run', '--reporter=default', '--reporter=json'])
            self.assertEqual(calls[1][-3:], ['observability-logs.test.ts=7', 'observability-export.test.ts=12', 'o7-server-health.test.ts=2'])
            evidence = json.loads(output.read_text())
            self.assertEqual(len(evidence['runtimes']['node']), 3)
            self.assertEqual(len(evidence['runtimes']['bun']), 2)
            self.assertEqual(evidence['runtimes']['node'][0]['exit_code'], 1)

    def test_missing_node_still_checks_bun_and_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            calls = []
            def invoke(argv, **kwargs):
                calls.append(argv)
                if argv[0] != 'bun':
                    raise FileNotFoundError('missing node')
                return subprocess.CompletedProcess(argv, 0)
            self.assertEqual(R.run(root, root / 'out.json', root, invoke), 1)
            self.assertEqual([c[0] for c in calls[-2:]], ['bun', 'bun'])

    def test_failed_vitest_retains_fresh_full_report_and_all_runtimes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / 'runtime-evidence/typescript.json'
            output.parent.mkdir()
            retained = output.parent / 'vitest.json'
            retained.write_text('stale')
            fresh = json.dumps({'success': False, 'testResults': [{'message': 'full coordination error'}]})
            calls = []
            def invoke(argv, **kwargs):
                calls.append(argv)
                if len(calls) == 1:
                    self.assertFalse(retained.exists())
                    (root / 'o7-vitest.json').write_text(fresh)
                return subprocess.CompletedProcess(argv, 1 if len(calls) == 1 else 0)
            self.assertEqual(R.run(root, output, root, invoke), 1)
            self.assertEqual(retained.read_text(), fresh)
            self.assertEqual(len(calls), 5)
            # A later failed invocation without a fresh report cannot retain
            # the previous run's diagnostic as current evidence.
            self.assertEqual(R.run(root, output, root, lambda argv, **kwargs: subprocess.CompletedProcess(argv, 1)), 1)
            self.assertFalse(retained.exists())

    def test_runtime_is_after_core_and_before_expensive_release_producers(self):
        workflow = (ROOT / '.forgejo/workflows/linux.yml').read_text()
        matches = list(re.finditer(r'^  ([\w-]+):$', workflow[workflow.index('\njobs:'):], re.M))
        body = workflow[workflow.index('\njobs:'):]
        blocks = {match[1]: body[match.end():matches[index+1].start() if index+1 < len(matches) else len(body)] for index, match in enumerate(matches)}
        dependencies = {}
        for job, block in blocks.items():
            need = re.search(r'^    needs: (.+)$', block, re.M)
            dependencies[job] = set(re.findall(r'[\w-]+', need[1])) if need else set()
        def ancestors(job, stack=()):
            self.assertNotIn(job, stack, 'candidate dependency cycle')
            self.assertIn(job, dependencies, 'unknown dependency')
            return dependencies[job] | set().union(*(ancestors(parent, (*stack, job)) for parent in dependencies[job]))
        for job in dependencies:
            ancestors(job)
        self.assertEqual(dependencies['typescript'], {'test'})
        for job in ('build-linux-arm64-wheel', 'build-windows-wheel', 'ffi-release-linux', 'ffi-natives-cross', 'npm-native-linux-x64', 'npm-native-cross', 'npm-native-darwin', 'mobile-ios', 'mobile-android', 'nightly-sanitize', 'nightly-miri'):
            self.assertTrue({'test', 'typescript'} <= ancestors(job), job)
        ts = blocks['typescript']
        self.assertIn('--features coordination-fixture,test-hooks --bin aster-coordination-fixture', ts)
        self.assertLess(ts.index('Verify and reuse the tested normal NAPI addon'), ts.index('run: bun run build:test-hooks'))
        self.assertIn('release-candidate.py verify-input', ts)
        self.assertNotIn('run: bun run build\n', ts)

    def test_actual_o7_checker_accepts_full_report_and_rejects_missing_failed_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / 'full.json'
            pairs = [('observability-logs.test.ts', 7), ('observability-export.test.ts', 12), ('o7-server-health.test.ts', 2)]
            data = {'testResults': [{'name': '/suite/' + name, 'assertionResults': [{'status': 'passed'} for _ in range(count)]} for name, count in pairs]}
            data['testResults'].append({'name': '/suite/unrelated.test.ts', 'assertionResults': [{'status': 'passed'}]})
            command = ['node', str(ROOT / 'bindings/typescript/scripts/check-vitest-count.mjs'), str(report)] + [f'{name}={count}' for name, count in pairs]
            report.write_text(json.dumps(data))
            self.assertEqual(subprocess.run(command, capture_output=True).returncode, 0)
            data['testResults'][0]['assertionResults'][0]['status'] = 'failed'
            report.write_text(json.dumps(data))
            self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)
            data['testResults'].pop(0)
            report.write_text(json.dumps(data))
            self.assertNotEqual(subprocess.run(command, capture_output=True).returncode, 0)

    def test_java_bundle_uses_supplied_library_without_cargo(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / 'bindings/java/scripts/bundle-ffi.sh'
            script.parent.mkdir(parents=True)
            script.write_bytes((ROOT / 'bindings/java/scripts/bundle-ffi.sh').read_bytes())
            uname = subprocess.check_output(['uname', '-s'], text=True).strip()
            machine = subprocess.check_output(['uname', '-m'], text=True).strip()
            system, extension = ('macos', 'dylib') if uname == 'Darwin' else ('linux', 'so')
            arch = 'aarch64' if machine in ('arm64', 'aarch64') else 'x86_64'
            name = f'libaster_transport_ffi.{extension}'
            native = root / 'target/release' / name
            native.parent.mkdir(parents=True)
            native.write_bytes(b'exact-verified-release-native')
            fakebin = root / 'bin'
            fakebin.mkdir()
            cargo = fakebin / 'cargo'
            cargo.write_text('#!/bin/sh\necho forbidden-rebuild >&2\nexit 99\n')
            cargo.chmod(0o755)
            with patch.dict(os.environ, {'PATH': str(fakebin) + os.pathsep + os.environ['PATH']}):
                subprocess.run(['bash', str(script)], check=True, stdout=subprocess.PIPE)
            bundled = root / 'bindings/java/aster-runtime/src/main/resources/native' / f'{system}-{arch}' / name
            self.assertEqual(bundled.read_bytes(), native.read_bytes())

    def test_qualifying_pytest_has_no_retries_or_sibling_cleanup(self):
        workflow = (ROOT / '.forgejo/workflows/linux.yml').read_text()
        commands = [line for line in workflow.splitlines() if '.venv/bin/pytest ' in line or '-m pytest ' in line]
        self.assertGreaterEqual(len(commands), 6)
        self.assertTrue(all('-o reruns=0' in line for line in commands))
        self.assertNotIn('--reruns=2', workflow)
        self.assertNotIn('runner_work=', workflow)
        self.assertNotIn("-path '*/tmp/aster-target'", workflow)

    def test_kache_root_and_scope_with_fake_reporter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            reporter = root / 'kache'
            reporter.write_text('#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\nwith open("calls.jsonl", "a") as out: out.write(json.dumps(sys.argv[1:])+"\\n")\nif "--output" in sys.argv: Path(sys.argv[sys.argv.index("--output")+1]).write_text("{}")\n')
            reporter.chmod(0o755)
            for target, scope in [('/cache/kache/targets/job-123', 'shared-linux-aggregate'), ('/runner/job/tmp/aster-target', 'current-job-parent')]:
                with patch.dict(os.environ, {'CARGO_TARGET_DIR': target, 'RUSTC_WRAPPER': str(reporter)}):
                    subprocess.run(['bash', str(ROOT / 'scripts/ci/kache-report.sh')], cwd=root, check=True, stdout=subprocess.PIPE)
                self.assertEqual(json.loads((root / 'cache-evidence/scope.json').read_text())['scope'], scope)
                calls = [json.loads(line) for line in (root / 'calls.jsonl').read_text().splitlines()]
                self.assertEqual(calls[0][calls[0].index('--root')+1], str(Path(target).parent))
                self.assertEqual(len(calls), 3 if scope.startswith('shared') else 2)
                (root / 'calls.jsonl').unlink()


if __name__ == '__main__':
    unittest.main()
