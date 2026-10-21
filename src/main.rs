mod anthropic;
mod backend;
mod client;
mod compat;
mod config;
mod grade;
mod hf;
mod launch;
mod models;
mod responses;
mod server;
mod sys;
mod sysprompt;
mod toolgroups;
mod tui;
mod usage;
mod vscode;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use config::Config;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::io::Write;

#[derive(Parser)]
#[command(name = "lmdeck", version, about = "Local models on Asahi Linux: MLX, GGUF and Strata on the Apple GPU; TUI + Ollama/OpenAI/Anthropic-compatible server")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the Ollama/OpenAI-compatible server (foreground)
    Serve,
    /// List local models
    #[command(alias = "list")]
    Ls,
    /// Show loaded models
    Ps,
    /// Download a model: org/repo (MLX) or org/repo:Q4_K_M (GGUF)
    Pull { model: String },
    /// Cancel a daemon download, keeping partial files for resume
    Cancel { model: String },
    /// Delete a local model
    Rm { model: String },
    /// Load a model and keep it resident
    Load { model: String },
    /// Unload a model (all if omitted)
    #[command(alias = "stop")]
    Unload { model: Option<String> },
    /// Chat with a model in the terminal (one-shot with a prompt, REPL without)
    Run { model: String, prompt: Option<String> },
    /// Search Hugging Face for MLX (default) or GGUF models
    Search {
        #[arg(default_value = "")]
        query: String,
        #[arg(long)]
        gguf: bool,
        #[arg(long, default_value = "relevance", value_parser = ["relevance", "downloads", "likes", "lastModified"])]
        sort: String,
    },
    /// Show runtime/daemon status and versions
    Status,
    /// Rank the local models for a goal (the TUI's `g`)
    Grade {
        #[arg(default_value = "balanced", value_parser = ["balanced", "smartest", "fastest", "efficient"])]
        goal: String,
    },
    /// Migrate the legacy $HOME mlx-omarchy install to the Omarchy packages
    Migrate {
        /// Also install the ANE driver package (omarchy-ane-dkms)
        #[arg(long)]
        ane: bool,
    },
    /// Update MLX: choose Omarchy packages or the official release installer
    Update,
    /// Select an installed MLX runtime; optionally remove the other runtime
    Runtime {
        #[arg(value_parser = ["omarchy", "custom"])]
        source: String,
        /// Keep only this runtime (models, chat data, and GPU driver are kept)
        #[arg(long)]
        only: bool,
    },
    /// llama.cpp management
    Llama {
        #[command(subcommand)]
        cmd: LlamaCmd,
    },
    /// Strata (Qwen3.8-Flash-Next) backend
    Strata {
        #[command(subcommand)]
        cmd: StrataCmd,
    },
    /// systemd user service
    Service {
        #[command(subcommand)]
        cmd: SvcCmd,
    },
    /// Install the launcher (.desktop) and Omarchy menu entries
    Desktop,
    /// VS Code / Copilot fallback status (the lmdeck VS Code extension applies it)
    Vscode,
    /// Run a coding agent on a local model (like `ollama launch`):
    /// claude | codex | opencode | t3 (registers a T3 Code provider, opens t1code)
    Launch {
        app: String,
        #[arg(long, short)]
        model: Option<String>,
        /// Extra arguments passed to the app
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum LlamaCmd {
    /// Build (or update) llama.cpp with Vulkan into ~/.local/share/lmdeck/llama.cpp
    Build,
}

#[derive(Subcommand)]
enum StrataCmd {
    /// Install or update Strata (checkout, server venv, Apple GPU Vulkan driver, engine) into strata.dir
    Install,
}

#[derive(Subcommand)]
enum SvcCmd {
    Install,
    Uninstall,
    Start,
    Stop,
    /// Start if stopped, stop if running (used by the Omarchy menu)
    Toggle,
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .thread_stack_size(512 * 1024)
        .enable_all()
        .build()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    let cfg = Config::load()?;
    let d = client::Daemon::new(cfg.base_url());
    match cli.cmd {
        None => tui::run(cfg).await,
        Some(Cmd::Serve) => server::run(cfg).await,
        Some(Cmd::Ls) => {
            let ms = models::scan(&cfg);
            println!("{:<58} {:<5} {:<11} {:<8} {:>9}", "NAME", "FMT", "QUANT", "PARAMS", "SIZE");
            for m in ms {
                println!("{:<58} {:<5} {:<11} {:<8} {:>9}", m.name, m.format.as_str(), m.quant, m.params, human(m.size));
            }
            Ok(())
        }
        Some(Cmd::Ps) => {
            let Some(s) = d.status().await else { bail!("daemon not running (lmdeck serve / lmdeck service start)") };
            println!("{:<58} {:<5} {:>9} {:>9} {:>8}", "NAME", "FMT", "SIZE", "RSS", "EXPIRES");
            for r in s["running"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{:<58} {:<5} {:>9} {:>9} {:>8}",
                    r["name"].as_str().unwrap_or(""),
                    r["format"].as_str().unwrap_or(""),
                    human(r["size"].as_u64().unwrap_or(0)),
                    human(r["rss"].as_u64().unwrap_or(0)),
                    r["expires_in_secs"].as_u64().map(|s| format!("{s}s")).unwrap_or("never".into())
                );
            }
            Ok(())
        }
        Some(Cmd::Pull { model }) => pull_cli(&cfg, &d, &model).await,
        Some(Cmd::Cancel { model }) => {
            let result = d.cancel(&model).await?;
            if result["ok"] != true { bail!("no active download for {model}"); }
            println!("Cancelled {model}; pull the same model again to resume.");
            Ok(())
        },
        Some(Cmd::Rm { model }) => {
            if d.up().await {
                d.delete(&model).await?;
            } else {
                let ms = models::scan(&cfg);
                let m = models::resolve(&model, &ms, &cfg).ok_or_else(|| anyhow::anyhow!("{model} not found"))?;
                models::delete(m, &cfg)?;
            }
            println!("deleted {model}");
            Ok(())
        }
        Some(Cmd::Load { model }) => {
            ensure_daemon(&d).await?;
            // Show progress while the (blocking) load request runs.
            let watcher = {
                let d = d.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                        if let Some(l) = d.status().await.and_then(|s| s.get("load").cloned()).filter(|l| !l.is_null()) {
                            eprint!("\r\x1b[2K{} [{:>3.0}%]", tui::load_label(&l), l["progress"].as_f64().unwrap_or(0.0) * 100.0);
                        }
                    }
                })
            };
            let v = d.load(&model).await;
            watcher.abort();
            eprint!("\r\x1b[2K");
            let v = v?;
            println!("{} loaded in {:.1}s", v["model"].as_str().unwrap_or(&model), v["load_secs"].as_f64().unwrap_or(0.0));
            Ok(())
        }
        Some(Cmd::Unload { model }) => {
            d.unload(model.as_deref()).await?;
            Ok(())
        }
        Some(Cmd::Run { model, prompt }) => {
            ensure_daemon(&d).await?;
            run_chat(&d, &model, prompt).await
        }
        Some(Cmd::Search { query, gguf, sort }) => {
            let c = hf::client();
            let kind = if gguf { "gguf" } else { "mlx" };
            let hits = if sort == "relevance" { hf::search(&c, &query, kind, 30).await? }
                else { hf::search_sorted(&c, &query, kind, 30, &sort).await? };
            for h in hits {
                println!("{:<64} ⬇{:>9} ♥{:>6}", h.id, h.downloads, h.likes);
            }
            Ok(())
        }
        Some(Cmd::Grade { goal }) => {
            let ms = models::scan(&cfg);
            let mut v = grade::build(&cfg, &ms);
            // The planner's decisions when the server runs (the same check a load does).
            if d.up().await {
                let mut plans = std::collections::HashMap::new();
                for m in &ms {
                    if let Ok(r) = d.http().get(format!("{}/lmdeck/plan?model={}", cfg.base_url(), m.name)).send().await {
                        if let Ok(p) = r.json::<serde_json::Value>().await {
                            plans.insert(m.name.clone(), p);
                        }
                    }
                }
                v.apply_plans(&plans);
            }
            v.set_goal(grade::GOALS.iter().position(|g| g.0.eq_ignore_ascii_case(&goal)).unwrap_or(0));
            println!("{:<3} {:<48} {:<7} {:>12} {:>5} {:>6} {:>7} {:>5} {:>6} {:<8} {:>5}", "#", "model", "fmt", "params", "bits", "intel", "tok/s", "W", "J/tok", "fit", "score");
            let mark = |s: grade::Src| match s { grade::Src::Measured => " ", grade::Src::User => "*", grade::Src::Estimate => "~" };
            let opt = |x: Option<(f64, grade::Src)>, p: usize| x.map(|(v, s)| format!("{v:.p$}{}", mark(s))).unwrap_or_else(|| "? ".into());
            for (i, r) in v.rows.iter().enumerate() {
                let params = match (r.total_b, r.active_b) {
                    (Some(t), Some(a)) if a < t * 0.95 => format!("{t:.0}B/{a:.1}B"),
                    (Some(t), _) => format!("{t:.1}B"),
                    _ => "?".into(),
                };
                println!("{:<3} {:<48} {:<7} {:>12} {:>5} {:>6} {:>7} {:>5} {:>6} {:<8} {:>5.0}", i + 1, r.name, r.format.as_str(), params,
                    r.bits.map(|b| format!("{b:.1}")).unwrap_or("?".into()), opt(r.intelligence, 0), opt(r.tps, 1), opt(r.watts, 0),
                    r.joules().map(|j| format!("{j:.1}")).unwrap_or("?".into()), r.fit, r.score);
            }
            println!("~ estimated  * from grades.toml  (blank) measured on this machine{}", if v.rows.iter().any(|r| !r.planned) { "; fit is an estimate (server not running)" } else { "" });
            Ok(())
        }
        Some(Cmd::Status) => {
            let mut v = sys::versions_local(&cfg);
            v.mlx_upstream = sys::upstream_release(&hf::client()).await;
            println!("{}", serde_json::to_string_pretty(&v)?);
            match d.status().await {
                Some(s) => println!("daemon: up at {} (pid {}, rss {})", cfg.base_url(), s["pid"], human(s["rss"].as_u64().unwrap_or(0))),
                None => println!("daemon: not running"),
            }
            Ok(())
        }
        Some(Cmd::Migrate { ane }) => run_script(&sys::script_migrate(ane)),
        Some(Cmd::Update) => run_script(&sys::script_update_menu(sys::versions_local(&cfg).mlx_selected == "not found")),
        Some(Cmd::Runtime { source, only }) => {
            let path = sys::mlx_runtime_path(&source);
            if !path.is_file() {
                bail!("{} runtime is not installed; run lmdeck update first", source);
            }
            let smoke = std::process::Command::new(&path)
                .args(["-c", "import mlx.core as mx; a=mx.ones((2,2)); mx.eval(a@a); print(mx.__version__, mx.default_device())"])
                .status()?;
            if !smoke.success() {
                bail!("selected runtime failed its GPU smoke test; settings unchanged");
            }
            if only {
                if let Some(s) = d.status().await {
                    if s["running"].as_array().is_some_and(|rs| rs.iter().any(|r| r["format"] == "mlx")) {
                        bail!("unload MLX models before removing a runtime (lmdeck unload)");
                    }
                }
            }
            crate::config::Config::edit(|c| { sys::set_mlx_executable(c, &path); Ok(()) }).map_err(anyhow::Error::msg)?;
            if d.up().await {
                d.reload().await?;
            }
            println!("Selected MLX runtime: {source} ({})", path.display());
            if only {
                run_script(&sys::script_remove_other_mlx(&source))?;
            }
            Ok(())
        }
        Some(Cmd::Llama { cmd: LlamaCmd::Build }) => run_script(&sys::script_build_llama(&cfg)),
        Some(Cmd::Strata { cmd: StrataCmd::Install }) => run_script(&sys::script_install_strata(&cfg)),
        Some(Cmd::Service { cmd }) => {
            match cmd {
                SvcCmd::Install => {
                    sys::install_service()?;
                    println!("installed and started {}", sys::unit_path().display());
                }
                SvcCmd::Uninstall => {
                    let _ = sys::systemctl(&["disable", "--now", "lmdeck"]);
                    let _ = std::fs::remove_file(sys::unit_path());
                    let _ = sys::systemctl(&["daemon-reload"]);
                }
                SvcCmd::Start => client::start_daemon()?,
                SvcCmd::Stop => client::stop_daemon(d.status().await.and_then(|s| s["pid"].as_u64()).map(|p| p as u32))?,
                SvcCmd::Toggle => match d.status().await {
                    Some(s) => {
                        client::stop_daemon(s["pid"].as_u64().map(|p| p as u32))?;
                        let _ = std::process::Command::new("notify-send").args(["lmdeck", "Local LLM server stopped"]).status();
                    }
                    None => {
                        if !sys::service_installed() {
                            sys::install_service()?;
                        } else {
                            client::start_daemon()?;
                        }
                        let _ = std::process::Command::new("notify-send").args(["lmdeck", &format!("Local LLM server on {}", cfg.base_url())]).status();
                    }
                },
                SvcCmd::Status => {
                    let o = sys::systemctl(&["status", "lmdeck", "--no-pager"])?;
                    std::io::stdout().write_all(&o.stdout)?;
                }
            }
            Ok(())
        }
        Some(Cmd::Launch { app, model, args }) => launch::run(&cfg, &d, &app, model, args).await,
        Some(Cmd::Vscode) => {
            if vscode::reset_settings_lines()? {
                println!("removed lmdeck lines from VS Code settings.json (the extension manages them now)");
            }
            match d.http().get(format!("{}/lmdeck/vscode", d.base)).send().await {
                Ok(r) => println!("{}", serde_json::to_string_pretty(&r.json::<Value>().await?)?),
                Err(_) => println!("daemon not running"),
            }
            Ok(())
        }
        Some(Cmd::Desktop) => {
            println!("wrote {}", sys::desktop_entry()?.display());
            println!("updated {}", sys::install_menu()?.display());
            Ok(())
        }
    }
}

