//! Live view for a running job: a full-screen ratatui dashboard on the
//! alternate screen (header, progress gauge, scrollable output, throughput
//! sparkline, key hints). Full-screen mode never queries the cursor position,
//! unlike ratatui's inline viewport. On exit the terminal is restored and a
//! one-line summary is printed to normal scrollback. Falls back to plain lines
//! (or NDJSON with --json) without a TTY.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Gauge, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Sparkline,
};
use ratatui::{DefaultTerminal, Frame};
use serde_json::json;

use crate::Ctx;

/// Simulated job: emits `total` lines, one per `interval`.
struct Job {
    total: u32,
    emitted: u32,
    interval: Duration,
    next_at: Instant,
    started: Instant,
}

impl Job {
    fn poll(&mut self) -> Option<String> {
        if self.emitted >= self.total || Instant::now() < self.next_at {
            return None;
        }
        self.emitted += 1;
        self.next_at += self.interval;
        let n = self.emitted;
        Some(match n % 7 {
            0 => format!("warning: unused variable `x{n}` in src/lib.rs:{n}"),
            _ => format!("   Compiling crate-{n} v0.{n}.0"),
        })
    }
    fn done(&self) -> bool {
        self.emitted >= self.total
    }
}

pub fn run(ctx: &Ctx, lines: u32, interval_ms: u64) -> Result<ExitCode> {
    let now = Instant::now();
    let mut job = Job {
        total: lines,
        emitted: 0,
        interval: Duration::from_millis(interval_ms),
        next_at: now,
        started: now,
    };
    if ctx.json || !ctx.interactive {
        return plain(ctx, &mut job);
    }
    // try_init installs a panic hook that restores the terminal first.
    let Ok(mut terminal) = ratatui::try_init() else {
        return plain(ctx, &mut job);
    };
    let result = live(&mut terminal, &mut job);
    ratatui::restore();
    let view = result?;
    let (word, code) = match view.cancelled {
        true => ("cancelled", ExitCode::from(130)),
        false => ("exited 0", ExitCode::SUCCESS),
    };
    println!(
        "job 4f2a {word} after {:.1}s: {} lines, {} warnings",
        job.started.elapsed().as_secs_f32(),
        view.lines.len(),
        view.warnings
    );
    Ok(code)
}

fn plain(ctx: &Ctx, job: &mut Job) -> Result<ExitCode> {
    while !job.done() {
        match job.poll() {
            Some(line) if ctx.json => println!(
                "{}",
                json!({"event": "output", "stream": "stdout", "line": line})
            ),
            Some(line) => println!("{line}"),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    if ctx.json {
        println!("{}", json!({"event": "exit", "code": 0}));
    }
    Ok(ExitCode::SUCCESS)
}

const RATE_BUCKET: Duration = Duration::from_millis(250);
const RATE_HISTORY: usize = 120;

#[derive(Default)]
struct View {
    lines: Vec<String>,
    warnings: usize,
    /// Lines emitted per RATE_BUCKET, newest last.
    rate: Vec<u64>,
    /// Lines scrolled back from the tail; 0 follows new output.
    scroll_back: usize,
    cancelled: bool,
}

fn live(terminal: &mut DefaultTerminal, job: &mut Job) -> Result<View> {
    let mut view = View {
        rate: vec![0],
        ..View::default()
    };
    let mut bucket_at = Instant::now() + RATE_BUCKET;
    let mut finished_at = None;
    loop {
        while let Some(line) = job.poll() {
            view.warnings += usize::from(line.starts_with("warning"));
            view.lines.push(line);
            *view.rate.last_mut().unwrap() += 1;
            if view.scroll_back > 0 {
                view.scroll_back += 1;
            }
        }
        if Instant::now() >= bucket_at && !job.done() {
            bucket_at += RATE_BUCKET;
            view.rate.push(0);
            if view.rate.len() > RATE_HISTORY {
                view.rate.remove(0);
            }
        }
        terminal.draw(|f| draw(f, job, &view, finished_at.is_some()))?;
        if view.cancelled {
            return Ok(view);
        }
        // Linger briefly on the final frame so the result is visible.
        if job.done() {
            let at = *finished_at.get_or_insert_with(Instant::now);
            if at.elapsed() > Duration::from_millis(800) {
                return Ok(view);
            }
        }
        if !event::poll(Duration::from_millis(30))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let max_back = view.lines.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => view.cancelled = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                view.cancelled = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                view.scroll_back = (view.scroll_back + 1).min(max_back)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.scroll_back = view.scroll_back.saturating_sub(1)
            }
            KeyCode::PageUp => view.scroll_back = (view.scroll_back + 10).min(max_back),
            KeyCode::PageDown => view.scroll_back = view.scroll_back.saturating_sub(10),
            KeyCode::Char('f') | KeyCode::End => view.scroll_back = 0,
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, job: &Job, view: &View, finished: bool) {
    let (state, accent) = match (view.cancelled, finished || job.done()) {
        (true, _) => ("CANCELLED", Color::Red),
        (false, true) => ("EXITED 0", Color::Green),
        _ => ("RUNNING", Color::Cyan),
    };
    let [header, gauge, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {state} "),
                Style::new().fg(Color::Black).bg(accent).bold(),
            ),
            "  aster exec ".bold(),
            Span::raw("job 4f2a → build-01").dim(),
            Span::raw(format!("  {:.1}s", job.started.elapsed().as_secs_f32())),
        ])),
        header,
    );

    let ratio = f64::from(job.emitted) / f64::from(job.total.max(1));
    f.render_widget(
        Gauge::default()
            .block(panel("progress", accent))
            .gauge_style(Style::new().fg(accent).bg(Color::DarkGray))
            .ratio(ratio)
            .label(format!(
                "{}/{} steps · {:.0}%",
                job.emitted,
                job.total,
                ratio * 100.0
            )),
        gauge,
    );

    let [output, side] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(32)]).areas(body);
    draw_output(f, output, view, accent);
    draw_side(f, side, view, accent);

    let follow = match view.scroll_back {
        0 => Span::raw("following").fg(Color::Green),
        n => Span::raw(format!("{n} lines back")).fg(Color::Yellow),
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            key_hint("q"),
            " cancel  ".dim(),
            key_hint("↑↓ PgUp PgDn"),
            " scroll  ".dim(),
            key_hint("f"),
            " follow  ".dim(),
            follow,
        ])),
        footer,
    );
}

