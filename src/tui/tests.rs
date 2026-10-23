use super::popup::Popup;
use super::*;
use ratatui::backend::TestBackend;

fn ui() -> Ui {
    let (tx, _) = mpsc::unbounded_channel();
    let mut ui = Ui::new(Config::default(), tx);
    ui.versions.mlx_selected = "Custom Installer".into();
    ui.versions.mlx_legacy = Some("0.32.4.dev-test".into());
    ui.versions.mlx_pkg = Some("0.7.28-1".into());
    ui
}

/// Draw at `width` x `height`; every click region must be inside and none may overlap.
fn draw(ui: &mut Ui, width: u16, height: u16) -> String {
    let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| ui.draw(f)).unwrap();
    for (r, _) in &ui.clicks {
        assert!(r.right() <= width && r.bottom() <= height, "out-of-bounds click region: {r:?}");
    }
    for (i, (a, _)) in ui.clicks.iter().enumerate() {
        for (b, _) in &ui.clicks[i + 1..] {
            let o = a.intersection(*b);
            assert!(o.width == 0 || o.height == 0, "overlapping controls: {a:?} and {b:?}");
        }
    }
    terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
}

fn press(ui: &mut Ui, code: KeyCode) -> Option<Action> {
    ui.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn click_key(ui: &mut Ui, key: KeyCode) -> Option<Action> {
    let r = ui.clicks.iter().find_map(|(r, c)| match c {
        Click::Key(k) if *k == key => Some(*r),
        _ => None,
    }).expect("button must be visible and clickable");
    ui.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x + r.width / 2, row: r.y, modifiers: KeyModifiers::NONE })
}

#[test]
fn every_screen_draws_at_every_size_with_its_actions_visible() {
    for tab in TABS {
        for (w, h) in [(160, 48), (120, 36), (80, 24), (60, 20), (40, 12)] {
            let mut ui = ui();
            ui.tab = tab;
            let text = draw(&mut ui, w, h);
            // The first action of every screen is always on screen (the bar has a fixed place).
            let first = ui.actions().remove(0);
            if w >= 80 {
                assert!(text.contains(&first.label), "{tab:?} at {w}x{h} lost its action bar");
            }
            assert!(ui.clicks.iter().any(|(_, c)| matches!(c, Click::Key(k) if *k == first.keys[0])), "{tab:?} at {w}x{h}");
        }
    }
}

#[test]
fn unavailable_actions_explain_instead_of_running() {
    let mut ui = ui();
    ui.tab = Tab::Models;
    ui.models.clear();
    // No model selected: Delete is shown but only says why.
    assert!(press(&mut ui, KeyCode::Char('d')).is_none());
    assert_eq!(ui.toast.as_ref().map(|t| t.0.as_str()), Some("select a model first"));
    // Home only offers what applies: nothing is loaded, so there is no Unload all.
    ui.tab = Tab::Home;
    assert!(!ui.actions().iter().any(|a| a.label == "Unload all"));
}

#[test]
fn keys_popup_lists_global_keys_and_this_screens_actions() {
    for tab in TABS {
        let mut ui = ui();
        ui.tab = tab;
        press(&mut ui, KeyCode::Char('?'));
        assert!(matches!(ui.popup, Some(Popup::Keys(_))));
        let text = draw(&mut ui, 120, 40);
        assert!(text.contains("recommend a model"), "{tab:?}");
        let label = ui.actions().remove(0).label;
        assert!(text.contains(&label), "{tab:?}: {label}");
        press(&mut ui, KeyCode::Esc);
        assert!(ui.popup.is_none());
    }
}

#[test]
fn backends_actions_show_a_plan_and_ask_first() {
    let mut ui = ui();
    ui.tab = Tab::Backends;
    let text = draw(&mut ui, 140, 44);
    assert!(text.contains("Update the Omarchy packages") && text.contains("Keeps"));
    // Enter asks; nothing runs until yes.
    assert!(press(&mut ui, KeyCode::Enter).is_none());
    assert!(matches!(ui.popup, Some(Popup::Confirm(..))));
    draw(&mut ui, 140, 44);
    assert!(click_key(&mut ui, KeyCode::Char('n')).is_none());
    assert!(ui.popup.is_none());
    press(&mut ui, KeyCode::Enter);
    assert!(matches!(press(&mut ui, KeyCode::Char('y')), Some(Action::Script(..))));
}

