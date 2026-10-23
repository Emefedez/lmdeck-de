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

/// The part of a repo's `config.json` that decides whether it is a loadable
/// standalone chat model. The Hub exposes this through `expand[]=config`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoConfig {
    #[serde(default)]
    pub architectures: Vec<String>,
    #[serde(default)]
    pub model_type: Option<String>,
    #[serde(default)]
    pub model_file: Option<String>,
}

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
    /// Smallest matching variant's estimated RAM, populated by TUI discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formats: Vec<String>,
    /// `config.json` facts from the search response, when expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<RepoConfig>,
    /// Set when this checkpoint cannot be loaded as a standalone chat model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
}

fn search_query(query: &str) -> &str {
    let query = query.trim().trim_end_matches('/');
    let query = query.strip_prefix("https://huggingface.co/")
        .or_else(|| query.strip_prefix("https://hf.co/"))
        .or_else(|| query.strip_prefix("hf.co/")).unwrap_or(query);
    query.split("/tree/").next().unwrap_or(query).split(':').next().unwrap_or(query)
}

/// kind: "mlx" | "gguf"
pub async fn search(c: &reqwest::Client, query: &str, kind: &str, limit: usize) -> Result<Vec<RepoHit>> {
    search_sorted(c, query, kind, limit, "relevance").await
}

pub async fn search_sorted(c: &reqwest::Client, query: &str, kind: &str, limit: usize, sort: &str) -> Result<Vec<RepoHit>> {
    if !matches!(kind, "mlx" | "gguf") { bail!("kind must be mlx or gguf"); }
    if !matches!(sort, "relevance" | "downloads" | "likes" | "lastModified") { bail!("invalid search sort"); }
    let query = search_query(query);
    let limit = limit.clamp(1, 100);
    // Search each word against the Hub's substring index, then require all words
    // locally. This finds "qwen 4b" in names such as "Qwen3-4B-4bit".
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.len() > 8 { bail!("use at most 8 search words"); }
    let mut searches = Vec::new();
    for word in if words.is_empty() { vec![String::new()] } else { words.clone() } {
        let q = vec![
            ("search", word), ("sort", if sort == "relevance" { "downloads" } else { sort }.into()),
            ("direction", "-1".into()), ("limit", if words.len() > 1 { 100 } else { limit }.to_string()),
            ("filter", kind.into()),
            ("expand[]", "downloads".into()), ("expand[]", "likes".into()),
            ("expand[]", "lastModified".into()), ("expand[]", "pipeline_tag".into()), ("expand[]", "gated".into()),
            ("expand[]", "config".into()),
        ];
        searches.push(async move {
            let r = auth(c.get(format!("{HF}/api/models")).query(&q)).send().await?.error_for_status()?;
            Ok::<Vec<RepoHit>, anyhow::Error>(r.json().await?)
        });
    }
    let mut hits: Vec<RepoHit> = futures_util::future::try_join_all(searches).await?.into_iter().flatten().collect();
    // An explicit repo or pasted URL should work even if its format tag is missing.
    if query.split('/').count() == 2 && !query.contains(char::is_whitespace) {
        let r = auth(c.get(format!("{HF}/api/models/{query}")).query(&[("expand[]", "config")])).send().await?;
        if r.status().is_success() { hits.push(r.json().await?); }
        else if r.status() != 404 { r.error_for_status()?; }
    }
    mark_unsupported(&mut hits, kind);
    rank_hits(&mut hits, query, sort);
    hits.truncate(limit);
    Ok(hits)
}

/// Flag checkpoints whose own config says they are not standalone chat models.
/// Only for MLX searches: a GGUF conversion may target a different runtime.
fn mark_unsupported(hits: &mut [RepoHit], kind: &str) {
    if kind != "mlx" {
        return;
    }
    for hit in hits {
        let Some(config) = &hit.config else { continue };
        hit.unsupported = crate::compat::mlx_unsupported(
            config.model_type.as_deref().unwrap_or(""),
            &config.architectures,
            config.model_file.as_deref(),
        );
    }
}

