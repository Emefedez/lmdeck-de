//! Popups: confirmations (with the plan of what will happen), text input, the keys of the
//! current screen, cancelling downloads, freeing memory for a model, and a model's settings.

use super::widgets::{centered, dim, fmt_gib, key_name, panel, Act};
use super::{Action, Click, Msg, Ui, GLOBAL_KEYS};
use crate::config::{set_setting, Config};
use crate::models::Format;
use crate::sys;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::time::Duration;

pub(super) enum Popup {
    Confirm(String, Confirmed),
    Text { title: String, buf: String, submit: Submit },
    /// The keys of the current screen (scroll offset).
    Keys(u16),
    /// Running downloads to cancel: (pull specs, selected).
    Downloads(Vec<String>, usize),
    /// Processes and memory tweaks; with a model, what it needs and a "make room" preset.
    Free { groups: Vec<sys::ProcGroup>, sel: usize, ctx: Option<(String, String)> },
    /// A load failed for lack of a GPU kernel: (model, a recommended download).
    GpuGap(String, Option<String>),
    /// A model's own settings: (model, selected field).
    ModelSettings(String, usize),
}

#[derive(Clone, Debug)]
pub(super) enum Confirmed {
    ForceStop,
    Delete(String),
    /// Run this model on the CPU.
    Cpu(String),
    /// Drop the model's own system prompt (use the backend's).
    ResetPrompt(String),
    Run(Action),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Field {
    Context,
    Device,
    KeepAlive,
    Prompt,
    PromptMode,
    GpuLayers,
    /// Which llama.cpp build runs this GGUF: auto / stock / prism.
    Engine,
    PromptPath,
    PromptGroup,
}

#[derive(Clone, Debug)]
pub(super) enum Submit {
    Search,
    Budget,
    Setting(&'static str),
    /// A model's field, then back to its settings popup.
    Model(String, Field),
}

impl Popup {
    pub fn text(title: impl Into<String>, buf: String, submit: Submit) -> Self {
        Popup::Text { title: title.into(), buf, submit }
    }
    pub fn free_memory(ctx: Option<(String, String)>) -> Self {
        Popup::Free { groups: sys::proc_groups(unsafe { libc::getuid() }, ctx.is_some()), sel: 0, ctx }
    }
}

/// A keep_alive value: a number of seconds, N with ms/s/m/h, or negative (forever).
fn valid_duration(t: &str) -> bool {
    if t.starts_with('-') || t.parse::<f64>().is_ok() {
        return true;
    }
    let (num, unit) = t.split_at(t.find(|c: char| c.is_alphabetic()).unwrap_or(t.len()));
    num.parse::<f64>().is_ok() && matches!(unit, "ms" | "s" | "m" | "h")
}

/// strata.prompt_group's values in their order on the slider.
pub(super) fn prompt_group_step(cur: &str, dir: i32) -> &'static str {
    const ORDER: [&str; 3] = ["off", "group", "merged"];
    let i = ORDER.iter().position(|v| *v == cur).unwrap_or(1) as i32;
    ORDER[(i + dir).rem_euclid(ORDER.len() as i32) as usize]
}

fn model_fields(format: Format) -> Vec<Field> {
    let mut f = vec![Field::Context, Field::Device, Field::KeepAlive, Field::Prompt, Field::PromptMode];
    if format == Format::Gguf {
        f.push(Field::GpuLayers);
        f.push(Field::Engine);
    }
    if format == Format::Strata {
        f.retain(|f| !matches!(f, Field::Device));
        f.push(Field::PromptPath);
        f.push(Field::PromptGroup);
    }
    f
}

impl Ui {
    pub(super) fn popup_key(&mut self, k: KeyEvent) -> Option<Action> {
        let popup = self.popup.take()?;
        match popup {
            Popup::Confirm(text, c) => match k.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => return self.confirmed(c),
                KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {}
                _ => self.popup = Some(Popup::Confirm(text, c)),
            },
            Popup::Text { title, mut buf, submit } => match k.code {
                KeyCode::Esc => {
                    if let Submit::Model(name, _) = submit {
                        self.popup = Some(Popup::ModelSettings(name, 0));
                    }
                }
                KeyCode::Enter => self.submit(title, buf, submit),
                KeyCode::Backspace => {
                    buf.pop();
                    self.popup = Some(Popup::Text { title, buf, submit });
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.popup = Some(Popup::Text { title, buf, submit });
                }
                _ => self.popup = Some(Popup::Text { title, buf, submit }),
            },
            Popup::Keys(scroll) => match k.code {
                KeyCode::Esc | KeyCode::Char('q' | '?') => {}
                KeyCode::Down | KeyCode::Char('j') => self.popup = Some(Popup::Keys(scroll.saturating_add(1))),
                KeyCode::Up | KeyCode::Char('k') => self.popup = Some(Popup::Keys(scroll.saturating_sub(1))),
                _ => self.popup = Some(Popup::Keys(scroll)),
            },
            Popup::Downloads(specs, mut sel) => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Enter | KeyCode::Char('x') => {
                    if let Some(spec) = specs.get(sel).cloned() {
                        self.spawn_daemon_op(format!("cancelling {spec}"), move |d| async move {
                            let r = d.cancel(&spec).await?;
                            Ok(if r["ok"] == true { format!("cancelled {spec}; download it again to resume") } else { "that download already finished".into() })
                        });
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    sel = (sel + 1).min(specs.len().saturating_sub(1));
                    self.popup = Some(Popup::Downloads(specs, sel));
                }
                KeyCode::Up | KeyCode::Char('k') => self.popup = Some(Popup::Downloads(specs, sel.saturating_sub(1))),
                _ => self.popup = Some(Popup::Downloads(specs, sel)),
            },
            Popup::Free { groups, sel, ctx } => return self.free_key(k, groups, sel, ctx),
            Popup::GpuGap(model, rec) => match k.code {
                KeyCode::Char('c') => self.set_cpu(model, true),
                KeyCode::Char('d') => {
                    if let Some(r) = rec {
                        self.spawn_daemon_op(format!("downloading {r}"), move |d| async move {
                            d.pull(&r).await?;
                            Ok(format!("downloading {r} (progress on Home)"))
                        });
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {}
                _ => self.popup = Some(Popup::GpuGap(model, rec)),
            },
            Popup::ModelSettings(name, sel) => return self.model_settings_key(k, name, sel),
        }
        None
    }

    fn confirmed(&mut self, c: Confirmed) -> Option<Action> {
        match c {
            Confirmed::ForceStop => {
                let pid = self.status.as_ref().and_then(|s| s["pid"].as_u64()).map(|p| p as u32);
                match crate::client::force_stop(pid) {
                    Ok(()) => self.toast("force-stopped lmdeck and its backends"),
                    Err(e) => self.toast(format!("force stop failed: {e}")),
                }
            }
            Confirmed::Cpu(name) => self.set_cpu(name, false),
            Confirmed::ResetPrompt(name) => {
                let _ = std::fs::remove_file(crate::sysprompt::model_file(&name));
                self.toast(format!("{name}: uses the backend's system prompt again"));
            }
            Confirmed::Delete(name) => {
                let (cfg, tx, d) = (self.cfg.clone(), self.tx.clone(), self.d.clone());
                tokio::spawn(async move {
                    let r = if d.up().await {
                        d.delete(&name).await.map_err(|e| e.to_string())
                    } else {
                        let ms = crate::models::scan(&cfg);
                        match crate::models::resolve(&name, &ms, &cfg) {
                            Some(m) => crate::models::delete(m, &cfg).map_err(|e| e.to_string()),
                            None => Err("not found".into()),
                        }
                    };
                    let _ = tx.send(Msg::Toast(match r {
                        Ok(()) => format!("deleted {name}"),
                        Err(e) => format!("delete failed: {e}"),
                    }));
                    let _ = tx.send(Msg::Models);
                });
            }
            Confirmed::Run(a) => return Some(a),
        }
        None
    }

    fn submit(&mut self, title: String, text: String, submit: Submit) {
        match submit {
            Submit::Search => self.search_submit(text),
            Submit::Budget => {
                if let Err(e) = self.budget_submit(&text) {
                    self.toast(e);
                    self.popup = Some(Popup::Text { title, buf: text, submit: Submit::Budget });
                }
            }
            Submit::Setting(key) => {
                if !self.edit_config(key, |c| set_setting(c, key, &text)) {
                    self.popup = Some(Popup::Text { title, buf: text, submit: Submit::Setting(key) });
                }
            }
            Submit::Model(name, field) => {
                let t = text.trim().to_string();
                let ok = self.edit_override(&name, |o| match field {
                    Field::Context => {
                        o.ctx_size = if t.is_empty() { None } else { Some(t.parse().map_err(|_| format!("'{t}' is not a number of tokens"))?) };
                        Ok(())
                    }
                    Field::KeepAlive => {
                        if !t.is_empty() && !valid_duration(&t) {
                            return Err(format!("'{t}' is not a duration (5m, 1h, 0, -1)"));
                        }
                        o.keep_alive = (!t.is_empty()).then(|| t.clone());
                        Ok(())
                    }
                    Field::GpuLayers => {
                        o.gpu_layers = if t.is_empty() { None } else { Some(t.parse().map_err(|_| format!("'{t}' is not a layer count"))?) };
                        Ok(())
                    }
                    _ => Ok(()),
                });
                let sel = model_fields(self.model_format(&name)).iter().position(|f| *f == field).unwrap_or(0);
                self.popup = Some(if ok { Popup::ModelSettings(name, sel) } else { Popup::Text { title, buf: text, submit: Submit::Model(name, field) } });
            }
        }
    }

    fn model_format(&self, name: &str) -> Format {
        self.models.iter().find(|m| m.name == name).map(|m| m.format).unwrap_or(Format::Mlx)
    }

    /// Change one model's override in config.toml; an override left empty is removed.
    fn edit_override(&mut self, name: &str, f: impl FnOnce(&mut crate::config::ModelOverride) -> Result<(), String>) -> bool {
        let n = name.to_string();
        self.edit_config(name, move |c| {
            let o = c.overrides.entry(n.clone()).or_default();
            f(o)?;
            if o.device.is_none() && o.ctx_size.is_none() && o.keep_alive.is_none() && o.system_prompt_mode.is_none() && o.gpu_layers.is_none() && o.engine.is_none() && o.extra_args.is_empty() && o.env.is_empty() {
                c.overrides.remove(&n);
            }
            Ok(())
        })
    }

    /// GPU <-> CPU for a model; CPU asks first (much slower), unless `then_load` (the GPU-gap popup).
    fn set_cpu(&mut self, name: String, then_load: bool) {
        let to_cpu = self.cfg.overrides.get(&name).and_then(|o| o.device.as_deref()) != Some("cpu");
        if self.edit_override(&name, |o| {
            o.device = to_cpu.then(|| "cpu".into());
            Ok(())
        }) {
            self.toast(format!("{name} runs on the {}", if to_cpu { "CPU (much slower)" } else { "Apple GPU" }));
            if then_load {
                self.load_model(name);
            }
        }
    }

    fn model_settings_key(&mut self, k: KeyEvent, name: String, sel: usize) -> Option<Action> {
        let fields = model_fields(self.model_format(&name));
        let field = fields[sel.min(fields.len() - 1)];
        let keep = |ui: &mut Ui, sel: usize| ui.popup = Some(Popup::ModelSettings(name.clone(), sel));
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {}
            KeyCode::Down | KeyCode::Char('j') => keep(self, (sel + 1).min(fields.len() - 1)),
            KeyCode::Up | KeyCode::Char('k') => keep(self, sel.saturating_sub(1)),
            KeyCode::Left | KeyCode::Right => {
                if field == Field::PromptGroup {
                    let next = prompt_group_step(&crate::config::get_setting(&self.cfg, "strata.prompt_group"), if k.code == KeyCode::Right { 1 } else { -1 });
                    self.edit_config("strata.prompt_group", |c| crate::config::set_setting(c, "strata.prompt_group", next));
                }
                if field == Field::PromptPath {
                    let cur = crate::config::get_setting(&self.cfg, "strata.prompt_path");
                    let dir = if k.code == KeyCode::Right { 1 } else { -1 };
                    let next = crate::config::prompt_chunk_step(&cur, dir);
                    self.edit_config("strata.prompt_path", |c| crate::config::set_setting(c, "strata.prompt_path", next));
                }
                keep(self, sel);
            }
            KeyCode::Char('x') => {
                // Back to the default for this field.
                match field {
                    Field::Prompt => {
                        if crate::sysprompt::model_file(&name).exists() {
                            self.popup = Some(Popup::Confirm(format!("Remove {name}'s own system prompt file and use the backend's?"), Confirmed::ResetPrompt(name)));
                            return None;
                        }
                    }
                    Field::PromptPath => {
                        self.edit_config("strata.prompt_path", |c| crate::config::set_setting(c, "strata.prompt_path", "off"));
                    }
                    Field::PromptGroup => {
                        self.edit_config("strata.prompt_group", |c| crate::config::set_setting(c, "strata.prompt_group", "group"));
                    }
                    f => {
                        self.edit_override(&name, |o| {
                            match f {
                                Field::Context => o.ctx_size = None,
                                Field::Device => o.device = None,
                                Field::KeepAlive => o.keep_alive = None,
                                Field::PromptMode => o.system_prompt_mode = None,
                                Field::GpuLayers => o.gpu_layers = None,
                                Field::Engine => o.engine = None,
                                Field::Prompt | Field::PromptPath | Field::PromptGroup => {}
                            }
                            Ok(())
                        });
                    }
                }
                keep(self, sel);
            }
            KeyCode::Enter | KeyCode::Char(' ') => match field {
                Field::PromptGroup => {
                    let next = prompt_group_step(&crate::config::get_setting(&self.cfg, "strata.prompt_group"), 1);
                    self.edit_config("strata.prompt_group", |c| crate::config::set_setting(c, "strata.prompt_group", next));
                    keep(self, sel);
                }
                Field::PromptPath => {
                    let cur = crate::config::get_setting(&self.cfg, "strata.prompt_path");
                    let next = crate::config::prompt_chunk_cycle(&cur);
                    self.edit_config("strata.prompt_path", |c| crate::config::set_setting(c, "strata.prompt_path", next));
                    keep(self, sel);
                }
                Field::Engine => {
                    let cur = self.cfg.overrides.get(&name).and_then(|o| o.engine.clone());
                    let next = match cur.as_deref() {
                        None => "stock",
                        Some("stock") => "prism",
                        _ => "",
                    };
                    let ok = self.edit_override(&name, |o| {
                        o.engine = (!next.is_empty()).then(|| next.to_string());
                        Ok(())
                    });
                    if ok {
                        self.toast(match next {
                            "" => format!("{name}: automatic engine again"),
                            "prism" => format!("{name} will run on the Prism build (ternary types, slower)"),
                            _ => format!("{name} forced onto the stock llama.cpp build"),
                        });
                    }
                    keep(self, sel);
                }
                Field::Context | Field::KeepAlive | Field::GpuLayers => {
                    let o = self.cfg.overrides.get(&name);
                    let cur = match field {
                        Field::Context => o.and_then(|o| o.ctx_size).map(|c| c.to_string()),
                        Field::KeepAlive => o.and_then(|o| o.keep_alive.clone()),
                        _ => o.and_then(|o| o.gpu_layers).map(|c| c.to_string()),
                    };
                    let what = match field { Field::Context => "Context size in tokens", Field::KeepAlive => "Keep loaded for (5m, 1h, 0, -1)", _ => "Layers on the GPU" };
                    self.popup = Some(Popup::text(format!("{what} for {name} (empty = default)"), cur.unwrap_or_default(), Submit::Model(name, field)));
                }
                Field::Device => {
                    let on_cpu = self.cfg.overrides.get(&name).and_then(|o| o.device.as_deref()) == Some("cpu");
                    if on_cpu {
                        self.set_cpu(name.clone(), false);
                        keep(self, sel);
                    } else {
                        self.popup = Some(Popup::Confirm(format!("Run {name} on the CPU? It is much slower than the Apple GPU; use it only for models whose ops the GPU backend lacks."), Confirmed::Cpu(name)));
                    }
                }
                Field::PromptMode => {
                    let modes = crate::sysprompt::MODES;
                    let cur = self.cfg.overrides.get(&name).and_then(|o| o.system_prompt_mode.clone());
                    let next = match cur.and_then(|c| modes.iter().position(|m| *m == c)) {
                        None => Some(modes[0].to_string()),
                        Some(i) if i + 1 < modes.len() => Some(modes[i + 1].to_string()),
                        Some(_) => None,
                    };
                    self.edit_override(&name, |o| {
                        o.system_prompt_mode = next;
                        Ok(())
                    });
                    keep(self, sel);
                }
                Field::Prompt => {
                    let format = self.model_format(&name);
                    return self.edit_model_prompt(name, format);
                }
            },
            _ => keep(self, sel),
        }
        None
    }

