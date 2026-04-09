# Aster Client Code Generation

**Status:** Design  
**Date:** 2026-04-09

## Purpose

Generate typed client libraries from Aster service manifests. The
generated code gives consumers compile-time type safety, IDE
autocompletion, and documentation — without requiring the producer's
source code.

This document defines the generation algorithm and rules shared across
all target languages. Language-specific structure and naming conventions
are appendices.

## Inputs

Client generation accepts a manifest from one of three sources:

1. **Published service:** `aster contract gen @handle/ServiceName`
   — fetches the manifest from `@aster` via the `get_manifest` RPC.

2. **Exported file:** `aster contract gen ./ServiceName.aster.json`
   — reads a manifest previously exported with `aster contract export`.

3. **Live P2P node:** `aster contract gen aster1.../ServiceName`
   — connects to the node, performs admission, reads the manifest from
   the registry doc.

All three produce identical output for the same contract.

## CLI Interface

```
aster contract gen <source> [options]

Options:
  --out <dir>         Output directory (required)
  --lang <language>   Target language: python (default), typescript, go
  --package <name>    Override root package name (default: derived from handle)
```

No other options for Day 0. Aster's manifest is simple enough that the
generator does not need style flags, model-only modes, or interface
toggles.

## Generation Algorithm

### Step 1: Resolve manifest

Fetch or read the manifest. The manifest contains:
- Service name, version, contract_id
- Method descriptors (name, pattern, request/response wire tags, fields)
- Nested type information (element types for list fields)

If the source is `@handle/ServiceName`, also record the handle for use
as a namespace in the output directory structure.

### Step 2: Collect all types

Walk every method descriptor and collect all referenced types by wire
tag. A type is referenced if it appears as:

- A method's `request_wire_tag` or `response_wire_tag`
- An `element_wire_tag` inside a field descriptor (for `list[T]` fields)
- Recursively: an `element_wire_tag` inside an element type's fields

Build a map: `wire_tag -> TypeRecord` where TypeRecord holds:
- Wire tag (e.g., `aster/DiscoverEntry`)
- Display name (e.g., `DiscoverEntry`)
- Fields list (name, type, default, element info)
- Set of services that reference this type

### Step 3: Classify types

For each type, determine its placement:

- **Service-scoped type:** Referenced only by methods of a single
  service, AND used as a direct request or response parameter (not
  just as a nested element). These go into the service's types file.

- **Shared type:** Referenced by methods of multiple services, OR
  referenced as a nested element type by types in multiple services.
  These get their own file.

The deduplication key is the **wire tag**, not the type name. Two types
with the same name but different wire tags are distinct.

### Step 4: Generate type files

For each type, generate a language-appropriate type definition:

- All fields from the manifest's field descriptors
- Default values where specified
- Wire tag annotation (language-specific: decorator, annotation, etc.)
- For list fields with element types: parameterized list type

Type files are generated before service files (services import types).

### Step 5: Generate service client files

For each service, generate a client class with:

- One method per RPC in the manifest
- Method signature matching the pattern:
  - `unary`: request -> response
  - `server_stream`: request -> async iterator of response
  - `client_stream`: async iterator of request -> response
  - `bidi_stream`: returns bidirectional channel
- Method body delegates to the base client's transport methods
- Default timeout from manifest (if specified)
- Idempotency annotation (if specified)

### Step 6: Generate package structure

Create the directory layout with:
- Package init files (language-specific)
- Import/export declarations
- A header comment in each file with contract_id, generation timestamp,
  and source

### Step 7: Print usage snippet

After generation, print a usage example to the terminal showing how to
import and use the generated client. Include the connection address if
known (e.g., the aster1 ticket used to connect).

## Output Directory Structure

The structure follows a convention that is opinionated but consistent
across languages. The exact file extensions and naming conventions are
language-specific (see appendices), but the logical layout is:

```
<out>/
  <namespace>/                       # handle or endpoint_id prefix
    types/
      <service_name>.py              # request/response types scoped to one service
      <shared_type>.py               # types used across multiple services
    services/
      <service_name>_v<N>.py         # typed client class
    __init__.py                      # package root with re-exports
```

### Namespace resolution

| Source | Namespace |
|--------|-----------|
| `@handle/Service` | handle (e.g., `emrul`) |
| `aster1.../Service` | first 8 chars of endpoint_id (e.g., `bda1158f`) |
| `./Service.aster.json` | `--package` flag, or `local` |

### File naming

Type and service file names are derived from the service name and type
name using the target language's naming convention:

| Language | Service file | Type file | Convention |
|----------|-------------|-----------|------------|
| Python | `publication_service_v1.py` | `discover_entry.py` | snake_case |
| TypeScript | `publicationServiceV1.ts` | `discoverEntry.ts` | camelCase |
| Go | `publication_service_v1.go` | `discover_entry.go` | snake_case |

## Type Mapping

The manifest stores types as string names. The generator maps them to
language-native types:

