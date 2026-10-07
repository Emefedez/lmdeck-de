//! Hugging Face Hub: search, repo listing, and a resumable, segmented,
//! sha256-verified downloader writing the standard hub cache layout
//! (`models--org--name/{blobs,snapshots/<sha>,refs/main}`), so MLX tools and
//! huggingface_hub see the same files.

use crate::config::{home, Config};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const HF: &str = "https://huggingface.co";

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("lmdeck/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .pool_max_idle_per_host(16)
        .build()
        .expect("http client")
}

pub fn token() -> Option<String> {
    if let Ok(t) = std::env::var("HF_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    let p = std::env::var("HF_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".cache/huggingface"));
    std::fs::read_to_string(p.join("token")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn auth(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match token() {
        Some(t) => rb.bearer_auth(t),
        None => rb,
    }
}

// ---------- search ----------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoHit {
    pub id: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub likes: u64,
    #[serde(default, rename = "lastModified")]
    pub last_modified: String,
    #[serde(default)]
    pub pipeline_tag: Option<String>,
    #[serde(default)]
    pub gated: serde_json::Value,
}

/// kind: "mlx" | "gguf"
pub async fn search(c: &reqwest::Client, query: &str, kind: &str, limit: usize) -> Result<Vec<RepoHit>> {
    let mut q: Vec<(&str, String)> = vec![
        ("search", query.to_string()),
        ("sort", "downloads".into()),
        ("direction", "-1".into()),
        ("limit", limit.to_string()),
    ];
    q.push(("filter", if kind == "gguf" { "gguf" } else { "mlx" }.into()));
    for f in ["downloads", "likes", "lastModified", "pipeline_tag", "gated"] {
        q.push(("expand[]", f.into()));
    }
    let r = auth(c.get(format!("{HF}/api/models")).query(&q)).send().await?.error_for_status()?;
    Ok(r.json().await?)
}

// ---------- repo tree ----------

#[derive(Debug, Clone, Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    ty: String,
    path: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    oid: String,
    #[serde(default)]
    lfs: Option<Lfs>,
}