pub fn run_script(script: &str) -> Result<()> {
    let st = std::process::Command::new("bash").arg("-c").arg(script).status()?;
    if !st.success() {
        bail!("script failed ({st})");
    }
    Ok(())
}

async fn ensure_daemon(d: &client::Daemon) -> Result<()> {
    if d.up().await {
        return Ok(());
    }
    eprintln!("starting lmdeck daemon…");
    client::start_daemon()?;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if d.up().await {
            return Ok(());
        }
    }
    bail!("daemon did not come up; see {}", config::log_dir().join("daemon.out").display())
}

async fn pull_cli(cfg: &Config, d: &client::Daemon, model: &str) -> Result<()> {
    let r = if d.up().await {
        d.http().post(format!("{}/api/pull", d.base)).json(&json!({"model": model})).send().await?
    } else {
        // No daemon: run the downloader in-process.
        let dl = hf::Downloads::default();
        let h = dl.start(hf::client(), cfg.clone(), model);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let v = h.view();
            progress_line(&v.name, v.completed, v.total, v.elapsed_secs);
            if h.finished() {
                eprintln!();
                return match v.status["state"].as_str() {
                    Some("done") => {
                        println!("success: {}", h.view().name);
                        Ok(())
                    }
                    _ => bail!("{}", v.status["detail"].as_str().unwrap_or("failed")),
                };
            }
        }
    };
    let t0 = std::time::Instant::now();
    let mut s = r.bytes_stream();
    let mut buf = Vec::new();
    while let Some(c) = s.next().await {
        buf.extend_from_slice(&c?);
        while let Some(p) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=p).collect();
            let v: Value = serde_json::from_slice(&line).unwrap_or_default();
            if let Some(e) = v.get("error") {
                eprintln!();
                bail!("{}", e.as_str().unwrap_or("pull failed"));
            }
            if let (Some(t), Some(c)) = (v["total"].as_u64(), v["completed"].as_u64()) {
                progress_line(v["status"].as_str().unwrap_or(""), c, t, t0.elapsed().as_secs_f64());
            } else if let Some(st) = v["status"].as_str() {
                eprint!("\r\x1b[2K{st}");
            }
        }
    }
    eprintln!();
    Ok(())
}