/// Fetch just `config.json` facts for one repo (for the repo detail screen).
pub async fn repo_config(c: &reqwest::Client, repo: &str) -> Result<RepoConfig> {
    #[derive(Deserialize)]
    struct Info {
        #[serde(default)]
        config: Option<RepoConfig>,
    }
    let r = auth(c.get(format!("{HF}/api/models/{repo}")).query(&[("expand[]", "config")])).send().await?.error_for_status()?;
    Ok(r.json::<Info>().await?.config.unwrap_or_default())
}

/// Search supported formats, then check actual variants rather than trusting repo tags.
/// Unreadable repos are counted so discovery can report incomplete results.
pub async fn search_discover(c: &reqwest::Client, cfg: &Config, query: &str, kind: &str, budget: Option<u64>, sort: &str) -> Result<(Vec<RepoHit>, usize)> {
    let mut hits = if kind == "both" {
        let (mut mlx, gguf) = tokio::try_join!(
            search_sorted(c, query, "mlx", 100, sort),
            search_sorted(c, query, "gguf", 100, sort)
        )?;
        mlx.extend(gguf);
        mlx
    } else {
        search_sorted(c, query, kind, 100, sort).await?
    };
    // Each source is already ranked, including normalized repo URLs. Merge without
    // applying the raw query again, which would discard pasted URLs.
    rank_hits(&mut hits, search_query(query), sort);
    let checked = futures_util::stream::iter(hits.into_iter().map(|mut hit| async move {
        let files = repo_files(c, &hit.id, "main").await?;
        let variants = discovery_choices(cfg, &hit.id, &files, kind, budget);
        for variant in &variants {
            if !hit.formats.iter().any(|format| format == variant.format) {
                hit.formats.push(variant.format.into());
            }
        }
        hit.estimated_bytes = variants.iter().map(|v| crate::compat::estimated_memory(cfg, v.size, v.format)).min();
        Ok::<_, anyhow::Error>(hit.estimated_bytes.map(|_| hit))
    })).buffered(8).collect::<Vec<_>>().await;
    let mut unreadable = 0;
    let mut hits = Vec::new();
    for result in checked {
        match result {
            Ok(Some(hit)) => hits.push(hit),
            Ok(None) => {},
            Err(_) => unreadable += 1,
        }
    }
    hits.truncate(60);
    Ok((hits, unreadable))
}

fn matches_search_word(id: &str, word: &str) -> bool {
    if !word.starts_with(|c: char| c.is_ascii_digit()) { return id.contains(word); }
    // Parameter counts are tokens: searching 4b should not suggest 14b or 4bit.
    id.match_indices(word).any(|(start, _)| {
        let before = id[..start].chars().next_back();
        let after = id[start + word.len()..].chars().next();
        before.is_none_or(|c| !c.is_ascii_digit())
            && after.is_none_or(|c| !c.is_ascii_alphabetic())
    })
}

fn rank_hits(hits: &mut Vec<RepoHit>, query: &str, sort: &str) {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    hits.retain(|h| words.iter().all(|w| matches_search_word(&h.id.to_lowercase(), w)));
    let query = query.to_lowercase();
    hits.sort_by(|a, b| {
        let relevance = |h: &RepoHit| {
            let id = h.id.to_lowercase();
            let name = id.rsplit('/').next().unwrap_or(&id);
            if id == query || name == query { 0 } else if name.starts_with(&query) { 1 } else { 2 }
        };
        let order = match sort {
            "likes" => b.likes.cmp(&a.likes),
            "lastModified" => b.last_modified.cmp(&a.last_modified),
            "downloads" => b.downloads.cmp(&a.downloads),
            _ => relevance(a).cmp(&relevance(b)).then(b.downloads.cmp(&a.downloads)),
        };
        order.then(a.id.cmp(&b.id))
    });
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(h.id.clone()));
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

