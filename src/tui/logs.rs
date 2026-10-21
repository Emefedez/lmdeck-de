//! Logs: the server's log, or the log of the loaded (else selected) model's backend.

use super::widgets::{panel, Act};
use super::{Action, Ui};
use crate::config::log_dir;
use crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

impl Ui {
    pub(super) fn logs_actions(&self) -> Vec<Act> {
        vec![
            Act::new(KeyCode::Char('m'), if self.logs_model { "Show the server log" } else { "Show the model's log" }),
            Act::new(KeyCode::End, "Jump to the end").when(self.logs_scroll > 0, "already at the end"),
        ]
    }

    pub(super) fn logs_act(&mut self, key: KeyCode) -> Option<Action> {
        match key {
            KeyCode::Char('m') => self.logs_model = !self.logs_model,
            KeyCode::End => {}
            _ => return None,
        }
        self.logs_scroll = 0;
        self.load_logs();
        None
    }

    fn logs_path(&self) -> std::path::PathBuf {
        if self.logs_model {
            let name = self.running_names().first().cloned().or_else(|| self.selected_model().map(|m| m.name.clone())).unwrap_or_default();
            log_dir().join(format!("{}.log", name.replace(['/', ':'], "_")))
        } else {
            log_dir().join("lmdeck.log")
        }
    }

    pub(super) fn load_logs(&mut self) {
        self.logs = crate::sys::read_tail(&self.logs_path(), 128 * 1024);
    }

    pub(super) fn draw_logs(&self, f: &mut Frame, area: Rect) {
        let lines: Vec<&str> = self.logs.lines().collect();
        let h = area.height.saturating_sub(2) as usize;
        let end = lines.len().saturating_sub(self.logs_scroll as usize);
        let text = lines[end.saturating_sub(h)..end].join("\n");
        let title = format!("{} · {} · ↑↓ scroll", if self.logs_model { "Model log" } else { "Server log" }, super::widgets::tilde(&self.logs_path().display().to_string()));
        f.render_widget(Paragraph::new(text).block(panel(&title)), area);
    }
}
