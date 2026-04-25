# Consuming aster-rpc-internal from a Kotlin/JVM Gradle project

The artifacts live on **GitHub Packages** (`maven.pkg.github.com/aster-rpc/aster-rpc-internal`), which is a private Maven repo and requires auth.

---

## 1. Create a GitHub Personal Access Token

GitHub Packages requires a token even for repos you own. Go to <https://github.com/settings/tokens> (classic tokens) and create one with **`read:packages`** scope. Save it.

Don't put it in `build.gradle.kts`. Two clean options:

**Option A — `~/.gradle/gradle.properties`:**

```properties
gpr.user=your-github-username
gpr.token=ghp_xxxxxxxxxxxxxxxxxxxx
```

**Option B — env vars** (better for CI):

```bash
export GITHUB_ACTOR=your-github-username
export GITHUB_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxx
```

---

## 2. `build.gradle.kts` — add the repo + `os-detector`

The `aster-runtime` artifact is split into a **main jar** plus a **per-platform classifier jar** (the native `.so`/`.dylib` lives in the classifier). You depend on both; the OS detector picks the right classifier for the host running the build.

```kotlin
plugins {
    kotlin("jvm") version "2.1.0"
    // Resolves ${osdetector.classifier} to e.g. linux-x86_64, osx-aarch_64
    id("com.google.osdetector") version "1.7.3"
}

repositories {
    mavenCentral()
    maven {
        name = "GitHubPackages"
        url = uri("https://maven.pkg.github.com/aster-rpc/aster-rpc-internal")
        credentials {
            username = providers.gradleProperty("gpr.user")
                .orElse(providers.environmentVariable("GITHUB_ACTOR"))
                .get()
            password = providers.gradleProperty("gpr.token")
                .orElse(providers.environmentVariable("GITHUB_TOKEN"))
                .get()
        }
    }
}

// Aster's classifier convention is `<os>-<arch>` with arch as `x86_64` or
// `aarch64` (e.g. linux-x86_64, macos-aarch64). Map osdetector's slightly
// different output (osx vs macos, aarch_64 vs aarch64).
val asterClassifier: String = run {
    val os = when {
        osdetector.os == "osx" -> "macos"
        else -> osdetector.os // "linux", "windows"
    }
    val arch = when (osdetector.arch) {
        "x86_64" -> "x86_64"
        "aarch_64" -> "aarch64"
        else -> osdetector.arch
    }
    "$os-$arch"
}

dependencies {
    val asterVersion = "0.2.0-SNAPSHOT"

    // Main jar: loader code, codegen-emitted classes, framework. No natives.
    implementation("site.aster:aster-runtime:$asterVersion")

    // Per-platform classifier jar: just the FFI shared library bundled at
    // /native/<os>-<arch>/lib... so IrohLibrary's classpath resolver can
    // extract it to a temp file at startup.
    runtimeOnly("site.aster:aster-runtime:$asterVersion:$asterClassifier@jar")

    // Annotations + codegen — pull whichever you need:
    implementation("site.aster:aster-annotations:$asterVersion")
    // For Kotlin services, the KSP processor:
    // ksp("site.aster:aster-codegen-ksp:$asterVersion")
    // For Java services, the annotation processor:
    // annotationProcessor("site.aster:aster-codegen-apt:$asterVersion")
}
```

---

## 3. JVM flags

`aster-runtime` uses Java FFM (`java.lang.foreign`) and needs `--enable-native-access`:

```kotlin
// For tests
tasks.test {
    jvmArgs("--enable-native-access=ALL-UNNAMED")
}

// For the application plugin, if you have one
application {
    applicationDefaultJvmArgs = listOf("--enable-native-access=ALL-UNNAMED")
}
```

JVM 25+ is required (the runtime POM targets `<release>25</release>`).

---

## 4. Smoke test

```kotlin
import site.aster.ffi.IrohLibrary

fun main() {
    val lib = IrohLibrary.getInstance()
    println("Aster ABI v${lib.abiVersionMajor()}.${lib.abiVersionMinor()}.${lib.abiVersionPatch()}")
}
```

`./gradlew run` should print `Aster ABI v1.0.0` after pulling the jars on the first run.

---

## Troubleshooting

- **`401 Unauthorized` from GitHub Packages** → token missing/expired/wrong scope. Needs `read:packages`.
- **`UnsatisfiedLinkError: Native library libaster_transport_ffi... not found at classpath resource /native/<os>-<arch>/...`** → the classifier dependency wasn't pulled. Check `osdetector.classifier` actually resolves to one of `linux-x86_64`, `macos-aarch64` (the platforms currently published — see the build matrix in `.github/workflows/build-java.yml`). Other platforms haven't been built yet.
- **Override for local dev / unsupported platform** → set `IROH_LIB_PATH=/abs/path/to/libaster_transport_ffi.dylib` (or `.so`/`.dll`) before launching the JVM, or pass `-Daster.ffi.lib=...`. The loader checks these first.
- **Working in an `aster-rpc-internal` checkout** → the loader walks up to find `target/{release,debug}/lib...` so a plain `cargo build -p aster_transport_ffi` is enough, no Maven deps needed.
