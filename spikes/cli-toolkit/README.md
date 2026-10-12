# CLI toolkit spike

Throwaway evidence for the toolkit decision in
[`aster-cli-rust.md`](../../docs/_internal/working_ideas/aster-cli-rust.md#toolkit-decision-2026-10-12).
Standalone crate, not a workspace member. Not product code.

Minimal-surface stack: clap (default features off), ratatui (crossterm), rustyline,
anstyle, unicode-width, keyring-core with native stores, and serde/toml/
serde_json (already in the Aster facade).

| Command | Exercises |
| --- | --- |
| `profile list\|show\|delete` | Config load/atomic save, credential store, `--json`, hand-rolled table and y/N prompt that refuses without a TTY |
| `shell` | rustyline: completion derived from the clap tree, file history, dispatch through the same parser |
| `watch` | Full-screen ratatui dashboard (stand-in for `aster exec watch`): gauge, scrollable output, sparkline, summary on exit; plain or NDJSON fallback |
| `mcp` | Hand-rolled MCP stdio server, modern (2026-07-28) and legacy (`initialize`) eras, concurrent calls, cancellation |
| `completions <shell>` | clap_complete |

```bash
cargo build
export ASTER_HOME=/tmp/fixture                  # never point tests at a real ~/.aster
export ASTER_KEYRING_SERVICE=aster-spike-test   # never touch real `aster` credential entries
./target/debug/aster-spike profile list
./target/debug/aster-spike watch --lines 40
```

`profile delete` removes the credential-store entry under
`ASTER_KEYRING_SERVICE` (default `aster`), so always set the override when
testing.
