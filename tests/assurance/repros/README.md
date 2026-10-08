# Parked client-stream diagnostic

This explicit real-QUIC reproducer is excluded from the ordinary TypeScript
suite. The original failure is retained; the defect is fixed by `29fc3409`.
The normal lifecycle suite now contains regression cases for parked success,
rejection and deadline, with eventual native cleanup. The historical expected
failure below applies to the recorded pre-fix source, not current fixed source.
No `xfail` or expected-failure green result was used.

With a source-compatible native addon available, run from the repository root:

```sh
rtk proxy bindings/typescript/node_modules/.bin/vitest run \
  --config tests/assurance/repros/vitest-parked.config.ts
```

The preserved first run used the existing macOS arm64 test-hooks addon reporting
0.3.805, SHA-256
`c2c37112dea67c0375511597cd78f1c6e4eec703ee13c708ea01e42e7e0c1397`.
It was temporarily substituted for the local addon, which was restored after
the diagnostic. No fresh native build was performed.

Original pre-fix failure: the actual native response reports `INVALID_ARGUMENT`, with
zero server live calls, while the outer JS `clientStream` Promise stays pending
for the five-second watchdog. The public response hang is the confirmed defect.
The cooperative request iterator also exposes `return()` cleanup, which is not
invoked before that watchdog; this is a separate cleanup assertion. Arbitrary
user-source `next()` work cannot generally be force-cancelled.

The test always releases its parked source before waiting for teardown and
closes both native peers. The exact first source, output and identities are in
`.aster/assurance/codex-rpc-lifecycle-20261008/`. The historical explicit-run config has not been rerun against the fix;
final normal focused tests pass 14/14. Their source/artifact scopes and raw logs
are in `.aster/assurance/codex-ts-client-stream-fix-20261008/`.
