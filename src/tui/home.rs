//! Home: is the server up, what is loaded, how much memory is free, what is downloading.

use super::popup::{Confirmed, Popup};
use super::widgets::{all_downloads, active_downloads, compact, dim, dur, kv, load_label, panel, plural, Act};
use super::{Action, Ui};
use crate::{client, human, sys};
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Gauge, Paragraph, Row, Table};
use ratatui::Frame;
use serde_json::Value;

impl Ui {
    pub(super) fn home_actions(&self) -> Vec<Act> {
        let up = self.status.is_some();
        let loaded = !self.running().is_empty();
        let stale = self.running().iter().any(|r| r["stale"].as_bool() == Some(true));
        let active = !active_downloads(&self.status).is_empty();
        let finished = all_downloads(&self.status).iter().any(|d| matches!(d.state.as_str(), "done" | "failed" | "cancelled"));
        // Only what applies now: the bar changes with the state.
        let mut a = vec![Act::new(KeyCode::Char('s'), if up { "Stop server" } else { "Start server" })];
        if loaded {
            a.push(Act::new(KeyCode::Char('u'), "Unload all"));
        }
        if stale {
            a.push(Act::new(KeyCode::Char('r'), "Reload to apply settings"));
        }
        a.push(Act::new(KeyCode::Char('f'), "Free memory"));
        if active {
            a.push(Act::new(KeyCode::Char('x'), "Cancel download"));
        }
        if finished {
            a.push(Act::new(KeyCode::Char('c'), "Clear finished downloads"));
        }
        if up {
            a.push(Act::new(KeyCode::Char('k'), "Force stop"));
        }
        if !sys::service_installed() {
            a.push(Act::new(KeyCode::Char('i'), "Start at login"));
        }
        if !sys::desktop_entry_installed() {
            a.push(Act::new(KeyCode::Char('l'), "Add to app launcher"));
        }
        a
    }

