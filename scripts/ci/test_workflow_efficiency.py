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
            self.assertEqual(len(calls), 7)
            evidence = json.loads(output.read_text())
            self.assertEqual(evidence['runtimes']['node'][0]['exit_code'], 124)
            self.assertEqual(len(evidence['runtimes']['bun']), 3)

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
            self.assertEqual(len(calls), 7)
            self.assertEqual(calls[0][:5], ['npx', 'vitest', 'run', '--reporter=default', '--reporter=json'])
            self.assertEqual(calls[1][-3:], ['observability-logs.test.ts=7', 'observability-export.test.ts=13', 'o7-server-health.test.ts=3'])
            evidence = json.loads(output.read_text())
            self.assertEqual(len(evidence['runtimes']['node']), 4)
            self.assertEqual(len(evidence['runtimes']['bun']), 3)
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
            self.assertEqual(len(calls), 7)
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
        self.assertEqual(dependencies['typescript'], {'rust-tests', 'test'})
        for job in ('build-linux-arm64-wheel', 'build-windows-wheel', 'ffi-release-linux', 'ffi-natives-cross', 'npm-native-linux-x64', 'npm-native-cross', 'npm-native-darwin', 'relay-image', 'mobile-ios', 'mobile-android', 'nightly-sanitize', 'nightly-miri'):
            self.assertTrue({'rust-tests', 'test', 'typescript'} <= ancestors(job), job)
        ts = blocks['typescript']
        self.assertIn('--features coordination-fixture,test-hooks --bin aster-coordination-fixture', ts)
        self.assertLess(ts.index('Verify and reuse the tested normal NAPI addon'), ts.index('run: bun run build:test-hooks'))
        self.assertIn('release-candidate.py verify-input', ts)
        self.assertNotIn('run: bun run build\n', ts)

    def test_actual_o7_checker_accepts_full_report_and_rejects_missing_failed_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / 'full.json'
            pairs = [('observability-logs.test.ts', 7), ('observability-export.test.ts', 13), ('o7-server-health.test.ts', 3)]
            data = {'testResults': [{'name': '/suite/' + name, 'assertionResults': [{'status': 'passed'} for _ in range(count)]} for name, count in pairs]}
            data['testResults'].append({'name': '/suite/unrelated.test.ts', 'assertionResults': [{'status': 'passed'}]})
            command = ['node', str(ROOT / 'bindings/typescript/scripts/check-vitest-count.mjs'), str(report)] + [f'{name}={count}' for name, count in pairs]
            report.write_text(json.dumps(data))
            self.assertEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 0)
            original = json.dumps(data)
            for status in ('failed', 'pending', 'todo'):
                with self.subTest(status=status):
                    data = json.loads(original)
                    data['testResults'][0]['assertionResults'][0]['status'] = status
                    report.write_text(json.dumps(data))
                    self.assertNotEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 0)
            for change in ('missing-case', 'extra-case'):
                with self.subTest(change=change):
                    data = json.loads(original)
                    results = data['testResults'][0]['assertionResults']
                    if change == 'missing-case':
                        results.pop()
                    else:
                        results.append({'status': 'passed'})
                    report.write_text(json.dumps(data))
                    self.assertNotEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 0)
            data = json.loads(original)
            data['testResults'].pop(0)
            report.write_text(json.dumps(data))
            self.assertNotEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 0)

    def test_macos_and_linux_observability_inventory_agrees(self):
        workflow = (ROOT / '.forgejo/workflows/linux.yml').read_text()
        macos = re.search(r'node ../../scripts/check-vitest-count\.mjs "\$out" \\\n((?:[^\n]*test\.ts=\d+[^\n]*\n)+)', workflow)
        self.assertIsNotNone(macos)
        pairs = re.findall(r'[\w-]+\.test\.ts=\d+', macos[1])
        linux = R.gates(ROOT, ROOT / 'temporary')[1][3][3:]
        self.assertEqual(pairs, linux)

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

    def test_macos_ffi_runs_platform_checks_and_linux_keeps_full_suites(self):
        workflow = (ROOT / '.forgejo/workflows/linux.yml').read_text()

        def job(name):
            body = workflow.split(f'\n  {name}:\n', 1)[1]
            return re.split(r'\n  [\w-]+:\n', body, maxsplit=1)[0]

        macos, go, java = job('ffi-macos'), job('ffi-go'), job('ffi-java')
        # Full suites stay on Linux.
        self.assertIn('go test -count=1 -timeout 600s ./...', go)
        self.assertIn('mvn -B package -Dnative.classifier=linux-x86_64', java)
        self.assertNotIn('-Dtest=', java)
        # macOS runs only the selected platform checks.
        self.assertNotIn('./...\n', macos.replace('go vet ./...\n', '').replace('CGO_ENABLED=0 go test ./...\n', ''))
        self.assertNotIn('go run ./cmd', macos)
        run = re.search(r"-run '\^\(TestABI_\|(TestEndpoint_ConnectAccept)\$\|(TestRPC_AllPatterns)\$\)'", macos)
        self.assertIsNotNone(run, 'Go platform-check filter')
        self.assertIn('"$passed" -lt 7', macos)
        go_sources = ''.join(p.read_text() for p in (ROOT / 'bindings/go').glob('*_test.go'))
        self.assertEqual(len(re.findall(r'^func TestABI_\w+\(t \*testing\.T\)', go_sources, re.M)), 5)
        for name in run.groups():
            self.assertRegex(go_sources, rf'(?m)^func {name}\(t \*testing\.T\)')
        self.assertIn("-Dtest='AbiContractTest,DispatchServerE2ETest#allFourPatternsAndErrors'", macos)
        self.assertIn('check site.aster.AbiContractTest 9', macos)
        self.assertIn('check site.aster.server.DispatchServerE2ETest 1', macos)
        tests = ROOT / 'bindings/java/aster-runtime/src/test/java/site/aster'
        self.assertEqual((tests / 'AbiContractTest.java').read_text().count('@Test'), 9)
        self.assertIn('void allFourPatternsAndErrors()', (tests / 'server/DispatchServerE2ETest.java').read_text())
        self.assertIn('site.aster.ffi.IrohLibrary.getInstance()', macos)

    def test_qualifying_pytest_has_no_retries_or_sibling_cleanup(self):
        workflow = (ROOT / '.forgejo/workflows/linux.yml').read_text()
        commands = [line for line in workflow.splitlines() if '.venv/bin/pytest ' in line or '-m pytest ' in line]
        self.assertGreaterEqual(len(commands), 6)
        self.assertTrue(all('-o reruns=0' in line for line in commands))
        self.assertNotIn('--reruns=2', workflow)
        self.assertNotIn('runner_work=', workflow)
        self.assertNotIn("-path '*/tmp/aster-target'", workflow)

    def test_every_enrollment_gated_aster_target_runs_with_a_count(self):
        # rust-tests discovers aster's enrollment / identity-publisher targets
        # by their #![cfg] line and requires each to report passing tests
        # under `--features identity-publisher`. A target whose gate needs a
        # feature that run does not enable would compile to zero tests there.
        import tomllib
        workflow = (ROOT / '.forgejo/workflows/linux.yml').read_text()
        step = workflow.split('- name: Enrollment JWT, durable policy and offline-root bootstrap', 1)[1]
        step = step.split('\n      - name: ', 1)[0]
        pattern = re.search(r"grep -l -E '([^']+)' aster/tests/\*\.rs", step)
        self.assertIsNotNone(pattern, 'discovery of enrollment-gated aster targets')
        self.assertIn('-p aster --features identity-publisher', step)
        self.assertIn('reported no passing tests under identity-publisher', step)
        features = tomllib.loads((ROOT / 'aster/Cargo.toml').read_text())['features']
        enabled, pending = set(), ['default', 'identity-publisher']
        while pending:
            name = pending.pop()
            if name in enabled:
                continue
            enabled.add(name)
            pending += [f for f in features.get(name, []) if f in features]
        discovered = []
        for path in sorted((ROOT / 'aster/tests').glob('*.rs')):
            text = path.read_text()
            if not re.search(pattern[1], text, re.M):
                continue
            discovered.append(path.stem)
            gate = re.search(r'^#!\[cfg\((.*?)\)\]\s*$', text, re.M | re.S)
            self.assertNotIn('not(', gate[1], path.name)
            needed = set(re.findall(r'feature\s*=\s*"([^"]+)"', gate[1]))
            self.assertLessEqual(needed, enabled, f'{path.name} needs features the run does not enable')
        for target in ('enrollment_gateway_identity', 'enrollment_provider_tokens', 'node_config', 'identity_publisher', 'enrollment_replica_trust'):
            self.assertIn(target, discovered)

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