fn draw_output(f: &mut Frame, area: Rect, view: &View, accent: Color) {
    let rows = area.height.saturating_sub(2) as usize;
    let end = view.lines.len().saturating_sub(view.scroll_back);
    let start = end.saturating_sub(rows);
    let items: Vec<ListItem> = view.lines[start..end]
        .iter()
        .map(|l| match l.strip_prefix("warning:") {
            Some(rest) => ListItem::new(Line::from(vec![
                "warning:".fg(Color::Yellow).bold(),
                Span::raw(rest.to_owned()),
            ])),
            None => ListItem::new(l.as_str()),
        })
        .collect();
    f.render_widget(List::new(items).block(panel("output", accent)), area);
    let mut state = ScrollbarState::new(view.lines.len().saturating_sub(rows)).position(start);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area.inner(ratatui::layout::Margin::new(0, 1)),
        &mut state,
    );
}

fn draw_side(f: &mut Frame, area: Rect, view: &View, accent: Color) {
    let [stats, spark] = Layout::vertical([Constraint::Length(6), Constraint::Fill(1)]).areas(area);
    let per_sec = view.rate.iter().rev().skip(1).take(4).sum::<u64>();
    let stat = |k: &str, v: String, c: Color| {
        Line::from(vec![format!(" {k:<10}").dim(), Span::raw(v).fg(c).bold()])
    };
    f.render_widget(
        Paragraph::new(vec![
            stat("node", "build-01".into(), Color::White),
            stat("lines", view.lines.len().to_string(), Color::White),
            stat("warnings", view.warnings.to_string(), Color::Yellow),
            stat("rate", format!("{per_sec}/s"), accent),
        ])
        .block(panel("job", accent)),
        stats,
    );
    let width = spark.width.saturating_sub(2) as usize;
    let tail = &view.rate[view.rate.len().saturating_sub(width)..];
    f.render_widget(
        Sparkline::default()
            .block(panel("throughput", accent))
            .data(tail)
            .style(Style::new().fg(accent)),
        spark,
    );
}

fn panel(title: &str, accent: Color) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(accent).add_modifier(Modifier::BOLD),
        ))
}

fn key_hint(k: &str) -> Span<'_> {
    Span::styled(
        format!(" {k} "),
        Style::new().fg(Color::Black).bg(Color::Gray),
    )
}
