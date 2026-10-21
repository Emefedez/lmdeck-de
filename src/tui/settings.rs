//! Settings: every setting in sections, with its description, default and when a change takes
//! effect in a pane of its own. Changes go through `Config::edit`, which writes only the changed
//! key into config.toml.

use super::popup::{Popup, Submit};
use super::widgets::{dim, heading, kv, panel, Act};
use super::{Action, Click, Ui};
use crate::config::{applies, get_setting, set_setting, Config, Kind, SETTINGS};
use crate::models::Format;
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

impl Ui {
    fn setting(&self) -> &'static crate::config::Setting {
        &SETTINGS[self.settings_sel.selected().unwrap_or(0).min(SETTINGS.len() - 1)]
    }

    pub(super) fn settings_actions(&self) -> Vec<Act> {
        let s = self.setting();
        let value = get_setting(&self.cfg, s.key);
        let default = get_setting(&Config::default(), s.key);
        let enter = match s.kind {
            Kind::Bool => if value == "true" { "Turn off" } else { "Turn on" },
            Kind::Choice(_) => "Next value",
            Kind::Prompt => "Open in your editor",
            _ => "Edit",
        };
        let mut acts = vec![
            Act::new(KeyCode::Enter, enter),
            Act::new(KeyCode::Right, "Change").or(KeyCode::Left).hint("←/→").when(matches!(s.kind, Kind::Bool | Kind::Choice(_) | Kind::Gib), "only on/off, choices and GiB sliders change with ←/→; Enter edits"),
            Act::new(KeyCode::Char('r'), "Reset to default").when(value != default && s.kind != Kind::Prompt, "already the default"),
            Act::new(KeyCode::Char('o'), "Open config.toml"),
        ];
        // ctx-trim (a separate TUI) edits local_tools / tool_allow / tool_deny per tool; offered
        // only where it is installed.
        if crate::config::which("ctx-trim").is_some() {
            acts.push(Act::new(KeyCode::Char('t'), "Trim agent tools (ctx-trim)"));
        }
        acts
    }

    pub(super) fn settings_act(&mut self, key: KeyCode) -> Option<Action> {
        let s = self.setting();
        match key {
            KeyCode::Enter => match s.kind {
                Kind::Bool | Kind::Choice(_) => self.step_setting(s.key, 1),
                Kind::Prompt => return self.edit_backend_prompt(s.key),
                _ => self.popup = Some(Popup::text(format!("{}: {}", s.key, s.help), get_setting(&self.cfg, s.key), Submit::Setting(s.key))),
            },
            KeyCode::Right => self.step_setting(s.key, 1),
            KeyCode::Left => self.step_setting(s.key, -1),
            KeyCode::Char('r') => {
                let default = get_setting(&Config::default(), s.key);
                self.edit_config(s.key, |c| set_setting(c, s.key, &default));
            }
            KeyCode::Char('t') => return Some(Action::Edit(vec!["ctx-trim".into(), "--tab".into(), "lmdeck".into()])),
            KeyCode::Char('o') => {
                let p = crate::config::config_path();
                if !p.exists() {
                    let _ = self.cfg.save();
                }
                return Some(Action::Edit(crate::sysprompt::editor_cmd(&self.cfg, &p)));
            }
            _ => {}
        }
        None
    }

    /// ←/→ (and Enter for on/off and choices): the next or previous value.
    fn step_setting(&mut self, key: &'static str, dir: i32) {
        let s = SETTINGS.iter().find(|s| s.key == key).unwrap();
        let cur = get_setting(&self.cfg, key);
        let next = match s.kind {
            Kind::Bool => (if cur == "true" { "false" } else { "true" }).to_string(),
            Kind::Choice(opts) => {
                let i = opts.iter().position(|o| *o == cur).unwrap_or(0) as i32;
                opts[(i + dir).rem_euclid(opts.len() as i32) as usize].to_string()
            }
            Kind::Gib => match gib_step(key, &cur, dir) {
                Some(v) => v,
                None => return,
            },
            _ => return,
        };
        self.edit_config(key, |c| set_setting(c, key, &next));
    }

    fn edit_backend_prompt(&mut self, key: &str) -> Option<Action> {
        let f = match key {
            "mlx.system_prompt" => Format::Mlx,
            "llama.system_prompt" => Format::Gguf,
            _ => Format::Strata,
        };
        let p = crate::sysprompt::backend_file(f);
        if !p.exists() {
            if let Err(e) = crate::sysprompt::set_backend_text(f, "") {
                self.toast(format!("{}: {e}", p.display()));
                return None;
            }
        }
        self.editing = Some((p.clone(), None));
        Some(Action::Edit(crate::sysprompt::editor_cmd(&self.cfg, &p)))
    }

    pub(super) fn draw_settings(&mut self, f: &mut Frame, area: Rect) {
        let narrow = area.width < 100;
        let (list, detail) = if narrow {
            let [l, d] = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).areas(area);
            (l, d)
        } else {
            let [l, d] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(area);
            (l, d)
        };
        let mut rows: Vec<Row> = Vec::new();
        let mut row_setting: Vec<Option<usize>> = Vec::new();
        let mut display_sel = None;
        let mut section = "";
        for (i, s) in SETTINGS.iter().enumerate() {
            if s.section != section {
                if !section.is_empty() {
                    rows.push(Row::new(vec![Cell::from("")]));
                    row_setting.push(None);
                }
                section = s.section;
                rows.push(Row::new(vec![Cell::from(Span::styled(s.section.to_uppercase(), Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)))]));
                row_setting.push(None);
            }
            if self.settings_sel.selected() == Some(i) {
                display_sel = Some(rows.len());
            }
            let value = get_setting(&self.cfg, s.key);
            let shown = match s.kind {
                Kind::Prompt => crate::sysprompt::backend_file(match s.key { "mlx.system_prompt" => Format::Mlx, "llama.system_prompt" => Format::Gguf, _ => Format::Strata })
                    .exists()
                    .then(|| "file".to_string())
                    .unwrap_or_else(|| "none".into()),
                Kind::Gib | Kind::Choice(_) | Kind::Bool => format!("◂ {value} ▸"),
                _ => value.clone(),
            };
            let changed = value != get_setting(&Config::default(), s.key);
            let short = s.key.split_once('.').map(|(_, b)| b).unwrap_or(s.key);
            rows.push(Row::new(vec![
                Cell::from(format!("  {short}")).style(Style::new().fg(Color::Cyan)),
                Cell::from(shown).style(if changed { Style::new().fg(Color::Yellow) } else { Style::new() }),
            ]));
            row_setting.push(Some(i));
        }
        let mut st = TableState::default().with_selected(display_sel);
        let t = Table::new(rows, [Constraint::Length(26), Constraint::Min(10)])
            .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .block(panel("Settings · yellow = changed from the default"));
        f.render_stateful_widget(t, list, &mut st);
        for row in 0..list.height.saturating_sub(2) {
            if let Some(Some(i)) = row_setting.get(st.offset() + row as usize) {
                self.clicks.push((Rect::new(list.x + 1, list.y + 1 + row, list.width.saturating_sub(2), 1), Click::Row(*i)));
            }
        }
        let s = self.setting();
        let default = get_setting(&Config::default(), s.key);
        let mut l = vec![heading(s.key.to_string()), Line::raw(s.help.to_string()), Line::raw("")];
        l.push(kv("Value", &get_setting(&self.cfg, s.key)));
        l.push(kv("Default", if default.is_empty() { "(empty)" } else { &default }));
        if let Kind::Choice(opts) = s.kind {
            l.push(kv("Choices", &opts.join(" / ")));
        }
        l.push(kv("Applies", applies(s.key)));
        l.push(Line::raw(""));
        l.push(dim(format!("Saved to {}; only this key is rewritten.", super::widgets::tilde(&crate::config::config_path().display().to_string()))));
        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(panel("About this setting")), detail);
    }
}

