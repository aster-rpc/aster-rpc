//! A remote shell over Aster RPC — the "SSH replacement" shape in ~140 lines.
//!
//! The service is a single **bidirectional** stream: keystrokes flow up as
//! `ShellIn` frames, terminal output flows down as `ShellOut` frames, both at
//! once. The server backs each call with a *real PTY*, so the session is
//! indistinguishable from a local terminal — `tmux`, `vim`, and `htop` all work,
//! and window resize is a genuine `TIOCSWINSZ` ioctl.
//!
//! Two frame kinds travel up the same stream:
//! - `Data` — raw bytes typed at the terminal (Ctrl-C rides here as 0x03; the
//!   tty line discipline turns it into SIGINT for you).
//! - `Resize` — a new (rows, cols); the server resizes the PTY in place.
//!
//! To stay deterministic the server runs a controlled shell (`/bin/sh` on Unix,
//! `cmd.exe` on Windows) with a cleaned environment, not the user's `$SHELL` with
//! its prompt/rc-file noise. This `main` drives it with a *scripted* program
//! (resize, run a command, exit) so the example is self-contained and runnable.
//!
//! NOTE on interactivity: the generated bidi client buffers a prebuilt
//! `Vec<ShellIn>` and closes the input stream once it has been sent (see
//! `RpcConnection::bidi`). That is exactly right for a scripted session, but a
//! *live* interactive client must interleave sends with reads — an incremental
//! streaming-send handle the generated stub does not yet expose. Wiring that up
//! (raw-mode stdin → frames) is the natural follow-up; the server side here is
//! already fully interactive.
//!
//! Run with: `cargo run -p aster --example shell --features rpc`

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::Context;
use aster::rpc::{async_trait, RequestStream, ResponseSink, RpcConnection, Server, RPC_ALPN};
use aster::{AsterConfig, Node, RelayMode, Result};
use fory_derive::ForyStruct;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

// ── Wire types ──────────────────────────────────────────────────────────────
// Flat structs with an explicit `kind` discriminator: the safe, cross-binding
// Fory shape (every binding decodes a plain struct identically).

const KIND_DATA: i32 = 0;
const KIND_RESIZE: i32 = 1;

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "shell/ShellIn")]
struct ShellIn {
    kind: i32,
    data: Vec<u8>, // Data: the bytes
    rows: i32,     // Resize: new height
    cols: i32,     // Resize: new width
}

impl ShellIn {
    fn data(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: KIND_DATA,
            data: bytes.into(),
            rows: 0,
            cols: 0,
        }
    }
    fn resize(rows: i32, cols: i32) -> Self {
        Self {
            kind: KIND_RESIZE,
            data: Vec::new(),
            rows,
            cols,
        }
    }
}

#[derive(ForyStruct, aster::AsterType, Debug, Default, Clone, PartialEq)]
#[aster(wire = "shell/ShellOut")]
struct ShellOut {
    data: Vec<u8>, // terminal output (stdout+stderr, merged by the tty)
    eof: bool,     // the child exited; `exit_code` is now meaningful
    exit_code: i32,
}

impl ShellOut {
    fn out(bytes: Vec<u8>) -> Self {
        Self {
            data: bytes,
            eof: false,
            exit_code: 0,
        }
    }
    fn eof(code: i32) -> Self {
        Self {
            data: Vec::new(),
            eof: true,
            exit_code: code,
        }
    }
}

/// Clamp client-supplied dimensions into the `u16` the ioctl expects, so a bad
/// or hostile frame can't wrap to a tiny/garbage size.
fn clamp_size(rows: i32, cols: i32) -> PtySize {
    PtySize {
        rows: rows.clamp(1, u16::MAX as i32) as u16,
        cols: cols.clamp(1, u16::MAX as i32) as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Reaps the PTY child on drop. Dropping a `portable-pty` child does NOT kill
/// the process, so without this an early return (client disconnect, write error)
/// would leak the shell. On the normal path we `take()` the child out to `wait()`
/// for its exit code, leaving the guard a no-op.
struct ChildGuard(Option<Box<dyn portable_pty::Child + Send + Sync>>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
        }
    }
}

/// A controlled shell with a cleaned environment, for a deterministic demo.
fn shell_command() -> CommandBuilder {
    #[cfg(unix)]
    {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.env_clear();
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.env("TERM", "xterm");
        cmd.env("PS1", "$ "); // quiet, deterministic prompt
        cmd
    }
    #[cfg(windows)]
    {
        // Don't clear the env on Windows: cmd.exe needs SystemRoot et al.
        CommandBuilder::new("cmd.exe")
    }
}

