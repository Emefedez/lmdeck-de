//! System helpers: memory, process RSS, logging, time formatting, versions,
//! and the shell scripts behind migrate/update/build actions.

use crate::config::{home, llama_dir, log_dir, which, Config};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shmem from /proc/meminfo in bytes. On Asahi, GPU buffers (GEM shmem)
/// show up here, which makes it a usable "weights uploaded" gauge.
pub fn shmem() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Shmem:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()))
        .unwrap_or(0)
        * 1024
}

/// (MemTotal, MemAvailable) in bytes.
pub fn mem() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get = |k: &str| {
        s.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (get("MemTotal:"), get("MemAvailable:"))
}

fn rss(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .unwrap_or(0)
        * 1024
}

/// RSS of a process and its direct children (uclampset execs, so usually just one).
pub fn tree_rss(pid: u32) -> u64 {
    let mut total = rss(pid);
    if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
        for c in s.split_whitespace().filter_map(|c| c.parse::<u32>().ok()) {
            total += rss(c);
        }
    }
    total
}

/// Processes of the current user that share a command name, biggest first.
#[derive(Clone, Debug)]
pub struct ProcGroup {
    pub name: String,
    pub pids: Vec<u32>,
    pub rss: u64,
    /// Why killing this would be a bad idea (lmdeck itself, the desktop, ...).
    pub protected: Option<&'static str>,
    /// Opt-in action instead of a plain kill: (id, what it does).
    pub tweak: Option<(&'static str, &'static str)>,
}

/// Name shown for a process: VS Code extensions and node/bun scripts get their own
/// lines so the hungry one is visible instead of one big "code" row.
fn display_name(name: &str, pid: u32) -> String {
    let Ok(cmd) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else { return name.to_string() };
    let args: Vec<&str> = cmd.split('\0').filter(|a| !a.is_empty()).collect();
    if name == "libkrun VM" || args.first().is_some_and(|a| a.ends_with("/muvm")) {
        return "muvm VM".to_string();
    }
    if let Some(ext) = args.iter().find_map(|a| a.split("/extensions/").nth(1)).and_then(|r| r.split('/').next()) {
        let base = ext.rsplit_once('-').map(|(n, _)| n).unwrap_or(ext);
        return format!("{name}: ext {base}");
    }
    if matches!(name, "node" | "bun" | "node-MainThread") {
        let script = args.iter().skip(1).filter(|a| !a.starts_with('-') && **a != "run").take(2)
            .map(|a| a.rsplit('/').next().unwrap_or(a)).collect::<Vec<_>>().join(" ");
        if !script.is_empty() {
            return format!("{name}: {script}");
        }
    }
    name.to_string()
}

/// Opt-in actions shown above the process list. Nothing here runs by itself.
pub fn tweaks(groups: &[ProcGroup]) -> Vec<ProcGroup> {
    let mut out = vec![ProcGroup {
        name: "[PRESET] Make room for this model automatically".into(),
        pids: vec![], rss: 0, protected: None,
        tweak: Some(("preset", "Least disruptive first, re-checking after each step and stopping once it fits: lower the reserves, drop Hyprland effects, disable optional shell plugins, then close the muvm VM. Then loads the model. It never kills your other apps.")),
    }];
    let vm: Vec<&ProcGroup> = groups.iter().filter(|g| g.name == "muvm VM" || g.name == "muvm" || g.name == "muvm-run").collect();
    if !vm.is_empty() {
        out.push(ProcGroup {
            name: "[action] Close the muvm VM".into(),
            pids: vm.iter().flat_map(|g| g.pids.clone()).collect(),
            rss: vm.iter().map(|g| g.rss).sum(),
            protected: None,
            tweak: Some(("kill", "Stops muvm and everything running inside it (Windows/x86 apps). The VM can hold up to its --mem cap, not just its RSS.")),
        });
    }
    out.push(ProcGroup {
        name: "[action] Hyprland: no blur/shadows/animations/transparency".into(),
        pids: vec![], rss: 0, protected: None,
        tweak: Some(("hypr-lite", "Runtime only via hyprctl keyword (undo: hyprctl reload). Frees GPU/shmem buffers, not much RAM.")),
    });
    let on = optional_plugins_enabled();
    if !on.is_empty() {
        out.push(ProcGroup {
            name: format!("[action] Disable {} optional Omarchy shell plugins", on.len()),
            pids: vec![], rss: 0, protected: None,
            tweak: Some(("plugins-lite", "Turns off: expose.switch, macnook, vantage, hw-monitor, dev gallery, disk speed test (whichever are on). Small RAM win (Quickshell stays); restorable from this list.")),
        });
    }
    if !plugins_disabled_by_us().is_empty() {
        out.push(ProcGroup {
            name: "[action] Restore the shell plugins lmdeck disabled".into(),
            pids: vec![], rss: 0, protected: None,
            tweak: Some(("plugins-restore", "Re-enables exactly the plugins this tool turned off earlier.")),
        });
    }
    out.push(ProcGroup {
        name: "[action] Stop the tmux server".into(),
        pids: vec![], rss: groups.iter().filter(|g| g.name.starts_with("tmux")).map(|g| g.rss).sum(), protected: None,
        tweak: Some(("tmux", "Ends every tmux session and what runs in them. Small RAM win; shells inside are lost.")),
    });
    out
}

/// Non-essential Omarchy shell plugins that are safe to switch off under memory pressure.
const OPTIONAL_PLUGINS: &[&str] = &[
    "expose.switch", "david.macnook", "jmthomas00.vantage", "meviusisback.hw-monitor", "omarchy.dev-gallery", "omarchy.disk-speedtest",
];

fn optional_plugins_enabled() -> Vec<String> {
    let Ok(o) = std::process::Command::new("omarchy-plugin-list").output() else { return Vec::new() };
    String::from_utf8_lossy(&o.stdout).lines().filter_map(|l| {
        let mut it = l.split_whitespace();
        let (id, state) = (it.next()?, it.next()?);
        (state == "enabled" && OPTIONAL_PLUGINS.contains(&id)).then(|| id.to_string())
    }).collect()
}

