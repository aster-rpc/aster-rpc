# C ABI health configuration regression

`health_config.c` exercises real listeners using `aster_config_from_env` and
`aster_config_from_toml`: disabled configuration, occupied-port rejection,
explicit ephemeral-port override, readiness/check failure and recovery, and
shutdown followed by socket rebind. It also checks that node creation copies
its configuration before `aster_config_release`.

Run against an explicitly identified native library, not an arbitrary installed
release. From the repository root on macOS:

```sh
rtk proxy cc -Wall -Wextra -Werror -Iffi tests/assurance/native/health_config.c -Ltarget/debug -laster_transport_ffi -Wl,-rpath,"$PWD/target/debug" -o /tmp/aster-health-config
rtk proxy /tmp/aster-health-config /tmp/aster-health-config.toml
```

On Linux add `-ldl` and select the actual library directory in both `-L` and
rpath. The program prints the library path loaded by `dladdr`; record its hash
and source/build identity with the result. Assertions must remain enabled.
The TOML argument must name an owned temporary file: the fixture writes and
removes it. Environment changes are confined to the test process.

The original Linux 767 artifact failed the precise occupied-port assertion
with `INVALID_ARGUMENT` (configuration was lost) rather than
`CONNECTION_REFUSED`. The fresh macOS library with the metadata fix passes.
The Rust owned-config round-trip regression provides same-platform failing-
before/passing-after proof; the C runs do not claim a Linux fixed build was
executed. Durable raw evidence is under
`.aster/assurance/health-variant-20261008/` (ignored).

## Java confined-arena ownership control

`JavaArenaProbe.java` is a bounded standalone JVM control, separate from the
normal release gate. It sends and verifies exactly 8 MiB through real native
QUIC streams, then checks JVM Native Memory Tracking `Other` committed growth
from the warmed baseline. Completed asynchronous sends must not retain their
FFM payload allocations; the control permits 1 MiB of bookkeeping noise.
It does not claim to measure Rust allocations or all process memory.

First compile the current Java sources and run the named endpoint fixture,
using JDK 25 and an explicitly identified compatible native library:

```sh
rtk proxy /path/to/mvn -f bindings/java/pom.xml -pl aster-runtime -am -Dtest=NodeEndpointTest -Dsurefire.failIfNoSpecifiedTests=false -Daster.ffi.lib=/absolute/path/libaster_transport_ffi.dylib test
rtk proxy python3 tests/assurance/native/run_java_arena_probe.py --java-home /path/to/jdk-25 --native /absolute/path/libaster_transport_ffi.dylib
```

The runner uses the classpath recorded by the named Maven fixture and prints
the selected native library SHA256. Record that library's source provenance and
the Java source hashes with the raw result. Run the same control before and
after an arena ownership correction; do not invoke a broad suite or rebuild
Rust for this control. The fixture owns and closes its streams, connections,
endpoints, and child `jcmd` processes. It checks exact payload bytes before the
allocation assertion, which also verifies that closing temporary arenas after
synchronous native submission preserves the native asynchronous copy.
