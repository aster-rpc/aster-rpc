#!/usr/bin/env python3
"""Every feature-gated Rust test target must be run by some CI invocation.

A test file behind `#![cfg(feature = ...)]` (or a `[[test]]` with
`required-features`) compiles to an empty binary that reports "0 passed"
under any feature set that does not satisfy its gate, which in a job log is
indistinguishable from a pass. That is how fourteen enrollment targets and
seventeen coordination-testing targets went unrun for months (2026-10-16).

This guard reads every `cargo test` invocation in the Forgejo workflows and
`scripts/ci/*.sh`, resolves the features each enables on each package (with
the crate's feature closure), and requires every test target -- and every
module of a multi-file target such as `aster/tests/it/` -- of every crate CI
tests to be selected by at least one invocation whose features satisfy its
gate. Platform predicates are evaluated as on the Linux runners; a target
that is off on Linux under every feature set is platform-specific, not
feature-gated, and is not this guard's concern.

Invocations whose target list is computed at run time (discovery by #![cfg]
line) count as selecting every target of their package; the step-specific
tests pin what each discovery actually matches.
"""
from __future__ import annotations

import re
import shlex
import tomllib
import unittest
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Workspace crates with feature-gated test targets that no CI job tests at
# all. Listed so that adding one is a visible decision; empty it by giving the
# crate a test step, which brings it under the guard below.
UNTESTED_CRATES: dict[str, str] = {
    'aster-expose': 'no CI step runs `cargo test -p aster-expose` (tests/edge_integration.rs needs `edge`)',
}


# ---------------------------------------------------------------- crates ---

@dataclass
class Unit:
    """A test target, or one module of a multi-file test target."""
    crate: str
    target: str
    label: str
    gates: list[str]
    required: set[str]


@dataclass
class Crate:
    name: str
    path: Path
    features: dict[str, list[str]]
    units: list[Unit] = field(default_factory=list)

    def closure(self, requested: set[str], default: bool, all_features: bool) -> set[str]:
        if all_features:
            return set(self.features)
        pending = list(requested) + (['default'] if default else [])
        enabled: set[str] = set()
        while pending:
            name = pending.pop()
            if name in enabled or '/' in name or name.startswith('dep:'):
                continue
            enabled.add(name)
            pending += self.features.get(name, [])
        return enabled


def inner_cfgs(text: str) -> list[str]:
    """The predicates of the file's leading `#![cfg(...)]` attributes."""
    gates = []
    for match in re.finditer(r'^#!\[cfg\(', text, re.M):
        depth, start = 1, match.end()
        i = start
        while depth:
            depth += {'(': 1, ')': -1}.get(text[i], 0)
            i += 1
        gates.append(text[start:i - 1])
    return gates


def module_units(crate: str, target: str, main: Path, gates: list[str], required: set[str]) -> list[Unit]:
    units = []
    text = main.read_text()
    for match in re.finditer(r'^((?:#\[[^\n]*\]\n)*)(?:pub )?mod (\w+);', text, re.M):
        attrs, name = match[1], match[2]
        path_attr = re.search(r'#\[path = "([^"]+)"\]', attrs)
        candidates = [main.parent / path_attr[1]] if path_attr else [main.parent / f'{name}.rs', main.parent / name / 'mod.rs']
        source = next((c for c in candidates if c.exists()), None)
        if source is None:
            continue
        outer = [m for m in re.findall(r'#\[cfg\((.*)\)\]', attrs)]
        units.append(Unit(crate, target, f'{target}::{name}', gates + outer + inner_cfgs(source.read_text()), required))
    return units


