//! A *live, interactive* remote shell over Aster RPC — the "SSH replacement"
//! shape. Run it and you get a real shell prompt you can type at; `tmux`, `vim`,
//! and `htop` work, and resizing your terminal resizes the remote PTY.
//!
//! The service is a single **bidirectional** stream: keystrokes flow up as
//! `ShellIn` frames, terminal output flows down as `ShellOut` frames, both at
//! once. The server backs each call with a *real PTY* running your `$SHELL`.
//!
//! Two frame kinds travel up the same stream:
//! - `Data` — raw bytes typed at the terminal (Ctrl-C rides here as 0x03,
//!   Ctrl-D as 0x04; the tty line discipline turns them into SIGINT / EOF).
//! - `Resize` — a new (rows, cols); the server resizes the PTY in place.
//!
//! The client uses the incremental bidi API: `open_streaming()` returns a
//! `BidiCall` sender whose `send` flushes each frame immediately (so keystrokes
//! aren't buffered), plus a response stream it drains to stdout — exactly what an
//! interactive session needs, where requests are produced over time rather than
//! known up front. The whole thing is one process: it starts an in-memory server
//! node and connects a client node to it over loopback QUIC.
//!
//! Run with: `cargo run -p aster --example shell --features rpc`
//! (run it from a real terminal; type `exit` to quit.)

use std::io::{Read, Write};
use std::sync::Arc;
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

/// The shell to run on the server side: the user's `$SHELL` (so the session
/// feels native), falling back to `/bin/sh`, or `cmd.exe` on Windows.
fn shell_command() -> CommandBuilder {
    #[cfg(unix)]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
        );
        cmd
    }
    #[cfg(windows)]
    {
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

// ── Client: raw-mode terminal driving the bidi stream ────────────────────────

/// Puts the local terminal in raw mode and restores it on drop, so a panic or
/// early return never leaves the user's terminal wedged.
struct RawModeGuard(bool);

impl RawModeGuard {
    fn enter() -> Self {
        Self(crossterm::terminal::enable_raw_mode().is_ok())
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn term_size() -> (i32, i32) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    (rows as i32, cols as i32)
}

async fn run_client(client: &ShellClient) -> Result<i32> {
    let (sink, mut responses) = client.open_streaming().await?;
    let sink = Arc::new(sink);

    // Raw mode so keystrokes (and Ctrl-C / Ctrl-D) reach the remote PTY directly.
    let _raw = RawModeGuard::enter();

    // The server doesn't spawn the shell until the first frame arrives — open by
    // sending our current window size (also what a real terminal client does).
    let (rows, cols) = term_size();
    sink.send(&ShellIn::resize(rows, cols)).await?;

    // stdin → Data frames. Local stdin reads are blocking, so use a thread that
    // hands chunks to an async task which forwards them through the sink.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let stdin_sink = sink.clone();
    let stdin_task = tokio::spawn(async move {
        while let Some(bytes) = stdin_rx.recv().await {
            if stdin_sink.send(&ShellIn::data(bytes)).await.is_err() {
                break;
            }
        }
    });

    // SIGWINCH → Resize frames, so the remote PTY tracks the local window.
    #[cfg(unix)]
    let winch_task = {
        let winch_sink = sink.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sig = match signal(SignalKind::window_change()) {
                Ok(s) => s,
                Err(_) => return,
            };
            while sig.recv().await.is_some() {
                let (rows, cols) = term_size();
                if winch_sink.send(&ShellIn::resize(rows, cols)).await.is_err() {
                    break;
                }
            }
        })
    };

    // responses → stdout, until the remote shell exits.
    let mut stdout = std::io::stdout();
    let mut exit = 0;
    while let Some(frame) = responses.recv().await {
        let f = frame?;
        if !f.data.is_empty() {
            stdout.write_all(&f.data).ok();
            stdout.flush().ok();
        }
        if f.eof {
            exit = f.exit_code;
            break;
        }
    }

    stdin_task.abort();
    #[cfg(unix)]
    winch_task.abort();
    let _ = sink.finish().await;
    Ok(exit)
}

// ── Driver: two in-process nodes ─────────────────────────────────────────────

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

    let exit = run_client(&client).await;

    client_node.shutdown().await;
    server.shutdown().await;
    // Raw mode is already restored (RawModeGuard dropped); print on a fresh line.
    println!("\r\nremote shell exited with code {}", exit?);
    Ok(())
}