fn progress_line(label: &str, done: u64, total: u64, secs: f64) {
    let pct = if total > 0 { done as f64 * 100.0 / total as f64 } else { 0.0 };
    let rate = done as f64 / secs.max(0.1);
    eprint!("\r\x1b[2K{label}  {pct:5.1}%  {}/{}  {}/s", human(done), human(total), human(rate as u64));
}

async fn run_chat(d: &client::Daemon, model: &str, prompt: Option<String>) -> Result<()> {
    let mut history: Vec<Value> = Vec::new();
    let interactive = prompt.is_none();
    if interactive {
        eprintln!("loading {model}…");
        let v = d.load(model).await?;
        eprintln!("ready ({:.1}s). Ctrl-D to exit, /clear to reset.", v["load_secs"].as_f64().unwrap_or(0.0));
    }
    let mut next = prompt;
    loop {
        let p = match next.take() {
            Some(p) => p,
            None if interactive => {
                eprint!("\x1b[1m>>> \x1b[0m");
                std::io::stderr().flush()?;
                let mut s = String::new();
                if std::io::stdin().read_line(&mut s)? == 0 {
                    return Ok(());
                }
                s.trim_end().to_string()
            }
            None => return Ok(()),
        };
        if p == "/clear" {
            history.clear();
            continue;
        }
        if p.is_empty() {
            continue;
        }
        history.push(json!({"role": "user", "content": p}));
        let r = d.http().post(format!("{}/api/chat", d.base)).json(&json!({"model": model, "messages": history, "stream": true})).send().await?;
        if !r.status().is_success() {
            let v: Value = r.json().await.unwrap_or_default();
            bail!("{}", v["error"].as_str().unwrap_or("chat failed"));
        }
        let mut s = r.bytes_stream();
        let mut buf = Vec::new();
        let mut answer = String::new();
        let mut thinking = false;
        let mut out = std::io::stdout();
        while let Some(c) = s.next().await {
            buf.extend_from_slice(&c?);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let v: Value = serde_json::from_slice(&line).unwrap_or_default();
                if let Some(e) = v.get("error") {
                    bail!("{e}");
                }
                if let Some(t) = v.pointer("/message/thinking").and_then(|t| t.as_str()) {
                    if !thinking {
                        write!(out, "\x1b[2m")?;
                        thinking = true;
                    }
                    write!(out, "{t}")?;
                }
                if let Some(c) = v.pointer("/message/content").and_then(|t| t.as_str()) {
                    if thinking && !c.is_empty() {
                        write!(out, "\x1b[0m\n")?;
                        thinking = false;
                    }
                    write!(out, "{c}")?;
                    answer.push_str(c);
                }
                out.flush()?;
                if v["done"].as_bool() == Some(true) {
                    let pe = v["prompt_eval_count"].as_f64().unwrap_or(0.0);
                    let pd = v["prompt_eval_duration"].as_f64().unwrap_or(1.0) / 1e9;
                    let en = v["eval_count"].as_f64().unwrap_or(0.0);
                    let ed = v["eval_duration"].as_f64().unwrap_or(1.0) / 1e9;
                    eprintln!(
                        "\x1b[0m\n\x1b[2m[prompt {pe:.0} tok @ {:.1} tok/s · gen {en:.0} tok @ {:.1} tok/s · ttft {:.2}s]\x1b[0m",
                        pe / pd.max(1e-6),
                        en / ed.max(1e-6),
                        pd
                    );
                }
            }
        }
        history.push(json!({"role": "assistant", "content": answer}));
        if !interactive {
            return Ok(());
        }
    }
}

pub fn human(b: u64) -> String {
    let b = b as f64;
    if b >= (1u64 << 30) as f64 {
        format!("{:.1}G", b / (1u64 << 30) as f64)
    } else if b >= (1u64 << 20) as f64 {
        format!("{:.0}M", b / (1u64 << 20) as f64)
    } else if b >= 1024.0 {
        format!("{:.0}K", b / 1024.0)
    } else {
        format!("{b:.0}B")
    }
}