#[test]
fn updating_mlx_keeps_the_selected_runtime() {
    let mut ui = ui();
    // The Custom release is selected: neither update script selects a runtime.
    for it in ui.backend_items().into_iter().take(2) {
        match it.run {
            Some(Action::Script(_, s)) => assert!(!s.contains("lmdeck runtime"), "{}", it.label),
            other => panic!("{other:?}"),
        }
    }
    // The selected runtime cannot be selected again; the other one can.
    ui.versions.mlx_selected = "Custom Installer".into();
    let items = ui.backend_items();
    let use_custom = items.iter().find(|i| i.label == "Use the Custom runtime").unwrap();
    assert!(use_custom.why_not.is_some());
    assert!(items.iter().find(|i| i.label == "Use the package runtime").unwrap().why_not.is_none());
}

#[test]
fn strata_update_refuses_over_local_changes() {
    let mut ui = ui();
    ui.versions.strata_commit = Some("abc 2026-10-21 x".into());
    ui.versions.strata_dirty = vec!["vulkan/src/mtl_rt_vk.cpp".into()];
    let it = ui.backend_items().into_iter().find(|i| i.label == "Update Strata").unwrap();
    assert!(it.why_not.unwrap().contains("local changes"));
}

#[tokio::test]
async fn discover_cycles_formats_and_applies_a_budget() {
    let mut ui = ui();
    ui.tab = Tab::Discover;
    for kind in ["gguf", "both", "mlx"] {
        press(&mut ui, KeyCode::Char('t'));
        assert_eq!(ui.search_kind(), kind);
        assert!(!ui.catalog_mode);
    }
    press(&mut ui, KeyCode::Char('m'));
    assert!(matches!(ui.popup, Some(Popup::Text { .. })));
    for c in "bad".chars() {
        press(&mut ui, KeyCode::Char(c));
    }
    press(&mut ui, KeyCode::Enter);
    assert!(matches!(ui.popup, Some(Popup::Text { .. })), "an invalid budget keeps the input open");
    ui.popup = Some(Popup::text("x", "8.5".into(), super::popup::Submit::Budget));
    press(&mut ui, KeyCode::Enter);
    assert_eq!(ui.search_budget, Some((8 << 30) + (1 << 29)));
    for (w, h) in [(60, 20), (80, 24), (120, 36)] {
        draw(&mut ui, w, h);
    }
    ui.search_task.take().unwrap().abort();
}

#[test]
fn unsupported_checkpoints_are_marked_in_search_and_detail() {
    let mut ui = ui();
    ui.tab = Tab::Discover;
    ui.catalog_mode = false;
    ui.hits = vec![RepoHit {
        id: "org/draft".into(),
        formats: vec!["mlx".into()],
        unsupported: Some("DFlash2DraftModel is a speculative/auxiliary checkpoint, not a standalone chat model".into()),
        ..Default::default()
    }];
    let text = draw(&mut ui, 160, 40);
    assert!(text.contains("unsupported"), "search row must say unsupported");
    let file = |path: &str, size| RepoFile { path: path.into(), size, sha256: None, blob: String::new() };
    ui.repo = Some(RepoView {
        repo: "org/draft".into(),
        files: Some(vec![file("config.json", 128), file("model.safetensors", 1 << 30)]),
        unsupported: Some("DFlash2DraftModel is a speculative/auxiliary checkpoint, not a standalone chat model".into()),
        err: None,
        sel: TableState::default().with_selected(Some(0)),
    });
    let text = draw(&mut ui, 160, 40);
    assert!(text.contains("not loadable by the MLX runtime"), "repo detail must say why");
    assert!(ui.repo_choices().iter().all(|(_, label, _)| !label.contains("recommended")), "must not recommend an unsupported MLX variant");
}