    pub(super) fn home_act(&mut self, key: KeyCode) -> Option<Action> {
        let pid = self.status.as_ref().and_then(|s| s["pid"].as_u64()).map(|p| p as u32);
        match key {
            KeyCode::Char('s') => {
                if self.status.is_some() {
                    let _ = client::stop_daemon(pid);
                    self.toast("server stopping");
                } else {
                    match client::start_daemon() {
                        Ok(()) => self.toast("server starting"),
                        Err(e) => self.toast(format!("start failed: {e}")),
                    }
                }
            }
            KeyCode::Char('u') => self.spawn_daemon_op("unloading all".into(), |d| async move {
                d.unload(None).await?;
                Ok("all models unloaded".into())
            }),
            KeyCode::Char('r') => self.spawn_daemon_op("reloading models to apply settings".into(), |d| async move {
                d.restart(None).await?;
                Ok("reloaded".into())
            }),
            KeyCode::Char('f') => self.popup = Some(Popup::free_memory(None)),
            KeyCode::Char('x') => self.popup = Some(Popup::Downloads(active_downloads(&self.status), 0)),
            KeyCode::Char('c') => {
                let d = self.d.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let _ = d.clear_downloads().await;
                    let _ = tx.send(super::Msg::Status(d.status().await));
                });
            }
            KeyCode::Char('k') => {
                self.popup = Some(Popup::Confirm("Force-stop lmdeck? This kills the server and every model backend (SIGKILL); use it when Stop does not work.".into(), Confirmed::ForceStop));
            }
            KeyCode::Char('i') => match sys::install_service() {
                Ok(()) => self.toast("user service installed: the server starts at login"),
                Err(e) => self.toast(format!("service install failed: {e}")),
            },
            KeyCode::Char('l') => match sys::desktop_entry().and_then(|_| sys::install_menu()) {
                Ok(_) => self.toast("launcher entry and Omarchy menu rows added"),
                Err(e) => self.toast(format!("failed: {e}")),
            },
            _ => {}
        }
        None
    }

    pub(super) fn draw_home(&mut self, f: &mut Frame, area: Rect) {
        let narrow = area.width < 100;
        if narrow {
            let tall = area.height >= 34;
            let [s, l, m, b, u, d] = Layout::vertical([
                Constraint::Length(8), Constraint::Min(4), Constraint::Length(8),
                Constraint::Length(if tall { 6 } else { 0 }), Constraint::Length(5), Constraint::Length(4),
            ]).areas(area);
            self.draw_server(f, s);
            self.draw_loaded(f, l);
            self.draw_memory(f, m);
            if tall {
                self.draw_backends_summary(f, b);
            }
            self.draw_usage(f, u);
            self.draw_downloads(f, d);
        } else {
            let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
            let [s, l, b] = Layout::vertical([Constraint::Length(8), Constraint::Min(4), Constraint::Length(6)]).areas(left);
            let [m, u, d] = Layout::vertical([Constraint::Length(9), Constraint::Length(6), Constraint::Min(3)]).areas(right);
            self.draw_server(f, s);
            self.draw_loaded(f, l);
            self.draw_backends_summary(f, b);
            self.draw_memory(f, m);
            self.draw_usage(f, u);
            self.draw_downloads(f, d);
        }
    }

    fn draw_server(&self, f: &mut Frame, area: Rect) {
        let base = self.cfg.base_url();
        let mut l = vec![match &self.status {
            Some(s) => Line::from(vec![
                Span::styled(" ON ", Style::new().fg(Color::Black).bg(Color::Green).bold()),
                Span::raw(format!("  pid {} · {} · up {}", s["pid"], human(s["rss"].as_u64().unwrap_or(0)), dur(s["uptime_secs"].as_u64().unwrap_or(0)))),
            ]),
            None => Line::from(vec![Span::styled(" OFF ", Style::new().fg(Color::Black).bg(Color::DarkGray).bold()), Span::styled("  models load on the first request once it runs", Style::new().fg(Color::DarkGray))]),
        }];
        l.push(kv("Ollama API", &base));
        l.push(kv("OpenAI API", &format!("{base}/v1")));
        let lan = match self.status.as_ref().filter(|s| s["host"].as_str() == Some("0.0.0.0")) {
            Some(s) => {
                let ip = s["ips"].as_array().and_then(|a| a.iter().find(|i| !i["iface"].as_str().unwrap_or("").starts_with("docker")).or(a.first())).and_then(|i| i["ip"].as_str()).unwrap_or("-");
                format!("exposed at http://{ip}:{}", self.cfg.server.port)
            }
            None => "this machine only (server.host)".into(),
        };
        l.push(kv("Network", &lan));
        l.push(kv("At login", if sys::service_installed() { if sys::service_active() { "user service, running" } else { "user service, stopped" } } else { "no (i)" }));
        let c = &self.cfg.server;
        l.push(kv("Residency", &format!("unloads after {} idle, max {} loaded", c.keep_alive, c.max_loaded)));
        f.render_widget(Paragraph::new(l).block(panel("Server")), area);
    }

    fn draw_loaded(&self, f: &mut Frame, area: Rect) {
        let block = panel("Loaded");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let progress = self.status.as_ref().and_then(|s| s.get("load")).filter(|l| !l.is_null()).cloned();
        let table_area = match &progress {
            Some(l) => {
                let [label, gauge, rest] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).areas(inner);
                f.render_widget(Line::from(Span::styled(load_label(l), Style::new().fg(Color::Cyan))), label);
                let p = l["progress"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0);
                f.render_widget(Gauge::default().ratio(p).gauge_style(Style::new().fg(Color::Cyan).bg(Color::Reset)).use_unicode(true).label(format!("{:.0}%", p * 100.0)), gauge);
                rest
            }
            None => inner,
        };
        let running = self.running();
        if running.is_empty() {
            if progress.is_none() {
                f.render_widget(Paragraph::new(vec![dim("Nothing loaded."), dim("Load one in Models (2), or let a client's first request load it.")]), table_area);
            }
            return;
        }
        let rows: Vec<Row> = running.iter().map(|r| {
            let state = if r["in_flight"].as_u64().unwrap_or(0) > 0 {
                Cell::from("working").fg(Color::Cyan)
            } else if r["stale"].as_bool() == Some(true) {
                Cell::from("settings changed (r)").fg(Color::Yellow)
            } else {
                Cell::from(r["expires_in_secs"].as_u64().map(|s| format!("unloads in {}", dur(s))).unwrap_or_else(|| "stays loaded".into()))
            };
            Row::new(vec![Cell::from(r["name"].as_str().unwrap_or("").rsplit('/').next().unwrap_or("").to_string()), Cell::from(r["format"].as_str().unwrap_or("").to_string()), Cell::from(human(r["rss"].as_u64().unwrap_or(0))), state])
        }).collect();
        let t = Table::new(rows, [Constraint::Min(16), Constraint::Length(6), Constraint::Length(7), Constraint::Length(22)])
            .header(Row::new(["Model", "Fmt", "RAM", "State"]).style(Style::new().fg(Color::DarkGray)));
        f.render_widget(t, table_area);
    }

    fn draw_memory(&self, f: &mut Frame, area: Rect) {
        let (total, avail) = sys::mem();
        let block = panel("Memory · f to free some");
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [gauge, text] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
        let used = 1.0 - avail as f64 / total.max(1) as f64;
        f.render_widget(Gauge::default().ratio(used.clamp(0.0, 1.0)).gauge_style(Style::new().fg(if used > 0.85 { Color::Yellow } else { Color::Green }).bg(Color::Reset)).use_unicode(true).label(format!("{} free of {}", human(avail), human(total))), gauge);
        let c = &self.cfg.server;
        let gib = |g: f64| (g * (1u64 << 30) as f64) as u64;
        let vm = sys::muvm_rss();
        let reserve = gib(c.mem_reserve_gib) + if vm.is_some() { gib(c.vm_reserve_gib) } else { 0 };
        let stock = crate::compat::gpu_budget();
        let heap = match c.gpu_heap.trim() {
            "auto" | "" => format!("auto (stock {}, raised per model)", human(stock)),
            "stock" => format!("stock {} (50% of RAM)", human(stock)),
            n => format!("{n} GiB for every model"),
        };
        let mut l = vec![
            kv("For models", &human(avail.saturating_sub(reserve))),
            kv("Kept free", &format!("{:.1} GiB desktop {} {:.1} GiB VM{}", c.mem_reserve_gib, if vm.is_some() { "+" } else { "(+" }, c.vm_reserve_gib, if vm.is_some() { "" } else { " when it runs)" })),
            kv("GPU heap", &heap),
        ];
        if let Some(rss) = vm {
            l.push(kv("muvm VM", &format!("running, {}", human(rss))));
        }
        let loaded: u64 = self.running().iter().map(|r| r["rss"].as_u64().unwrap_or(0)).sum();
        if loaded > 0 {
            l.push(kv("Models use", &human(loaded)));
        }
        l.push(dim("Sliders: f here, or Settings → Memory"));
        f.render_widget(Paragraph::new(l).wrap(ratatui::widgets::Wrap { trim: true }), text);
    }

    fn draw_backends_summary(&self, f: &mut Frame, area: Rect) {
        let v = &self.versions;
        let count = |fmt: crate::models::Format| self.models.iter().filter(|m| m.format == fmt).count();
        let disk: u64 = self.models.iter().map(|m| m.size).sum();
        let mlx = match v.mlx_selected.as_str() {
            "Custom Installer" => format!("Custom {}", v.mlx_legacy.as_deref().unwrap_or("")),
            "Omarchy package" => format!("package {}", v.mlx_pkg.as_deref().unwrap_or("")),
            other => other.to_string(),
        };
        let l = vec![
            kv("MLX", &format!("{mlx} · {}", plural(count(crate::models::Format::Mlx), "model"))),
            kv("GGUF", &format!("{} · {}", if v.llama_path.is_some() { "llama.cpp ready" } else { "llama.cpp not built" }, plural(count(crate::models::Format::Gguf), "model"))),
            kv("Strata", &format!("{} · {}", if v.strata_engine { "engine built" } else if v.strata_commit.is_some() { "engine not built" } else { "not installed" }, plural(count(crate::models::Format::Strata), "model"))),
            kv("On disk", &human(disk)),
        ];
        f.render_widget(Paragraph::new(l).block(panel("Backends (4) · library")), area);
    }

    fn draw_usage(&self, f: &mut Frame, area: Rect) {
        let u = self.status.as_ref().map(|s| s["usage"].clone()).unwrap_or(Value::Null);
        let today = &u["today"];
        let mut l = vec![kv("Today", &format!("{} requests · {} in · {} out", today["requests"].as_u64().unwrap_or(0), compact(today["prompt"].as_u64().unwrap_or(0)), compact(today["completion"].as_u64().unwrap_or(0))))];
        match u["last"].as_object() {
            Some(last) => {
                let n = |k: &str| last.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                l.push(kv("Last", &format!("{} · {:.1} tok/s · prompt {:.0} tok/s · TTFT {:.1}s", last.get("model").and_then(|m| m.as_str()).unwrap_or("").rsplit('/').next().unwrap_or(""), n("gen_tps"), n("prompt_tps"), n("ttft_secs"))));
            }
            None => l.push(kv("Last", "no requests yet")),
        }
        if let Some(week) = u["week"].as_array() {
            let vals: Vec<u64> = week.iter().map(|d| d["tokens"].as_u64().unwrap_or(0)).collect();
            let peak = vals.iter().copied().max().unwrap_or(0).max(1);
            let bars: Vec<char> = "▁▂▃▄▅▆▇█".chars().collect();
            let spark: String = vals.iter().map(|v| bars[((*v as f64 / peak as f64) * 7.0).round() as usize]).collect();
            l.push(kv("7 days", &format!("{spark}  peak {}", compact(peak))));
        }
        f.render_widget(Paragraph::new(l).block(panel("Usage")), area);
    }

    pub(super) fn draw_downloads(&self, f: &mut Frame, area: Rect) {
        let dl = all_downloads(&self.status);
        let block = panel("Downloads");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if dl.is_empty() {
            f.render_widget(Paragraph::new(dim("none; find models in Get models (3)")), inner);
            return;
        }
        let rows = Layout::vertical(vec![Constraint::Length(1); dl.len().min(inner.height as usize)]).split(inner);
        for (i, d) in dl.iter().take(rows.len()).enumerate() {
            let pct = if d.total > 0 { d.completed as f64 / d.total as f64 } else { 0.0 };
            let rate = d.completed as f64 / d.elapsed.max(0.1);
            let label = match d.state.as_str() {
                "done" => format!("✓ {}  {}", d.name, human(d.total)),
                "failed" => format!("✗ {}  {}", d.name, d.detail),
                "cancelled" => format!("⊘ {} (download it again to resume)", d.name),
                "resolving" => format!("… {}", d.name),
                st => format!("{} {}  {}/{}  {}/s{}", if st == "verifying" { "⧗" } else { "↓" }, d.name, human(d.completed), human(d.total), human(rate as u64), if st == "verifying" { " verifying" } else { "" }),
            };
            let color = match d.state.as_str() {
                "done" => Color::Green,
                "failed" => Color::Red,
                _ => Color::Cyan,
            };
            f.render_widget(Gauge::default().ratio(pct.clamp(0.0, 1.0)).label(label).gauge_style(Style::new().fg(color).bg(Color::Reset)).use_unicode(true), rows[i]);
        }
    }
}
