//! System helpers: memory, process RSS, logging, time formatting, versions,
//! and the shell scripts behind migrate/update/build actions.

use crate::config::{home, llama_dir, log_dir, which, Config};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub llama: Option<String>,
    pub llama_path: Option<String>,
    pub ane: bool,
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
    Versions {
        mlx_install: install.into(),
        mlx_pkg: pkg,
        mlx_pkg_repo: repo,
        mlx_legacy: legacy,
        mlx_upstream: None,
        mlx_bin: which(cfg.mlx.command.first().map(String::as_str).unwrap_or("mlx-omarchy")).map(|p| p.display().to_string()),
        llama,
        llama_path: llama_path.map(|p| p.display().to_string()),
        ane: Path::new("/dev/accel/accel0").exists(),
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

say "1/4 Installing packages with Omarchy's package helper"
if command -v omarchy-pkg-add >/dev/null; then
  omarchy-pkg-add {pkgs} || {{
    warn "omarchy-pkg-add failed; trying the AUR helper (omarchy-pkg-aur-add)"
    omarchy-pkg-aur-add {pkgs} || die "could not install {pkgs}"
  }}
else
  sudo pacman -S --needed {pkgs} || die "pacman failed"
fi

say "2/4 Verifying the packaged runtime on the Apple GPU"
/usr/bin/mlx-omarchy -c 'import mlx.core as mx; a=mx.random.normal((256,256)); mx.eval((a@a).sum()); print("  ", mx.__version__, mx.default_device())' \
  || die "packaged runtime failed its smoke test; legacy install left untouched"

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

pub fn script_update_mlx() -> String {
    format!(
        r#"{HEADER}
say "Updating omarchy-mlx packages"
if ! pacman -Q omarchy-mlx >/dev/null 2>&1; then
  die "omarchy-mlx is not installed as a package; use Migrate first."
fi
before=$(pacman -Q omarchy-mlx)
# Sync + upgrade only the mlx packages' dependency closure would be a partial
# upgrade; Arch only supports full upgrades, so this runs one.
sudo pacman -Syu --needed {pkgs} || die "pacman failed"
say "$before -> $(pacman -Q omarchy-mlx)"
mlx-omarchy -c 'import mlx.core as mx; print("  ", mx.__version__, mx.default_device())' || warn "smoke test failed"
say "Done. Reload models in lmdeck to pick up the new runtime."
"#,
        pkgs = PKGS.join(" ")
    )
}

pub fn script_build_llama(cfg: &Config) -> String {
    let dir = llama_dir();
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    format!(
        r#"{HEADER}
DIR="{dir}"
REF="{gitref}"
say "Building llama.cpp ($REF) with the Vulkan backend into $DIR"
need=()
for p in git cmake shaderc spirv-headers vulkan-headers vulkan-icd-loader; do pacman -Q "$p" >/dev/null 2>&1 || need+=("$p"); done
if ((${{#need[@]}})); then
  say "Installing build deps: ${{need[*]}}"
  if command -v omarchy-pkg-add >/dev/null; then omarchy-pkg-add "${{need[@]}}"; else sudo pacman -S --needed "${{need[@]}}"; fi || die "deps"
fi
mkdir -p "$DIR"
if [[ -d "$DIR/src/.git" ]]; then
  git -C "$DIR/src" fetch --depth 1 origin "$REF" && git -C "$DIR/src" checkout -q FETCH_HEAD || die "git fetch"
else
  git clone --depth 1 --branch "$REF" https://github.com/ggml-org/llama.cpp "$DIR/src" || die "git clone"
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
        gitref = cfg.llama.git_ref
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
  "apps.lmdeck": {{"icon":"󰧑","label":"lmdeck","aliases":["lmdeck","llm","models","mlx","ollama","gguf"],"description":"Local models: MLX on the Apple GPU + GGUF, Ollama/OpenAI-compatible server","action":"{launch} {exe}"}},
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
        bash_n(&super::script_update_mlx());
        bash_n(&super::script_build_llama(&crate::config::Config::default()));
    }

    #[test]
    fn rfc3339_epoch() {
        assert_eq!(super::rfc3339(std::time::UNIX_EPOCH), "1970-01-01T00:00:00.000000000Z");
    }
}