fn plugins_state_path() -> PathBuf {
    crate::config::log_dir().parent().map(|p| p.join("disabled-plugins.json")).unwrap_or_else(|| PathBuf::from("/tmp/lmdeck-disabled-plugins.json"))
}

fn plugins_disabled_by_us() -> Vec<String> {
    std::fs::read_to_string(plugins_state_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Run a tweak by id; returns a message for the toast.
pub fn run_tweak(id: &str) -> String {
    match id {
        "hypr-lite" => {
            let mut failed = 0;
            for kv in ["decoration:blur:enabled false", "decoration:shadow:enabled false", "animations:enabled false", "decoration:active_opacity 1.0", "decoration:inactive_opacity 1.0"] {
                let ok = std::process::Command::new("hyprctl").args(["keyword"]).args(kv.split(' ')).output().map(|o| o.status.success()).unwrap_or(false);
                failed += (!ok) as u32;
            }
            if failed == 0 { "Hyprland effects off (hyprctl reload to restore)".into() } else { format!("{failed} hyprctl settings failed") }
        }
        "plugins-lite" => {
            let on = optional_plugins_enabled();
            let mut done = plugins_disabled_by_us();
            for id in &on {
                if std::process::Command::new("omarchy-plugin-disable").arg(id).output().map(|o| o.status.success()).unwrap_or(false) && !done.contains(id) {
                    done.push(id.clone());
                }
            }
            let _ = std::fs::write(plugins_state_path(), serde_json::to_string(&done).unwrap_or_default());
            format!("disabled {} shell plugins (restore from this list)", on.len())
        }
        "plugins-restore" => {
            let ids = plugins_disabled_by_us();
            for id in &ids {
                let _ = std::process::Command::new("omarchy-plugin-enable").arg(id).output();
            }
            let _ = std::fs::remove_file(plugins_state_path());
            format!("re-enabled {} shell plugins", ids.len())
        }
        "tmux" => match std::process::Command::new("tmux").arg("kill-server").output() {
            Ok(o) if o.status.success() => "tmux server stopped".into(),
            _ => "tmux was not running".into(),
        },
        _ => String::new(),
    }
}

fn protected_reason(name: &str, pid: u32) -> Option<&'static str> {
    let me = std::process::id();
    if pid == me || name == "lmdeck" {
        return Some("lmdeck");
    }
    if std::fs::read_to_string(format!("/proc/{pid}/cmdline")).map(|c| c.contains("mlx_fast_server") || c.contains("llama-server")).unwrap_or(false) {
        return Some("model backend: unload from Models");
    }
    match name {
        "systemd" | "(sd-pam)" | "dbus-daemon" | "dbus-broker" | "Hyprland" | "hyprland" | "start-hyprland" | "uwsm" | "pipewire"
        | "wireplumber" | "login" | "sshd" | "bash" | "fish" | "zsh" => Some("session / desktop"),
        _ => None,
    }
}

/// Group `uid`'s processes by name for the memory freer.
pub fn proc_groups(uid: u32, with_tweaks: bool) -> Vec<ProcGroup> {
    let mut map: std::collections::HashMap<String, ProcGroup> = std::collections::HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(st) = std::fs::read_to_string(format!("/proc/{pid}/status")) else { continue };
        let field = |k: &str| st.lines().find_map(|l| l.strip_prefix(k)).map(str::trim);
        let comm = field("Name:").unwrap_or("").to_string();
        let name = display_name(&comm, pid);
        let owner = field("Uid:").and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u32>().ok());
        let rss_kb = field("VmRSS:").and_then(|v| v.split_whitespace().next()).and_then(|v| v.parse::<u64>().ok());
        let (Some(owner), Some(rss_kb)) = (owner, rss_kb) else { continue }; // kernel threads have no VmRSS
        if owner != uid || name.is_empty() {
            continue;
        }
        let g = map.entry(name.clone()).or_insert_with(|| ProcGroup { name: name.clone(), pids: vec![], rss: 0, protected: None, tweak: None });
        g.pids.push(pid);
        g.rss += rss_kb * 1024;
        if g.protected.is_none() {
            g.protected = protected_reason(&comm, pid);
        }
    }
    let all: Vec<ProcGroup> = map.into_values().collect();
    let mut v: Vec<ProcGroup> = all.iter().filter(|g| g.rss >= 80 << 20).cloned().collect();
    v.sort_by(|a, b| b.rss.cmp(&a.rss));
    let mut out = if with_tweaks { tweaks(&all) } else { Vec::new() };
    out.extend(v);
    out
}

pub fn self_rss() -> u64 {
    rss(std::process::id())
}

pub fn log(msg: &str) {
    let line = format!("{} {msg}", now_rfc3339());
    eprintln!("{line}");
    let _ = std::fs::create_dir_all(log_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_dir().join("lmdeck.log")) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

pub fn tail(p: &Path, n: usize) -> String {
    let s = read_tail(p, 64 * 1024);
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The most useful line of a crashed backend's log: the last Python exception
/// or llama.cpp error, else the last non-empty line.
pub fn error_summary(p: &Path) -> String {
    let text = read_tail(p, 64 * 1024);
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("-----")).collect();
    let is_err = |l: &&str| {
        let head = l.split(':').next().unwrap_or("");
        (head.ends_with("Error") || head.ends_with("Exception")) && !head.contains(' ')
            || l.starts_with("error:")
            || l.contains("failed to load model")
            || l.contains("error loading model")
    };
    lines.iter().rev().find(|l| is_err(l)).or(lines.last()).map(|l| l.chars().take(400).collect()).unwrap_or_default()
}

/// Text appended to `p` after byte `offset` (at most the last `max` bytes).
pub fn read_since(p: &Path, offset: u64, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(p) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= offset {
        return String::new();
    }
    let _ = f.seek(SeekFrom::Start(offset.max(len.saturating_sub(max))));
    let mut b = Vec::new();
    let _ = f.read_to_end(&mut b);
    String::from_utf8_lossy(&b).into_owned()
}

pub fn read_tail(p: &Path, max: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(p) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(max)));
    let mut b = Vec::new();
    let _ = f.read_to_end(&mut b);
    String::from_utf8_lossy(&b).into_owned()
}

