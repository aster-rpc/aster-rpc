# Aster

Aster is a peer-to-peer SDK for typed RPC, streaming, and shared data. It connects
services by peer identity and uses [Iroh](https://iroh.computer/) for QUIC
connections, NAT traversal, and relay fallback. Applications can define who may
connect and which methods a caller may use.

It is useful when services run on devices or networks where maintaining a public
address for every service is impractical. A connection can cross network
boundaries while the application keeps its service contracts and authorization
rules. Direct connections depend on network conditions; relays provide a fallback.

**The current public preview is 0.3.893.** APIs may change before general
availability. See [known limitations](docs/KNOWN_ISSUES.md) before adopting it.

- [Documentation](https://docs.aster.site/): concepts, tutorials, and API reference.
- [First service](https://docs.aster.site/docs/quickstart/python): Python and TypeScript setup.
- [Rust guide](docs/aster-rust-getstarted.md): the native Rust API.
- [Public packages](https://forge.emrul.dev/Aster/-/packages): versioned preview downloads.

## Language support

Rust, Python, TypeScript, Java, .NET, and Go use the shared Rust engine. RPC
payloads use [Apache Fory](https://fory.apache.org/); matching service contracts
allow callers and servers to use different languages.

| Language | Preview distribution |
| --- | --- |
| Rust | Source crates in the public Aster Cargo registry |
| Python | Platform wheels and a source archive on Forgejo |
| TypeScript | Packages in the Forgejo npm registry |
| Java | Packages in the Forgejo Maven registry |
| .NET | Packages in the Forgejo NuGet registry |
| Go | Binding tested from source; a standalone public Go module is not yet available |

The current tutorials focus on Python and TypeScript. More language examples
will be added separately. Kotlin is not a separately qualified binding in this preview.

## Install the preview

Version 0.3.893 is available from Forgejo without a token. This version was not
published to PyPI or npmjs.org; an unqualified install from those registries may
select an older release.

### Python

Download a wheel for your platform from the
[0.3.893 Python package](https://forge.emrul.dev/Aster/-/packages/generic/aster-python/0.3.893).
Wheels are available for Linux x86-64 and ARM64, macOS Apple Silicon, and Windows x64.

For example, on **macOS Apple Silicon**, create a virtual environment with
[uv](https://docs.astral.sh/uv/) and install the wheel directly:

```sh
uv venv
uv pip install https://forge.emrul.dev/api/packages/Aster/generic/aster-python/0.3.893/aster_rpc-0.3.893-cp39-abi3-macosx_11_0_arm64.whl
uv run python -c 'import aster; print(aster.__version__)'
```

For another platform, use its wheel from the package page. The package also
provides `SHA256SUMS` for checking downloaded files.

### TypeScript

Add the Aster registry to your project's `.npmrc`:

```ini
@aster-rpc:registry=https://forge.emrul.dev/api/packages/Aster/npm/
```

Then install the preview:

```sh
npm install @aster-rpc/aster@0.3.893
```

The scoped registry setting leaves other npm dependencies on your existing
registry. Follow the [TypeScript build setup](https://docs.aster.site/docs/guides/typescript-build-setup)
for decorators and generated type metadata.

### Rust

Add the registry to `.cargo/config.toml`:

```toml
[registries.aster]
index = "sparse+https://forge.emrul.dev/api/packages/Aster/cargo/"
```

Then add Aster to `Cargo.toml`:

```toml
[dependencies]
aster = { version = "=0.3.893", registry = "aster", features = ["rpc"] }
```

Commit your application's `Cargo.lock`. The
[Rust consumer guide](docs/rust-sdk-consumer-guide.md) covers Fory payload derives,
optional features, and direct access to the native dependencies.

### Java and .NET

The [public package catalogue](https://forge.emrul.dev/Aster/-/packages)
provides Maven and NuGet packages for 0.3.893. Use the registry configuration
shown with the package. The .NET binding also depends on
`Apache.Fory` version `1.7.6-aster.1` from the same Forgejo NuGet feed.

## Try a service

Start with the [first-service tutorial](https://docs.aster.site/docs/quickstart/python).
For streaming, sessions, authorization, and cross-language clients, continue to
[Mission Control](https://docs.aster.site/docs/quickstart/mission-control).

The repository includes examples under `examples/python/`,
`examples/typescript/`, and `examples/rust/`. The tutorials explain the setup and
credentials each example needs.

## Architecture

Application APIs call the shared Rust engine. Rust applications use the `aster`
crate; foreign-language bindings bridge into Rust through PyO3, NAPI-RS, or the
C ABI. The engine manages transport, RPC dispatch, and shared services.

| Layer | Responsibility |
| --- | --- |
| Language APIs | Typed services, clients, and language-specific async interfaces |
| RPC | Contract identity, payload framing, deadlines, and streaming |
| Authorization | Peer admission and permissions for service calls |
| Iroh transport | Peer connections, NAT traversal, and relay fallback |

The [architecture reference](https://docs.aster.site/docs/reference/architecture)
and [trust model](https://docs.aster.site/docs/concepts/trust-model) describe the
configuration and boundaries of these layers.

## Working on the SDK

The workspace contains the `aster` Rust facade, shared code in `core/`,
foreign-language APIs in `bindings/`, the C ABI in `ffi/`, and tests in `tests/`.
Some binding sources are not yet included in the public source mirror; use the
published packages when evaluating the preview.

For a complete development checkout, use the Rust toolchain pinned by
`rust-toolchain.toml` and follow [CONTRIBUTING.md](CONTRIBUTING.md). Released Rust
applications use the registry configuration above; they do not need the
workspace's fork patches.

## License

[Apache License 2.0](LICENSE).