/// The next value of a GiB slider, or None at its end.
pub(super) fn gib_step(key: &str, cur: &str, dir: i32) -> Option<String> {
    let step = 0.5 * dir as f64;
    match key {
        "server.mem_reserve_gib" => Some(format!("{:.1}", (cur.parse::<f64>().ok()? + step).clamp(0.5, 8.0))),
        "server.vm_reserve_gib" => Some(format!("{:.1}", (cur.parse::<f64>().ok()? + step).clamp(0.0, 8.0))),
        "server.gpu_heap" => {
            let total = crate::sys::mem().0 as f64 / (1u64 << 30) as f64;
            let stock = crate::compat::gpu_budget() as f64 / (1u64 << 30) as f64;
            Some(match cur.trim() {
                // Leaving "auto" starts from the stock heap.
                "auto" | "" => format!("{:.1}", (stock * 2.0).round() / 2.0),
                "stock" => format!("{:.1}", (stock * 2.0).round() / 2.0 + step),
                n => {
                    let next = ((n.parse::<f64>().ok()? + step) * 2.0).round() / 2.0;
                    if next < 4.0 { "auto".into() } else { format!("{:.1}", next.min((total - 2.0).floor())) }
                }
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gib_slider_steps_and_clamps() {
        assert_eq!(super::gib_step("server.mem_reserve_gib", "1.5", 1).as_deref(), Some("2.0"));
        assert_eq!(super::gib_step("server.mem_reserve_gib", "0.5", -1).as_deref(), Some("0.5"));
        assert_eq!(super::gib_step("server.gpu_heap", "4.0", -1).as_deref(), Some("auto"));
    }
}
