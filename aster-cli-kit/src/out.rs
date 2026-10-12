//! Writing results: [`Report`] for one-shot commands, NDJSON events for
//! streams, styled text and width-aligned tables. No styling or table crate:
//! anstyle renders escape codes, [`Styler`] decides whether to emit them.

use std::io::{self, Write};

use anstyle::{AnsiColor, Style};
use serde::Serialize;
use unicode_width::UnicodeWidthStr;

use crate::ctx::Ctx;
use crate::error::Result;

pub const ERROR: Style = AnsiColor::Red.on_default().bold();
pub const HINT: Style = AnsiColor::Cyan.on_default();
pub const OK: Style = AnsiColor::Green.on_default().bold();
pub const HEADER: Style = Style::new().bold();
pub const DIM: Style = Style::new().dimmed();

#[derive(Debug, Clone, Copy)]
pub struct Styler {
    enabled: bool,
}

impl Styler {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn paint(self, style: Style, text: &str) -> String {
        if self.enabled {
            format!("{}{text}{}", style.render(), style.render_reset())
        } else {
            text.to_string()
        }
    }
}

/// A command result: serialised as-is for `--json`, rendered by `human`
/// otherwise. The JSON top level must be an object.
pub trait Report: Serialize {
    fn human(&self, style: Styler) -> String;
}

impl Ctx {
    /// Print a command result in the selected form.
    pub fn emit<R: Report>(&self, report: &R) -> Result<()> {
        let text = match (self.json, self.stdout_tty) {
            (true, true) => serde_json::to_string_pretty(report)?,
            (true, false) => serde_json::to_string(report)?,
            (false, _) => report.human(self.out()),
        };
        self.print(&text)
    }

    /// Print one NDJSON event of a stream.
    pub fn event<E: Serialize>(&self, event: &E) -> Result<()> {
        self.print(&serde_json::to_string(event)?)
    }

    /// Print a line to stdout. A closed pipe (`aster … | head`) is not an
    /// error: the reader has what it wanted.
    pub fn print(&self, text: &str) -> Result<()> {
        let mut out = io::stdout().lock();
        match writeln!(out, "{text}").and_then(|()| out.flush()) {
            Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e.into()),
            _ => Ok(()),
        }
    }
}

/// Left-aligned columns separated by two spaces, aligned by display width;
/// the last column is not padded. The header is bold when styling is on.
pub fn table(style: Styler, header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.width()).collect();
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.width());
        }
    }
    let line = |cells: &[String]| {
        let mut s = String::new();
        for (i, cell) in cells.iter().enumerate().take(cols) {
            s.push_str(cell);
            if i + 1 < cols {
                s.push_str(&" ".repeat(widths[i] - cell.width() + 2));
            }
        }
        s.trim_end().to_string()
    };
    let head: Vec<String> = header.iter().map(|h| h.to_string()).collect();
    let mut out = vec![style.paint(HEADER, &line(&head))];
    out.extend(rows.iter().map(|r| line(r)));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_by_display_width() {
        let rows = vec![
            vec!["*".into(), "東京".into(), "a".into()],
            vec!["".into(), "dev".into(), "b".into()],
        ];
        let t = table(Styler::new(false), &["", "NAME", "KEY"], &rows);
        assert_eq!(t, "   NAME  KEY\n*  東京  a\n   dev   b");
    }

    #[test]
    fn styler_off_emits_no_escapes() {
        assert_eq!(Styler::new(false).paint(ERROR, "x"), "x");
        assert!(Styler::new(true).paint(ERROR, "x").contains('\x1b'));
    }
}