pub async fn run_hook(cmd: &str) {
    log(&format!("hook: {cmd}"));
    let _ = tokio::process::Command::new("sh").arg("-c").arg(cmd).status().await;
}

pub fn now_rfc3339() -> String {
    rfc3339(SystemTime::now())
}

pub fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let dd = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{:09}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        d.subsec_nanos()
    )
}

// ---------- versions ----------

pub const MLX_REPO: &str = "joshuaswarren/omarchy-mlx";
pub const LEGACY_PREFIX: &str = ".local/share/mlx-omarchy";
pub const PKGS: &[&str] = &["omarchy-mlx", "omarchy-mlx-vulkan"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Versions {
    /// "package" | "legacy" | "both" | "none"
    pub mlx_install: String,
    pub mlx_pkg: Option<String>,
    pub mlx_pkg_repo: Option<String>,
    pub mlx_legacy: Option<String>,
    pub mlx_upstream: Option<String>,
    pub mlx_bin: Option<String>,
    #[serde(default)]
    pub mlx_selected: String,
    pub llama: Option<String>,
    pub llama_path: Option<String>,
    pub ane: bool,
    /// The Strata checkout: "<sha> <date> <subject>", or None when not installed.
    #[serde(default)]
    pub strata_commit: Option<String>,
    /// Files changed in the Strata checkout (an update refuses to run over them).
    #[serde(default)]
    pub strata_dirty: Vec<String>,
    #[serde(default)]
    pub strata_engine: bool,
    #[serde(default)]
    pub strata_driver: bool,
}

fn cmd_out(prog: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(prog).args(args).output().ok()?;
    if !o.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub fn legacy_prefix() -> PathBuf {
    home().join(LEGACY_PREFIX)
}

pub fn legacy_version() -> Option<String> {
    let sp = legacy_prefix().join("venv/lib");
    for py in std::fs::read_dir(&sp).ok()?.flatten() {
        for e in std::fs::read_dir(py.path().join("site-packages")).ok()?.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(v) = n.strip_prefix("mlx_omarchy-").and_then(|v| v.strip_suffix(".dist-info")) {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub fn mlx_runtime_path(source: &str) -> PathBuf {
    match source {
        "custom" => home().join(".local/bin/mlx-omarchy"),
        _ => PathBuf::from("/usr/bin/mlx-omarchy"),
    }
}

pub fn set_mlx_executable(cfg: &mut Config, path: &Path) {
    if cfg.mlx.command.is_empty() {
        cfg.mlx.command = vec![path.display().to_string(), "-m".into(), "mlx_lm.server".into()];
    } else {
        cfg.mlx.command[0] = path.display().to_string();
    }
}

/// Local-only probe (fast, no network).
pub fn versions_local(cfg: &Config) -> Versions {
    let pkg = cmd_out("pacman", &["-Q", "omarchy-mlx"]).and_then(|s| s.split_whitespace().nth(1).map(str::to_string));
    let repo = cmd_out("pacman", &["-Si", "omarchy-mlx"]).and_then(|s| {
        s.lines().find(|l| l.starts_with("Version")).and_then(|l| l.split(':').nth(1)).map(|v| v.trim().to_string())
    });
    let legacy = legacy_version();
    let install = match (&pkg, &legacy) {
        (Some(_), Some(_)) => "both",
        (Some(_), None) => "package",
        (None, Some(_)) => "legacy",
        _ => "none",
    };
    let llama_path = cfg.llama_server();
    let llama = llama_path.as_ref().and_then(|p| {
        let o = std::process::Command::new(p).arg("--version").output().ok()?;
        let s = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        s.lines().find(|l| l.starts_with("version:")).map(|l| l.trim_start_matches("version:").trim().to_string())
    });
    let mlx_bin = which(cfg.mlx.command.first().map(String::as_str).unwrap_or("mlx-omarchy"))
        .and_then(|p| p.canonicalize().ok());
    let mlx_selected = match &mlx_bin {
        Some(bin) if mlx_runtime_path("custom").canonicalize().ok().as_ref() == Some(bin) => "Custom Installer",
        Some(bin) if mlx_runtime_path("omarchy").canonicalize().ok().as_ref() == Some(bin) => "Omarchy package",
        Some(_) => "Other executable",
        None => "not found",
    };
    Versions {
        mlx_install: install.into(),
        mlx_pkg: pkg,
        mlx_pkg_repo: repo,
        mlx_legacy: legacy,
        mlx_upstream: None,
        mlx_bin: mlx_bin.map(|p| p.display().to_string()),
        mlx_selected: mlx_selected.into(),
        llama,
        llama_path: llama_path.map(|p| p.display().to_string()),
        ane: Path::new("/dev/accel/accel0").exists(),
        strata_commit: cfg.strata.dir().join(".git").exists().then(|| {
            cmd_out("git", &["-C", &cfg.strata.dir().display().to_string(), "log", "-1", "--format=%h %cs %s"]).unwrap_or_default()
        }),
        strata_dirty: cmd_out("git", &["-C", &cfg.strata.dir().display().to_string(), "status", "--porcelain", "--untracked-files=no"])
            .map(|s| s.lines().map(|l| l.get(3..).unwrap_or(l).to_string()).collect())
            .unwrap_or_default(),
        strata_engine: cfg.strata.dir().join("build-vulkan/strata").exists(),
        strata_driver: cfg.strata.vk_icd().exists(),
    }
}

/// Latest GitHub release tag (cached for 6h in the state dir).
pub async fn upstream_release(c: &reqwest::Client) -> Option<String> {
    let cache = crate::config::state_dir().join("upstream-release");
    if let Ok(m) = std::fs::metadata(&cache) {
        if m.modified().ok()?.elapsed().ok()?.as_secs() < 6 * 3600 {
            return std::fs::read_to_string(&cache).ok().map(|s| s.trim().to_string());
        }
    }
    let v: serde_json::Value = c
        .get(format!("https://api.github.com/repos/{MLX_REPO}/releases/latest"))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let _ = std::fs::create_dir_all(cache.parent()?);
    let _ = std::fs::write(&cache, &tag);
    Some(tag)
}

// ---------- action scripts (run in the foreground terminal) ----------

const HEADER: &str = r#"set -uo pipefail
say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mnote:\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Omarchy for Mac lists the upstream [omarchy] repo as `Usage = Sync`: pacman
# can see omarchy-mlx there but `pacman -S`/omarchy-pkg-add refuse to install
# from it. Fetch the same signed packages from that repo and install them with
# `pacman -U` (signatures are still verified against the pacman keyring).
omarchy_repo_urls() {
  local server db f p
  server="$(pacman-conf --repo=omarchy Server 2>/dev/null | head -n1)"
  db=/var/lib/pacman/sync/omarchy.db
  [[ -n $server && -r $db ]] || return 1
  for p in "$@"; do
    f="$(tar -xOf "$db" --wildcards "$p-[0-9]*/desc" 2>/dev/null | awk '/^%FILENAME%/{getline; print; exit}')"
    [[ -n $f ]] || { echo "not in [omarchy] repo db: $p" >&2; return 1; }
    printf '%s/%s\n' "$server" "$f"
  done
}
omarchy_repo_version() {
  tar -xOf /var/lib/pacman/sync/omarchy.db --wildcards "$1-[0-9]*/desc" 2>/dev/null | awk '/^%VERSION%/{getline; print; exit}'
}
install_mlx_pkgs() {
  if command -v omarchy-pkg-add >/dev/null && omarchy-pkg-add "$@" 2>/dev/null; then
    return 0
  fi
  warn "omarchy-pkg-add can't install from [omarchy] here (repo is sync-only on Omarchy for Mac); installing the signed packages directly"
  mapfile -t urls < <(omarchy_repo_urls "$@") || return 1
  ((${#urls[@]})) || return 1
  printf '   %s\n' "${urls[@]}"
  sudo pacman -U --needed "${urls[@]}"
}
"#;

/// Move a legacy `install.sh` ($HOME venv) install to the Omarchy packages.
pub fn script_migrate(with_ane: bool) -> String {
    let pkgs = if with_ane { "omarchy-mlx omarchy-mlx-vulkan omarchy-ane-dkms" } else { "omarchy-mlx omarchy-mlx-vulkan" };
    format!(
        r#"{HEADER}
say "Migrating mlx-omarchy: \$HOME script install -> Omarchy packages ({pkgs})"
if [[ -d "$HOME/{LEGACY_PREFIX}/venv" ]]; then
  say "Legacy install found: $HOME/{LEGACY_PREFIX}"
else
  warn "No legacy venv under $HOME/{LEGACY_PREFIX}; installing packages only."
fi

say "1/4 Installing packages ({pkgs})"
install_mlx_pkgs {pkgs} || die "could not install {pkgs} (try 'omarchy-update' to refresh package databases, then run this again)"

say "2/4 Verifying the packaged runtime on the Apple GPU"
/usr/bin/mlx-omarchy -c 'import mlx.core as mx; a=mx.random.normal((256,256)); mx.eval((a@a).sum()); print("  ", mx.__version__, mx.default_device())' \
  || die "packaged runtime failed its smoke test; legacy install left untouched"
lmdeck runtime omarchy || die "could not select the Omarchy runtime"

say "3/4 Retiring the legacy install (venv, launchers, desktop entry, shims; models and chat data are kept)"
if command -v mlx-omarchy-retire-legacy >/dev/null; then
  mlx-omarchy-retire-legacy
  read -rp "Apply the removals above? [Y/n] " ok
  if [[ "${{ok:-Y}}" =~ ^[Yy]$ ]]; then
    mlx-omarchy-retire-legacy --yes
  else
    warn "Skipped. Old launchers in ~/.local/bin shadow /usr/bin until removed."
  fi
else
  warn "mlx-omarchy-retire-legacy not found in the package; remove $HOME/{LEGACY_PREFIX}/venv manually."
fi
hash -r

say "4/4 Refreshing launcher menu"
omarchy-menu refresh >/dev/null 2>&1 || true
say "Now using: $(command -v mlx-omarchy)"
mlx-omarchy-info 2>/dev/null | head -20 || true
say "Done."
"#
    )
}

/// Update the packaged runtime. `select`: also make it lmdeck's MLX runtime (only when no
/// working runtime is selected; otherwise the choice stays yours).
pub fn script_update_mlx(select: bool) -> String {
    format!(
        r#"{HEADER}
say "Updating omarchy-mlx packages"
say "Full system upgrade through Omarchy first (Arch does not support partial upgrades)"
omarchy update || die "omarchy update failed"
have="$(pacman -Q omarchy-mlx 2>/dev/null | awk '{{print $2}}')" || have=""
want="$(omarchy_repo_version omarchy-mlx)"
if [[ -n $have && -n $want && $(vercmp "$want" "$have") -le 0 ]]; then
  say "omarchy-mlx $have is current"
else
  say "omarchy-mlx $have -> $want"
  # [omarchy] is sync-only here, so the system upgrade skips these; install directly.
  install_mlx_pkgs {pkgs} || die "update failed"
fi
{select}
say "Done. Reload MLX models in lmdeck to use the new version."
"#,
        pkgs = PKGS.join(" "),
        select = if select { r#"lmdeck runtime omarchy || die "could not select the Omarchy runtime""# } else { r#"say "lmdeck's selected MLX runtime is unchanged""# },
    )
}

/// Official release installer: a user runtime alongside the Omarchy packages. `select` as above.
pub fn script_update_mlx_custom(select: bool) -> String {
    format!(
        r#"{HEADER}
say "Resolving the latest omarchy-mlx GitHub release"
release="$(curl -fsSL https://api.github.com/repos/{MLX_REPO}/releases/latest | python3 -c 'import json, re, sys; tag=json.load(sys.stdin)["tag_name"]; assert re.fullmatch(r"v[0-9]+(?:\.[0-9]+)+(?:[-+][A-Za-z0-9.-]+)?", tag), "invalid release tag"; print(tag)')" || die "could not resolve the latest release"
say "Installing $release with the official Custom Installer"
warn "Installs into ~/.local/share/mlx-omarchy; ~/.local/bin/mlx-omarchy takes precedence over the packaged runtime."
installer="$(mktemp)" || die "could not create installer file"
trap 'rm -f "$installer"' EXIT
curl -fsSL "https://raw.githubusercontent.com/{MLX_REPO}/$release/install.sh" -o "$installer" || die "could not download the release installer"
# The official installer recreates this venv with --clear and deletes its
# temporary wheel on exit. Disable pip caching too, so updates don't retain
# downloaded copies of old dependencies outside that replaced runtime.
PIP_NO_CACHE_DIR=1 MLX_OMARCHY_VERSION="$release" MLX_OMARCHY_HOME="$HOME/{LEGACY_PREFIX}" bash "$installer" || die "Custom Installer failed"
"$HOME/.local/bin/mlx-omarchy" -c 'import mlx.core as mx; print("  ", mx.__version__, mx.default_device())' || die "installed runtime failed its smoke test"
{select}
say "Done. Reload MLX models in lmdeck to use the new version."
"#,
        select = if select { r#"lmdeck runtime custom || die "could not select the custom runtime""# } else { r#"say "lmdeck's selected MLX runtime is unchanged""# },
    )
}

/// Remove only the unselected runtime, keeping model data and the GPU driver.
pub fn script_remove_other_mlx(source: &str) -> String {
    if source == "custom" {
        format!(r#"{HEADER}
if pacman -Q omarchy-mlx >/dev/null 2>&1; then
  say "Removing the packaged MLX runtime; keeping the Vulkan driver"
  sudo pacman -D --asexplicit omarchy-mlx-vulkan || die "could not preserve the Vulkan driver"
  sudo pacman -R omarchy-mlx || die "package removal failed"
fi
"#)
    } else {
        format!(r#"{HEADER}
if [[ -d "$HOME/{LEGACY_PREFIX}/venv" || -e "$HOME/.local/bin/mlx-omarchy" ]]; then
  say "Removing the custom runtime; keeping models and chat data"
  /usr/bin/mlx-omarchy-retire-legacy --yes || die "custom runtime removal failed"
fi
"#)
    }
}

pub fn script_update_menu(select: bool) -> String {
    format!(
        r#"printf 'Update MLX\n\n1. Omarchy\n2. Custom Installer\nq. Cancel\n\n'
while read -rp 'Choose [1/2/q]: ' choice; do
  case "$choice" in
    1)
{omarchy}
      break ;;
    2)
{custom}
      break ;;
    q|Q) exit 0 ;;
    *) printf 'Choose 1, 2, or q.\n' ;;
  esac
done
"#,
        omarchy = script_update_mlx(select),
        custom = script_update_mlx_custom(select),
    )
}

/// Install or update Strata: the checkout (cfg.strata.repo / git_ref), the server's venv, the
/// Apple GPU Vulkan driver it runs on (honeykrisp-strata's private ICD; the desktop's Mesa is
/// not touched) and, once the branch has it, the Vulkan engine.
pub fn script_install_strata(cfg: &Config) -> String {
    let dir = cfg.strata.dir();
    let icd = cfg.strata.vk_icd();
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    format!(
        r#"{HEADER}
DIR="{dir}"
REPO="{repo}"
REF="{gitref}"
ICD="{icd}"
say "Strata ($REF) into $DIR"
need=()
for p in git python cmake ninja; do pacman -Q "$p" >/dev/null 2>&1 || need+=("$p"); done
if ((${{#need[@]}})); then
  say "Installing: ${{need[*]}}"
  if command -v omarchy-pkg-add >/dev/null; then omarchy-pkg-add "${{need[@]}}"; else sudo pacman -S --needed "${{need[@]}}"; fi || die "deps"
fi
if [[ -d "$DIR/.git" ]]; then
  # Never discard work in the checkout: stop on local changes, and only fast-forward.
  [[ -z "$(git -C "$DIR" status --porcelain --untracked-files=no)" ]] || die "$DIR has local changes; commit or stash them, then run this again"
  git -C "$DIR" fetch origin "$REF" || die "git fetch"
  if [[ "$(git -C "$DIR" rev-parse --abbrev-ref HEAD)" != "$REF" ]]; then
    git -C "$DIR" checkout -q "$REF" 2>/dev/null || git -C "$DIR" checkout -q -b "$REF" FETCH_HEAD || die "could not switch to $REF"
  fi
  git -C "$DIR" merge -q --ff-only FETCH_HEAD || die "$DIR has commits that are not on origin/$REF; update it by hand"
else
  git clone --branch "$REF" "$REPO" "$DIR" || die "git clone"
fi
say "Commit: $(git -C "$DIR" log -1 --format='%h %cs %s' | cut -c1-120)"
say "Server packages (.venv)"
[[ -x "$DIR/.venv/bin/python" ]] || python -m venv "$DIR/.venv" || die "venv"
"$DIR/.venv/bin/pip" install -q --disable-pip-version-check -r "$DIR/requirements.txt" || die "pip"
if [[ -f "$ICD" ]]; then
  say "Vulkan driver: $ICD ($(cat "$(dirname "$ICD")/../../../mesa-git-sha" 2>/dev/null || echo built))"
else
  say "Building the Apple GPU Vulkan driver for Strata (private ICD; the desktop keeps its Mesa)"
  HK="$(dirname "$DIR")/honeykrisp-strata"
  [[ -d "$HK/.git" ]] || git clone https://github.com/Emefedez/honeykrisp-strata "$HK" || die "clone honeykrisp-strata"
  for p in meson clang llvm libclc spirv-llvm-translator spirv-tools glslang python-mako python-packaging python-yaml; do pacman -Q "$p" >/dev/null 2>&1 || need+=("$p"); done
  ((${{#need[@]}})) && sudo pacman -S --needed "${{need[@]}}"
  bash "$HK/build-icd.sh" || die "driver build"
fi
if [[ -f "$DIR/vulkan/CMakeLists.txt" ]] && grep -q STRATA_ENABLE_VULKAN "$DIR/vulkan/CMakeLists.txt"; then
  say "Building the Vulkan engine"
  # Slang (the kernel compiler, a library the engine links) and llama.cpp's ggml at the commit Strata pins
  SLANG="$HOME/.local/share/slang/2026.18.2"
  if [[ ! -f "$SLANG/include/slang.h" ]]; then
    say "Slang 2026.18.2 into $SLANG"
    mkdir -p "$SLANG"
    curl -fL https://github.com/shader-slang/slang/releases/download/v2026.18.2/slang-2026.18.2-linux-aarch64.tar.gz | tar -xz -C "$SLANG" || die "slang download"
  fi
  LLAMA="$DIR/third_party/llama.cpp"
  if [[ ! -f "$LLAMA/ggml/CMakeLists.txt" ]]; then
    (cd "$DIR" && "$DIR/.venv/bin/python" -c 'import setup; setup.get_llama_cpp()') || die "llama.cpp download"
  fi
  cmake -S "$DIR/vulkan" -B "$DIR/build-vulkan" -G Ninja -DCMAKE_BUILD_TYPE=Release -DSTRATA_ENABLE_VULKAN=ON \
    -DSTRATA_GGML_DIR="$LLAMA" -DSLANG_DIR="$SLANG" || die "cmake configure"
  cmake --build "$DIR/build-vulkan" -j{jobs} --target strata || die "engine build"
  say "Engine: $DIR/build-vulkan/strata"
  say "Model setup (download, pack, the experts' streaming copy, the MTP draft layer, the run config):"
  say "  $DIR/.venv/bin/python $DIR/vulkan/setup_asahi.py --model Q2_0 --yes --no-start"
else
  warn "This branch has no Vulkan engine yet (the port is in progress): the server is installed,"
  warn "Strata models are listed but refuse to load until the engine is built. strata.engine = mock tests the plumbing."
fi
say "Done. Run configs (strata-<model>.json) in $DIR show up as strata/<model> in lmdeck."
"#,
        dir = dir.display(),
        repo = cfg.strata.repo,
        gitref = cfg.strata.git_ref,
        icd = icd.display(),
    )
}

pub fn script_build_llama(cfg: &Config) -> String {
    let dir = llama_dir();
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let repo = if cfg.llama.repo.trim().is_empty() {
        "https://github.com/ggml-org/llama.cpp".to_string()
    } else {
        cfg.llama.repo.trim().to_string()
    };
    format!(
        r#"{HEADER}
DIR="{dir}"
REF="{gitref}"
REPO="{repo}"
say "Building llama.cpp ($REF) with the Vulkan backend into $DIR"
{own}
need=()
for p in git cmake shaderc spirv-headers vulkan-headers vulkan-icd-loader; do pacman -Q "$p" >/dev/null 2>&1 || need+=("$p"); done
if ((${{#need[@]}})); then
  say "Installing build deps: ${{need[*]}}"
  if command -v omarchy-pkg-add >/dev/null; then omarchy-pkg-add "${{need[@]}}"; else sudo pacman -S --needed "${{need[@]}}"; fi || die "deps"
fi
mkdir -p "$DIR"
if [[ -d "$DIR/src/.git" ]]; then
  git -C "$DIR/src" remote set-url origin "$REPO" || die "git remote"
  git -C "$DIR/src" fetch --depth 1 origin "$REF" && git -C "$DIR/src" checkout -q FETCH_HEAD || die "git fetch"
else
  git clone --depth 1 --branch "$REF" "$REPO" "$DIR/src" || die "git clone"
fi
say "Commit: $(git -C "$DIR/src" log -1 --format='%h %cs %s')"
cmake -S "$DIR/src" -B "$DIR/build" -DCMAKE_BUILD_TYPE=Release -DGGML_VULKAN=ON -DGGML_NATIVE=ON \
  -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF -DLLAMA_BUILD_SERVER=ON \
  -DBUILD_SHARED_LIBS=OFF || die "cmake configure"
cmake --build "$DIR/build" -j{jobs} --target llama-server llama-bench || die "build"
mkdir -p "$DIR/bin"
install -m755 "$DIR/build/bin/llama-server" "$DIR/build/bin/llama-bench" "$DIR/bin/"
"$DIR/bin/llama-server" --version 2>&1 | head -3
say "Vulkan devices seen by llama.cpp:"
"$DIR/bin/llama-server" --list-devices 2>&1 | grep -iE 'vulkan|apple' | head -5
say "Done. GGUF models now load through $DIR/bin/llama-server"
"#,
        dir = dir.display(),
        gitref = cfg.llama.git_ref,
        repo = repo,
        own = if cfg.llama.server_path.is_empty() { String::new() } else {
            format!("warn \"llama.server_path is set ({}): lmdeck keeps using that binary; clear the setting to use this build\"", cfg.llama.server_path)
        },
    )
}

// ---------- user service + menu ----------

pub fn exe_path() -> PathBuf {
    let installed = home().join(".local/bin/lmdeck");
    if installed.exists() {
        installed
    } else {
        std::env::current_exe().unwrap_or(installed)
    }
}

pub fn unit_path() -> PathBuf {
    crate::config::home().join(".config/systemd/user/lmdeck.service")
}

pub fn unit_text() -> String {
    format!(
        r#"[Unit]
Description=lmdeck local model server (Ollama/OpenAI compatible; MLX + llama.cpp)
After=network.target

[Service]
Type=simple
ExecStart={exe} serve
Restart=on-failure
RestartSec=2
# lmdeck stops its backends itself (a Strata model first saves its conversation):
# SIGTERM goes to lmdeck alone, what is left after it exits gets SIGKILL.
KillMode=mixed
TimeoutStopSec=60
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin
# The daemon itself is tiny; backends inherit normal priority.
Nice=0

[Install]
WantedBy=default.target
"#,
        exe = exe_path().display()
    )
}

pub fn systemctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("systemctl").arg("--user").args(args).output()
}

pub fn service_installed() -> bool {
    unit_path().exists()
}

pub fn service_active() -> bool {
    systemctl(&["is-active", "--quiet", "lmdeck"]).map(|o| o.status.success()).unwrap_or(false)
}

pub fn install_service() -> anyhow::Result<()> {
    let p = unit_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(&p, unit_text())?;
    systemctl(&["daemon-reload"])?;
    let o = systemctl(&["enable", "--now", "lmdeck"])?;
    if !o.status.success() {
        anyhow::bail!("systemctl enable failed: {}", String::from_utf8_lossy(&o.stderr));
    }
    Ok(())
}

pub fn desktop_entry_installed() -> bool {
    home().join(".local/share/applications/lmdeck.desktop").exists()
}

pub fn desktop_entry() -> anyhow::Result<PathBuf> {
    let p = home().join(".local/share/applications/lmdeck.desktop");
    std::fs::create_dir_all(p.parent().unwrap())?;
    let launcher = if which("omarchy-launch-or-focus-tui").is_some() {
        "omarchy-launch-or-focus-tui "
    } else if which("omarchy-launch-tui").is_some() {
        "omarchy-launch-tui "
    } else {
        "xdg-terminal-exec "
    };
    std::fs::write(
        &p,
        format!(
            "[Desktop Entry]\nType=Application\nName=lmdeck\nComment=Local models: MLX (Apple GPU) + GGUF, Ollama-compatible server\nExec={launcher}{}\nIcon=utilities-terminal\nCategories=Development;Utility;\nTerminal=false\n",
            exe_path().display()
        ),
    )?;
    let _ = std::process::Command::new("omarchy-menu").arg("refresh").output();
    Ok(p)
}

/// Add (or refresh) lmdeck rows in the Omarchy menu extension file.
pub fn install_menu() -> anyhow::Result<PathBuf> {
    let p = home().join(".config/omarchy/extensions/omarchy-menu.jsonc");
    let exe = exe_path().display().to_string();
    let launch = if which("omarchy-launch-or-focus-tui").is_some() { "omarchy-launch-or-focus-tui" } else { "omarchy-launch-tui" };
    let block = format!(
        r#"  // lmdeck-menu-start
  "apps.lmdeck": {{"icon":"󰧑","label":"lmdeck","aliases":["lmdeck","llm","models","mlx","ollama","gguf","strata"],"description":"Local models on the Apple GPU: MLX, GGUF and Strata; Ollama/OpenAI-compatible server","action":"{launch} {exe}"}},
  "apps.lmdeck-xtreme": {{"icon":"󱐋","label":"lmdeck XTreme","aliases":["xtreme","headless","overnight","lmdeck-xtreme"],"description":"Desktop off, the model in the freed memory: agents on a task list, or a model server for other computers","action":"{launch} {exe} xtreme plan"}},
  "setup.lmdeck-server": {{"icon":"󰒋","label":"Local LLM Server","aliases":["llm-server","lmdeck-server"],"description":"Toggle the lmdeck model server (port 11434)","action":"{exe} service toggle","checked":"systemctl --user is-active --quiet lmdeck"}},
  // lmdeck-menu-end
"#
    );
    let cur = std::fs::read_to_string(&p).unwrap_or_else(|_| "{\n}\n".into());
    let new = if let (Some(a), Some(b)) = (cur.find("  // lmdeck-menu-start"), cur.find("// lmdeck-menu-end\n")) {
        format!("{}{}{}", &cur[..a], block, &cur[b + "// lmdeck-menu-end\n".len()..])
    } else if let Some(i) = cur.find("  // Extend the Quickshell") {
        format!("{}{}\n{}", &cur[..i], block, &cur[i..])
    } else {
        let i = cur.rfind('}').unwrap_or(cur.len());
        format!("{}{}{}", &cur[..i], block, &cur[i..])
    };
    if new != cur {
        if p.exists() {
            let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            std::fs::copy(&p, p.with_extension(format!("jsonc.bak.lmdeck-{ts}")))?;
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, new)?;
    }
    let _ = std::process::Command::new("omarchy-menu").arg("refresh").output();
    Ok(p)
}

#[cfg(test)]
mod tests {
    fn bash_n(script: &str) {
        let o = std::process::Command::new("bash").arg("-n").arg("-c").arg(script).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }

    #[test]
    fn scripts_parse() {
        bash_n(&super::script_migrate(false));
        bash_n(&super::script_migrate(true));
        bash_n(&super::script_update_mlx(false));
        bash_n(&super::script_update_mlx(true));
        bash_n(&super::script_update_mlx_custom(false));
        bash_n(&super::script_update_mlx_custom(true));
        bash_n(&super::script_update_menu(false));
        bash_n(&super::script_remove_other_mlx("custom"));
        bash_n(&super::script_remove_other_mlx("omarchy"));
        bash_n(&super::script_build_llama(&crate::config::Config::default()));
    }

    #[test]
    fn mlx_update_uses_omarchy_and_stops_on_failure() {
        for fail in [false, true] {
            let mocks = format!(
                r#"
omarchy() {{
  [[ "$*" == update ]] || return 99
  echo UPDATE >&2
  return {status}
}}
pacman() {{
  [[ "$*" == '-Q omarchy-mlx' ]] || return 99
  echo 'omarchy-mlx 1.0-1'
}}
tar() {{ printf '%%VERSION%%\n1.0-1\n'; }}
vercmp() {{ echo 0; }}
sudo() {{ echo 'unexpected sudo invocation' >&2; return 99; }}
mlx-omarchy() {{ echo SMOKE >&2; }}
lmdeck() {{ [[ "$*" == 'runtime omarchy' ]] || return 99; echo SMOKE >&2; }}
"#,
                status = if fail { 23 } else { 0 }
            );
            let output = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("{mocks}\n{}", super::script_update_mlx(true)))
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), !fail, "{stderr}");
            if fail {
                assert!(stderr.contains("omarchy update failed"), "{stderr}");
                assert!(!stderr.contains("SMOKE"), "{stderr}");
                assert!(!String::from_utf8_lossy(&output.stdout).contains("is current"));
            } else {
                assert_eq!(stderr, "UPDATE\nSMOKE\n");
                assert!(String::from_utf8_lossy(&output.stdout).contains("omarchy-mlx 1.0-1 is current"));
            }
        }
    }

    #[test]
    fn update_menu_cancel_runs_no_updater() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(format!("omarchy() {{ exit 99; }}\ncurl() {{ exit 99; }}\n{}", super::script_update_menu(false)))
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"invalid\nq\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        for choice in ["1. Omarchy", "2. Custom Installer", "q. Cancel", "Choose 1, 2, or q."] {
            assert!(stdout.contains(choice), "{stdout}");
        }
    }

    #[test]
    fn custom_update_pins_release_and_stops_on_installer_failure() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("lmdeck-installer-test-{}", std::process::id()));
        let bin = root.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let launcher = bin.join("mlx-omarchy");
        std::fs::write(&launcher, "#!/bin/bash\necho SMOKE >&2\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        for fail in [false, true] {
            let mocks = format!(r#"
curl() {{
  if [[ "$2" == https://api.github.com/repos/joshuaswarren/omarchy-mlx/releases/latest ]]; then
    echo '{{"tag_name":"v0.7.31"}}'
  elif [[ "$2" == https://raw.githubusercontent.com/joshuaswarren/omarchy-mlx/v0.7.31/install.sh && "$3" == -o ]]; then
    printf '%s\n' '[[ "$MLX_OMARCHY_VERSION" == v0.7.31 ]] || exit 99' '[[ "$MLX_OMARCHY_HOME" == "$HOME/.local/share/mlx-omarchy" ]] || exit 99' '[[ "$PIP_NO_CACHE_DIR" == 1 ]] || exit 99' 'echo INSTALL >&2' 'exit {status}' > "$4"
  else
    return 99
  fi
}}
lmdeck() {{ [[ "$*" == 'runtime custom' ]] || return 99; echo SELECT >&2; }}
"#, status = if fail { 23 } else { 0 });
            let output = std::process::Command::new("bash")
                .arg("-c").arg(format!("{mocks}\n{}", super::script_update_mlx_custom(fail)))
                .env("HOME", &root).output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.success(), !fail, "{stderr}");
            if fail {
                assert!(stderr.contains("Custom Installer failed"), "{stderr}");
                assert!(!stderr.contains("SMOKE"), "{stderr}");
            } else {
                // Updating does not change which runtime lmdeck uses.
                assert_eq!(stderr, "INSTALL\nSMOKE\n");
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selecting_mlx_preserves_server_arguments() {
        let mut cfg = crate::config::Config::default();
        cfg.mlx.command = vec!["mlx-omarchy".into(), "-m".into(), "mlx_lm.server".into(), "--custom-flag".into()];
        super::set_mlx_executable(&mut cfg, std::path::Path::new("/custom/mlx-omarchy"));
        assert_eq!(cfg.mlx.command, ["/custom/mlx-omarchy", "-m", "mlx_lm.server", "--custom-flag"]);
        cfg.mlx.command.clear();
        super::set_mlx_executable(&mut cfg, std::path::Path::new("/usr/bin/mlx-omarchy"));
        assert_eq!(cfg.mlx.command, ["/usr/bin/mlx-omarchy", "-m", "mlx_lm.server"]);
    }

    #[test]
    fn removing_packaged_runtime_preserves_gpu_driver() {
        let output = std::process::Command::new("bash").arg("-c")
            .arg(format!("pacman() {{ return 0; }}\nsudo() {{ echo \"$*\"; }}\n{}", super::script_remove_other_mlx("custom")))
            .output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("pacman -D --asexplicit omarchy-mlx-vulkan\npacman -R omarchy-mlx\n"), "{stdout}");
        assert!(!stdout.contains("-Rns"));
    }

    #[test]
    fn error_summary_picks_exception() {
        let p = std::env::temp_dir().join("lmdeck-test-errlog.log");
        std::fs::write(&p, "Traceback:\n  File x\nRuntimeError: [omarchy] Take dtype is not implemented\n----------------------------------------\n").unwrap();
        assert_eq!(super::error_summary(&p), "RuntimeError: [omarchy] Take dtype is not implemented");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn rfc3339_epoch() {
        assert_eq!(super::rfc3339(std::time::UNIX_EPOCH), "1970-01-01T00:00:00.000000000Z");
    }
}

/// Local calendar date `days_ago` days back, YYYY-MM-DD (matches the agents panel).
pub fn local_date(days_ago: i64) -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) - days_ago * 86400;
    let tt = t as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&tt, &mut tm) };
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// Non-loopback IPv4 addresses with their interface names.
pub fn lan_ips() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() {
                    let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                    out.push((name, ip.to_string()));
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// Resident memory of running muvm/libkrun VMs (FEX/Proton games run inside
/// them), or None when no VM is up.
pub fn muvm_rss() -> Option<u64> {
    let mut total = 0u64;
    let mut found = false;
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let comm = comm.trim();
        if comm == "muvm" || comm.starts_with("krun") || comm.starts_with("libkrun") {
            found = true;
            total += rss(pid);
        }
    }
    found.then_some(total)
}
