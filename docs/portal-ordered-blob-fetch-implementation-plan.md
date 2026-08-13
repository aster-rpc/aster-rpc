# Ordered multi-provider blob fetch — as built

Status: implemented 2026-08-13. This file started as an implementation plan and
now records what shipped and why, including the decisions that are not obvious
from the diff.

Consumer: portal-sync Phase 6 item D. Portal ranks fetch candidates itself
(direct before relay, RTT for small transfers, throughput for large) and needs
to hand Aster an ordered list. Ranking stays Portal's; ordered *consumption*
plus a truthful report is what Aster gained.

Pins: `iroh-blobs` moved from `60d098c9` to `7e0da6f7`
(`aster-iroh-blobs-v0.103.1`).

**This is already released.** The registry wave was published mid-development:
`iroh-blobs 0.103.0` in the Aster registry is byte-identical to fork rev
`1f01a087` (verified by extracting the `.crate` and diffing its whole `src/`
tree — 0 differing lines; `60d098c9` differs by 462 and the current tip by 31),
and `aster 0.3.12` already exports `download_hash_from`,
`download_hash_to_store_from`, `download_collection_from`, `FetchStrategy` and
`bytes_transferred`. portal-sync consumes
`aster = { version = "0.3", registry = "aster" }` with no patch block and no
path override, so it is already building and testing against this feature.

The crate-version move to **0.103.1** is hygiene, not a fix: the delta between
the immutable 0.103.0 archive and the fork tip is one softened doc comment on
`BytesTransferred` plus one added assertion inside `mod tests` — no behavioural
code. It restores "one version, one source state" so the BOM rev and the
published archive agree again. Do **not** yank 0.103.0; nothing consuming it is
broken. It ships with the next wave
(`scripts/release/publish-native-stack.py publish`, then a tagged Aster), after
which portal-sync picks it up with `cargo update -p aster`.

Provenance is checkable and worth re-checking rather than inferring from pin
history — the mistake that produced the earlier "delivery is blocked" claim was
assuming the published archive matched the pin that preceded this work. See
`docs/_internal/rust-sdk-maintainer-guide.md`; note that
`publish-native-stack.py check` validates the *fork at the BOM rev*, so on its
own it cannot detect divergence between the BOM and what the registry actually
serves.

## What was already upstream — not reimplemented

`execute_get` (`src/api/downloader.rs`) walks providers strictly in order and
continues to the next on connect-or-transfer failure; each attempt recalls
`local_for_request` and asks only for `local.missing()`, so retries are
resumable. `Vec<EndpointId>`'s blanket `ContentDiscovery` yields in iteration
order (`Shuffled` deliberately does not, and is not used). `SplitStrategy::Split`
runs per-child requests `buffered_unordered(32)`, each walking the same list.

## Why the fork needed patching

The progress stream could not be folded into an accurate report:

1. `split_request` fetched the HashSeq root through a `Drain` sink, so in Split
   mode the provider that served the root was invisible.
2. `handle_download_split_impl` returned as soon as `futs.next()` yielded
   `None`, abandoning buffered fan-in events. Measured: 6 of 13 `PartComplete`
   events lost with 12 small children.
3. `TryProvider` was emitted before the `local.is_complete()` check, so an
   already-resident request reported an attempt against a provider that was
   never dialled — which would have poisoned Portal's ranking feedback.
4. `Progress(u64)` is a cumulative offset that includes bytes already local and
   is re-based per child in Split mode, so it cannot be summed into a payload
   total.

Two rejected alternatives, both of which produce wrong numbers:

- `max(Progress) - local_bytes_before`: a resident child emits no `Progress`, so
  with a 1 MiB resident child and a 4 KiB missing child this saturates to zero.
- Before/after `store.remote().local(haf).local_bytes()` delta: when a child is
  resident but the HashSeq root is not, the "before" lookup cannot enumerate
  children, so Split correctly skips the resident child while the "after" lookup
  counts it — reporting its full size as transferred.

Fork patch (functional-patch row 8 in `/Users/emrul/dev/aster/iroh/docs/iroh-patches.md`):
the root fetch reports through the real sink instead of `Drain`, except for its
legacy `Progress` offsets — split mode runs two independent `Progress` counters
(the root's, and the child aggregator's, which sums the latest per-child offset
and cannot see the root), so forwarding both made the bar jump backwards, e.g.
`[384, 1, 2, …]`. The fan-in is drained after the futures stream ends (dropping
`futs` and the producer handle so it can terminate); `TryProvider` moves after
the local-complete early return; and an additive
`DownloadProgressItem::BytesTransferred(u64)` reports each attempt's payload,
from the returned `Stats::payload_bytes_read` on success and from the last raw
sink value on failure, so a provider that transferred part of a blob before
failing is still counted. Provider ordering, failover, resumption and split
scheduling are unchanged.

