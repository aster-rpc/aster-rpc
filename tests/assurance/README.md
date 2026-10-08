# Bounded native loader assurance

`test_native_loader_failures.py` executes real loader/linkage failures on macOS arm64. Prerequisites: clang with x86_64 cross compilation, Python, Node, installed TypeScript workspace dependencies, .NET SDK9+ and restored Aster package dependencies, JDK22+ selected by `JAVA_HOME`, and existing Java target/classes for referenced FFI layout classes. It recompiles the actual IrohLibrary source and current managed Aster binding; it does not compile Rust.

From the repository root, using retained local artifacts:

```sh
rtk proxy env JAVA_HOME=/path/to/jdk \
  ASTER_LOADER_VALID_ADDON=/path/to/retained/napi.dylib \
  ASTER_LOADER_FFI=/path/to/retained/ffi.dylib \
  ASTER_LOADER_EVIDENCE=/path/to/evidence \
  python3 -m unittest discover -s tests/assurance -p test_native_loader_failures.py -v
```

Go requires installed Go1.25+ and cached/resolvable module dependencies. Omit `ASTER_LOADER_FFI` to skip the Go fixture consumer, or `ASTER_LOADER_VALID_ADDON` to skip the native positive control. Skips are explicit exclusions, never proof of loader coverage. Other operating systems skip this Mach-O-specific harness.

Java loads the exact fixture through its production explicit-path override. .NET uses SetDllImportResolver to select the fixture, then invokes the actual Runtime ABI and native-symbol guards; production .NET package discovery order is outside this test. Go links an actual consumer of the binding: reexport fixtures override ABI major/minor while the retained native supplies other symbols; rejected diagnostics must identify the actual loaded ABI. Missing library, missing ABI symbol, and genuine x86_64 image are rejected by the Go external linker. These are linkage failures, not runtime CheckABI outcomes.

Python tests CPython's actual extension loader directly, with missing file, missing PyInit export and genuine foreign architecture. TypeScript uses the actual SDK loader in a clean subprocess sandbox, including a genuinely registered empty NAPI addon and a valid retained native positive control. C-ABI major/minor checks apply to Java/.NET/Go, not Python's extension namespace or Node's NAPI ABI. Tests do not claim every optional native symbol is validated eagerly, nor Linux/Windows adverse image coverage.
