# Assurance evidence tools

Use Python 3.11 or newer. Start with the current work in
[release_test_status.md](../../docs/_internal/release_test_status.md).

```sh
rtk proxy .venv/bin/python scripts/assurance/record.py seed
rtk proxy .venv/bin/python scripts/assurance/record.py run \
  --scenario RPC-02 --scenario RPC-03 \
  --artifact bindings/python/aster/_aster.abi3.so --timeout 60 -- \
  .venv/bin/python -m pytest tests/python/test_review_bidi_lifecycle.py \
  --reruns=0 --timeout=15 -q
```

Evidence goes to ignored `.aster/assurance/` by default; `--output PATH` before
the subcommand selects another retained location. Each invocation gets its own
command log and JSON report; no retries or replacement of earlier results.
The seed derives scenario IDs from the catalogue and refuses to overwrite a
ledger. Populate and review applicability before freezing the variant manifest.

This initial recorder is for exploration. `COMMAND_OK` means only exit status
zero, not scenario PASS. Inspect discovery counts, assertions and skips yourself.
It captures source identity, supplied file hashes, platform and elapsed time;
the command must report runtime versions and actual loaded-library identity.
It does not establish native freshness, disable a test framework's retries,
schedule builds or certify a candidate. Never put secrets in command arguments.

Timeout terminates the command's process group on POSIX, and remains a failure.
Supplied artifact changes or tracked source changes during execution invalidate
the result. Untracked file names are recorded, but their contents are not hashed;
integrate test/source files before candidate qualification. Copy retained
evidence to release storage before retiring a checkout; it is not Git history.

Verify the recorder:

```sh
rtk proxy .venv/bin/python -m unittest discover -s scripts/assurance -p 'test_*.py' -v
```
