# Aster Registry & Blob Download Flow

**Status:** Implemented  
**Date:** 2026-04-09

## Overview

This document describes how a consumer discovers services and downloads
manifests after connecting to a producer. There are two layers: the **doc
layer** (mutable index of published services) and the **blob layer**
(immutable content-addressed storage for manifests and type definitions).

## Architecture

```
Consumer                          Producer
   │                                 │
   │── consumer admission ──────────▶│
   │◀── services[] + registry_namespace ──│
   │                                 │
   │── join doc (namespace_id) ─────▶│  Doc layer
   │◀── doc sync (ArtifactRef entries) ──│
   │                                 │
   │── download_collection_hash ────▶│  Blob layer
   │◀── manifest.json bytes ─────────│
```

## Step 1: Consumer Admission

The consumer connects to the producer and performs an admission handshake
over the `aster.consumer_admission` ALPN. On success, the producer returns
a `ConsumerAdmissionResponse` containing:

- `services`: list of `ServiceSummary` (name, version, contract_id)
- `registry_namespace`: 64-char hex string — the namespace ID of the
  producer's registry doc
- `root_pubkey`: the producer's root public key

The `registry_namespace` is the **only credential needed** to read the
registry doc. In iroh-docs, `Capability::Read(NamespaceId)` grants read
access — knowing the namespace ID is the read capability.

### Why this is safe

- The admission response travels over an already-authenticated QUIC stream
  (TLS 1.3, mutual endpoint authentication).
- The namespace ID is not public information — it is shared only with
  admitted consumers.
- Even if intercepted, the namespace ID alone is insufficient without a
  QUIC connection to a node hosting the replica.

## Step 2: Registry Doc Sync

The consumer calls `docs_client.join_and_subscribe_namespace(namespace_id)`
which:

1. Creates a `Capability::Read(NamespaceId)` from the hex string
2. Calls `import_namespace(capability)` to register the doc locally
3. Subscribes to live events
4. Starts sync — the consumer's iroh endpoint already knows the
   producer's address from the admission connection

The doc contains entries keyed by `contracts/{contract_id}`, each storing
a JSON `ArtifactRef`:

```json
{
  "contract_id": "34bd73bd...",
  "collection_hash": "c466a056...",
  "ticket": "blob...",           // legacy — not used
  "published_by": "41192b93...",
  "published_at_epoch_ms": 1775682658476,
  "collection_format": "index"
}
```

## Step 3: Blob Download

For each service, the consumer extracts the `collection_hash` from the
`ArtifactRef` and downloads the blob collection directly by hash:

```python
files = await blobs_client.download_collection_hash(
    collection_hash,    # 64-char hex — from ArtifactRef
    remote_node_id,     # 64-char hex — from the connection
)
```

This uses `HashAndFormat { hash, format: BlobFormat::HashSeq }` to tell
the iroh downloader to fetch the collection root AND all child blobs.
The collection typically contains:

- `manifest.json` — contract manifest with method descriptors
- `contract.bin` — canonical binary contract (for BLAKE3 verification)
- Per-type definition files

### Why we bypass blob tickets

The `ArtifactRef.ticket` field contains a legacy `BlobTicket` string.
Blob tickets embed the full `EndpointAddr` (relay URL, direct addresses)
which is redundant when the consumer is already connected. They also
require `BlobTicket::deserialize` which has proven fragile across
different iroh build configurations.

`download_collection_hash(hash, node_id)` sidesteps both issues:
- No ticket parsing — just a hash and node ID
- Uses `HashAndFormat` to correctly request HashSeq format
- The endpoint already knows the peer's address

## Auth Model Summary

| Layer | Credential | How verified |
|-------|-----------|--------------|
| QUIC transport | Endpoint identity (ed25519) | TLS 1.3 handshake |
| Consumer admission | ConsumerEnrollmentCredential | Server validates credential + policy |
| Registry doc (read) | NamespaceId (32 bytes) | iroh-docs: knowing ID = read access |
| Blob download | Content hash | Content-addressed: hash = address |

There are no bearer tokens or signatures at the doc/blob layer. The
transport layer provides authentication, the namespace ID provides
authorization (scoped to a single doc), and content addressing provides
integrity.

## Wire Format: registry_namespace

The `registry_namespace` field in `ConsumerAdmissionResponse` is a
64-character hex string encoding the 32-byte namespace public key.

Previous versions used `registry_ticket` which was a full iroh
`DocTicket` string (~160 bytes, postcard + base32 encoded). The ticket
included redundant endpoint addressing information.

The `TicketCredential::RegistryRead` variant in the aster1 ticket format
carries the same 32-byte namespace ID for out-of-band sharing scenarios.
