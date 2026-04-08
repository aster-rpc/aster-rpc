# Compact Ticket Format

Status: **design spec — not implemented**
Date: 2026-04-08

## Motivation

The current NodeAddr format is a newline-delimited plaintext encoding (hex endpoint_id + full relay URL + text IP:port pairs), base64-encoded. A typical address is ~200 characters — too long to share in a chat message, speak aloud, or fit in a QR code comfortably.

The relay URL alone (`https://euc1-1.relay.n0.iroh-canary.iroh.link./`) is 48 bytes of text encoding ~6 bytes of actual information (an IP + port).

## Design

A single compact binary format that serves as:
- **Endpoint address** — share with a friend to connect
- **Connect ticket** — includes a consumer admission credential
- **Enroll ticket** — includes a producer enrollment credential
- **Registry ticket** — includes a doc namespace + read capability

### Wire format

```
┌──────────────────────────────────────────────────────┐
│  endpoint_id          32 bytes                       │
│  header               1 byte                         │
│  relay addr           0 / 6 / 18 bytes               │
│  direct addrs         variable                       │
│  credential (opt)     variable, bounded              │
└──────────────────────────────────────────────────────┘
```

**Header byte:**

```
  7 6 5 4   3 2     1 0
 ┌───────┬─────┬────────┐
 │version│relay│reserved│
 │ (4b)  │type │  (2b)  │
 │       │(2b) │        │
 └───────┴─────┴────────┘

relay_type:
  00 = no relay
  01 = IPv4 relay (6 bytes: 4 addr + 2 port)
  10 = IPv6 relay (18 bytes: 16 addr + 2 port)
  11 = reserved
```

**Direct addresses:**

```
  1 byte: count (4 bits) + type bitfield position
  
  For each address, 2 bits from a packed bitfield:
    00 = IPv4 direct  (4 + 2 = 6 bytes)
    01 = IPv6 direct  (16 + 2 = 18 bytes)
    10 = reserved
    11 = reserved
  
  Bitfield is ceil(count * 2 / 8) bytes, immediately after count byte.
  Addresses follow in order, each 6 or 18 bytes.
```

**Optional credential (TLV):**

If bytes remain after the direct addresses section:

```
  1 byte:  credential type
  2 bytes: payload length (big-endian, max 220)
  n bytes: payload
```

### Credential types

| Type | Name | Payload | Typical size |
|------|------|---------|-------------|
| 0x00 | open | none | 0 bytes |
| 0x01 | consumer RCAN | JSON credential (signature, attributes, expiry) | ~96-128 bytes |
| 0x02 | enrollment | JSON enrollment credential | ~96-128 bytes |
| 0x03 | registry | namespace_id (32) + read_capability (32) | 64 bytes |

### Size limits

**Hard upper bound: 256 bytes total wire format.**

This ensures the base58-encoded string is at most ~350 characters — fits in a single QR code, chat message, or CLI argument.

### Encoding

The wire bytes are encoded as **base58** (Bitcoin alphabet) for human sharing. This avoids base64's `+/=` characters that cause problems in URLs, shells, and chat formatting.

Prefix: `aster1` (6 chars) to make tickets visually identifiable and allow version discrimination.

Final format: `aster1<base58_payload>`

### Size examples

| Scenario | Wire bytes | Encoded |
|----------|-----------|---------|
| Endpoint + IPv4 relay (share with friend) | 39 bytes | ~59 chars |
| Endpoint + IPv4 relay + 2 IPv4 direct | 52 bytes | ~77 chars |
| Endpoint + IPv4 relay + 1 IPv6 direct | 57 bytes | ~83 chars |
| Endpoint + relay + consumer RCAN | ~170 bytes | ~237 chars |
| Endpoint + relay + registry access | ~106 bytes | ~150 chars |

vs current base64 NodeAddr: **~200 chars** (endpoint + relay + 2 direct only, no credential)

### Comparison

```
Current (base64 NodeAddr, no credential):
YmRhMTE1OGYxZWY5ZGU1ZWFhODIyMDA5YTIyYzE4YmM4OTc4MjdiNDJkMGNm
MjM2NGNlNDIxNDcxYWJkYTVjYQpodHRwczovL2V1YzEtMS5yZWxheS5uMC5p
cm9oLWNhbmFyeS5pcm9oLmxpbmsuLwo4Ny4yNTQuMC4xMzYKMTkyLjE2OC4x
LjE3OTo1MzMzMg==
(~200 chars, 4 lines)

Compact (same info):
aster1<~77 chars>
(1 line, copy-pasteable)
```

## Parsing rules

1. Strip `aster1` prefix.
2. Base58 decode.
3. Read 32-byte endpoint_id.
4. Read header byte — extract version + relay type.
5. Read relay address (0, 6, or 18 bytes per relay type).
6. Read direct address count + bitfield + addresses.
7. If bytes remain: read credential TLV.
8. Reject if total > 256 bytes.

## Future considerations

- Version field (4 bits) allows format evolution.
- Reserved bits in header and address type bitfield allow extension.
- Credential type 0xFF could indicate "credential follows in separate channel" for very large credentials.
- Could add a checksum byte for typo detection (last 4 bits of blake3 hash).