#[derive(Debug, Clone, Deserialize)]
struct Lfs {
    oid: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoFile {
    pub path: String,
    pub size: u64,
    /// LFS sha256 (also the blob name) when the file is in LFS/Xet.
    pub sha256: Option<String>,
    /// Blob name in the cache (sha256 for LFS, git oid otherwise).
    pub blob: String,
}

pub async fn repo_sha(c: &reqwest::Client, repo: &str, rev: &str) -> Result<String> {
    let r = auth(c.get(format!("{HF}/api/models/{repo}/revision/{rev}"))).send().await?;
    if r.status() == 401 || r.status() == 403 {
        bail!("{repo} is gated or private: accept its license on huggingface.co and put a token in ~/.cache/huggingface/token");
    }
    if r.status() == 404 {
        bail!("{repo} not found on Hugging Face");
    }
    let v: serde_json::Value = r.error_for_status()?.json().await?;
    v.get("sha").and_then(|s| s.as_str()).map(str::to_string).ok_or_else(|| anyhow!("no sha for {repo}"))
}

pub async fn repo_files(c: &reqwest::Client, repo: &str, sha: &str) -> Result<Vec<RepoFile>> {
    let mut url = format!("{HF}/api/models/{repo}/tree/{sha}?recursive=true&expand=false");
    let mut out = Vec::new();
    loop {
        let r = auth(c.get(&url)).send().await?.error_for_status()?;
        let next = r
            .headers()
            .get("link")
            .and_then(|l| l.to_str().ok())
            .and_then(|l| l.split(',').find(|p| p.contains("rel=\"next\"")))
            .and_then(|p| p.split(['<', '>']).nth(1))
            .map(str::to_string);
        let entries: Vec<TreeEntry> = r.json().await?;
        for e in entries.into_iter().filter(|e| e.ty == "file") {
            let (size, sha256, blob) = match e.lfs {
                Some(l) => (l.size, Some(l.oid.clone()), l.oid),
                None => (e.size, None, e.oid),
            };
            out.push(RepoFile { path: e.path, size, sha256, blob });
        }
        match next {
            Some(n) => url = n,
            None => break,
        }
    }
    Ok(out)
}

/// Group a repo's GGUF files into quant tags with total size.
pub fn gguf_quants(files: &[RepoFile]) -> Vec<(String, u64)> {
    let mut m: Vec<(String, u64)> = Vec::new();
    for f in files.iter().filter(|f| f.path.to_lowercase().ends_with(".gguf")) {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        if name.to_lowercase().starts_with("mmproj") {
            continue;
        }
        let tag = crate::models::quant_tag(&crate::models::split_key(name));
        match m.iter_mut().find(|(t, _)| *t == tag) {
            Some(e) => e.1 += f.size,
            None => m.push((tag, f.size)),
        }
    }
    m.sort_by_key(|(_, s)| *s);
    m
}

// ---------- pull planning ----------

#[derive(Debug, Clone, Serialize)]
pub struct PullPlan {
    pub repo: String,
    pub sha: String,
    pub name: String,
    pub files: Vec<RepoFile>,
}

impl PullPlan {
    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

pub async fn plan(c: &reqwest::Client, cfg: &Config, spec: &str) -> Result<PullPlan> {
    let spec = crate::models::normalize(spec);
    let (repo, tag) = match spec.split_once(':') {
        Some((r, t)) => (r.to_string(), Some(t.to_string())),
        None => (spec.clone(), None),
    };
    if repo.split('/').count() != 2 {
        bail!("expected a Hugging Face repo like org/name or org/name:Q4_K_M, got {spec}");
    }
    let sha = repo_sha(c, &repo, "main").await?;
    let files = repo_files(c, &repo, &sha).await?;
    let lower = |f: &RepoFile| f.path.to_lowercase();
    let has_st = files.iter().any(|f| lower(f).ends_with(".safetensors")) && files.iter().any(|f| f.path == "config.json");
    let ggufs = gguf_quants(&files);

    let want_gguf = tag.is_some() || !has_st;
    if want_gguf {
        if ggufs.is_empty() {
            bail!("{repo} has no GGUF or MLX safetensors weights");
        }
        let pick = match &tag {
            Some(t) => ggufs
                .iter()
                .find(|(q, _)| q.eq_ignore_ascii_case(t))
                .or_else(|| ggufs.iter().find(|(q, _)| q.to_lowercase().contains(&t.to_lowercase())))
                .ok_or_else(|| {
                    anyhow!("no quant {t} in {repo}; available: {}", ggufs.iter().map(|g| g.0.as_str()).collect::<Vec<_>>().join(", "))
                })?
                .0
                .clone(),
            None => {
                let pref = [cfg.download.gguf_default_quant.as_str(), "Q4_K_M", "UD-Q4_K_XL", "Q4_K_S", "Q5_K_M", "Q4_0", "Q6_K", "Q8_0"];
                pref.iter()
                    .find_map(|p| ggufs.iter().find(|(q, _)| q.eq_ignore_ascii_case(p)))
                    .unwrap_or(&ggufs[0])
                    .0
                    .clone()
            }
        };
        let chosen: Vec<RepoFile> = files
            .iter()
            .filter(|f| {
                let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
                if !name.to_lowercase().ends_with(".gguf") {
                    return false;
                }
                if name.to_lowercase().starts_with("mmproj") {
                    return cfg.download.gguf_mmproj;
                }
                crate::models::quant_tag(&crate::models::split_key(&name)) == pick
            })
            .cloned()
            .collect();
        // If several mmproj variants exist keep the smallest f16-ish one only.
        let chosen = dedup_mmproj(chosen);
        return Ok(PullPlan { name: format!("{repo}:{pick}"), repo, sha, files: chosen });
    }

    let skip = |p: &str| {
        let l = p.to_lowercase();
        l.ends_with(".gguf")
            || l.starts_with("original/")
            || l.ends_with(".pth")
            || l.ends_with(".pt")
            || l.ends_with(".onnx")
            || l.ends_with(".msgpack")
            || l.ends_with(".h5")
            || (l.ends_with(".bin") && l.contains("pytorch_model"))
            || l.starts_with("consolidated")
            || l == ".gitattributes"
    };
    let chosen = files.into_iter().filter(|f| !skip(&f.path)).collect();
    Ok(PullPlan { name: repo.clone(), repo, sha, files: chosen })
}

fn dedup_mmproj(files: Vec<RepoFile>) -> Vec<RepoFile> {
    let (mm, mut rest): (Vec<_>, Vec<_>) = files.into_iter().partition(|f| f.path.to_lowercase().contains("mmproj"));
    if let Some(best) = mm.iter().min_by_key(|f| {
        let l = f.path.to_lowercase();
        (if l.contains("f16") { 0 } else { 1 }, f.size)
    }) {
        rest.push(best.clone());
    }
    rest
}

// ---------- download manager ----------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase", tag = "state", content = "detail")]
pub enum DlStatus {
    Resolving,
    Downloading,
    Verifying,
    Done,
    Failed(String),
    Cancelled,
}

pub struct Download {
    pub name: Mutex<String>,
    pub spec: String,
    pub total: AtomicU64,
    pub done: AtomicU64,
    pub status: Mutex<DlStatus>,
    pub file: Mutex<String>,
    pub cancel: AtomicBool,
    pub started: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadView {
    pub spec: String,
    pub name: String,
    pub total: u64,
    pub completed: u64,
    pub status: serde_json::Value,
    pub file: String,
    pub elapsed_secs: f64,
}

impl Download {
    pub fn view(&self) -> DownloadView {
        DownloadView {
            spec: self.spec.clone(),
            name: self.name.lock().unwrap().clone(),
            total: self.total.load(Ordering::Relaxed),
            completed: self.done.load(Ordering::Relaxed),
            status: serde_json::to_value(&*self.status.lock().unwrap()).unwrap_or_default(),
            file: self.file.lock().unwrap().clone(),
            elapsed_secs: self.started.elapsed().as_secs_f64(),
        }
    }
    fn set(&self, s: DlStatus) {
        *self.status.lock().unwrap() = s;
    }
    pub fn finished(&self) -> bool {
        matches!(*self.status.lock().unwrap(), DlStatus::Done | DlStatus::Failed(_) | DlStatus::Cancelled)
    }
}

#[derive(Clone, Default)]
pub struct Downloads {
    pub map: Arc<Mutex<HashMap<String, Arc<Download>>>>,
}

impl Downloads {
    /// Start (or join) a pull. Returns the shared progress handle.
    pub fn start(&self, c: reqwest::Client, cfg: Config, spec: &str) -> Arc<Download> {
        let key = crate::models::normalize(spec);
        let mut map = self.map.lock().unwrap();
        if let Some(d) = map.get(&key) {
            if !d.finished() {
                return d.clone();
            }
        }
        let d = Arc::new(Download {
            name: Mutex::new(key.clone()),
            spec: key.clone(),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            status: Mutex::new(DlStatus::Resolving),
            file: Mutex::new(String::new()),
            cancel: AtomicBool::new(false),
            started: Instant::now(),
        });
        map.insert(key.clone(), d.clone());
        let dd = d.clone();
        tokio::spawn(async move {
            match pull(&c, &cfg, &key, &dd).await {
                Ok(()) => dd.set(DlStatus::Done),
                Err(e) if dd.cancel.load(Ordering::Relaxed) => {
                    let _ = e;
                    dd.set(DlStatus::Cancelled)
                }
                Err(e) => dd.set(DlStatus::Failed(format!("{e:#}"))),
            }
        });
        d
    }

