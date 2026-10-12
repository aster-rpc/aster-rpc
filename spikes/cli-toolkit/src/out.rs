//! Terminal output without a styling or table library: anstyle for escape
//! codes, our own colour decision, and column alignment by display width.

use std::io::IsTerminal;

use anstyle::{AnsiColor, Style};
use unicode_width::UnicodeWidthStr;

pub const ERR: Style = AnsiColor::Red.on_default().bold();
pub const ACTIVE: Style = AnsiColor::Green.on_default().bold();
pub const HEADER: Style = Style::new().bold();

/// Colour only on a terminal, and never when NO_COLOR is set (no-color.org).
fn colour(stream_is_tty: bool) -> bool {
    stream_is_tty && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub fn paint(style: Style, text: &str, stderr: bool) -> String {
    let tty = if stderr {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    };
    if colour(tty) {
        format!("{}{text}{}", style.render(), style.render_reset())
    } else {
        text.to_string()
    }
}

pub fn error(msg: &str) {
    eprintln!("{} {msg}", paint(ERR, "error:", true));
}

/// Left-aligned columns separated by two spaces; the last column is not padded.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.width()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.width());
        }
    }
    let line = |cells: Vec<String>, style: Option<Style>| {
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate() {
            let pad = if i + 1 < cols {
                widths[i] - cell.width() + 2
            } else {
                0
            };
            match style {
                Some(st) => s.push_str(&paint(st, cell, false)),
                None => s.push_str(cell),
            }
            s.push_str(&" ".repeat(pad));
        }
        s.trim_end().to_string()
    };
    let mut out = vec![line(
        header.iter().map(|h| h.to_string()).collect(),
        Some(HEADER),
    )];
    out.extend(rows.iter().map(|r| line(r.clone(), None)));
    out.join("\n")
}