    /// Open prompts/models/<model>.md in the editor. A model without one gets a new file seeded
    /// with the backend's prompt, so you see what applies now; left unchanged it is removed again.
    fn edit_model_prompt(&mut self, name: String, format: Format) -> Option<Action> {
        let p = crate::sysprompt::model_file(&name);
        let mut seed = None;
        if !p.exists() {
            let text = crate::sysprompt::backend_text(format);
            if let Err(e) = std::fs::create_dir_all(p.parent().unwrap()).and_then(|_| std::fs::write(&p, &text)) {
                self.toast(format!("{}: {e}", p.display()));
                return None;
            }
            seed = Some(text);
        }
        self.editing = Some((p.clone(), seed));
        Some(Action::Edit(crate::sysprompt::editor_cmd(&self.cfg, &p)))
    }

    fn free_key(&mut self, k: KeyEvent, mut groups: Vec<sys::ProcGroup>, mut sel: usize, ctx: Option<(String, String)>) -> Option<Action> {
        let uid = unsafe { libc::getuid() };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return None,
            KeyCode::Down | KeyCode::Char('j') => sel = (sel + 1).min(groups.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => sel = sel.saturating_sub(1),
            KeyCode::Char('l') if ctx.is_some() => {
                let model = ctx.map(|c| c.0).unwrap_or_default();
                self.toast(format!("trying {model} again…"));
                self.load_model(model);
                return None;
            }
            KeyCode::Char('p') if ctx.is_some() => {
                self.make_room(ctx.map(|c| c.0).unwrap_or_default());
                return None;
            }
            KeyCode::Char('r') => groups = sys::proc_groups(uid, ctx.is_some()),
            // The sliders: desktop reserve [ ], VM reserve { }, GPU heap - +
            KeyCode::Char(ch @ ('[' | ']' | '{' | '}' | '-' | '+' | '=')) => {
                let key = match ch {
                    '[' | ']' => "server.mem_reserve_gib",
                    '{' | '}' => "server.vm_reserve_gib",
                    _ => "server.gpu_heap",
                };
                let dir = if matches!(ch, ']' | '}' | '+' | '=') { 1 } else { -1 };
                let cur = crate::config::get_setting(&self.cfg, key);
                if let Some(next) = super::settings::gib_step(key, &cur, dir) {
                    self.edit_config(key, |c| set_setting(c, key, &next));
                }
            }
            KeyCode::Enter | KeyCode::Char('X') => {
                if let Some(g) = groups.get(sel).cloned() {
                    if g.tweak.is_some_and(|(id, _)| id == "preset") {
                        if let Some((model, _)) = &ctx {
                            self.make_room(model.clone());
                            return None;
                        }
                    } else if let Some((id, _)) = g.tweak.filter(|(id, _)| *id != "kill") {
                        let msg = sys::run_tweak(id);
                        self.toast(msg);
                        groups = sys::proc_groups(uid, ctx.is_some());
                    } else if let Some(why) = g.protected {
                        self.toast(format!("{} is protected ({why})", g.name));
                    } else {
                        let sig = if k.code == KeyCode::Char('X') { libc::SIGKILL } else { libc::SIGTERM };
                        for pid in &g.pids {
                            unsafe { libc::kill(*pid as i32, sig) };
                        }
                        self.toast(format!("asked {} to {} ({} processes, {})", g.name, if sig == libc::SIGKILL { "die (SIGKILL)" } else { "quit" }, g.pids.len(), fmt_gib(g.rss)));
                        std::thread::sleep(Duration::from_millis(400));
                        groups = sys::proc_groups(uid, ctx.is_some());
                    }
                }
            }
            _ => {}
        }
        sel = sel.min(groups.len().saturating_sub(1));
        self.popup = Some(Popup::Free { groups, sel, ctx });
        None
    }