    pub fn cancel(&self, spec: &str) -> bool {
        let key = crate::models::normalize(spec);
        match self.map.lock().unwrap().get(&key) {
            Some(d) => {
                d.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn list(&self) -> Vec<DownloadView> {
        let mut v: Vec<_> = self.map.lock().unwrap().values().map(|d| d.view()).collect();
        v.sort_by(|a, b| a.spec.cmp(&b.spec));
        v
    }

    pub fn clear_finished(&self) {
        self.map.lock().unwrap().retain(|_, d| !d.finished());
    }
}

async fn pull(c: &reqwest::Client, cfg: &Config, spec: &str, d: &Download) -> Result<()> {
    let plan = plan(c, cfg, spec).await?;
    *d.name.lock().unwrap() = plan.name.clone();
    d.total.store(plan.total(), Ordering::Relaxed);
    d.set(DlStatus::Downloading);

    let repo_dir = cfg.hf_cache().join(format!("models--{}", plan.repo.replace('/', "--")));
    let blobs = repo_dir.join("blobs");
    let snap = repo_dir.join("snapshots").join(&plan.sha);
    tokio::fs::create_dir_all(&blobs).await?;
    tokio::fs::create_dir_all(&snap).await?;

    let sem = Arc::new(tokio::sync::Semaphore::new(cfg.download.parallel_files.max(1)));
    let mut tasks = Vec::new();
    // Big files first so the long poles start early.
    let mut files = plan.files.clone();
    files.sort_by(|a, b| b.size.cmp(&a.size));
    let d_ptr: &Download = d;
    for f in files {
        let sem = sem.clone();
        let blobs = blobs.clone();
        let snap = snap.clone();
        let url = format!("{HF}/{}/resolve/{}/{}", plan.repo, plan.sha, f.path);
        let conns = cfg.download.connections.max(1);
        tasks.push(async move {
            let _p = sem.acquire().await;
            if d_ptr.cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            *d_ptr.file.lock().unwrap() = f.path.clone();
            let blob = blobs.join(&f.blob);
            fetch_file(c, &url, &blob, f.size, f.sha256.as_deref(), conns, d_ptr)
                .await
                .with_context(|| format!("downloading {}", f.path))?;
            link_snapshot(&snap, &f.path, &f.blob)?;
            Ok::<_, anyhow::Error>(())
        });
    }
    let results = futures_util::future::join_all(tasks).await;
    for r in results {
        r?;
    }
    tokio::fs::write(repo_dir.join("refs").join("main"), &plan.sha).await.or_else(|_| {
        std::fs::create_dir_all(repo_dir.join("refs"))?;
        std::fs::write(repo_dir.join("refs/main"), &plan.sha)
    })?;
    Ok(())
}

fn link_snapshot(snap: &Path, rel: &str, blob: &str) -> Result<()> {
    let link = snap.join(rel);
    if let Some(p) = link.parent() {
        std::fs::create_dir_all(p)?;
    }
    let depth = rel.split('/').count() - 1 + 2; // snapshots/<sha>/<dirs...>
    let target = format!("{}blobs/{blob}", "../".repeat(depth));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(target, &link)?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct SegState {
    size: u64,
    segs: Vec<[u64; 3]>, // start, end(excl), done
}

const SEG_MIN: u64 = 64 << 20;

async fn fetch_file(
    c: &reqwest::Client,
    url: &str,
    blob: &Path,
    size: u64,
    sha256: Option<&str>,
    conns: usize,
    d: &Download,
) -> Result<()> {
    if let Ok(m) = tokio::fs::metadata(blob).await {
        if m.len() == size {
            d.done.fetch_add(size, Ordering::Relaxed);
            return Ok(());
        }
    }
    let part = PathBuf::from(format!("{}.incomplete", blob.display()));
    let state_path = PathBuf::from(format!("{}.incomplete.lmdeck", blob.display()));

    let segs: Vec<[u64; 3]> = match std::fs::read(&state_path).ok().and_then(|b| serde_json::from_slice::<SegState>(&b).ok()) {
        Some(s) if s.size == size && part.exists() => s.segs,
        _ => {
            // Fresh, or resume a plain single-stream partial (huggingface_hub style).
            let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0).min(size);
            let n = if size >= SEG_MIN * 2 { conns.min((size / SEG_MIN) as usize).max(1) } else { 1 };
            if n == 1 || have > 0 {
                vec![[0, size, have]]
            } else {
                let step = size.div_ceil(n as u64);
                (0..n as u64).map(|i| [i * step, ((i + 1) * step).min(size), 0]).collect()
            }
        }
    };
    let already: u64 = segs.iter().map(|s| s[2]).sum();
    d.done.fetch_add(already, Ordering::Relaxed);

    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&part)?;
    file.set_len(size)?;
    let file = Arc::new(file);
    let progress: Arc<Vec<AtomicU64>> = Arc::new(segs.iter().map(|s| AtomicU64::new(s[2])).collect());

    let save = {
        let segs = segs.clone();
        let progress = progress.clone();
        let state_path = state_path.clone();
        move || {
            let st = SegState {
                size,
                segs: segs.iter().zip(progress.iter()).map(|(s, p)| [s[0], s[1], p.load(Ordering::Relaxed)]).collect(),
            };
            let _ = std::fs::write(&state_path, serde_json::to_vec(&st).unwrap());
        }
    };
    save();

    let futs = segs.iter().enumerate().map(|(i, s)| {
        let (start, end) = (s[0], s[1]);
        let file = file.clone();
        let progress = progress.clone();
        async move {
            let mut attempts = 0;
            loop {
                let done = progress[i].load(Ordering::Relaxed);
                if start + done >= end {
                    return Ok::<_, anyhow::Error>(());
                }
                match fetch_range(c, url, &file, start + done, end, &progress[i], d).await {
                    Ok(()) => attempts = 0,
                    Err(e) => {
                        if d.cancel.load(Ordering::Relaxed) {
                            return Err(anyhow!("cancelled"));
                        }
                        attempts += 1;
                        if attempts > 6 {
                            return Err(e);
                        }
                        tokio::time::sleep(Duration::from_millis(500 * (1 << attempts.min(5)))).await;
                    }
                }
            }
        }
    });
    let saver = {
        let save = save.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                save();
            }
        }
    };
    let res = tokio::select! {
        r = futures_util::future::try_join_all(futs) => r,
        _ = saver => unreachable!(),
    };
    save();
    res?;
    drop(file);

