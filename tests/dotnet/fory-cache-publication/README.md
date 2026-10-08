# Fory 1.7.6 concurrent metadata publication reproduction

Status: confirmed scheduled-interleaving codec defect; no fix validated.
This standalone diagnostic intentionally exits **2** on the affected generator.
It is not a normal passing CI test or proof that CI run 160 took this ordering.

The payload is created through public `ThreadSafeFory` APIs. One decoder pauses
immediately after the generated serializer stores its shared metadata reference
and before it stores the corresponding match flag. A second decoder reads the
same valid payload. The only change to package-generated serializer code is
`ProbeHook.AfterPublish()` between those two existing writes. No metadata values
or payload bytes are changed. Waits are bounded to ten seconds.

`generated/` preserves the original output from the installed Apache.Fory 1.7.6
source generator and is excluded from compilation. `PatchedGenerated.cs` is that
output with the single scheduling hook. The project disables the analyzer to
avoid generating a duplicate serializer. It has no native Aster dependency.

Original agent command (run in the retained evidence directory):

```sh
dotnet run --project Probe.csproj --no-restore
```

For a fresh checkout, restore this project before running it. The already tested
source files are copied here byte-for-byte; they were not rerun at this new path.
Observed on macOS ARM64, SDK 10.0.301, target net9.0:

```text
second Count=0 Message=
first Count=37 Message=correct
```

Exit status: 2. A correct decoder must return `37` and `correct` to both callers.
The original unscheduled 16-worker/128-round concurrency test passed; this
reproduction makes one valid scheduling order deterministic.

SHA-256 provenance:

| Artifact | SHA-256 |
| --- | --- |
| Apache.Fory 1.7.6 package | `fe9bd97e8f8f6797c2fc3ce43dd13050a3fd1b5487ca37c1c3800ef5bf450a5c` |
| Fory.Generator.dll | `41eafe168a65ab3ce246bc28a29292aa8e18b6327a4544735852d2f024bd4d2d` |
| Fory.dll | `4f1ba6a5e1c8d11138ac4312247193f6b76a7532695a243c8de2365ce22bdde0` |
| Original emitted source | `ea35c1b7ef4826f32a8b19610b3e7791f3e8e5e85d7b00beb2e10b2a37872cdf` |
| Instrumented emitted source | `39fe9706ce37aef6f8a38c93349f15b888f45569f52b2c4a97e1c358acb3faea` |

Generated serializer logic comes from Apache Fory 1.7.6 (Apache License 2.0).
This fixture preserves evidence; production must be corrected in the generator,
not by copying this serializer into the SDK. See the
[investigation](../../../docs/_internal/dotnet_concurrent_rpc_investigation.md).