    /// Free memory step by step (least disruptive first) until `model` fits, then load it.
    /// It never closes other apps: lower reserves, Hyprland effects, optional shell plugins, the VM.
    fn make_room(&mut self, model: String) {
        self.toast("making room…");
        let (d, tx, base) = (self.d.clone(), self.tx.clone(), self.cfg.base_url());
        tokio::spawn(async move {
            let fits = |d: &crate::client::Daemon| {
                let url = format!("{base}/lmdeck/plan?model={model}");
                let http = d.http().clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let v: Option<serde_json::Value> = match http.get(url).send().await { Ok(r) => r.json().await.ok(), Err(_) => None };
                    v.and_then(|v| v["ok"].as_bool()).unwrap_or(false)
                }
            };
            let say = |m: String| { let _ = tx.send(Msg::Toast(m)); };
            let mut steps = vec!["reserves", "hypr-lite", "plugins-lite", "vm"];
            if fits(&d).await {
                steps.clear();
            }
            for step in steps {
                match step {
                    "reserves" => {
                        let r = Config::edit(|c| {
                            c.server.mem_reserve_gib = c.server.mem_reserve_gib.min(1.5);
                            c.server.vm_reserve_gib = c.server.vm_reserve_gib.min(0.5);
                            Ok(())
                        });
                        if r.is_ok() {
                            let _ = d.reload().await;
                            say("lowered the reserves to 1.5 GiB (desktop) + 0.5 GiB (VM)".into());
                        }
                    }
                    "vm" => {
                        let vm: Vec<u32> = sys::proc_groups(unsafe { libc::getuid() }, false).iter().filter(|g| g.name == "muvm VM" || g.name == "muvm" || g.name == "muvm-run").flat_map(|g| g.pids.clone()).collect();
                        if !vm.is_empty() {
                            for pid in vm {
                                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                            }
                            say("closing the muvm VM…".into());
                            tokio::time::sleep(Duration::from_secs(3)).await;
                        }
                    }
                    id => say(sys::run_tweak(id)),
                }
                if fits(&d).await {
                    break;
                }
            }
            if fits(&d).await {
                say("enough memory now, loading…".into());
                let _ = tx.send(Msg::RetryLoad(model));
            } else {
                say("still not enough: close big apps from the list (Enter on a row), then press l".into());
            }
        });
    }