#[test]
fn opened_repo_obeys_format_and_budget_filters() {
    let mut ui = ui();
    ui.tab = Tab::Discover;
    ui.catalog_mode = false;
    let file = |path: &str, size| RepoFile { path: path.into(), size, sha256: None, blob: String::new() };
    ui.repo = Some(RepoView {
        repo: "org/mixed".into(),
        files: Some(vec![file("config.json", 128), file("model.safetensors", 4 << 30), file("model-Q4_K_M.gguf", 2 << 30), file("model-Q8_0.gguf", 6 << 30)]),
        unsupported: None,
        err: None,
        sel: TableState::default().with_selected(Some(0)),
    });
    assert_eq!(ui.repo_choices().len(), 1);
    ui.search_kind = 1;
    assert!(ui.repo_choices().iter().all(|v| v.0.contains(':')));
    ui.search_kind = 2;
    assert_eq!(ui.repo_choices().len(), 3);
    ui.search_budget = Some(crate::compat::estimated_memory(&ui.cfg, 2 << 30, "gguf"));
    let fits = ui.repo_choices();
    assert_eq!(fits.len(), 1);
    assert!(fits[0].0.ends_with(":Q4_K_M"));
    let text = draw(&mut ui, 120, 36);
    assert!(text.contains("Q4_K_M") && !text.contains("Q8_0"));
    // Esc goes back to the list.
    press(&mut ui, KeyCode::Esc);
    assert!(ui.repo.is_none());
}

#[test]
fn stale_search_cannot_replace_newer_results() {
    let mut ui = ui();
    ui.search_generation = 2;
    ui.searching = true;
    ui.handle(Msg::Search(1, Ok((vec![RepoHit { id: "org/old".into(), ..Default::default() }], 0))));
    assert!(ui.hits.is_empty() && ui.searching);
    ui.handle(Msg::Search(2, Ok((vec![RepoHit { id: "org/new".into(), ..Default::default() }], 0))));
    assert_eq!(ui.hits[0].id, "org/new");
    assert!(!ui.searching);
}

#[test]
fn g_opens_models_by_recommendation_and_cycles_goals() {
    let mut ui = ui();
    ui.models.clear();
    press(&mut ui, KeyCode::Char('g'));
    assert_eq!(ui.tab, Tab::Home, "no models: nothing to recommend");
    ui.grade = Some(crate::grade::build(&ui.cfg, &[]));
    ui.models = vec![];
    ui.tab = Tab::Models;
    ui.sort = super::models::Sort::Name;
    draw(&mut ui, 120, 36);
    press(&mut ui, KeyCode::Char('o'));
    assert_eq!(ui.sort, super::models::Sort::Size);
}

#[test]
fn settings_detail_pane_always_shows_the_selected_setting() {
    let mut ui = ui();
    ui.tab = Tab::Settings;
    for (w, h) in [(160, 48), (80, 24), (60, 20)] {
        for i in [0, crate::config::SETTINGS.len() / 2, crate::config::SETTINGS.len() - 1] {
            ui.settings_sel.select(Some(i));
            let text = draw(&mut ui, w, h);
            let key = crate::config::SETTINGS[i].key;
            assert!(text.contains(key), "{key} at {w}x{h}");
        }
    }
    // ←/→ only applies to on/off, choices and sliders.
    ui.settings_sel.select(Some(crate::config::SETTINGS.iter().position(|s| s.key == "server.port").unwrap()));
    assert!(ui.actions()[1].why_not.is_some());
    assert!(ui.actions()[1].button().contains("←/→"));
    ui.settings_sel.select(Some(crate::config::SETTINGS.iter().position(|s| s.key == "mlx.fast_path").unwrap()));
    assert!(ui.actions()[1].why_not.is_none());
}

