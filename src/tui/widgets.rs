//! Small building blocks shared by the screens.

use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, TableState};
use serde_json::Value;

/// One action of a screen: the keys that run it (the first is shown), its label, and why it
/// does not apply right now (shown dimmed; the key then says why instead of running it).
pub struct Act {
    pub keys: Vec<KeyCode>,
    pub label: String,
    pub why_not: Option<String>,
    /// Shown instead of the first key (e.g. "←/→").
    pub key_hint: Option<&'static str>,
}

impl Act {
    pub fn new(key: KeyCode, label: impl Into<String>) -> Self {
        Self { keys: vec![key], label: label.into(), why_not: None, key_hint: None }
    }
    /// Also run by `key`.
    pub fn or(mut self, key: KeyCode) -> Self {
        self.keys.push(key);
        self
    }
    /// Unavailable unless `ok`, with the reason.
    pub fn when(mut self, ok: bool, why: impl Into<String>) -> Self {
        if !ok && self.why_not.is_none() {
            self.why_not = Some(why.into());
        }
        self
    }
    pub fn hint(mut self, h: &'static str) -> Self {
        self.key_hint = Some(h);
        self
    }
    pub fn button(&self) -> String {
        format!("{} [{}]", self.label, self.key_hint.map(str::to_string).unwrap_or_else(|| key_name(self.keys[0])))
    }
}

pub fn key_name(k: KeyCode) -> String {
    match k {
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Delete => "Del".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        other => format!("{other:?}"),
    }
}

/// Content-sized buttons, wrapped onto rows with a blank row between them.
pub fn button_cells(area: Rect, acts: &[Act]) -> Vec<(usize, Rect)> {
    let mut cells = Vec::new();
    let (mut x, mut y) = (area.x, area.y);
    for (i, a) in acts.iter().enumerate() {
        let width = (Line::from(a.button()).width() as u16 + 4).min(area.width);
        if x != area.x && x.saturating_add(width) > area.right() {
            x = area.x;
            y = y.saturating_add(2);
        }
        if width == 0 || y >= area.bottom() {
            break;
        }
        cells.push((i, Rect::new(x, y, width, 1)));
        x = x.saturating_add(width + 2);
    }
    cells
}

pub fn button_height(width: u16, acts: &[Act]) -> u16 {
    button_cells(Rect::new(0, 0, width, u16::MAX), acts).last().map(|(_, r)| r.bottom()).unwrap_or(0)
}

/// A bar of `width` cells, filled to `ratio`, then the text beside it (never on top of the fill,
/// where a gauge's label becomes unreadable).
pub fn bar_line(ratio: f64, width: usize, color: Color, text: String) -> Line<'static> {
    let n = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    Line::from(vec![
        Span::styled("█".repeat(n), Style::new().fg(color)),
        Span::styled("░".repeat(width - n), Style::new().fg(Color::DarkGray)),
        Span::raw(format!(" {text}")),
    ])
}

pub fn panel(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(format!(" {title} "), Style::new().fg(Color::Cyan)))
}

/// `path` with the home directory written as ~.
pub fn tilde(path: &str) -> String {
    let home = crate::config::home().display().to_string();
    match path.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

pub fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

pub fn kv(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<12}"), Style::new().fg(Color::DarkGray)), Span::raw(v.to_string())])
}

pub fn dim(s: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(s.into(), Style::new().fg(Color::DarkGray)))
}

pub fn heading(s: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(s.into(), Style::new().fg(Color::Magenta).add_modifier(ratatui::style::Modifier::BOLD)))
}

pub fn move_sel(s: &mut TableState, n: usize, d: i32) {
    if n == 0 {
        s.select(None);
        return;
    }
    let i = s.selected().unwrap_or(0) as i64 + d as i64;
    s.select(Some(i.clamp(0, n as i64 - 1) as usize));
}

/// A rectangle of at most `w` x `h` centered in `a`.
pub fn centered(a: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(a.width), h.min(a.height));
    Rect::new(a.x + (a.width - w) / 2, a.y + (a.height - h) / 2, w, h)
}

pub fn dur(s: u64) -> String {
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

pub fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

pub fn fmt_gib(b: u64) -> String {
    format!("{:.1} GiB", crate::backend::gib(b))
}

pub fn parse_search_budget(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let gib = text.parse::<f64>().map_err(|_| "Enter a positive RAM budget in GiB, or leave empty for unlimited".to_string())?;
    let bytes = gib * (1u64 << 30) as f64;
    if !gib.is_finite() || bytes < 1.0 || bytes >= u64::MAX as f64 {
        return Err("RAM budget must be a finite positive number in GiB".into());
    }
    Ok(Some(bytes as u64))
}

pub struct DlRow {
    pub name: String,
    pub state: String,
    pub detail: String,
    pub total: u64,
    pub completed: u64,
    pub elapsed: f64,
}

pub fn all_downloads(s: &Option<Value>) -> Vec<DlRow> {
    s.as_ref()
        .and_then(|s| s["downloads"].as_array())
        .map(|a| {
            a.iter()
                .map(|d| DlRow {
                    name: d["name"].as_str().unwrap_or("").into(),
                    state: d["status"]["state"].as_str().unwrap_or("").into(),
                    detail: d["status"]["detail"].as_str().unwrap_or("").into(),
                    total: d["total"].as_u64().unwrap_or(0),
                    completed: d["completed"].as_u64().unwrap_or(0),
                    elapsed: d["elapsed_secs"].as_f64().unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Pull specs of the downloads still running.
pub fn active_downloads(s: &Option<Value>) -> Vec<String> {
    s.as_ref()
        .and_then(|s| s["downloads"].as_array())
        .map(|a| {
            a.iter()
                .filter(|d| matches!(d["status"]["state"].as_str(), Some("downloading" | "resolving" | "verifying")))
                .map(|d| d["spec"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// "Loading X — weights 6.7/15.0G · 32s · ETA 40s" for a /lmdeck/status load object.
pub fn load_label(l: &Value) -> String {
    let human = crate::human;
    let name = l["model"].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").to_string();
    let el = l["elapsed_secs"].as_f64().unwrap_or(0.0);
    let detail = match l["phase"].as_str().unwrap_or("") {
        // Strata: the engine's own stage (and the share of the wait it is at)
        "loading" if l["stage"].is_string() => format!("{} ({:.0}%)", l["stage"].as_str().unwrap_or(""), 100.0 * l["progress"].as_f64().unwrap_or(0.0)),
        "loading" => format!(
            "weights {}/{}{}",
            human(l["loaded_bytes"].as_u64().unwrap_or(0)),
            human(l["total_bytes"].as_u64().unwrap_or(0)),
            l["eta_secs"].as_f64().map(|e| format!(" · ETA {e:.0}s")).unwrap_or_default()
        ),
        "warming up" => match l["warmup"].as_array() {
            Some(w) if w.len() == 2 => format!("warming up {}/{} tokens", w[0], w[1]),
            _ => "warming up (compiling GPU kernels)".into(),
        },
        "starting" => "starting backend".into(),
        p => p.to_string(),
    };
    format!("Loading {name} — {detail} · {el:.0}s")
}