    if let Some(want) = sha256 {
        d.set(DlStatus::Verifying);
        let p = part.clone();
        let got = tokio::task::spawn_blocking(move || sha256_file(&p)).await??;
        d.set(DlStatus::Downloading);
        if !got.eq_ignore_ascii_case(want) {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&state_path);
            d.done.fetch_sub(size, Ordering::Relaxed);
            bail!("sha256 mismatch (got {got}, want {want}); partial removed, pull again");
        }
    }
    std::fs::rename(&part, blob)?;
    let _ = std::fs::remove_file(&state_path);
    Ok(())
}

async fn fetch_range(
    c: &reqwest::Client,
    url: &str,
    file: &std::fs::File,
    from: u64,
    end: u64,
    prog: &AtomicU64,
    d: &Download,
) -> Result<()> {
    let r = auth(c.get(url).header("Range", format!("bytes={}-{}", from, end - 1))).send().await?;
    let status = r.status();
    if status == 401 || status == 403 {
        bail!("access denied ({status}); the repo may be gated — add a token to ~/.cache/huggingface/token");
    }
    if !status.is_success() {
        bail!("HTTP {status}");
    }
    let ranged = status == reqwest::StatusCode::PARTIAL_CONTENT;
    if !ranged && from != 0 {
        bail!("server ignored Range request");
    }
    let mut off = from;
    let mut buf: Vec<u8> = Vec::with_capacity(4 << 20);
    let mut stream = r.bytes_stream();
    let flush = |buf: &mut Vec<u8>, off: &mut u64| -> std::io::Result<()> {
        if !buf.is_empty() {
            file.write_all_at(buf, *off)?;
            *off += buf.len() as u64;
            prog.fetch_add(buf.len() as u64, Ordering::Relaxed);
            d.done.fetch_add(buf.len() as u64, Ordering::Relaxed);
            buf.clear();
        }
        Ok(())
    };
    while let Some(chunk) = stream.next().await {
        if d.cancel.load(Ordering::Relaxed) {
            flush(&mut buf, &mut off)?;
            bail!("cancelled");
        }
        let chunk = chunk?;
        let room = (end - off - buf.len() as u64) as usize;
        let take = chunk.len().min(room);
        buf.extend_from_slice(&chunk[..take]);
        if buf.len() >= 2 << 20 {
            flush(&mut buf, &mut off)?;
        }
        if off + buf.len() as u64 >= end {
            break;
        }
    }
    flush(&mut buf, &mut off)?;
    if off < end {
        bail!("connection closed early");
    }
    Ok(())
}

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