#[test]
fn memory_popup_shows_the_sliders_and_what_is_missing() {
    let mut ui = ui();
    ui.popup = Some(Popup::free_memory(Some(("org/m".into(), "org/m needs ~18.0 GiB on the GPU, but only 16.5 GiB can be given to it right now".into()))));
    let text = draw(&mut ui, 120, 40);
    assert!(text.contains("Desktop reserve") && text.contains("GPU heap"), "{text}");
    assert!(text.contains("free about 1.5 GiB more"), "{text}");
}

#[test]
fn home_shows_reserves_heap_and_residency() {
    let mut ui = ui();
    let text = draw(&mut ui, 140, 44);
    for s in ["Kept free", "GPU heap", "For models", "Residency"] {
        assert!(text.contains(s), "{s}");
    }
}

#[test]
fn tabs_and_rows_support_the_mouse() {
    let mut ui = ui();
    draw(&mut ui, 120, 36);
    click_key(&mut ui, KeyCode::Char('5'));
    assert_eq!(ui.tab, Tab::Settings);
    draw(&mut ui, 120, 36);
    let (r, i) = ui.clicks.iter().find_map(|(r, c)| match c { Click::Row(i) if *i > 0 => Some((*r, *i)), _ => None }).unwrap();
    ui.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x, row: r.y, modifiers: KeyModifiers::NONE });
    assert_eq!(ui.settings_sel.selected(), Some(i));
}

/// `cargo test print_screens -- --ignored --nocapture`: every screen with this machine's models,
/// versions and server state, as text.
#[tokio::test]
#[ignore]
async fn print_screens() {
    let cfg = Config::load().unwrap();
    let (tx, _) = mpsc::unbounded_channel();
    let mut ui = Ui::new(cfg.clone(), tx);
    ui.versions = sys::versions_local(&cfg);
    ui.status = ui.d.status().await;
    let (w, h) = (std::env::var("W").ok().and_then(|v| v.parse().ok()).unwrap_or(120), std::env::var("H").ok().and_then(|v| v.parse().ok()).unwrap_or(36));
    for tab in TABS {
        ui.tab = tab;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| ui.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        println!("===== {tab:?}");
        for y in 0..h {
            let line: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{}", line.trim_end());
        }
    }
}

