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
            Kind::Choice(_) | Kind::PromptChunk | Kind::ToolModel => "Next value",
            Kind::Prompt => "Open in your editor",
            _ => "Edit",
        };
        let mut acts = vec![
            Act::new(KeyCode::Enter, enter),
            Act::new(KeyCode::Right, "Change").or(KeyCode::Left).hint("←/→").when(matches!(s.kind, Kind::Bool | Kind::Choice(_) | Kind::Gib | Kind::PromptChunk | Kind::ToolModel), "only on/off, choices, sliders and models change with ←/→; Enter edits"),
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
                Kind::Bool | Kind::Choice(_) | Kind::ToolModel => self.step_setting(s.key, 1),
                Kind::PromptChunk => {
                    let cur = get_setting(&self.cfg, s.key);
                    let next = crate::config::prompt_chunk_cycle(&cur);
                    self.edit_config(s.key, |c| set_setting(c, s.key, next));
                }
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
            Kind::PromptChunk => crate::config::prompt_chunk_step(&cur, dir).to_string(),
            Kind::ToolModel => {
                let mut opts = vec![String::new()];
                for m in &self.models {
                    if m.tools && !opts.contains(&m.name) {
                        opts.push(m.name.clone());
                    }
                }
                let i = opts.iter().position(|o| o == &cur).unwrap_or(0) as i32;
                opts[(i + dir).rem_euclid(opts.len() as i32) as usize].clone()
            }
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
                Kind::Gib | Kind::Choice(_) | Kind::Bool | Kind::PromptChunk => format!("◂ {value} ▸"),
                Kind::ToolModel => {
                    let display = if value.is_empty() {
                        if s.key == "vscode.model" { "(auto)" } else { "(vscode.model)" }
                    } else {
                        &value
                    };
                    format!("◂ {display} ▸")
                }
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
        } else if s.kind == Kind::PromptChunk {
            l.push(kv("Choices", &crate::config::STRATA_PROMPT_CHUNKS.join(" / ")));
        } else if s.kind == Kind::ToolModel {
            let tools: Vec<&str> = self.models.iter().filter(|m| m.tools).map(|m| m.name.as_str()).collect();
            let summary = if tools.is_empty() {
                "(none local)".to_string()
            } else {
                format!("(empty = {}) / {}", if s.key == "vscode.model" { "auto" } else { "vscode.model" }, tools.join(" / "))
            };
            l.push(kv("Choices", &summary));
        }
        l.push(kv("Applies", applies(s.key)));
        l.push(Line::raw(""));

        if s.key == "strata.prompt_path" {
            let val = get_setting(&self.cfg, s.key);
            let mut track = vec![Span::styled("Chunk slider:  ", Style::new().fg(Color::Cyan))];
            for (i, &chunk) in crate::config::STRATA_PROMPT_CHUNKS.iter().enumerate() {
                if i > 0 {
                    track.push(Span::styled(" ── ", Style::new().fg(Color::DarkGray)));
                }
                if chunk == val {
                    track.push(Span::styled(format!("▶ {chunk} ◀"), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
                } else {
                    track.push(Span::styled(format!("[{chunk}]"), Style::new().fg(Color::DarkGray)));
                }
            }
            l.push(Line::from(track));
            l.push(Line::raw(""));

            if let Some(m) = crate::config::strata_prompt_metrics(&val) {
                let make_stars = |stars: u8| -> (String, String) {
                    ("★".repeat(stars as usize), "☆".repeat(5usize.saturating_sub(stars as usize)))
                };

                let (s_f, s_e) = make_stars(m.speed_stars());
                let (u_f, u_e) = make_stars(m.usability_stars());
                let (n_f, n_e) = make_stars(m.noise_heat_stars());

                let speed_desc = format!("{:.1} tok/s ({:.1}x, {:.1} s)", m.tok_s, m.speedup, m.read_s);
                let usability_desc = format!("{:.1} GB decode cache ({:.2} GB buffer)", m.decode_cache_gib, m.buffer_gib);
                let noise_desc = match m.mode {
                    "off" => "Cool & quiet (SSD streaming)",
                    "1024" => "Light heat & fan",
                    "2048" => "Balanced power & heat (best on 32 GB)",
                    "4096" => "Higher power & fan (GPU matrix units saturated)",
                    _ => "",
                };

                l.push(Line::from(vec![
                    Span::styled("Speed:        ", Style::new().fg(Color::Cyan)),
                    Span::styled(s_f, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(s_e, Style::new().fg(Color::DarkGray)),
                    Span::raw(format!("  {speed_desc}")),
                ]));
                l.push(Line::from(vec![
                    Span::styled("Usability:    ", Style::new().fg(Color::Cyan)),
                    Span::styled(u_f, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(u_e, Style::new().fg(Color::DarkGray)),
                    Span::raw(format!("  {usability_desc}")),
                ]));
                l.push(Line::from(vec![
                    Span::styled("Noise & heat: ", Style::new().fg(Color::Cyan)),
                    Span::styled(n_f, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(n_e, Style::new().fg(Color::DarkGray)),
                    Span::raw(format!("  {noise_desc}")),
                ]));
                l.push(Line::raw(""));
                l.push(Span::styled(format!("Note: {}", m.note), Style::new().fg(Color::White)).into());
                l.push(dim("Bench: 10,609-token prompt, Q2_0, ~13 GB arena on M1 Max."));
                l.push(Line::raw(""));
            }
        }
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

    #[test]
    fn prompt_chunk_slider_steps_and_clamps() {
        use crate::config::prompt_chunk_step;
        assert_eq!(prompt_chunk_step("off", -1), "off");
        assert_eq!(prompt_chunk_step("off", 1), "1024");
        assert_eq!(prompt_chunk_step("1024", -1), "off");
        assert_eq!(prompt_chunk_step("1024", 1), "2048");
        assert_eq!(prompt_chunk_step("2048", -1), "1024");
        assert_eq!(prompt_chunk_step("2048", 1), "4096");
        assert_eq!(prompt_chunk_step("4096", -1), "2048");
        assert_eq!(prompt_chunk_step("4096", 1), "4096");
    }

    #[test]
    fn prompt_chunk_cycles() {
        use crate::config::prompt_chunk_cycle;
        assert_eq!(prompt_chunk_cycle("off"), "1024");
        assert_eq!(prompt_chunk_cycle("1024"), "2048");
        assert_eq!(prompt_chunk_cycle("2048"), "4096");
        assert_eq!(prompt_chunk_cycle("4096"), "off");
    }

    #[test]
    fn strata_prompt_stars_derived_from_bench() {
        use crate::config::strata_prompt_metrics;
        let off = strata_prompt_metrics("off").unwrap();
        assert_eq!(off.speed_stars(), 1);
        assert_eq!(off.usability_stars(), 5);
        assert_eq!(off.noise_heat_stars(), 5);

        let c1024 = strata_prompt_metrics("1024").unwrap();
        assert_eq!(c1024.speed_stars(), 3);
        assert_eq!(c1024.usability_stars(), 4);
        assert_eq!(c1024.noise_heat_stars(), 4);

        let c2048 = strata_prompt_metrics("2048").unwrap();
        assert_eq!(c2048.speed_stars(), 4);
        assert_eq!(c2048.usability_stars(), 3);
        assert_eq!(c2048.noise_heat_stars(), 3);

        let c4096 = strata_prompt_metrics("4096").unwrap();
        assert_eq!(c4096.speed_stars(), 5);
        assert_eq!(c4096.usability_stars(), 2);
        assert_eq!(c4096.noise_heat_stars(), 2);
    }

    #[test]
    fn strata_prompt_path_setting_roundtrip() {
        use crate::config::{get_setting, needs_reload, set_setting, Config};
        let mut cfg = Config::default();
        assert_eq!(get_setting(&cfg, "strata.prompt_path"), "off");
        assert!(needs_reload("strata.prompt_path"));

        assert!(set_setting(&mut cfg, "strata.prompt_path", "2048").is_ok());
        assert_eq!(get_setting(&cfg, "strata.prompt_path"), "2048");
        assert_eq!(cfg.strata.prompt_path, "2048");

        assert!(set_setting(&mut cfg, "strata.prompt_path", "invalid").is_err());
    }
}
