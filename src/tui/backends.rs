//! Backends: install, update and choose what runs the models (MLX, GGUF, Strata). Every action
//! has a plan (what it does, what it changes, what it keeps) shown before anything runs, and is
//! unavailable, with the reason, when its preconditions do not hold. Nothing here switches the
//! selected runtime, resets a checkout or rewrites a setting as a side effect.

use super::popup::{Confirmed, Popup};
use super::widgets::{dim, heading, kv, panel, plural, tilde, Act};
use super::{Action, Click, Ui};
use crate::models::Format;
use crate::sys;
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

pub(super) struct Item {
    pub section: &'static str,
    pub label: String,
    pub why_not: Option<String>,
    pub plan: String,
    pub run: Option<Action>,
}

impl Ui {
    fn mlx_loaded(&self) -> bool {
        self.running().iter().any(|r| r["format"] == "mlx")
    }

    /// The selected MLX runtime as `lmdeck runtime` names it.
    fn mlx_source(&self) -> Option<&'static str> {
        match self.versions.mlx_selected.as_str() {
            "Omarchy package" => Some("omarchy"),
            "Custom Installer" => Some("custom"),
            _ => None,
        }
    }

    pub(super) fn backend_items(&self) -> Vec<Item> {
        let v = &self.versions;
        let exe = sys::exe_path().display().to_string();
        let none_selected = self.mlx_source().is_none();
        let keeps_sel = if none_selected { "No MLX runtime is selected now, so this one becomes lmdeck's.".to_string() } else { format!("Keeps lmdeck's selected MLX runtime ({}).", v.mlx_selected) };
        let mut items = vec![];

        // ---------- MLX ----------
        items.push(Item {
            section: "MLX",
            label: if v.mlx_pkg.is_some() { "Update the Omarchy packages".into() } else { "Install the Omarchy packages".into() },
            why_not: None,
            plan: format!(
                "Runs `omarchy update` first (a full system upgrade: Arch does not support partial upgrades), then installs or updates omarchy-mlx and omarchy-mlx-vulkan, the signed packages from Omarchy's repo.\n\nChanges: system packages (sudo asks for your password).\nKeeps: your models, chats and lmdeck settings. {keeps_sel}"
            ),
            run: Some(Action::Script("update the Omarchy MLX packages".into(), sys::script_update_mlx(none_selected))),
        });
        items.push(Item {
            section: "MLX",
            label: if v.mlx_legacy.is_some() { "Update the Custom release".into() } else { "Install the Custom release".into() },
            why_not: None,
            plan: format!(
                "Downloads omarchy-mlx's latest GitHub release{} and runs its official installer into ~/.local/share/mlx-omarchy (a user runtime; it can be newer than the package).\n\nChanges: ~/.local/share/mlx-omarchy and ~/.local/bin/mlx-omarchy.\nKeeps: the Omarchy packages, your models and settings. {keeps_sel}",
                v.mlx_upstream.as_deref().map(|u| format!(" ({u})")).unwrap_or_default()
            ),
            run: Some(Action::Script("install the Custom MLX release".into(), sys::script_update_mlx_custom(none_selected))),
        });
        for (source, label, installed) in [("omarchy", "package", v.mlx_pkg.is_some()), ("custom", "Custom", v.mlx_legacy.is_some())] {
            if !installed {
                continue;
            }
            let path = sys::mlx_runtime_path(source);
            items.push(Item {
                section: "MLX",
                label: format!("Use the {label} runtime"),
                why_not: if self.mlx_source() == Some(source) { Some(format!("the {label} runtime is already selected")) } else if self.mlx_loaded() { Some("unload the MLX models first (Home: Unload all)".into()) } else { None },
                plan: format!("Smoke-tests {} on the GPU, then sets the first word of mlx.command to it. The rest of mlx.command is kept, and the setting is left unchanged if the test fails.", tilde(&path.display().to_string())),
                run: Some(Action::Exec(format!("use the {label} MLX runtime"), vec![exe.clone(), "runtime".into(), source.into()])),
            });
        }
        if let (Some(_), Some(_), Some(sel)) = (&v.mlx_pkg, &v.mlx_legacy, self.mlx_source()) {
            let other = if sel == "omarchy" { "the Custom release (~/.local/share/mlx-omarchy)" } else { "the omarchy-mlx package (omarchy-mlx-vulkan, the GPU driver, stays)" };
            items.push(Item {
                section: "MLX",
                label: "Remove the runtime lmdeck does not use".into(),
                why_not: self.mlx_loaded().then(|| "unload the MLX models first (Home: Unload all)".into()),
                plan: format!("Checks the selected runtime ({}) on the GPU, then removes {other}.\n\nKeeps: your models, chats and settings.", v.mlx_selected),
                run: Some(Action::Exec("remove the unused MLX runtime".into(), vec![exe.clone(), "runtime".into(), sel.into(), "--only".into()])),
            });
        }
        if v.mlx_legacy.is_some() && v.mlx_pkg.is_none() {
            items.push(Item {
                section: "MLX",
                label: "Move the script install to the packages".into(),
                why_not: None,
                plan: "Installs the Omarchy packages, checks them on the GPU, selects them, then runs upstream's mlx-omarchy-retire-legacy (a dry run first; you confirm) to remove the old venv, launchers and shims.\n\nKeeps: your models (~/.cache/huggingface) and chats. If the packaged runtime fails its check, the old install stays.".into(),
                run: Some(Action::Script("move MLX to the Omarchy packages".into(), sys::script_migrate(v.ane))),
            });
        }

        // ---------- GGUF ----------
        let own = !self.cfg.llama.server_path.is_empty();
        let built = crate::config::llama_dir().join("bin/llama-server").exists();
        items.push(Item {
            section: "GGUF",
            label: if built { format!("Update llama.cpp ({})", self.cfg.llama.git_ref) } else { format!("Build llama.cpp ({})", self.cfg.llama.git_ref) },
            why_not: None,
            plan: format!(
                "Fetches llama.cpp at {} into ~/.local/share/lmdeck/llama.cpp and builds llama-server with Vulkan. Missing build tools are installed first (git, cmake, shaderc, Vulkan headers).\n\nChanges: ~/.local/share/lmdeck/llama.cpp. The new binary replaces the old one only once it built.\nKeeps: your GGUF models and settings.{}",
                self.cfg.llama.git_ref,
                if own { format!("\n\nNote: llama.server_path is set ({}), so lmdeck keeps using that binary; clear it in Settings to use this build.", self.cfg.llama.server_path) } else { String::new() }
            ),
            run: Some(Action::Script("build llama.cpp (Vulkan)".into(), sys::script_build_llama(&self.cfg))),
        });

        // ---------- Strata ----------
        let dir = self.cfg.strata.dir();
        let installed = v.strata_commit.is_some();
        let dirty = (!v.strata_dirty.is_empty()).then(|| format!("{} has local changes ({}); commit or stash them first", dir.display(), v.strata_dirty.join(", ")));
        items.push(Item {
            section: "Strata",
            label: if installed { "Update Strata".into() } else { "Install Strata".into() },
            why_not: dirty,
            plan: format!(
                "{} {} at branch {} in {}, sets up its server's Python packages, builds the Apple GPU driver it runs on if missing (honeykrisp-strata, a private copy: the desktop keeps its Mesa), and builds the Vulkan engine.\n\nChanges: {} and ~/.local/share/lmdeck/honeykrisp-strata.\nKeeps: downloaded models and run configs. An update only fast-forwards: it stops if the checkout has commits of its own.\nThe model itself is not downloaded (see Set up a Strata model).",
                if installed { "Fast-forwards" } else { "Clones" },
                self.cfg.strata.repo,
                self.cfg.strata.git_ref,
                dir.display(),
                dir.display()
            ),
            run: Some(Action::Script("install Strata".into(), sys::script_install_strata(&self.cfg))),
        });
        let py = dir.join(".venv/bin/python").display().to_string();
        items.push(Item {
            section: "Strata",
            label: "Set up a Strata model".into(),
            why_not: if !installed { Some("install Strata first".into()) } else if !v.strata_engine { Some("the engine is not built yet: run Update Strata".into()) } else { None },
            plan: format!(
                "Runs Strata's setup ({}/vulkan/setup_asahi.py), which asks which model to set up, then downloads it (tens of GB), packs it, writes the experts' streaming copy and the run config strata-<model>.json. lmdeck lists it as strata/<model>.\n\nChanges: the model's data folder and the run config (an existing one is kept as a .bak copy).",
                dir.display()
            ),
            run: Some(Action::Exec("set up a Strata model".into(), vec![py, dir.join("vulkan/setup_asahi.py").display().to_string(), "--no-start".into()])),
        });
        items
    }

    pub(super) fn backends_actions(&self) -> Vec<Act> {
        let items = self.backend_items();
        let sel = items.get(self.backends_sel);
        vec![
            Act::new(KeyCode::Enter, sel.map(|i| i.label.clone()).unwrap_or_else(|| "Run".into())).when(sel.is_some(), "select an action").when(sel.and_then(|i| i.why_not.clone()).is_none(), sel.and_then(|i| i.why_not.clone()).unwrap_or_default()),
            Act::new(KeyCode::Char('r'), "Check again"),
        ]
    }

    pub(super) fn backends_act(&mut self, key: KeyCode) -> Option<Action> {
        match key {
            KeyCode::Char('r') => {
                let _ = std::fs::remove_file(crate::config::state_dir().join("upstream-release"));
                self.refresh_versions();
                self.toast("checking the backends…");
            }
            KeyCode::Enter => {
                let item = self.backend_items().into_iter().nth(self.backends_sel)?;
                if let Some(run) = item.run {
                    self.popup = Some(Popup::Confirm(format!("{}\n\n{}", item.label, item.plan), Confirmed::Run(run)));
                }
            }
            _ => {}
        }
        None
    }

    pub(super) fn draw_backends(&mut self, f: &mut Frame, area: Rect) {
        let items = self.backend_items();
        self.backends_sel = self.backends_sel.min(items.len().saturating_sub(1));
        let narrow = area.width < 100;
        let (list, plan) = if narrow {
            let [l, p] = Layout::vertical([Constraint::Min(8), Constraint::Length(9)]).areas(area);
            (l, p)
        } else {
            let [l, p] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);
            (l, p)
        };
        let block = panel("Backends");
        let inner = block.inner(list);
        f.render_widget(block, list);
        let mut lines: Vec<Line> = vec![];
        let mut item_rows: Vec<(usize, u16)> = vec![];
        for section in ["MLX", "GGUF", "Strata"] {
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            let count = self.models.iter().filter(|m| m.format == match section { "MLX" => Format::Mlx, "GGUF" => Format::Gguf, _ => Format::Strata }).count();
            lines.push(heading(format!("{section} · {}", plural(count, "model"))));
            lines.extend(self.backend_status(section));
            for (i, it) in items.iter().enumerate().filter(|(_, it)| it.section == section) {
                item_rows.push((i, lines.len() as u16));
                let selected = i == self.backends_sel;
                let mut style = if it.why_not.is_some() { Style::new().fg(Color::DarkGray) } else { Style::new().fg(Color::Cyan) };
                if selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                lines.push(Line::from(Span::styled(format!(" {} {} ", if selected { "▶" } else { "·" }, it.label), style)));
            }
        }
        // Keep the selected action visible.
        let sel_row = item_rows.iter().find(|(i, _)| *i == self.backends_sel).map(|r| r.1).unwrap_or(0);
        let scroll = sel_row.saturating_sub(inner.height.saturating_sub(2));
        for (i, row) in &item_rows {
            if *row >= scroll && *row - scroll < inner.height {
                self.clicks.push((Rect::new(inner.x, inner.y + row - scroll, inner.width, 1), Click::Row(*i)));
            }
        }
        f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
        let (title, text, why) = match items.get(self.backends_sel) {
            Some(it) => (it.label.clone(), it.plan.clone(), it.why_not.clone()),
            None => ("Plan".into(), String::new(), None),
        };
        let mut l: Vec<Line> = vec![];
        if let Some(w) = why {
            l.push(Line::from(Span::styled(format!("Not available: {w}"), Style::new().fg(Color::Yellow))));
            l.push(Line::raw(""));
        }
        l.extend(text.lines().map(|t| Line::raw(t.to_string())));
        l.push(Line::raw(""));
        l.push(dim("Enter shows this plan again and asks before anything runs."));
        f.render_widget(Paragraph::new(l).wrap(Wrap { trim: false }).block(panel(&title)), plan);
    }

    fn backend_status(&self, section: &str) -> Vec<Line<'static>> {
        let v = &self.versions;
        match section {
            "MLX" => {
                let selected_version = match v.mlx_selected.as_str() {
                    "Custom Installer" => v.mlx_legacy.clone(),
                    "Omarchy package" => v.mlx_pkg.clone(),
                    _ => None,
                };
                let mut l = vec![
                    kv("Selected", &format!("{}{}", v.mlx_selected, selected_version.map(|s| format!(" {s}")).unwrap_or_default())),
                    kv("Package", v.mlx_pkg.as_deref().unwrap_or("not installed")),
                    kv("Custom", v.mlx_legacy.as_deref().unwrap_or("not installed")),
                ];
                if let Some(u) = &v.mlx_upstream {
                    l.push(kv("Latest", &format!("{u} (GitHub release)")));
                }
                l
            }
            "GGUF" => {
                let mut l = vec![kv("Binary", &match (&v.llama_path, &v.llama) {
                    (Some(p), Some(ver)) => format!("{} ({ver})", tilde(p)),
                    (Some(p), None) => tilde(p),
                    _ => "not built (optional: only GGUF models need it)".into(),
                })];
                if !self.cfg.llama.server_path.is_empty() {
                    l.push(dim("  from llama.server_path in Settings"));
                }
                l
            }
            _ => {
                let mut l = vec![kv("Checkout", v.strata_commit.as_deref().unwrap_or("not installed (optional: the 125B MoE)"))];
                if v.strata_commit.is_some() {
                    l.push(kv("Engine", if v.strata_engine { "built" } else { "not built" }));
                    l.push(kv("Driver", if v.strata_driver { "built (private Vulkan ICD)" } else { "not built" }));
                }
                if !v.strata_dirty.is_empty() {
                    l.push(Line::from(Span::styled(format!("  local changes: {}", v.strata_dirty.join(", ")), Style::new().fg(Color::Yellow))));
                }
                l
            }
        }
    }
}