def load_workspace() -> dict[str, Crate]:
    members = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['members']
    crates = {}
    for member in members:
        manifest = tomllib.loads((ROOT / member / 'Cargo.toml').read_text())
        crate = Crate(manifest['package']['name'], ROOT / member, manifest.get('features', {}))
        tests_dir = crate.path / 'tests'
        declared = {t['name']: t for t in manifest.get('test', [])}
        sources = {}
        if tests_dir.is_dir():
            for path in sorted(tests_dir.glob('*.rs')):
                sources[path.stem] = path
            for path in sorted(tests_dir.glob('*/main.rs')):
                sources[path.parent.name] = path
        for name, entry in declared.items():
            if 'path' in entry:
                sources[name] = crate.path / entry['path']
        for name, source in sorted(sources.items()):
            required = set(declared.get(name, {}).get('required-features', []))
            gates = inner_cfgs(source.read_text())
            crate.units.append(Unit(crate.name, name, name, gates, required))
            if source.name == 'main.rs':
                crate.units += module_units(crate.name, name, source, gates, required)
        crates[crate.name] = crate
    return crates


# ------------------------------------------------------------- cfg logic ---

LINUX = {'unix': True, 'windows': False, 'test': True, 'debug_assertions': True}
LINUX_KEYS = {'target_os': 'linux', 'target_family': 'unix', 'target_arch': 'x86_64', 'target_pointer_width': '64'}


def evaluate(predicate: str, features: set[str]) -> bool:
    tokens = re.findall(r'"[^"]*"|\w+|[(),=]', predicate)
    pos = 0

    def expr() -> bool:
        nonlocal pos
        name = tokens[pos]
        pos += 1
        if name in ('all', 'any', 'not'):
            assert tokens[pos] == '(', predicate
            pos += 1
            values = []
            while tokens[pos] != ')':
                values.append(expr())
                if tokens[pos] == ',':
                    pos += 1
            pos += 1
            if name == 'not':
                assert len(values) == 1, predicate
                return not values[0]
            return all(values) if name == 'all' else any(values)
        if pos < len(tokens) and tokens[pos] == '=':
            value = tokens[pos + 1].strip('"')
            pos += 2
            if name == 'feature':
                return value in features
            assert name in LINUX_KEYS, f'teach the guard the cfg key {name!r} ({predicate})'
            return LINUX_KEYS[name] == value
        assert name in LINUX, f'teach the guard the cfg predicate {name!r} ({predicate})'
        return LINUX[name]

    value = expr()
    assert pos == len(tokens), predicate
    return value


def unit_enabled(unit: Unit, features: set[str]) -> bool:
    return unit.required <= features and all(evaluate(g, features) for g in unit.gates)


# ------------------------------------------------------------ invocations ---

@dataclass
class Invocation:
    source: str
    packages: list[str]
    features: set[str]
    default: bool
    all_features: bool
    tests: set[str]
    all_tests: bool
    dynamic: bool
    command: str


def ci_scripts() -> list[tuple[str, str]]:
    """(label, shell text) per workflow step and per CI script."""
    chunks = []
    for path in sorted((ROOT / '.forgejo/workflows').glob('*.yml')):
        text = path.read_text()
        for i, step in enumerate(re.split(r'\n\s*- name: ', text)):
            chunks.append((f'{path.name} step {step.splitlines()[0][:60]!r}' if i else path.name, step))
    for path in sorted((ROOT / 'scripts/ci').glob('*.sh')):
        chunks.append((str(path.relative_to(ROOT)), path.read_text()))
    return chunks


def strip_comments(text: str) -> str:
    return '\n'.join(line for line in text.splitlines() if not line.lstrip().startswith('#'))


def cargo_test_args(line: str) -> str | None:
    match = re.search(r'(?<![\w-])cargo test\b', line)
    if not match:
        return None
    rest = line[match.end():]
    cut = re.search(r'\s2>&1|\s\|\|?\s|\s&&\s|;|\)', rest)
    return rest[:cut.start()] if cut else rest


