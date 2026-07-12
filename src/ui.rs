//! Terminal presentation: ANSI colours and a small rounded "summary card"
//! renderer. Everything degrades to plain text when stderr is not a TTY, when
//! `NO_COLOR` is set, or when `TERM=dumb`.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Whether we may emit ANSI colours on stderr (computed once).
pub fn colored() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false) {
            return false;
        }
        std::io::stderr().is_terminal()
    })
}

/// Wrap `s` in the SGR `code` (e.g. `"1;36"`), or return it untouched when
/// colours are disabled.
pub fn paint(code: &str, s: &str) -> String {
    if colored() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}

/// A single line of card content, tracking its *visible* width (colour escapes
/// have zero display width but count in `String::len`, so we track it by hand).
#[derive(Default)]
pub struct Line {
    s: String,
    w: usize,
}

impl Line {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append uncoloured text.
    pub fn plain(mut self, t: &str) -> Self {
        self.s.push_str(t);
        self.w += display_width(t);
        self
    }

    /// Append text wrapped in SGR `code`.
    pub fn styled(mut self, code: &str, t: &str) -> Self {
        self.s.push_str(&paint(code, t));
        self.w += display_width(t);
        self
    }

    /// Pad with spaces until the visible width reaches `to` columns.
    pub fn pad_to(mut self, to: usize) -> Self {
        while self.w < to {
            self.s.push(' ');
            self.w += 1;
        }
        self
    }

    /// A labelled statistic: dim `label`, then the `value` in `code`, laid out
    /// in a fixed-width column so several cells align on one row.
    pub fn stat(self, label: &str, value: impl std::fmt::Display, code: &str) -> Self {
        let value = value.to_string();
        self.styled("2", label)
            .plain("  ")
            .styled(code, &value)
            .pad_to_next_col()
    }

    fn pad_to_next_col(self) -> Self {
        // Align stat cells on a fixed column grid.
        let grid = 16;
        let target = ((self.w / grid) + 1) * grid;
        self.pad_to(target)
    }
}

/// A rounded box with a coloured title, e.g.
///
/// ```text
/// ╭─ chunkloader ────────────────╮
/// │  example.com                 │
/// ╰──────────────────────────────╯
/// ```
pub struct Card {
    title: String,
    title_code: String,
    rows: Vec<Line>,
}

impl Card {
    pub fn new(title: &str, title_code: &str) -> Self {
        Self {
            title: title.to_string(),
            title_code: title_code.to_string(),
            rows: Vec::new(),
        }
    }

    pub fn line(mut self, line: Line) -> Self {
        self.rows.push(line);
        self
    }

    pub fn blank(self) -> Self {
        self.line(Line::new())
    }

    /// Render the card to stderr.
    pub fn print(&self) {
        let title_w = display_width(&self.title);
        let inner = self
            .rows
            .iter()
            .map(|r| r.w)
            .max()
            .unwrap_or(0)
            .max(title_w);

        // Top border: ╭─ <title> <dashes>╮  (inner span = inner + 4)
        let dashes = inner + 1 - title_w;
        let corner = |c: &str| paint("2", c);
        eprintln!(
            "{}{} {} {}{}",
            corner("╭─"),
            "",
            paint(&self.title_code, &self.title),
            corner(&"─".repeat(dashes)),
            corner("╮"),
        );

        for r in &self.rows {
            let pad = " ".repeat(inner - r.w);
            eprintln!("{}  {}{}  {}", corner("│"), r.s, pad, corner("│"));
        }

        eprintln!("{}{}", corner("╰"), corner(&format!("{}╯", "─".repeat(inner + 4))));
    }
}

/// Approximate display width of `text` (assumes no ANSI escapes are present —
/// callers build [`Line`]s so raw escapes never reach here). Wide CJK is out of
/// scope; every char counts as one column.
fn display_width(text: &str) -> usize {
    text.chars().count()
}

// Semantic colour codes used across the summary.
pub const BRAND: &str = "1;38;5;208"; // bold orange
pub const CHUNK: &str = "38;5;39"; // blue
pub const ASSET: &str = "38;5;213"; // pink
pub const SCRIPT: &str = "38;5;220"; // amber
pub const STYLE: &str = "38;5;51"; // cyan
pub const SOURCE: &str = "38;5;120"; // light green
pub const OK: &str = "1;38;5;46"; // bold green
pub const WARN: &str = "1;38;5;214"; // bold orange-yellow
pub const FAIL: &str = "1;38;5;196"; // bold red