Six tests in `api::downloader::tests` pin the behaviour; each defect was
reintroduced to confirm its test fails. Note that the statement order of
`pool.get_or_connect` and the local-bitfield scan is inert — `get_or_connect` is
an `async fn`, so building its future does no work until it is awaited. It is
left where upstream had it, and no concurrency is claimed.

## Public surface

`aster::{FetchStrategy, FetchReport, ProviderOutcome}` and three methods on
`Blobs`:

```rust
download_hash_from(&self, hash, from: &[NodeId], format, strategy) -> Result<(Vec<u8>, FetchReport)>
download_hash_to_store_from(&self, hash, from: &[NodeId], format, strategy) -> Result<FetchReport>
download_collection_from(&self, hash, from: &[NodeId], strategy) -> Result<(Vec<(String, Vec<u8>)>, FetchReport)>
```

Definitions, as documented on the types:

- **attempted** — a `TryProvider` was observed, i.e. a real dial/transfer.
- **failed** — at least one request emitted `ProviderFailed`.
- **served** — at least one `PartComplete`, attributed to the provider that
  request last announced with `TryProvider`. Requests are keyed by `GetRequest`
  value (`Eq + Hash`), never by `Arc` identity.
- **bytes_transferred** — payload pulled off the wire: the sum of what each
  attempt successfully decoded. Excludes protocol overhead and resident data. A
  provider that transferred part of a blob before failing is counted up to its
  last valid chunk and the next resumes from what is stored, so ordinary
  failover counts each byte once — `downloader_counts_bytes_from_a_partial_provider`
  asserts the sum equals the blob size exactly across a failed attempt and the
  one that completes it. It is a transfer metric rather than a content size,
  though: a chunk that fails verification is not counted (the decode error
  propagates before its progress update is sent), and a range that was decoded
  but failed to be stored is counted again when re-fetched.

A provider can legitimately appear in both `failed()` and `served()`: it may
complete one split child and fail another.

`FetchStrategy::Whole` means "do not split the HashSeq" — **not** "one
provider"; ordered failover applies in both variants. `SplitChildren` honours
order **per child, not fleet-wide**: every child walks the list from the top, so
"direct before relay" is a per-request property. It is rejected for
`BlobFormat::Raw`, where `split_request` would assert `size % 32 == 0` on the
blob's own bytes.

An empty provider slice and `SplitChildren + Raw` are rejected as
`Error::InvalidArgument` before any network work; the core engine rejects them
again for direct callers. Duplicate ids stay legal, are passed through in the
caller's order, and are aggregated by id in the report. No variant was added to
`Error` (it is not `#[non_exhaustive]`); terminal failures name the tried and
failed providers in the message.

There is no `errors` field on the report: any `Error` or `DownloadError` makes
the call return `Err`, after the stream has been fully drained.

## Layering

`core/src/lib.rs` holds one private engine, `CoreBlobsClient::run_download`,
which consumes the progress stream into `CoreFetchReport` via
`FetchAccumulator`. The public core variants are `download_hash_multi`,
`download_hash_to_store_multi` and `download_collection_hash_multi`; only the
bytes-returning ones read back from the store, preserving the no-readback / ~2×
RSS property of the `_to_store` path. The three pre-existing methods keep their
exact signatures and delegate with a one-element vec — proven by
`cargo check -p aster_transport_ffi -p aster-transport-napi -p aster_rs`. The
legacy format parsing was deduplicated without changing behaviour: only the
literal `"hash_seq"` selects HashSeq.

No Python/TypeScript/Java/Kotlin entry points were added. The dynamic/streaming
`ContentDiscovery` variant is still out of scope — it is a stream trait, so it
stays additive, and there is no consumer.

## Verification

- Fork: `cargo fmt --all --check`, `cargo clippy --all-features --tests -D warnings`,
  `cargo test --all-features` (125 passed, 2 ignored) before the pin moved.
- Aster: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -D warnings`,
  `cargo test -p aster --features rpc,expose --test blob_fetch_multi`
  (**10 tests**, non-zero count checked on the raw `running N tests` line),
  `cargo test -p aster_transport_core` (252), `cargo test -p aster --all-features`,
  and the three binding crates via `cargo check`.

`aster/tests/blob_fetch_multi.rs` covers ordered preference, failover from an
offline peer and from an unaddressable id, resumption without re-fetching
resident content, split across two partial holders, resident children skipped in
split mode, the all-candidates-fail error, and the degenerate cases. Byte
assertions are exact, derived from a control download by a fresh consumer rather
than hard-coded, since a collection's metadata child is itself a HashSeq child.