def expand_invocations(label: str, text: str) -> list[tuple[str, str]]:
    """Every `cargo test` argument string in one shell chunk, with shell
    functions, literal arrays and literal for-loops resolved."""
    text = strip_comments(re.sub(r'\\\n\s*', ' ', text))
    functions = {}
    for match in re.finditer(r'^\s*(\w+)\(\)\s*\{\n(.*?)\n\s*\}\s*$', text, re.M | re.S):
        body = match[2]
        args = None
        for line in body.splitlines():
            args = cargo_test_args(line) if args is None else args
        if args is not None and '"$@"' in args:
            shifts = sum(int(n or 1) for n in re.findall(r'\bshift(?: (\d+))?\b', body))
            functions[match[1]] = (args, shifts)
            text = text.replace(match[0], '')
    # Literal arrays only: one built up with `name+=(...)` is computed.
    arrays = {m[1]: m[2] for m in re.finditer(r'(\w+)=\(([^()$]*)\)', text)
              if f'{m[1]}+=(' not in text}
    loops = {m[1]: m[2].split() for m in re.finditer(r'\bfor (\w+) in ([^;\n$*]+); do', text)}
    found = []
    for line in text.splitlines():
        args = cargo_test_args(line)
        words = line.split()
        if args is None and words and words[0] in functions:
            template, shifts = functions[words[0]]
            call = shlex.split(line)[1 + shifts:]
            args = template.replace('"$@"', ' '.join(shlex.quote(a) for a in call))
        if args is None:
            continue
        for name, body in arrays.items():
            args = args.replace(f'"${{{name}[@]}}"', body)
        variants = [args]
        for name, values in loops.items():
            if re.search(rf'\$\{{?{name}\b', args):
                variants = [re.sub(rf'"?\$\{{?{name}\}}?"?', value, v) for v in variants for value in values]
        found += [(label, v) for v in variants]
    return found


def parse(label: str, args: str) -> Invocation:
    tokens = shlex.split(args)
    inv = Invocation(label, [], set(), True, False, set(), False, False, args.strip())
    selected = False
    i = 0
    takes_value = {'--color', '--target', '--target-dir', '--manifest-path', '--profile', '-j', '--jobs', '--bin', '--example', '--bench'}
    while i < len(tokens):
        token = tokens[i]
        value = tokens[i + 1] if i + 1 < len(tokens) else ''
        if token == '--':
            break
        if token in ('-p', '--package'):
            inv.packages.append(value)
            i += 1
        elif token in ('--features', '-F') or token.startswith('--features='):
            raw = token.split('=', 1)[1] if '=' in token else value
            i += 0 if '=' in token else 1
            assert '$' not in raw, f'{label}: unresolved --features {raw!r} in `cargo test{args}`'
            inv.features |= {f for f in re.split(r'[ ,]+', raw) if f}
        elif token == '--no-default-features':
            inv.default = False
        elif token == '--all-features':
            inv.all_features = True
        elif token == '--test':
            selected = True
            if '$' in value:
                inv.dynamic = True
            else:
                inv.tests.add(value)
            i += 1
        elif token in ('--tests', '--all-targets'):
            selected, inv.all_tests = True, True
        elif token in ('--lib', '--bins', '--doc', '--examples', '--benches'):
            selected = True
        elif token in takes_value:
            selected = selected or token in ('--bin', '--example', '--bench')
            i += 1
        elif '$' in token:
            # A computed argument list (`"${test_args[@]}"`).
            selected, inv.dynamic = True, True
        i += 1
    if not selected:
        inv.all_tests = True
    return inv


def ci_invocations() -> list[Invocation]:
    return [parse(label, args) for chunk in ci_scripts() for label, args in expand_invocations(*chunk)]


# ------------------------------------------------------------------ tests ---

