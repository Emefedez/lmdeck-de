//! Get models: the omarchy-mlx catalog and a Hugging Face search; open a repo to see its
//! variants with their memory estimate and fit, and download one.

use super::popup::{Popup, Submit};
use super::widgets::{active_downloads, compact, dim, fmt_gib, kv, panel, Act};
use super::{Action, Click, Msg, RepoView, Ui};
use crate::hf::{self, RepoFile, RepoHit};
use crate::human;
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::Frame;

const KINDS: [&str; 3] = ["mlx", "gguf", "both"];
const SORTS: [(&str, &str); 4] = [("relevance", "relevance"), ("downloads", "downloads"), ("likes", "likes"), ("lastModified", "updated")];

impl Ui {
    pub(super) fn search_kind(&self) -> &'static str {
        KINDS[self.search_kind]
    }

    pub(super) fn discover_actions(&self) -> Vec<Act> {
        let active = !active_downloads(&self.status).is_empty();
        if let Some(rv) = &self.repo {
            let has = rv.files.is_some() && !self.repo_choices().is_empty();
            let alt = crate::compat::assess_mlx(&self.cfg, &rv.repo, None).recommend.is_some();
            return [
                Act::new(KeyCode::Enter, "Download").when(has, "no variant to download (still loading, or none matches the filters)"),
                Act::new(KeyCode::Esc, "Back").or(KeyCode::Left),
                Act::new(KeyCode::Char('a'), "Open the recommended alternative").when(alt, "no alternative known for this repo"),
            ]
            .into_iter()
            .chain(active.then(|| Act::new(KeyCode::Char('x'), "Cancel download")))
            .collect();
        }
        let n = if self.catalog_mode { self.catalog.len() } else { self.hits.len() };
        let mut a = vec![
            Act::new(KeyCode::Char('/'), "Search Hugging Face"),
            Act::new(KeyCode::Enter, "Open").or(KeyCode::Right).when(n > 0, "nothing to open"),
            Act::new(KeyCode::Char('c'), if self.catalog_mode { "Show search results" } else { "Show the omarchy-mlx catalog" }),
            Act::new(KeyCode::Char('t'), format!("Format: {}", self.search_kind().to_uppercase())),
            Act::new(KeyCode::Char('m'), format!("RAM budget: {}", self.search_budget.map(fmt_gib).unwrap_or_else(|| "any".into()))),
            Act::new(KeyCode::Char('o'), format!("Sort: {}", SORTS[self.search_sort].1)),
        ];
        if active {
            a.push(Act::new(KeyCode::Char('x'), "Cancel download"));
        }
        a
    }

    pub(super) fn discover_act(&mut self, key: KeyCode) -> Option<Action> {
        if key == KeyCode::Char('x') {
            self.popup = Some(Popup::Downloads(active_downloads(&self.status), 0));
            return None;
        }
        if self.repo.is_some() {
            match key {
                KeyCode::Esc | KeyCode::Left => self.repo = None,
                KeyCode::Char('a') => {
                    let repo = self.repo.as_ref().unwrap().repo.clone();
                    if let Some(r) = crate::compat::assess_mlx(&self.cfg, &repo, None).recommend {
                        self.open_repo(r);
                    }
                }
                KeyCode::Enter => {
                    let i = self.repo.as_ref().unwrap().sel.selected().unwrap_or(0);
                    if let Some((spec, _, _)) = self.repo_choices().get(i).cloned() {
                        self.spawn_daemon_op(format!("downloading {spec}"), move |d| async move {
                            d.pull(&spec).await?;
                            Ok(format!("downloading {spec} (progress below and on Home)"))
                        });
                    }
                }
                _ => {}
            }
            return None;
        }
        match key {
            KeyCode::Char('/') => self.popup = Some(Popup::text(format!("Search Hugging Face ({}): words, org/repo or a URL", self.search_kind().to_uppercase()), self.query.clone(), Submit::Search)),
            KeyCode::Char('m') => self.popup = Some(Popup::text("Largest estimated RAM in GiB (empty = any)", self.search_budget.map(|b| format!("{}", crate::backend::gib(b))).unwrap_or_default(), Submit::Budget)),
            KeyCode::Char('t') => {
                self.search_kind = (self.search_kind + 1) % KINDS.len();
                self.catalog_mode = false;
                self.search();
            }
            KeyCode::Char('o') => {
                self.search_sort = (self.search_sort + 1) % SORTS.len();
                self.catalog_mode = false;
                self.search();
            }
            KeyCode::Char('c') => {
                self.catalog_mode = !self.catalog_mode;
                self.hits_sel.select(Some(0));
                if !self.catalog_mode && self.hits.is_empty() {
                    self.search();
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                let i = self.hits_sel.selected().unwrap_or(0);
                let repo = if self.catalog_mode { self.catalog.get(i).and_then(|c| c["repo"].as_str()).map(str::to_string) } else { self.hits.get(i).map(|h| h.id.clone()) };
                if let Some(r) = repo {
                    self.open_repo(r);
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn discover_nav(&mut self, d: i32) {
        if let Some(n) = self.repo.as_ref().map(|_| self.repo_choices().len()) {
            super::widgets::move_sel(&mut self.repo.as_mut().unwrap().sel, n, d);
        } else {
            let n = if self.catalog_mode { self.catalog.len() } else { self.hits.len() };
            super::widgets::move_sel(&mut self.hits_sel, n, d);
        }
    }

    /// A row click: true when it was already selected (the second click opens / downloads).
    pub(super) fn discover_click(&mut self, i: usize) -> bool {
        let sel = match &mut self.repo {
            Some(rv) => &mut rv.sel,
            None => &mut self.hits_sel,
        };
        std::mem::replace(sel, TableState::default().with_selected(Some(i))).selected() == Some(i)
    }

    pub(super) fn search_submit(&mut self, text: String) {
        self.query = text;
        self.catalog_mode = false;
        self.search();
    }

    pub(super) fn budget_submit(&mut self, text: &str) -> Result<(), String> {
        self.search_budget = super::widgets::parse_search_budget(text)?;
        self.catalog_mode = false;
        self.search();
        Ok(())
    }

    pub(super) fn search(&mut self) {
        if let Some(task) = self.search_task.take() {
            task.abort();
        }
        let (q, kind, budget, cfg, tx) = (self.query.clone(), self.search_kind(), self.search_budget, self.cfg.clone(), self.tx.clone());
        let sort = SORTS[self.search_sort].0;
        self.searching = true;
        self.search_generation += 1;
        let generation = self.search_generation;
        self.repo = None;
        self.hits.clear();
        self.hits_sel.select(Some(0));
        self.search_task = Some(tokio::spawn(async move {
            let r = hf::search_discover(&hf::client(), &cfg, &q, kind, budget, sort).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::Search(generation, r));
        }));
    }

    pub(super) fn search_done(&mut self, generation: u64, r: Result<(Vec<RepoHit>, usize), String>) {
        if generation != self.search_generation {
            return;
        }
        self.searching = false;
        match r {
            Ok((h, unreadable)) => {
                self.hits = h;
                self.hits_sel.select(Some(0));
                if unreadable > 0 {
                    self.toast(format!("{unreadable} repos could not be checked and were left out"));
                }
            }
            Err(e) => {
                self.hits.clear();
                self.toast(format!("search failed: {e}"));
            }
        }
    }

    fn open_repo(&mut self, repo: String) {
        let tx = self.tx.clone();
        self.repo = Some(RepoView { repo: repo.clone(), files: None, err: None, sel: TableState::default().with_selected(Some(0)) });
        tokio::spawn(async move {
            let c = hf::client();
            let r: Result<Vec<RepoFile>, String> = async {
                let sha = hf::repo_sha(&c, &repo, "main").await?;
                hf::repo_files(&c, &repo, &sha).await
            }
            .await
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Repo(repo, r));
        });
    }

    /// Variants of the opened repo: (pull spec, label, size).
    pub(super) fn repo_choices(&self) -> Vec<(String, String, u64)> {
        let Some(rv) = &self.repo else { return vec![] };
        let Some(files) = &rv.files else { return vec![] };
        let choices = if self.catalog_mode {
            hf::choices(&rv.repo, files, &self.cfg.download.gguf_default_quant)
        } else {
            hf::discovery_choices(&self.cfg, &rv.repo, files, self.search_kind(), self.search_budget)
        };
        let advice: Vec<_> = choices.iter().map(|c| crate::compat::hardware_advice(&self.cfg, c.size, c.format)).collect();
        let recommended = crate::compat::recommended_choice(&choices, &advice)
            .filter(|&i| choices[i].format != "mlx" || crate::compat::assess_mlx(&self.cfg, &rv.repo, None).level != "unlikely");
        choices
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let label = if recommended == Some(i) { format!("{} · recommended", c.label) } else if c.default && c.format == "gguf" { format!("{} ★", c.label) } else { c.label };
                (c.spec, label, c.size)
            })
            .collect()
    }

    pub(super) fn draw_discover(&mut self, f: &mut Frame, area: Rect) {
        let [main, dl] = Layout::vertical([Constraint::Min(6), Constraint::Length(5)]).areas(area);
        let [list, detail] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(main);
        if self.catalog_mode {
            let rows: Vec<Row> = self.catalog.iter().map(|c| {
                let q = &c["qualification"];
                let st = |k: &str| q[k]["status"].as_str().unwrap_or("-").to_string();
                let qual = if st("generation") == "qualified" && st("http") == "qualified" { "✓ qualified" } else { "untested" };
                let size = c["memory"]["weights_bytes"].as_u64().unwrap_or(0);
                Row::new(vec![
                    Cell::from(c["id"].as_str().unwrap_or("").to_string()),
                    Cell::from(c["kind"].as_str().unwrap_or("").to_string()),
                    Cell::from(human(size)).fg(if crate::compat::hardware_advice(&self.cfg, size, "mlx").fit == "fits" { Color::Reset } else { Color::Yellow }),
                    Cell::from(qual).fg(if qual.starts_with('✓') { Color::Green } else { Color::DarkGray }),
                ])
            }).collect();
            let title = if self.catalog.is_empty() { "omarchy-mlx catalog (not found: install omarchy-mlx)".into() } else { format!("omarchy-mlx catalog ({})", self.catalog.len()) };
            let t = Table::new(rows, [Constraint::Min(24), Constraint::Length(9), Constraint::Length(7), Constraint::Length(12)])
                .header(Row::new(["Id", "Kind", "Size", "Apple GPU"]).style(Style::new().fg(Color::DarkGray)))
                .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
                .block(panel(&title));
            f.render_stateful_widget(t, list, &mut self.hits_sel);
        } else {
            let rows: Vec<Row> = self.hits.iter().map(|h| {
                let v = h.formats.iter().any(|x| x == "mlx").then(|| crate::compat::assess_mlx(&self.cfg, &h.id, None));
                let (mk, color) = match v.as_ref().map(|v| v.level) {
                    Some("unlikely") => ("✗ ", Color::Red),
                    Some("untested") => ("? ", Color::Yellow),
                    Some("qualified") => ("✓ ", Color::Green),
                    _ => ("  ", Color::Reset),
                };
                Row::new(vec![
                    Cell::from(Line::from(vec![Span::styled(mk, Style::new().fg(color)), Span::raw(h.id.clone())])),
                    Cell::from(h.formats.join("/").to_uppercase()),
                    Cell::from(h.estimated_bytes.map(fmt_gib).unwrap_or_else(|| "—".into())),
                    Cell::from(compact(h.downloads)),
                    Cell::from(h.last_modified.get(..10).unwrap_or("").to_string()).fg(Color::DarkGray),
                ])
            }).collect();
            let title = format!("Hugging Face · \"{}\"{}", self.query, if self.searching { " · checking variants…" } else { "" });
            let t = Table::new(rows, [Constraint::Min(20), Constraint::Length(9), Constraint::Length(10), Constraint::Length(7), Constraint::Length(10)])
                .header(Row::new(["Repo", "Format", "Est. RAM", "⬇", "Updated"]).style(Style::new().fg(Color::DarkGray)))
                .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
                .block(panel(&title));
            f.render_stateful_widget(t, list, &mut self.hits_sel);
        }
        if self.repo.is_none() {
            let count = if self.catalog_mode { self.catalog.len() } else { self.hits.len() };
            for row in 0..list.height.saturating_sub(3) {
                let index = self.hits_sel.offset() + row as usize;
                if index >= count {
                    break;
                }
                self.clicks.push((Rect::new(list.x + 1, list.y + 2 + row, list.width.saturating_sub(2), 1), Click::Row(index)));
            }
        }
        self.draw_repo(f, detail);
        self.draw_downloads(f, dl);
    }

    fn draw_repo(&mut self, f: &mut Frame, detail: Rect) {
        let choices = self.repo_choices();
        let Some(rv) = &self.repo else {
            let mut l = vec![
                Line::from(vec![Span::styled("MLX ", Style::new().fg(Color::Magenta).bold()), Span::raw("runs on the Apple GPU through omarchy-mlx.")]),
                Line::from(vec![Span::styled("GGUF ", Style::new().fg(Color::Blue).bold()), Span::raw("runs through llama.cpp on Vulkan (build it in Backends).")]),
                Line::raw(""),
                dim("RAM is estimated for your context settings. The catalog lists what the omarchy-mlx project has qualified on this GPU; other MLX repos may use ops it lacks."),
            ];
            if self.catalog_mode {
                if let Some(c) = self.hits_sel.selected().and_then(|i| self.catalog.get(i)) {
                    l.push(Line::raw(""));
                    l.push(kv("Repo", c["repo"].as_str().unwrap_or("")));
                    if let Some(s) = c["extension"]["scope"].as_str() {
                        l.push(dim(s.to_string()));
                    }
                }
            }
            f.render_widget(Paragraph::new(l).wrap(Wrap { trim: true }).block(panel("Details")), detail);
            return;
        };
        if let Some(e) = &rv.err {
            f.render_widget(Paragraph::new(e.clone()).fg(Color::Red).wrap(Wrap { trim: true }).block(panel(&rv.repo)), detail);
            return;
        }
        if rv.files.is_none() {
            f.render_widget(Paragraph::new(dim("loading the file list…")).block(panel(&rv.repo)), detail);
            return;
        }
        let verdict = Some(crate::compat::assess_mlx(&self.cfg, &rv.repo, None)).filter(|v| v.level != "ok" && choices.iter().any(|c| !c.0.contains(':')));
        let block = panel(&rv.repo);
        let inner = block.inner(detail);
        f.render_widget(block, detail);
        let [vrect, hrect, trect] = Layout::vertical([Constraint::Length(if verdict.is_some() { 3 } else { 0 }), Constraint::Length(3), Constraint::Min(2)]).areas(inner);
        if let Some(v) = &verdict {
            let color = match v.level { "unlikely" => Color::Red, "untested" => Color::Yellow, _ => Color::Green };
            let text = match v.level { "unlikely" => "likely won't run on the Apple GPU", "untested" => "untested on omarchy-mlx", _ => "qualified on omarchy-mlx" };
            let mut l = vec![Line::from(Span::styled(format!("{} {text}", crate::compat::marker(v.level)), Style::new().fg(color).bold()))];
            if !v.reason.is_empty() && v.level != "qualified" {
                l.push(dim(v.reason.clone()));
            }
            f.render_widget(Paragraph::new(l).wrap(Wrap { trim: true }), vrect);
        }
        let sel = rv.sel.selected();
        if let Some((spec, _, size)) = sel.and_then(|i| choices.get(i)) {
            let hw = crate::compat::hardware_advice(&self.cfg, *size, if spec.contains(':') { "gguf" } else { "mlx" });
            f.render_widget(Paragraph::new(dim(format!("Est. RAM {} · free {} · {} context tokens. {}", human(hw.estimated_bytes), human(hw.available_bytes), hw.context_tokens, hw.advice))).wrap(Wrap { trim: true }), hrect);
        }
        let rows: Vec<Row> = choices.iter().map(|(spec, label, size)| {
            let hw = crate::compat::hardware_advice(&self.cfg, *size, if spec.contains(':') { "gguf" } else { "mlx" });
            let color = match hw.fit { "fits" => Color::Green, "tight" => Color::Yellow, _ => Color::Red };
            Row::new(vec![Cell::from(label.clone()), Cell::from(format!("{} / ~{}", human(*size), human(hw.estimated_bytes))), Cell::from(hw.fit).fg(color)])
        }).collect();
        let t = Table::new(rows, [Constraint::Min(12), Constraint::Length(17), Constraint::Length(8)])
            .header(Row::new(["Variant", "Disk / est. RAM", "Fit now"]).style(Style::new().fg(Color::DarkGray)))
            .row_highlight_style(Style::new().bg(Color::DarkGray).bold());
        let rv = self.repo.as_mut().unwrap();
        f.render_stateful_widget(t, trect, &mut rv.sel);
        for row in 0..trect.height.saturating_sub(1) {
            let index = rv.sel.offset() + row as usize;
            if index >= choices.len() {
                break;
            }
            self.clicks.push((Rect::new(trect.x, trect.y + 1 + row, trect.width, 1), Click::Row(index)));
        }
    }
}