/// Downloadable variants of a repo: the MLX/safetensors repo and each GGUF quant.
#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    pub spec: String,
    pub label: String,
    pub size: u64,
    pub format: &'static str,
    pub default: bool,
}

pub fn choices(repo: &str, files: &[RepoFile], default_quant: &str) -> Vec<Choice> {
    let mut out = Vec::new();
    let has_st = files.iter().any(|f| f.path.ends_with(".safetensors")) && files.iter().any(|f| f.path == "config.json");
    if has_st {
        let size = files.iter().filter(|f| !f.path.ends_with(".gguf")).map(|f| f.size).sum();
        out.push(Choice { spec: repo.to_string(), label: "MLX / safetensors".into(), size, format: "mlx", default: true });
    }
    for (q, size) in gguf_quants(files) {
        let default = q.eq_ignore_ascii_case(default_quant);
        out.push(Choice { spec: format!("{repo}:{q}"), label: format!("GGUF {q}"), size, format: "gguf", default });
    }
    out
}

/// Apply discovery filters to individual downloadable variants.
pub fn discovery_choices(cfg: &Config, repo: &str, files: &[RepoFile], kind: &str, budget: Option<u64>) -> Vec<Choice> {
    choices(repo, files, &cfg.download.gguf_default_quant).into_iter()
        .filter(|v| kind == "both" || v.format == kind)
        .filter(|v| v.size > 0 && budget.is_none_or(|b| crate::compat::estimated_memory(cfg, v.size, v.format) <= b))
        .collect()
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
    pub cancel: Arc<AtomicBool>,
    cancelled: tokio::sync::Notify,
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
    async fn cancelled(&self) {
        let notified = self.cancelled.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.cancel.load(Ordering::Relaxed) { notified.await; }
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
            cancel: Arc::new(AtomicBool::new(false)),
            cancelled: tokio::sync::Notify::new(),
            started: Instant::now(),
        });
        map.insert(key.clone(), d.clone());
        crate::usage::EVENTS.bump();
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
            crate::usage::EVENTS.bump();
        });
        d
    }

    pub fn cancel(&self, spec: &str) -> bool {
        let key = crate::models::normalize(spec);
        match self.map.lock().unwrap().get(&key) {
            Some(d) if !d.finished() => {
                d.cancel.store(true, Ordering::Relaxed);
                d.cancelled.notify_waiters();
                crate::usage::EVENTS.bump();
                true
            }
            _ => false,
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
    let plan = tokio::select! {
        biased;
        _ = d.cancelled() => bail!("cancelled"),
        p = plan(c, cfg, spec) => p?,
    };
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
    if d.cancel.load(Ordering::Relaxed) { bail!("cancelled"); }
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
        biased;
        _ = d.cancelled() => Err(anyhow!("cancelled")),
        r = futures_util::future::try_join_all(futs) => r,
        _ = saver => unreachable!(),
    };
    save();
    res?;
    drop(file);

    if let Some(want) = sha256 {
        d.set(DlStatus::Verifying);
        let p = part.clone();
        let cancel = d.cancel.clone();
        let got = tokio::task::spawn_blocking(move || sha256_file_cancellable(&p, Some(&cancel))).await??;
        d.set(DlStatus::Downloading);
        if !got.eq_ignore_ascii_case(want) {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&state_path);
            d.done.fetch_sub(size, Ordering::Relaxed);
            bail!("sha256 mismatch (got {got}, want {want}); partial removed, pull again");
        }
    }
    if d.cancel.load(Ordering::Relaxed) { bail!("cancelled"); }
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