    // ---------- drawing ----------

    pub(super) fn draw_popup(&mut self, f: &mut Frame) {
        self.clicks.clear();
        let a = f.area();
        match self.popup.as_ref().unwrap() {
            Popup::Confirm(text, _) => {
                let text = text.clone();
                self.dialog(f, "Confirm", &text, &[Act::new(KeyCode::Char('y'), "Yes"), Act::new(KeyCode::Char('n'), "No")]);
            }
            Popup::Text { title, buf, .. } => {
                let w = a.width.saturating_sub(8).min(100);
                let r = centered(a, w, 3);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(format!("{buf}▏")).block(panel(title)), r);
            }
            Popup::Keys(scroll) => {
                let scroll = *scroll;
                let text = self.keys_text();
                let w = a.width.saturating_sub(4).min(90);
                let h = a.height.saturating_sub(2).min(text.len() as u16 + 2);
                let r = centered(a, w, h);
                f.render_widget(Clear, r);
                f.render_widget(Paragraph::new(text).scroll((scroll, 0)).block(panel(&format!("Keys · {} · Esc closes", self.tab.title()))), r);
            }
            Popup::Downloads(specs, sel) => {
                let lines: Vec<Line> = specs.iter().enumerate().map(|(i, s)| {
                    Line::styled(format!("{} {s}", if i == *sel { "▶" } else { " " }), if i == *sel { Style::new().fg(Color::Cyan).bold() } else { Style::new() })
                }).collect();
                let r = centered(a, a.width.saturating_sub(4).min(100), specs.len() as u16 + 4);
                f.render_widget(Clear, r);
                let block = panel("Cancel a download (the partial files stay, so it can resume)");
                let inner = block.inner(r);
                f.render_widget(block, r);
                let [list, help] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
                f.render_widget(Paragraph::new(lines), list);
                f.render_widget(Paragraph::new(dim("↑↓ select · Enter cancel it · Esc close")), help);
            }
            Popup::GpuGap(m, rec) => {
                let text = format!(
                    "{m} uses an operation omarchy-mlx cannot run on the Apple GPU yet.\n\nc: run it on the CPU (much slower){}\nEsc: do nothing",
                    rec.as_ref().map(|r| format!("\nd: download {r} instead (runs on the GPU)")).unwrap_or_default()
                );
                let mut acts = vec![Act::new(KeyCode::Char('c'), "Run on the CPU")];
                if rec.is_some() {
                    acts.push(Act::new(KeyCode::Char('d'), "Download the alternative"));
                }
                acts.push(Act::new(KeyCode::Esc, "Cancel"));
                self.dialog(f, "No Apple GPU kernel for this model", &text, &acts);
            }
            Popup::Free { .. } => self.draw_free(f),
            Popup::ModelSettings(name, sel) => {
                let (name, sel) = (name.clone(), *sel);
                self.draw_model_settings(f, &name, sel);
            }
        }
    }

    /// The Keys popup: what works everywhere, then this screen's actions (with why one does
    /// not apply right now).
    fn keys_text(&self) -> Vec<Line<'static>> {
        let mut l = vec![Line::from(Span::styled("Everywhere", Style::new().fg(Color::Magenta).bold()))];
        for (k, what) in GLOBAL_KEYS {
            l.push(Line::from(vec![Span::styled(format!("  {k:<10}"), Style::new().fg(Color::Cyan)), Span::raw(what.to_string())]));
        }
        l.push(Line::from(vec![Span::styled(format!("  {:<10}", "↑↓ j k"), Style::new().fg(Color::Cyan)), Span::raw("move in lists; the mouse clicks and scrolls too")]));
        l.push(Line::raw(""));
        l.push(Line::from(Span::styled(self.tab.title().to_string(), Style::new().fg(Color::Magenta).bold())));
        for a in self.actions() {
            let keys = a.keys.iter().map(|k| key_name(*k)).collect::<Vec<_>>().join(" ");
            let mut spans = vec![Span::styled(format!("  {keys:<10}"), Style::new().fg(Color::Cyan)), Span::raw(a.label.clone())];
            if let Some(w) = &a.why_not {
                spans.push(Span::styled(format!("  (now: {w})"), Style::new().fg(Color::DarkGray)));
            }
            l.push(Line::from(spans));
        }
        l
    }

    /// A centered dialog: wrapped text and a row of buttons.
    fn dialog(&mut self, f: &mut Frame, title: &str, text: &str, acts: &[Act]) {
        let a = f.area();
        let w = a.width.saturating_sub(4).min(90);
        let tw = w.saturating_sub(4).max(1) as usize;
        let lines: usize = text.lines().map(|l| l.chars().count().max(1).div_ceil(tw)).sum();
        let bh = super::widgets::button_height(w.saturating_sub(4), acts);
        let r = centered(a, w, lines as u16 + bh + 3);
        f.render_widget(Clear, r);
        let block = panel(title);
        let inner = block.inner(r).inner(Margin { horizontal: 1, vertical: 0 });
        f.render_widget(block, r);
        let [body, buttons] = Layout::vertical([Constraint::Min(0), Constraint::Length(bh)]).areas(inner);
        f.render_widget(Paragraph::new(text.to_string()).wrap(Wrap { trim: false }), body);
        self.draw_actions(f, buttons, acts);
    }

    fn draw_model_settings(&mut self, f: &mut Frame, name: &str, sel: usize) {
        let format = self.model_format(name);
        let fields = model_fields(format);
        let o = self.cfg.overrides.get(name);
        let m = self.models.iter().find(|m| m.name == name);
        let value = |fl: Field| -> String {
            match fl {
                Field::Context => o.and_then(|o| o.ctx_size).map(|c| format!("{c} tokens")).unwrap_or_else(|| format!("default ({})", match format {
                    Format::Gguf => format!("llama.ctx_size {}", self.cfg.llama.ctx_size),
                    Format::Mlx => format!("mlx.max_context {}", self.cfg.mlx.max_context),
                    Format::Strata => "the run config's".into(),
                })),
                Field::Device => if o.and_then(|o| o.device.as_deref()) == Some("cpu") { "CPU (slow)".into() } else { "Apple GPU".into() },
                Field::KeepAlive => o.and_then(|o| o.keep_alive.clone()).unwrap_or_else(|| format!("default ({})", self.cfg.server.keep_alive)),
                Field::Prompt => match crate::sysprompt::source(name, format) {
                    crate::sysprompt::Source::Model => "its own file".into(),
                    crate::sysprompt::Source::Off => "off (empty file)".into(),
                    crate::sysprompt::Source::Backend => "the backend's".into(),
                    crate::sysprompt::Source::None => "none".into(),
                },
                Field::PromptMode => o.and_then(|o| o.system_prompt_mode.clone()).unwrap_or_else(|| "the backend's".into()),
                Field::GpuLayers => o.and_then(|o| o.gpu_layers).map(|n| n.to_string()).unwrap_or_else(|| "auto (all that fit)".into()),
                Field::Engine => match o.and_then(|o| o.engine.as_deref()) {
                    Some("prism") => "Prism build".into(),
                    Some("stock") => "stock build".into(),
                    _ => {
                        let auto_prism = m.map(|m| m.needs_prism).unwrap_or(false) && self.cfg.prism_ready().is_some();
                        if auto_prism { "auto (Prism)".into() } else { "auto (stock)".into() }
                    }
                },
                Field::PromptPath => format!("◂ {} ▸", crate::config::get_setting(&self.cfg, "strata.prompt_path")),
                Field::PromptGroup => format!("◂ {} ▸", match crate::config::get_setting(&self.cfg, "strata.prompt_group").as_str() {
                    "merged" => "merged (+0.45 GB, untested)",
                    "off" => "off (one chunk at a time)",
                    _ => "group (4 chunks, experts read once)",
                }),
            }
        };
        let label = |fl: Field| match fl {
            Field::Context => "Context size",
            Field::Device => "Runs on",
            Field::KeepAlive => "Keep loaded",
            Field::Prompt => "System prompt",
            Field::PromptMode => "Prompt mode",
            Field::GpuLayers => "GPU layers",
            Field::Engine => "Engine",
            Field::PromptPath => "Prompt path",
            Field::PromptGroup => "Prompt groups",
        };
        let mut l: Vec<Line> = fields.iter().enumerate().map(|(i, fl)| {
            let st = if i == sel { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
            Line::from(vec![Span::styled(format!(" {:<14}", label(*fl)), st.fg(Color::Cyan)), Span::styled(format!(" {} ", value(*fl)), st)])
        }).collect();
        l.push(Line::raw(""));

        if fields[sel.min(fields.len() - 1)] == Field::PromptPath {
            let cur = crate::config::get_setting(&self.cfg, "strata.prompt_path");
            let mut track = vec![Span::styled(" Chunk: ", Style::new().fg(Color::Cyan))];
            for (i, &chunk) in crate::config::STRATA_PROMPT_CHUNKS.iter().enumerate() {
                if i > 0 {
                    track.push(Span::styled("──", Style::new().fg(Color::DarkGray)));
                }
                if chunk == cur {
                    track.push(Span::styled(format!("▶ {chunk} ◀"), Style::new().fg(Color::Yellow).bold()));
                } else {
                    track.push(Span::styled(format!("[{chunk}]"), Style::new().fg(Color::DarkGray)));
                }
            }
            l.push(Line::from(track));
            if let Some(m) = crate::config::strata_prompt_metrics(&cur) {
                let make_stars = |stars: u8| -> (String, String) {
                    ("★".repeat(stars as usize), "☆".repeat(5usize.saturating_sub(stars as usize)))
                };
                let (s_f, s_e) = make_stars(m.speed_stars());
                let (u_f, u_e) = make_stars(m.usability_stars());
                let (n_f, n_e) = make_stars(m.noise_heat_stars());
                l.push(Line::from(vec![
                    Span::styled(" Speed:        ", Style::new().fg(Color::Cyan)),
                    Span::styled(s_f, Style::new().fg(Color::Yellow).bold()),
                    Span::styled(s_e, Style::new().fg(Color::DarkGray)),
                    Span::raw(format!("  {:.1} tok/s ({:.1}x)", m.tok_s, m.speedup)),
                ]));
                l.push(Line::from(vec![
                    Span::styled(" Usability:    ", Style::new().fg(Color::Cyan)),
                    Span::styled(u_f, Style::new().fg(Color::Yellow).bold()),
                    Span::styled(u_e, Style::new().fg(Color::DarkGray)),
                    Span::raw(format!("  {:.1} GB decode cache ({:.2} GB buffer)", m.decode_cache_gib, m.buffer_gib)),
                ]));
                l.push(Line::from(vec![
                    Span::styled(" Noise & heat: ", Style::new().fg(Color::Cyan)),
                    Span::styled(n_f, Style::new().fg(Color::Yellow).bold()),
                    Span::styled(n_e, Style::new().fg(Color::DarkGray)),
                    Span::raw(format!("  {}", match m.mode {
                        "off" => "Cool & quiet",
                        "1024" => "Light heat & fan",
                        "2048" => "Balanced power & heat",
                        _ => "Higher power & fan",
                    })),
                ]));
            }
            l.push(Line::raw(""));
        }

        let help = match fields[sel.min(fields.len() - 1)] {
            Field::Context => "Tokens this model gets. Larger needs more memory for the attention cache.",
            Field::Device => "The CPU is a last resort for models with ops the Apple GPU backend lacks.",
            Field::KeepAlive => "How long it stays loaded after the last request: 5m, 1h, 0 (unload at once), -1 (forever).",
            Field::Prompt => "Enter opens prompts/models/<model>.md in your editor; x goes back to the backend's prompt.",
            Field::PromptMode => "prepend (before the client's), replace (instead of it), fallback (only when the client sends none).",
            Field::GpuLayers => "GGUF layers on the GPU; the rest run on the CPU. auto = as many as fit.",
            Field::Engine => "auto uses the Prism build only for GGUFs its header needs (PQ2_0/PTQ1_0); stock/prism force one.",
            Field::PromptPath => "←/→ or Enter: off (decode windows) / 1024 / 2048 / 4096 (GPU chunked path).",
            Field::PromptGroup => "group: 4 chunks share each layer's SSD reads (3x fewer, same answer) · merged: Splash-style one expert pass per 2 chunks, ~0.45 GB more while a prompt runs, untested · off.",
        };
        l.push(dim(help));
        l.push(dim("↑↓ select · Enter change · x back to the default · Esc close. Applies on the next load."));
        let a = f.area();
        let r = centered(a, a.width.saturating_sub(4).min(90), l.len() as u16 + 3);
        f.render_widget(Clear, r);
        let title = format!("Settings for {}", m.map(|m| m.name.rsplit('/').next().unwrap_or(&m.name).to_string()).unwrap_or_else(|| name.to_string()));
        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(panel(&title)), r);
        let inner = r.inner(Margin { horizontal: 1, vertical: 1 });
        for i in 0..fields.len() as u16 {
            if i < inner.height {
                self.clicks.push((Rect::new(inner.x, inner.y + i, inner.width, 1), Click::Row(i as usize)));
            }
        }
    }

    fn draw_free(&mut self, f: &mut Frame) {
        let Some(Popup::Free { groups, sel, ctx }) = &self.popup else { return };
        let (total, avail) = sys::mem();
        let a = f.area();
        let r = centered(a, a.width.saturating_sub(4).min(96), (groups.len() as u16 + 13).max(16));
        f.render_widget(Clear, r);
        let title = match ctx {
            Some((m, _)) => format!("Free memory for {m} · {} free of {}", fmt_gib(avail), fmt_gib(total)),
            None => format!("Processes · {} free of {}", fmt_gib(avail), fmt_gib(total)),
        };
        let block = panel(&title);
        let inner = block.inner(r);
        f.render_widget(block, r);
        // "needs ~X GiB ... but only Y" from the planner: how much more to free.
        let headline = ctx.as_ref().map(|(_, why)| {
            let num = |key: &str| why.split(key).nth(1).and_then(|r| r.split_whitespace().next()).and_then(|v| v.trim_end_matches(',').parse::<f64>().ok());
            match (num("needs ~"), num("but only ")) {
                (Some(need), Some(cap)) => format!("Needs ~{need:.1} GiB, only {cap:.1} GiB available to it: free about {:.1} GiB more, then press l. ({why})", (need - cap).max(0.0)),
                _ => why.clone(),
            }
        });
        let bar = |v: f64, max: f64| {
            let n = ((v / max) * 20.0).round().clamp(0.0, 20.0) as usize;
            format!("{}{}", "█".repeat(n), "░".repeat(20 - n))
        };
        let c = &self.cfg.server;
        let sliders = vec![
            Line::from(vec![Span::styled("Desktop reserve ", Style::new().fg(Color::DarkGray)), Span::raw(format!("{} {:.1} GiB  ", bar(c.mem_reserve_gib, 8.0), c.mem_reserve_gib)), Span::styled("[ less  ] more", Style::new().fg(Color::Cyan))]),
            Line::from(vec![Span::styled("VM reserve      ", Style::new().fg(Color::DarkGray)), Span::raw(format!("{} {:.1} GiB  ", bar(c.vm_reserve_gib, 8.0), c.vm_reserve_gib)), Span::styled("{ less  } more", Style::new().fg(Color::Cyan)), Span::styled("  (only while the VM runs)", Style::new().fg(Color::DarkGray))]),
            Line::from(vec![Span::styled("GPU heap        ", Style::new().fg(Color::DarkGray)), Span::raw(format!("{}  ", c.gpu_heap)), Span::styled("- less  + more", Style::new().fg(Color::Cyan)), Span::styled("  (auto = raised per model as needed)", Style::new().fg(Color::DarkGray))]),
        ];
        let [head, slide, list, help] = Layout::vertical([Constraint::Length(if headline.is_some() { 3 } else { 0 }), Constraint::Length(4), Constraint::Min(1), Constraint::Length(2)]).areas(inner);
        if let Some(h) = headline {
            f.render_widget(Paragraph::new(h).fg(Color::Yellow).wrap(Wrap { trim: true }), head);
        }
        f.render_widget(Paragraph::new(sliders), slide);
        let rows = list.height as usize;
        let start = sel.saturating_sub(rows.saturating_sub(1));
        let lines: Vec<Line> = groups.iter().enumerate().skip(start).take(rows).map(|(i, g)| {
            let name: String = g.name.chars().take(46).collect();
            let amount = if g.tweak.is_some() && g.rss == 0 { String::new() } else { format!("{:>3}x {:>9}", g.pids.len(), fmt_gib(g.rss)) };
            let note = g.protected.map(|_| "  (protected)").unwrap_or("");
            let style = if g.protected.is_some() {
                Style::new().fg(Color::DarkGray)
            } else if g.tweak.is_some() {
                Style::new().fg(Color::Cyan)
            } else if g.rss >= 1 << 30 {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new()
            };
            Line::styled(format!("{} {name:<46} {amount}{note}", if i == *sel { "▶" } else { " " }), if i == *sel { style.add_modifier(Modifier::REVERSED) } else { style })
        }).collect();
        f.render_widget(Paragraph::new(lines), list);
        let hint = groups.get(*sel).and_then(|g| g.tweak.map(|(_, d)| d.to_string()).or(g.protected.map(|p| format!("protected: {p}")))).unwrap_or_default();
        let keys = if ctx.is_some() { "Enter quit it / run it · X kill · [ ] { } - + sliders · p make room automatically · l try loading again · r refresh · Esc" } else { "Enter quit it (SIGTERM) · X kill (SIGKILL) · [ ] { } - + sliders · r refresh · Esc" };
        f.render_widget(Paragraph::new(vec![Line::raw(hint), dim(keys)]).wrap(Wrap { trim: true }), help);
    }
}