class FeatureGateCoverage(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.crates = load_workspace()
        cls.invocations = ci_invocations()

    def covering(self, unit: Unit) -> list[Invocation]:
        crate = self.crates[unit.crate]
        hits = []
        for inv in self.invocations:
            if unit.crate not in inv.packages:
                continue
            if not (inv.all_tests or inv.dynamic or unit.target in inv.tests):
                continue
            if unit_enabled(unit, crate.closure(inv.features, inv.default, inv.all_features)):
                hits.append(inv)
        return hits

    def test_every_feature_gated_test_target_runs_in_some_ci_invocation(self):
        tested = {p for inv in self.invocations for p in inv.packages}
        missing = []
        for name, crate in sorted(self.crates.items()):
            if name not in tested:
                continue
            for unit in crate.units:
                everything = crate.closure(set(crate.features), True, False)
                if not unit_enabled(unit, everything) and not unit_enabled(unit, crate.closure(set(), True, False)):
                    continue  # off on Linux under every feature set: platform-specific
                if not self.covering(unit):
                    gate = ' && '.join(unit.gates + [f'required-features {sorted(unit.required)}' if unit.required else ''])
                    missing.append(f'{name} {unit.label}: [{gate.strip(" &")}]')
        self.assertEqual(missing, [], 'feature-gated test targets no CI `cargo test` selects with its features '
                         '(each compiles to "0 passed" everywhere CI builds it):\n  ' + '\n  '.join(missing))

    def test_untested_crates_are_the_listed_ones(self):
        tested = {p for inv in self.invocations for p in inv.packages}
        gated = {name for name, crate in self.crates.items()
                 if any(unit.gates or unit.required for unit in crate.units)}
        self.assertEqual(gated - tested, set(UNTESTED_CRATES),
                         'crates with feature-gated tests that no CI step tests; give them a test step '
                         'or list them in UNTESTED_CRATES with a reason')

    def test_parser_sees_the_known_invocations(self):
        # Guard the guard: if the shell parsing silently stopped finding
        # invocations, every target would look unrun -- or, with a dynamic
        # one misread as covering everything, every target would look run.
        by_package = {}
        for inv in self.invocations:
            for package in inv.packages:
                by_package.setdefault(package, []).append(inv)
        coordination = [i for i in by_package['aster'] if i.features == {'coordination-testing', 'binding-support'}]
        self.assertTrue(coordination and all(i.dynamic for i in coordination), coordination)
        loop = [i for i in by_package['aster'] if i.features == {'coordination-client'} and not i.default]
        self.assertTrue(loop, 'the Coordination tests loop over coordination / coordination-client')
        self.assertTrue(any(i.tests == {'coordination'} and 'coordination-fixture' in i.features
                            for i in by_package['aster_transport_ffi']))
        self.assertTrue(any(i.all_features for i in by_package['aster-otel']))

    def test_coordination_discovery_covers_every_coordination_target(self):
        # The Coordination tests step: the loop runs bare `coordination` /
        # `coordination-client` gates under their own graphs, and the
        # discovery after it runs every other coordination-gated target
        # under coordination-testing,binding-support with a count.
        workflow = (ROOT / '.forgejo/workflows/linux.yml').read_text()
        step = workflow.split('      - name: Coordination tests\n', 1)[1].split('\n      - name: ', 1)[0]
        broad = re.search(r"matches=\$\(grep -l -E '([^']+)' aster/tests/\*\.rs\)", step)
        bare = re.search(r"grep -qE '([^']+)' \"\$path\"; then continue", step)
        self.assertIsNotNone(broad)
        self.assertIsNotNone(bare)
        self.assertIn('-p aster --features coordination-testing,binding-support', step)
        self.assertIn('reported no passing tests under coordination-testing,binding-support', step)
        crate = self.crates['aster']
        enabled = crate.closure({'coordination-testing', 'binding-support'}, True, False)
        discovered = []
        for path in sorted((ROOT / 'aster/tests').glob('*.rs')):
            text = path.read_text()
            if 'coordination' not in ''.join(inner_cfgs(text)):
                continue
            if re.search(broad[1], text, re.M) and not re.search(bare[1], text, re.M):
                discovered.append(path.stem)
                unit = next(u for u in crate.units if u.label == path.stem)
                self.assertTrue(unit_enabled(unit, enabled), f'{path.name} needs features the run does not enable')
            else:
                self.assertTrue(re.search(r'^#!\[cfg\(feature = "coordination(-client)?"\)\]$', text, re.M),
                                f'{path.name} is coordination-gated but neither the loop nor the discovery runs it')
        for target in ('coordination_client', 'coordination_store', 'coordination_process_loss',
                       'coordination_provisioning', 'coordination_topology10'):
            self.assertIn(target, discovered)


if __name__ == '__main__':
    unittest.main()