// ── Service ─────────────────────────────────────────────────────────────────

#[aster::service(name = "Shell", version = 1)]
trait Shell {
    /// Open an interactive session: send `ShellIn`, receive `ShellOut`.
    #[rpc(bidi_stream)]
    async fn open(&self, input: RequestStream<ShellIn>, out: ResponseSink<ShellOut>) -> Result<()>;
}

struct PtyShell;

#[async_trait]
impl Shell for PtyShell {
    async fn open(
        &self,
        mut input: RequestStream<ShellIn>,
        out: ResponseSink<ShellOut>,
    ) -> Result<()> {
        // Allocate a real PTY and spawn the shell against its slave end.
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;
        let child = pair
            .slave
            .spawn_command(shell_command())
            .context("spawn shell")?;
        drop(pair.slave); // so the reader sees EOF once the child exits

        // From here, any early return reaps the child via the guard's Drop.
        let mut guard = ChildGuard(Some(child));

        let master = pair.master;
        let mut writer = master.take_writer().context("pty writer")?;
        let mut reader = master.try_clone_reader().context("pty reader")?;

        // PTY reads are blocking, so pump them on a thread into an async channel.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        // The pump: client frames → PTY, PTY output → client, concurrently.
        // Runs until the PTY closes, the client disconnects, or an op fails. A
        // `?` here returns early; the guard kills the still-running child.
        let mut input_open = true;
        loop {
            tokio::select! {
                msg = input.recv(), if input_open => match msg? {
                    Some(m) if m.kind == KIND_DATA => {
                        writer.write_all(&m.data).context("pty write")?;
                        writer.flush().context("pty flush")?;
                    }
                    Some(m) if m.kind == KIND_RESIZE => {
                        master.resize(clamp_size(m.rows, m.cols)).context("pty resize")?;
                    }
                    Some(_) => {}            // unknown kind: ignore
                    None => input_open = false, // client done sending; keep draining output
                },
                chunk = rx.recv() => match chunk {
                    Some(bytes) => out.send(&ShellOut::out(bytes))?,
                    None => break,           // PTY closed → child has exited
                },
            }
        }

        // Normal exit: the child closed the PTY. Take it out of the guard and
        // reap it for the exit code (so the guard's Drop becomes a no-op).
        let mut child = guard.0.take().expect("child still present");
        let code =
            tokio::task::spawn_blocking(move || child.wait().map(|s| s.exit_code()).unwrap_or(0))
                .await
                .unwrap_or(0) as i32;
        out.send(&ShellOut::eof(code))?;
        Ok(())
    }
}

// ── Driver: two in-process nodes, a scripted session ────────────────────────

fn cfg() -> AsterConfig {
    AsterConfig::builder()
        .relay(RelayMode::Disabled)
        .bind_addr("127.0.0.1:0")
        .build()
}

async fn wait_for_addr(n: &Node) {
    for _ in 0..50 {
        if !n.addr().direct_addresses.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = Node::start_with_alpns(cfg(), vec![RPC_ALPN.to_vec()]).await?;
    let client_node = Node::start(cfg()).await?;
    wait_for_addr(&server).await;
    wait_for_addr(&client_node).await;
    client_node.add_peer(&server).unwrap();
    server.add_peer(&client_node).unwrap();

    let _h = Server::new(&server)
        .register(ShellServer::new(PtyShell))
        .serve();

    let conn: RpcConnection = client_node.rpc_connect(&server.id()).await?;
    let client = ShellClient::new(conn);

    // Resize to 40×100, then exit. On Unix we ask the tty its size first; because
    // we drove a real PTY, `stty size` reports the dimensions our Resize applied.
    #[cfg(unix)]
    let program = vec![
        ShellIn::resize(40, 100),
        ShellIn::data("stty size\n"),
        ShellIn::data("exit\n"),
    ];
    #[cfg(windows)]
    let program = vec![ShellIn::resize(40, 100), ShellIn::data("exit\r\n")];

    let frames = client.open(program).await?.collect().await?;

    let mut screen = Vec::new();
    let mut exit = 0;
    for f in &frames {
        screen.extend_from_slice(&f.data);
        if f.eof {
            exit = f.exit_code;
        }
    }
    let text = String::from_utf8_lossy(&screen);
    println!("--- remote shell output ---\n{text}\n--- exited with code {exit} ---");

    #[cfg(unix)]
    assert!(
        text.contains("40 100"),
        "the Resize frame should change the tty size"
    );

    client_node.shutdown().await;
    server.shutdown().await;
    Ok(())
}