| Manifest type | Python | TypeScript | Go |
|---------------|--------|------------|-----|
| `str` | `str` | `string` | `string` |
| `int` | `int` | `number` | `int64` |
| `float` | `float` | `number` | `float64` |
| `bool` | `bool` | `boolean` | `bool` |
| `bytes` | `bytes` | `Uint8Array` | `[]byte` |
| `list[X]` | `list[X]` | `X[]` | `[]X` |
| `dict[str, X]` | `dict[str, X]` | `Record<string, X>` | `map[string]X` |

Custom types (dataclasses with wire tags) are resolved to the generated
type from step 4.

## Generated File Header

Every generated file includes a header comment:

```
# Auto-generated by: aster contract gen @emrul/PublicationService
# Contract ID: d489dede400e91a5db8d72fdf396493011581d508065c7986f8c583a5ffbd262
# Generated at: 2026-04-09T01:23:45Z
# DO NOT EDIT — regenerate with: aster contract gen @emrul/PublicationService --out .
```

## What We Do NOT Generate

- Connection helpers (avoids accidental multiple QUIC connections)
- Server-side service implementations
- Test files
- Configuration or environment handling
- Authentication/credential setup

## Contract ID Verification

Every generated service client carries a `_contract_id` field matching
the manifest it was generated from. At connection time, the consumer
should verify that the producer's advertised contract_id (from the
admission response's ServiceSummary) matches the generated client's
`_contract_id`. A mismatch means the producer has been updated and the
client code is stale — the consumer should regenerate.

This check prevents silent wire-format incompatibilities: the consumer
would send request types that the producer can't deserialize, or receive
response types with unexpected fields.

The base `ServiceClient` class should enforce this check automatically
during the first RPC call or at client construction time.

## Regeneration

Running `aster contract gen` again with the same `--out` overwrites
the generated files. The contract_id in the header lets tools detect
whether the generated code is stale relative to the published service.

## Future Considerations (Post Day 0)

- **Versioned clients:** Generate v1 and v2 side by side when a service
  publishes a new version
- **Changelog diff:** Show what changed between the current generated
  code and the new manifest
- **Watch mode:** Regenerate automatically when the published manifest
  changes
- **SDK packaging:** Generate a full pip/npm/go package with
  pyproject.toml / package.json / go.mod

---

## Appendix A: Python Output Example

Source: `aster contract gen @emrul/PublicationService --out ./clients`

```
clients/
  emrul/
    __init__.py
    types/
      __init__.py
      publication_service_v1.py      # DiscoverRequest, DiscoverResult,
                                     # PublishPayload, PublishResult, ...
      discover_entry.py              # shared: used by Publication + Access
      signed_request.py              # shared: used by all signed services
    services/
      __init__.py
      publication_service_v1.py
```

### types/publication_service_v1.py

```python
"""Request and response types for PublicationService v1.

Auto-generated by: aster contract gen @emrul/PublicationService
Contract ID: d489dede...
DO NOT EDIT
"""

import dataclasses
from aster.codec import wire_type
from .discover_entry import DiscoverEntry


@wire_type("aster/DiscoverRequest")
@dataclasses.dataclass
class DiscoverRequest:
    query: str = ""
    limit: int = 20
    offset: int = 0


@wire_type("aster/DiscoverResult")
@dataclasses.dataclass
class DiscoverResult:
    services: list[DiscoverEntry] = dataclasses.field(default_factory=list)
    total: int = 0


# ... remaining request/response types for this service
```

### services/publication_service_v1.py

```python
"""Typed client for PublicationService v1.

Auto-generated by: aster contract gen @emrul/PublicationService
Contract ID: d489dede...
DO NOT EDIT
"""

from aster.client import ServiceClient
from ..types.publication_service_v1 import (
    DiscoverRequest,
    DiscoverResult,
    GetManifestRequest,
    GetManifestResult,
    # ...
)


class PublicationServiceClient(ServiceClient):
    """Typed client for PublicationService v1."""

    _service_name = "PublicationService"
    _service_version = 1
    _contract_id = "d489dede400e91a5..."

    async def discover(
        self, request: DiscoverRequest, *, timeout: float | None = None
    ) -> DiscoverResult:
        """Search for published services."""
        return await self._call_unary(
            "discover", request, DiscoverResult, timeout=timeout
        )

    async def get_manifest(
        self, request: GetManifestRequest, *, timeout: float | None = None
    ) -> GetManifestResult:
        """Fetch a published service manifest."""
        return await self._call_unary(
            "get_manifest", request, GetManifestResult, timeout=timeout
        )

    # ... remaining methods
```

### Usage (printed to terminal after generation)

```
Generated PublicationService client -> ./clients/emrul/

Usage:
  from aster import AsterClient
  from clients.emrul.services.publication_service_v1 import PublicationServiceClient
  from clients.emrul.types.publication_service_v1 import DiscoverRequest

  client = AsterClient(address="aster1...")
  await client.connect()
  pub = PublicationServiceClient(client)
  result = await pub.discover(DiscoverRequest(query="hello"))
```