fn sha256_file_cancellable(p: &Path, cancel: Option<&AtomicBool>) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 8 << 20];
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(spec: &str) -> Arc<Download> {
        Arc::new(Download { name: Mutex::new(spec.into()), spec: spec.into(),
            total: AtomicU64::new(0), done: AtomicU64::new(0), status: Mutex::new(DlStatus::Downloading),
            file: Mutex::new(String::new()), cancel: Arc::new(AtomicBool::new(false)),
            cancelled: tokio::sync::Notify::new(), started: Instant::now() })
    }

    #[test]
    fn search_requires_all_words_deduplicates_and_prioritizes_exact_names() {
        let hit = |id: &str, downloads| RepoHit { id: id.into(), downloads, ..Default::default() };
        let mut hits = vec![hit("a/Qwen3-4B-4bit", 10), hit("a/Qwen3-8B", 100), hit("a/Qwen3-14B", 100), hit("a/Qwen3-8B-4bit", 100), hit("a/Qwen3-4B-4bit", 10)];
        rank_hits(&mut hits, "qwen 4b", "relevance");
        assert_eq!(hits.len(), 1);
        let mut hits = vec![hit("a/Qwen3-4B-4bit", 100), hit("b/Qwen3-4B", 1)];
        rank_hits(&mut hits, "Qwen3-4B", "relevance");
        assert_eq!(hits[0].id, "b/Qwen3-4B");
        rank_hits(&mut hits, "Qwen3-4B", "downloads");
        assert_eq!(hits[0].downloads, 100);
    }

    #[test]
    fn mlx_searches_flag_draft_and_non_chat_configs() {
        let cfg = |arch: &str, model_type: &str| RepoConfig { architectures: vec![arch.into()], model_type: Some(model_type.into()), model_file: None };
        let mut hits = vec![
            RepoHit { id: "o/draft".into(), config: Some(cfg("DFlash2DraftModel", "qwen3")), ..Default::default() },
            RepoHit { id: "o/classifier".into(), config: Some(cfg("Qwen3ForSequenceClassification", "qwen3")), ..Default::default() },
            RepoHit { id: "o/chat".into(), config: Some(cfg("Qwen3ForCausalLM", "qwen3")), ..Default::default() },
            RepoHit { id: "o/unknown".into(), ..Default::default() },
        ];
        mark_unsupported(&mut hits, "mlx");
        assert!(hits[0].unsupported.as_deref().unwrap().contains("draft"));
        assert!(hits[1].unsupported.is_some());
        assert!(hits[2].unsupported.is_none());
        assert!(hits[3].unsupported.is_none());
        // GGUF may target another runtime, so MLX rules do not apply.
        let mut gguf = vec![RepoHit { id: "o/draft".into(), config: Some(cfg("DFlash2DraftModel", "qwen3")), ..Default::default() }];
        mark_unsupported(&mut gguf, "gguf");
        assert!(gguf[0].unsupported.is_none());
    }

    #[test]
    fn discovery_filters_mixed_repo_by_format_and_variant_ram() {
        let cfg = Config::default();
        let file = |path: &str, size| RepoFile { path: path.into(), size, sha256: None, blob: String::new() };
        let files = vec![file("config.json", 128), file("model.safetensors", 4 << 30),
            file("model-Q4_K_M-00001-of-00002.gguf", 1 << 30),
            file("model-Q4_K_M-00002-of-00002.gguf", 1 << 30),
            file("model-Q8_0.gguf", 6 << 30), file("mmproj-f16.gguf", 512 << 20)];
        let mlx = discovery_choices(&cfg, "org/mixed", &files, "mlx", None);
        assert_eq!(mlx.len(), 1);
        assert_eq!(mlx[0].format, "mlx");
        let gguf = discovery_choices(&cfg, "org/mixed", &files, "gguf", None);
        assert_eq!(gguf.len(), 2);
        assert!(gguf.iter().all(|v| v.format == "gguf"));
        assert_eq!(gguf[0].size, 2 << 30); // All weight shards, without the projector.
        assert_eq!(discovery_choices(&cfg, "org/mixed", &files, "both", None).len(), 3);
        let budget = crate::compat::estimated_memory(&cfg, 2 << 30, "gguf");
        let fits = discovery_choices(&cfg, "org/mixed", &files, "both", Some(budget));
        assert_eq!(fits.len(), 1);
        assert!(fits[0].spec.ends_with(":Q4_K_M"));
        assert!(discovery_choices(&cfg, "org/mixed", &files, "both", Some(budget - 1)).is_empty());
        assert!(discovery_choices(&cfg, "org/no-weights", &[file("README.md", 100)], "both", None).is_empty());
    }

    #[test]
    fn discovery_normalizes_repo_urls_before_merging_formats() {
        for query in ["https://huggingface.co/org/model/tree/main", "https://hf.co/org/model/", "org/model:Q4_K_M"] {
            let mut hits = vec![RepoHit { id: "org/model".into(), ..Default::default() }; 2];
            rank_hits(&mut hits, search_query(query), "relevance");
            assert_eq!(hits.len(), 1);
        }
    }

    #[tokio::test]
    async fn cancellation_wakes_all_waiters_and_ignores_completed_downloads() {
        let downloads = Downloads::default();
        let d = download("org/model");
        downloads.map.lock().unwrap().insert(d.spec.clone(), d.clone());
        let a = d.clone(); let b = d.clone();
        let one = tokio::spawn(async move { a.cancelled().await });
        let two = tokio::spawn(async move { b.cancelled().await });
        tokio::task::yield_now().await;
        assert!(downloads.cancel("org/model"));
        tokio::time::timeout(Duration::from_secs(1), async { one.await.unwrap(); two.await.unwrap(); }).await.unwrap();
        d.set(DlStatus::Done);
        assert!(!downloads.cancel("org/model"));
        assert!(!downloads.cancel("org/missing"));
    }

    #[tokio::test]
    async fn stalled_download_cancels_promptly_and_resumes_from_saved_offset() {
        use axum::{body::Body, http::{HeaderMap, StatusCode}, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let size = 5 << 20;
        let app = Router::new().route("/file", get(move |headers: HeaderMap| async move {
            let range = headers["range"].to_str().unwrap();
            let start: usize = range.trim_start_matches("bytes=").split('-').next().unwrap().parse().unwrap();
            if start == 0 {
                let stream = futures_util::stream::once(async { Ok::<_, std::io::Error>(bytes::Bytes::from(vec![7u8; 3 << 20])) })
                    .chain(futures_util::stream::pending());
                (StatusCode::PARTIAL_CONTENT, Body::from_stream(stream))
            } else {
                (StatusCode::PARTIAL_CONTENT, Body::from(vec![7u8; size - start]))
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = std::env::temp_dir().join(format!("lmdeck-cancel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blob = dir.join("blob");
        let downloads = Downloads::default();
        let d = download("org/model");
        downloads.map.lock().unwrap().insert(d.spec.clone(), d.clone());
        let dd = d.clone(); let path = blob.clone();
        let url = format!("http://{addr}/file"); let first_url = url.clone();
        let task = tokio::spawn(async move { fetch_file(&client(), &first_url, &path, size as u64, None, 1, &dd).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while d.done.load(Ordering::Relaxed) == 0 { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        downloads.cancel("org/model");
        assert!(tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap().is_err());
        assert!(!blob.exists());
        let state: SegState = serde_json::from_slice(&std::fs::read(dir.join("blob.incomplete.lmdeck")).unwrap()).unwrap();
        assert!(state.segs[0][2] > 0 && state.segs[0][2] < size as u64);
        let resumed = download("org/model");
        fetch_file(&client(), &url, &blob, size as u64, None, 1, &resumed).await.unwrap();
        assert_eq!(std::fs::read(&blob).unwrap(), vec![7u8; size]);
        assert_eq!(resumed.done.load(Ordering::Relaxed), size as u64);
        server.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
