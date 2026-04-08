# Short Service Addresses via aster.site

Status: **design note**
Date: 2026-04-08

## Problem

Sharing an Aster service endpoint requires a long base64 NodeAddr string (~120 chars) or a 64-char hex endpoint_id. Neither is shareable in conversation, chat, or README files.

## Solution

Services published to aster.site get short, human-readable addresses:

```
emrul/IdentityService
mycompany/PaymentService
```

Resolution path:
1. `aster shell emrul/IdentityService`
2. Shell queries aster.site directory → resolves handle `emrul` → endpoint_id
3. iroh DNS discovery resolves endpoint_id → relay URL
4. Connect via relay → admission → ready
5. iroh hole-punches to direct in background (if possible)

## Address tiers

| Tier | Format | Example | How |
|------|--------|---------|-----|
| Published | `handle/ServiceName` | `emrul/IdentityService` | aster.site directory lookup |
| Named peer | `peer-name` | `aster-app` | .aster-identity file |
| Endpoint ID | 64-char hex | `bda1158f1ef9...` | DNS discovery → relay |
| Full NodeAddr | base64 blob | `YmRhMTE1OGYx...` | Direct connection (includes relay + IPs) |

## Incentive

Publishing to aster.site gives you a short, memorable address. Private/unpublished services require the longer forms. This naturally encourages directory participation.

## Prerequisites

- aster.site directory service (exists in prototype)
- CLI resolver for `handle/service` format in `_resolve_peer_arg`
- DNS discovery working reliably (confirmed — relay path works after conn.close fix)
