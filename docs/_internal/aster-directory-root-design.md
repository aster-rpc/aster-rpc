# @aster Directory Root Design

**Status:** Day 0 agreed direction  
**Date:** 2026-04-09  
**Audience:** backend + CLI/shell implementers

## Day 0 UX

`/aster` should not try to show every handle in the system.

For Day 0, the root should render:

- `@currentuser`
- plus up to 20 additional handles from the backend

Exact-handle navigation remains first-class:

- `cd /aster/@handle`

So the shell root becomes a small curated handle list, not a global dump and
not a feed section hierarchy yet.

## Day 0 Backend Behavior

Add a simple endpoint that returns a bounded list of handles for the `/aster`
root view.

Suggested shape:

```python
list_directory_handles(ListDirectoryHandlesRequest) -> ListDirectoryHandlesResult
```

```python
ListDirectoryHandlesRequest:
  limit: int   # client may request, server caps at 20
```

```python
ListDirectoryHandlesResult:
  handles: list[DirectoryHandleEntry]
```

```python
DirectoryHandleEntry:
  handle: str
  registered_at: str
  last_published_at: str | None
  service_count: int
  display_name: str | None
```

### Selection Rule

Backend selection should be:

1. if there are handles with published services:
   return the most recently published handles
2. otherwise:
   return the most recently created handles

### Limit

- maximum 20 entries
- server-enforced cap

This keeps the shell and future site predictable at launch scale.

## Shell Rendering

For Day 0, `/aster` should render:

- `@currentuser/`
- then the returned handles, e.g.
  - `@alice/`
  - `@acme/`
  - `@bob/`

No special feed directories yet:

- no `recent/`
- no `trending/`
- no `starred/`
- no `shared/`

Those can come later if/when the product needs them.

## Exact Handle Pages

Exact handle pages stay separate from the root listing.

The existing model should remain:

- `/aster/@handle`
- backend uses exact handle lookup + `list_services(handle=...)`

This path should not depend on whether the handle appeared in the top-20 root
listing.

## Day 0 Non-Goals

Not part of this proposal:

- listing all handles
- favorites / stars
- pinned repos
- trending ranking
- shared-with-you feeds
- complex multi-section root UX

Those can be added later once the basic root listing proves out.

## Recommendation

Backend asks for Day 0:

1. Add `list_directory_handles(limit)` with a hard cap of 20.
2. Return most recently published handles, or recently created handles if none
   have published services yet.
3. Keep exact-handle browsing separate via existing handle-specific methods.

CLI/shell asks for Day 0:

1. Render `/aster` as `@currentuser` plus the returned curated handles.
2. Keep `cd /aster/@handle` as the canonical exact-handle path.
