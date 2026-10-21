//! Models: the local models, sorted by the grader (or by name, size, date), with a detail pane
//! that says what each model is, whether it fits now, and why it ranks where it does.

use super::popup::{Confirmed, Popup};
use super::widgets::{dim, heading, kv, load_label, panel, Act};
use super::{Action, Click, Ui};
use crate::grade::{self, Src};
use crate::models::{Format, LocalModel};
use crate::{human, sys};
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Recommended,
    Name,
    Size,
    Newest,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Recommended => Sort::Name,
            Sort::Name => Sort::Size,
            Sort::Size => Sort::Newest,
            Sort::Newest => Sort::Recommended,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Sort::Recommended => "recommended",
            Sort::Name => "name",
            Sort::Size => "size",
            Sort::Newest => "newest",
        }
    }
}

impl Ui {
    /// The grader's view of the local models, built when first needed (not on every frame).
    pub(super) fn grade_view(&mut self) -> &mut grade::View {
        if self.grade.is_none() {
            self.grade = Some(grade::build(&self.cfg, &self.models));
        }
        self.grade.as_mut().unwrap()
    }

    /// Indexes into `self.models` in display order.
    fn models_order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.models.len()).collect();
        match self.sort {
            Sort::Recommended => {
                if let Some(g) = &self.grade {
                    let pos = |name: &str| g.rows.iter().position(|r| r.name == name).unwrap_or(usize::MAX);
                    idx.sort_by_key(|&i| pos(&self.models[i].name));
                }
            }
            Sort::Name => idx.sort_by(|&a, &b| self.models[a].name.to_lowercase().cmp(&self.models[b].name.to_lowercase())),
            Sort::Size => idx.sort_by(|&a, &b| self.models[b].size.cmp(&self.models[a].size)),
            Sort::Newest => idx.sort_by(|&a, &b| self.models[b].modified.cmp(&self.models[a].modified)),
        }
        idx
    }

    pub(super) fn selected_model(&self) -> Option<&LocalModel> {
        let order = self.models_order();
        self.models_sel.selected().and_then(|i| order.get(i)).and_then(|&i| self.models.get(i))
    }

    fn grade_row(&self, name: &str) -> Option<&grade::Row> {
        self.grade.as_ref()?.rows.iter().find(|r| r.name == name)
    }

    pub(super) fn models_actions(&self) -> Vec<Act> {
        let m = self.selected_model();
        let none = "select a model first";
        let loaded = m.is_some_and(|m| self.running_names().contains(&m.name));
        let fits = m.and_then(|m| self.grade_row(&m.name)).is_none_or(|r| r.fit == "fits");
        vec![
            Act::new(KeyCode::Enter, if loaded { "Unload" } else { "Load" }).or(KeyCode::Char('l')).when(m.is_some(), none),
            Act::new(KeyCode::Char('c'), "Chat").when(m.is_some(), none),
            Act::new(KeyCode::Char('e'), "Model settings").when(m.is_some(), none),
            Act::new(KeyCode::Char('m'), "Make room").when(m.is_some(), none).when(!fits, "it fits in free memory now"),
            Act::new(KeyCode::Char('d'), "Delete").or(KeyCode::Delete).when(m.is_some(), none).when(!loaded, "unload it first"),
            Act::new(KeyCode::Char('o'), format!("Sort: {}", self.sort.label())),
            Act::new(KeyCode::Char('y'), "Copy name").when(m.is_some(), none),
        ]
    }

    pub(super) fn models_act(&mut self, key: KeyCode) -> Option<Action> {
        if key == KeyCode::Char('o') {
            self.sort = self.sort.next();
            self.models_sel.select(Some(0));
            return None;
        }
        let m = self.selected_model()?.clone();
        match key {
            KeyCode::Enter | KeyCode::Char('l') => {
                if self.running_names().contains(&m.name) {
                    let name = m.name.clone();
                    self.spawn_daemon_op(format!("unloading {name}"), move |d| async move {
                        d.unload(Some(&name)).await?;
                        Ok(format!("{name} unloaded"))
                    });
                } else {
                    self.load_model(m.name);
                }
            }
            KeyCode::Char('c') => {
                let exe = sys::exe_path().display().to_string();
                return Some(Action::Exec(format!("chat with {}", m.name), vec![exe, "run".into(), m.name]));
            }
            KeyCode::Char('e') => self.popup = Some(Popup::ModelSettings(m.name, 0)),
            KeyCode::Char('m') => self.open_make_room(m.name),
            KeyCode::Char('d') | KeyCode::Delete => {
                self.popup = Some(Popup::Confirm(format!("Delete {} ({}) from disk?", m.name, human(m.size)), Confirmed::Delete(m.name)));
            }
            KeyCode::Char('y') => {
                let ok = std::process::Command::new("wl-copy").arg(&m.name).status().map(|s| s.success()).unwrap_or(false);
                self.toast(if ok { format!("copied {}", m.name) } else { "wl-copy not available".into() });
            }
            _ => {}
        }
        None
    }

    /// Open the memory popup for `model` with the planner's current verdict.
    pub(super) fn open_make_room(&self, model: String) {
        let d = self.d.clone();
        let tx = self.tx.clone();
        let url = format!("{}/lmdeck/plan?model={model}", self.cfg.base_url());
        tokio::spawn(async move {
            let v: Option<serde_json::Value> = match d.http().get(url).send().await {
                Ok(r) => r.json().await.ok(),
                Err(_) => None,
            };
            let why = match v {
                Some(v) if v["ok"].as_bool() == Some(true) => format!("{model} fits right now; press l to load it."),
                Some(v) => v["error"].as_str().unwrap_or("not enough memory").to_string(),
                None => "the server is not running, so the planner could not be asked; start it on Home".into(),
            };
            let _ = tx.send(super::Msg::NeedMemory(model, why));
        });
    }

    pub(super) fn draw_models(&mut self, f: &mut Frame, area: Rect) {
        if self.sort == Sort::Recommended || self.grade.is_none() {
            self.grade_view();
        }
        if self.models.is_empty() {
            f.render_widget(Paragraph::new(vec![dim("No models yet."), dim("Get models (3) searches Hugging Face and the omarchy-mlx catalog.")]).block(panel("Models")), area);
            return;
        }
        let narrow = area.width < 110;
        let (list, detail) = if narrow {
            let [l, d] = Layout::vertical([Constraint::Min(5), Constraint::Length(10)]).areas(area);
            (l, d)
        } else {
            let [l, d] = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);
            (l, d)
        };
        let running = self.running_names();
        let order = self.models_order();
        let mark = |s: Src| match s { Src::Measured => "", Src::User => "*", Src::Estimate => "~" };
        let rows: Vec<Row> = order.iter().map(|&i| {
            let m = &self.models[i];
            let g = self.grade_row(&m.name);
            let fit = g.map(|g| g.fit).unwrap_or("?");
            let fit_color = match fit { "fits" => Color::Green, "tight" => Color::Yellow, _ => Color::Red };
            let short = m.name.rsplit('/').next().unwrap_or(&m.name).to_string();
            let mut cells = vec![
                Cell::from(if running.contains(&m.name) { "●" } else { " " }).fg(Color::Green),
                Cell::from(self.model_label(m, short)),
                Cell::from(m.format.as_str()).fg(match m.format { Format::Mlx => Color::Magenta, Format::Gguf => Color::Blue, Format::Strata => Color::Cyan }),
            ];
            if !narrow {
                cells.push(Cell::from(human(m.size)));
            }
            cells.push(Cell::from(fit).fg(fit_color));
            cells.push(Cell::from(g.and_then(|g| g.intelligence).map(|(v, s)| format!("{v:.0}{}", mark(s))).unwrap_or_else(|| "?".into())));
            if !narrow {
                cells.push(Cell::from(g.and_then(|g| g.tps).map(|(v, s)| format!("{v:.1}{}", mark(s))).unwrap_or_else(|| "?".into())));
            }
            cells.push(Cell::from(if self.sort == Sort::Recommended { g.map(|g| format!("{:.0}", g.score)).unwrap_or_default() } else { String::new() }).bold());
            let mut row = Row::new(cells);
            if fit == "too big" {
                row = row.fg(Color::DarkGray);
            }
            row
        }).collect();
        let (widths, header): (Vec<Constraint>, Vec<&str>) = if narrow {
            (vec![Constraint::Length(1), Constraint::Min(14), Constraint::Length(6), Constraint::Length(7), Constraint::Length(5), Constraint::Length(5)], vec!["", "Model", "Fmt", "Fit", "Intel", "Score"])
        } else {
            (vec![Constraint::Length(1), Constraint::Min(24), Constraint::Length(6), Constraint::Length(7), Constraint::Length(7), Constraint::Length(5), Constraint::Length(6), Constraint::Length(5)], vec!["", "Model", "Fmt", "Size", "Fit", "Intel", "tok/s", "Score"])
        };
        let title = match self.sort {
            Sort::Recommended => format!("Models · recommended for {} (g: next goal)", grade::GOALS[self.grade.as_ref().map(|g| g.goal).unwrap_or(0)].0),
            s => format!("Models · by {}", s.label()),
        };
        let t = Table::new(rows, widths)
            .header(Row::new(header).style(Style::new().fg(Color::DarkGray)))
            .row_highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .block(panel(&title));
        f.render_stateful_widget(t, list, &mut self.models_sel);
        for row in 0..list.height.saturating_sub(3) {
            let index = self.models_sel.offset() + row as usize;
            if index >= order.len() {
                break;
            }
            self.clicks.push((Rect::new(list.x + 1, list.y + 2 + row, list.width.saturating_sub(2), 1), Click::Row(index)));
        }
        self.draw_model_detail(f, detail);
    }

    /// Name plus compatibility marker and CPU tag.
    fn model_label(&self, m: &LocalModel, name: String) -> Line<'static> {
        let mut spans = vec![Span::raw(name)];
        if m.format == Format::Mlx {
            let v = crate::compat::assess_mlx(&self.cfg, m.repo.as_deref().unwrap_or(&m.name), m.qmode.as_deref());
            let color = match v.level { "unlikely" => Color::Red, "untested" => Color::Yellow, _ => Color::Green };
            let mk = crate::compat::marker(v.level);
            if !mk.is_empty() {
                spans.push(Span::styled(format!(" {mk}"), Style::new().fg(color)));
            }
        }
        if self.cfg.overrides.get(&m.name).and_then(|o| o.device.as_deref()) == Some("cpu") {
            spans.push(Span::styled(" cpu", Style::new().fg(Color::Yellow)));
        }
        Line::from(spans)
    }

    fn draw_model_detail(&self, f: &mut Frame, area: Rect) {
        let Some(m) = self.selected_model() else {
            f.render_widget(panel("Details"), area);
            return;
        };
        let g = self.grade_row(&m.name);
        let o = self.cfg.overrides.get(&m.name);
        let mut l = vec![heading(m.name.clone())];
        let params = match g.and_then(|g| g.total_b.zip(g.active_b)) {
            Some((t, a)) if a < t * 0.95 => format!("{t:.0}B total, {a:.1}B active"),
            Some((t, _)) => format!("{t:.1}B"),
            None => m.params.clone(),
        };
        l.push(kv("Model", &format!("{} · {} · {}", m.format.as_str(), m.quant, params)));
        let est = crate::compat::hardware_advice(&self.cfg, m.size, if m.format == Format::Gguf { "gguf" } else { "mlx" });
        l.push(kv("Size", &if m.format == Format::Strata { format!("{} on disk; experts stream from the SSD", human(m.size)) } else { format!("{} on disk, ~{} in memory", human(m.size), human(est.estimated_bytes)) }));
        l.push(kv("Fit now", g.map(|g| g.fit).unwrap_or("?")));
        if let Some(g) = g {
            let est = |s: Src| if s == Src::Measured { "" } else if s == Src::User { " (yours)" } else { " (est.)" };
            let intel = g.intelligence.map(|(v, s)| format!("intel {v:.0}{}", est(s))).unwrap_or_else(|| "intel ?".into());
            let speed = g.tps.map(|(v, s)| format!("{v:.1} tok/s{}", est(s))).unwrap_or_else(|| "speed ?".into());
            let power = match (g.watts, g.joules()) {
                (Some((w, s)), Some(j)) => format!("{w:.0} W, {j:.1} J/token{}", est(s)),
                _ => "power not measured yet".into(),
            };
            l.push(kv("Grade", &format!("{intel} · {speed} · {power}")));
        }
        if let Some(load) = self.status.as_ref().and_then(|s| s.get("load")).filter(|l| l["model"].as_str() == Some(&m.name)) {
            l.push(Line::from(Span::styled(load_label(load), Style::new().fg(Color::Cyan))));
        }
        let ctx = o.and_then(|o| o.ctx_size).map(|c| format!("{c} (yours)")).or(m.ctx_len.map(|c| format!("{c} (model max)"))).unwrap_or_else(|| "default".into());
        l.push(kv("Context", &ctx));
        let prompt = match crate::sysprompt::source(&m.name, m.format) {
            crate::sysprompt::Source::Model => "this model's own file",
            crate::sysprompt::Source::Off => "off for this model",
            crate::sysprompt::Source::Backend => "the backend's",
            crate::sysprompt::Source::None => "none",
        };
        l.push(kv("Sys prompt", prompt));
        if m.format == Format::Mlx {
            let v = crate::compat::assess_mlx(&self.cfg, m.repo.as_deref().unwrap_or(&m.name), m.qmode.as_deref());
            if v.level != "qualified" && !v.reason.is_empty() {
                l.push(kv("Apple GPU", &v.reason));
            }
        }
        l.push(Line::raw(""));
        l.push(dim("Intel is estimated from size and bits; grades.toml can give real scores. Speed and power are measured as you use a model."));
        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(panel("Details")), area);
    }
}