/// `cargo test screenshots -- --ignored`: the README's screenshots (docs/screenshots/*.svg),
/// drawn from this machine's models and server state with the Tokyo Night palette.
#[tokio::test]
#[ignore]
async fn screenshots() {
    use ratatui::style::{Color, Modifier};
    const PALETTE: [&str; 16] = [
        "#1a1b26", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#ad8ee6", "#449dab", "#a9b1d6",
        "#414868", "#ff7a93", "#b9f27c", "#ff9e64", "#7da6ff", "#bb9af7", "#0db9d7", "#c0caf5",
    ];
    let (bg0, fg0) = ("#1a1b26", "#a9b1d6");
    let hex = |c: Color, default: &str| -> String {
        match c {
            Color::Reset => default.to_string(),
            Color::Black => PALETTE[0].into(),
            Color::Red => PALETTE[1].into(),
            Color::Green => PALETTE[2].into(),
            Color::Yellow => PALETTE[3].into(),
            Color::Blue => PALETTE[4].into(),
            Color::Magenta => PALETTE[5].into(),
            Color::Cyan => PALETTE[6].into(),
            Color::Gray => PALETTE[7].into(),
            Color::DarkGray => PALETTE[8].into(),
            Color::LightRed => PALETTE[9].into(),
            Color::LightGreen => PALETTE[10].into(),
            Color::LightYellow => PALETTE[11].into(),
            Color::LightBlue => PALETTE[12].into(),
            Color::LightMagenta => PALETTE[13].into(),
            Color::LightCyan => PALETTE[14].into(),
            Color::White => PALETTE[15].into(),
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Indexed(i) => PALETTE.get(i as usize).map(|s| s.to_string()).unwrap_or_else(|| default.into()),
        }
    };
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let cfg = Config::load().unwrap();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    let (w, h) = (120u16, 34u16);
    let (cw, ch) = (8.4f64, 18.0f64);
    let shots: Vec<(&str, Tab, Option<fn(&mut Ui)>)> = vec![
        ("home", Tab::Home, None),
        ("models", Tab::Models, None),
        ("get-models", Tab::Discover, None),
        ("backends", Tab::Backends, None),
        ("settings", Tab::Settings, Some(|ui: &mut Ui| ui.settings_sel.select(Some(crate::config::SETTINGS.iter().position(|s| s.key == "server.gpu_heap").unwrap())))),
        // The process list trimmed to the desktop's own processes (the rest is what you run).
        ("memory", Tab::Home, Some(|ui: &mut Ui| {
            let mut p = Popup::free_memory(None);
            if let Popup::Free { groups, .. } = &mut p {
                groups.retain(|g| ["Hyprland", "quickshell", "ghostty", "bash", "xdg-desktop-por", "pipewire", "wireplumber", "mako", "swaybg", "walker"].contains(&g.name.as_str()));
            }
            ui.popup = Some(p);
        })),
    ];
    for (name, tab, setup) in shots {
        let (tx, _) = mpsc::unbounded_channel();
        let mut ui = Ui::new(cfg.clone(), tx);
        ui.versions = sys::versions_local(&cfg);
        ui.status = ui.d.status().await;
        // The planner's answers, as the TUI gets them.
        let mut plans = std::collections::HashMap::new();
        for m in &ui.models {
            if let Ok(r) = ui.d.http().get(format!("{}/lmdeck/plan?model={}", cfg.base_url(), m.name)).send().await {
                if let Ok(v) = r.json::<Value>().await {
                    plans.insert(m.name.clone(), v);
                }
            }
        }
        ui.plans = plans;
        ui.tab = tab;
        if let Some(f) = setup {
            f(&mut ui);
        }
        let mut terminal = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| ui.draw(f)).unwrap(); // first draw builds the grade
        terminal.draw(|f| ui.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.0}\" height=\"{:.0}\" viewBox=\"0 0 {:.0} {:.0}\" font-family=\"'JetBrains Mono','DejaVu Sans Mono',Menlo,monospace\" font-size=\"14\">\n<rect width=\"100%\" height=\"100%\" fill=\"{bg0}\" rx=\"8\"/>\n",
            w as f64 * cw + 24.0, h as f64 * ch + 24.0, w as f64 * cw + 24.0, h as f64 * ch + 24.0
        );
        for y in 0..h {
            let mut x = 0u16;
            while x < w {
                let c = &buf[(x, y)];
                let rev = c.modifier.contains(Modifier::REVERSED);
                let (mut fg, mut bg) = (hex(c.fg, fg0), hex(c.bg, bg0));
                if rev {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let bold = c.modifier.contains(Modifier::BOLD);
                // A run of cells with the same style.
                let mut text = String::new();
                let start = x;
                while x < w {
                    let d = &buf[(x, y)];
                    if d.fg != c.fg || d.bg != c.bg || d.modifier != c.modifier {
                        break;
                    }
                    text.push_str(d.symbol());
                    x += 1;
                }
                let (px, py) = (12.0 + start as f64 * cw, 12.0 + y as f64 * ch);
                let width = (x - start) as f64 * cw;
                if bg != bg0 {
                    svg.push_str(&format!("<rect x=\"{px:.1}\" y=\"{py:.1}\" width=\"{width:.1}\" height=\"{ch}\" fill=\"{bg}\"/>"));
                }
                if !text.trim().is_empty() {
                    svg.push_str(&format!(
                        "<text x=\"{px:.1}\" y=\"{:.1}\" fill=\"{fg}\"{} xml:space=\"preserve\" textLength=\"{width:.1}\" lengthAdjust=\"spacingAndGlyphs\">{}</text>",
                        py + 13.5,
                        if bold { " font-weight=\"bold\"" } else { "" },
                        esc(&text)
                    ));
                }
            }
            svg.push('\n');
        }
        svg.push_str("</svg>\n");
        std::fs::write(dir.join(format!("{name}.svg")), svg).unwrap();
    }
}
