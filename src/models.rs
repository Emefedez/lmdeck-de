//! Local model discovery: Hugging Face hub cache (MLX safetensors + GGUF) and
//! loose GGUF directories. Also a minimal GGUF header reader for metadata.

use crate::config::Config;
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Mlx,
    Gguf,
    /// A Strata run config (strata-<model>.json) served by Strata's own server.
    Strata,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Mlx => "mlx",
            Format::Gguf => "gguf",
            Format::Strata => "strata",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalModel {
    /// Canonical name: `org/repo` (MLX), `org/repo:TAG` (GGUF), `local/stem` (loose GGUF).
    pub name: String,
    pub format: Format,
    pub repo: Option<String>,
    /// MLX: snapshot dir. GGUF: first (or only) shard.
    pub path: PathBuf,
    pub files: Vec<PathBuf>,
    pub size: u64,
    pub quant: String,
    /// MLX quantization mode from config.json (affine, nvfp4, mxfp4, …).
    pub qmode: Option<String>,
    pub arch: String,
    /// Why this checkpoint cannot be loaded as a standalone chat model (MLX only).
    pub unsupported: Option<String>,
    pub params: String,
    pub ctx_len: Option<u64>,
    pub mmproj: Option<PathBuf>,
    #[serde(skip)]
    pub modified: SystemTime,
    pub tools: bool,
    pub thinking: bool,
    pub vision: bool,
}

impl LocalModel {
    pub fn digest(&self) -> String {
        // Stable pseudo-digest (Ollama clients display/compare it, nothing verifies it).
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.name.as_bytes());
        h.update(self.size.to_le_bytes());
        let out = h.finalize();
        out.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn family(&self) -> String {
        if self.arch.is_empty() { "unknown".into() } else { self.arch.clone() }
    }

    pub fn capabilities(&self) -> Vec<&'static str> {
        let mut c = vec!["completion"];
        if self.tools {
            c.push("tools");
        }
        if self.thinking {
            c.push("thinking");
        }
        if self.vision {
            c.push("vision");
        }
        if self.name.to_lowercase().contains("embed") {
            c = vec!["embedding"];
        }
        c
    }
}

pub fn scan(cfg: &Config) -> Vec<LocalModel> {
    let mut out = Vec::new();
    let cache = cfg.hf_cache();
    if let Ok(rd) = std::fs::read_dir(&cache) {
        for e in rd.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            let Some(rest) = fname.strip_prefix("models--") else { continue };
            let repo = rest.replacen("--", "/", 1);
            if let Some(snap) = snapshot_dir(&e.path()) {
                scan_snapshot(&repo, &snap, &mut out);
            }
        }
    }
    for d in &cfg.models.extra_dirs {
        scan_loose(&crate::config::expand(d), &mut out);
    }
    scan_strata(cfg, &mut out);
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Strata models: the run configs `strata-<model>.json` that Strata's setup writes into its
/// install dir. With `strata.engine = mock`, also `strata/mock` (the scripted test engine).
fn scan_strata(cfg: &Config, out: &mut Vec<LocalModel>) {
    let dir = cfg.strata.dir();
    let mut configs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let n = fname(p);
                    n.starts_with("strata-") && n.ends_with(".json") && n != "strata-mock.json"
                })
                .collect()
        })
        .unwrap_or_default();
    if cfg.strata.engine == "mock" {
        let p = dir.join("strata-mock.json");
        if !p.exists() && dir.join("serve/server.py").exists() {
            let _ = std::fs::write(&p, "{\"model_name\": \"qwen3.8-flash-next-mock\"}\n");
        }
        if p.exists() {
            configs.push(p);
        }
    }
    for p in configs {
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let stem = fname(&p).trim_start_matches("strata-").trim_end_matches(".json").to_string();
        let args: Vec<String> = v["args"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let arg = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
        // The model's big files: the GGUF(s) the engine maps and the experts' streaming copy.
        let mut files: Vec<PathBuf> = ["--native", "--ple-gguf"].iter().filter_map(|f| arg(f)).map(PathBuf::from).filter(|f| f.is_file()).collect();
        let experts: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(format!("{}.experts", f.display()))).filter(|f| f.is_file()).collect();
        files.extend(experts);
        let size = files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
        files.push(p.clone());
        let quant = stem.rsplit('-').next().unwrap_or("").to_uppercase();
        out.push(LocalModel {
            name: format!("strata/{stem}"),
            format: Format::Strata,
            repo: None,
            path: p.clone(),
            files,
            size,
            quant: if stem == "mock" { "mock".into() } else { quant },
            qmode: None,
            arch: "qwen3.8-flash-next".into(),
            unsupported: None,
            params: "125B".into(),
            ctx_len: arg("--max-context").and_then(|c| c.parse().ok()).or(Some(32768)),
            mmproj: None,
            modified: p.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH),
            tools: true,
            thinking: true,
            vision: false,
        });
    }
}

/// refs/main -> snapshots/<sha>; else most recently modified snapshot.
pub fn snapshot_dir(repo_dir: &Path) -> Option<PathBuf> {
    if let Ok(r) = std::fs::read_to_string(repo_dir.join("refs/main")) {
        let p = repo_dir.join("snapshots").join(r.trim());
        if p.is_dir() {
            return Some(p);
        }
    }
    std::fs::read_dir(repo_dir.join("snapshots"))
        .ok()?
        .flatten()
        .filter(|e| e.path().is_dir())
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH))
        .map(|e| e.path())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            // file_type() does not follow symlinks; HF snapshots are symlinks to blobs.
            match e.file_type() {
                Ok(t) if t.is_dir() => walk(&p, out),
                _ => out.push(p),
            }
        }
    }
}

fn file_size(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok().map(|m| m.len()) // follows symlink; None if dangling
}

fn scan_snapshot(repo: &str, snap: &Path, out: &mut Vec<LocalModel>) {
    let mut files = Vec::new();
    walk(snap, &mut files);
    let modified = std::fs::metadata(snap).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);

    let ggufs: Vec<&PathBuf> = files.iter().filter(|p| ext_is(p, "gguf")).collect();
    let has_safetensors = files.iter().any(|p| ext_is(p, "safetensors"));

    if !ggufs.is_empty() {
        let mmproj = ggufs
            .iter()
            .find(|p| fname(p).to_lowercase().starts_with("mmproj") && file_size(p).is_some())
            .map(|p| p.to_path_buf());
        let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for g in &ggufs {
            let n = fname(g);
            if n.to_lowercase().starts_with("mmproj") {
                continue;
            }
            groups.entry(split_key(&n)).or_default().push(g.to_path_buf());
        }
        for (key, mut parts) in groups {
            parts.sort();
            let sizes: Option<Vec<u64>> = parts.iter().map(|p| file_size(p)).collect();
            let Some(sizes) = sizes else { continue }; // incomplete download
            if is_split_incomplete(&parts) {
                continue;
            }
            let tag = quant_tag(&key);
            let meta = gguf_meta(&parts[0]).unwrap_or_default();
            let unsupported = gguf_unsupported(&meta);
            out.push(LocalModel {
                name: format!("{repo}:{tag}"),
                format: Format::Gguf,
                repo: Some(repo.to_string()),
                path: parts[0].clone(),
                size: sizes.iter().sum(),
                files: parts,
                quant: tag.clone(),
                qmode: None,
                arch: meta.arch,
                unsupported,
                params: if meta.size_label.is_empty() { params_from_name(repo) } else { meta.size_label },
                ctx_len: meta.ctx_len,
                vision: mmproj.is_some(),
                mmproj: mmproj.clone(),
                modified,
                tools: meta.tools,
                thinking: meta.thinking,
            });
        }
    }

    if has_safetensors && snap.join("config.json").exists() {
        let weights: Vec<&PathBuf> = files.iter().filter(|p| ext_is(p, "safetensors")).collect();
        if weights.iter().any(|p| file_size(p).is_none()) {
            return; // still downloading
        }
        let cfg: serde_json::Value = std::fs::read_to_string(snap.join("config.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let q = cfg.get("quantization").or_else(|| cfg.get("quantization_config"));
        let quant = match q.and_then(|q| q.get("bits")).and_then(|b| b.as_u64()) {
            Some(b) => format!("{b}bit"),
            None => q
                .and_then(|q| q.get("mode").or_else(|| q.get("quant_method")))
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| cfg.get("torch_dtype").and_then(|d| d.as_str()).unwrap_or("bf16").to_string()),
        };
        let text = cfg.get("text_config").unwrap_or(&cfg);
        let ctx_len = text
            .get("max_position_embeddings")
            .or_else(|| cfg.get("max_position_embeddings"))
            .and_then(|v| v.as_u64());
        let template = read_template(snap);
        let architectures: Vec<String> = cfg
            .get("architectures")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let unsupported = crate::compat::mlx_unsupported(
            cfg.get("model_type").and_then(|v| v.as_str()).unwrap_or(""),
            &architectures,
            cfg.get("model_file").and_then(|v| v.as_str()),
        );
        out.push(LocalModel {
            name: repo.to_string(),
            format: Format::Mlx,
            repo: Some(repo.to_string()),
            path: snap.to_path_buf(),
            size: files.iter().filter_map(|p| file_size(p)).sum(),
            files,
            qmode: q.and_then(|q| q.get("mode")).and_then(|m| m.as_str()).map(str::to_string).or_else(|| q.map(|_| "affine".to_string())),
            quant,
            arch: cfg.get("model_type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            unsupported,
            params: params_from_name(repo),
            ctx_len,
            mmproj: None,
            modified,
            tools: template.contains("tools"),
            thinking: template.contains("think"),
            vision: cfg.get("vision_config").is_some(),
        });
    }
}

fn read_template(snap: &Path) -> String {
    if let Ok(s) = std::fs::read_to_string(snap.join("chat_template.jinja")) {
        return s;
    }
    std::fs::read_to_string(snap.join("tokenizer_config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("chat_template").map(|t| t.to_string()))
        .unwrap_or_default()
}

fn scan_loose(dir: &Path, out: &mut Vec<LocalModel>) {
    let mut files = Vec::new();
    walk(dir, &mut files);
    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for f in files.iter().filter(|p| ext_is(p, "gguf")) {
        let n = fname(f);
        if n.to_lowercase().starts_with("mmproj") {
            continue;
        }
        groups.entry(split_key(&n)).or_default().push(f.clone());
    }
    for (key, mut parts) in groups {
        parts.sort();
        if is_split_incomplete(&parts) {
            continue;
        }
        let stem = key.trim_end_matches(".gguf").to_string();
        let meta = gguf_meta(&parts[0]).unwrap_or_default();
        let unsupported = gguf_unsupported(&meta);
        let mmproj = parts[0].parent().and_then(|d| {
            std::fs::read_dir(d).ok()?.flatten().map(|e| e.path()).find(|p| {
                ext_is(p, "gguf") && fname(p).to_lowercase().starts_with("mmproj")
            })
        });
        out.push(LocalModel {
            name: format!("local/{stem}"),
            format: Format::Gguf,
            repo: None,
            path: parts[0].clone(),
            size: parts.iter().filter_map(|p| file_size(p)).sum(),
            modified: std::fs::metadata(&parts[0]).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH),
            files: parts,
            quant: quant_tag(&key),
            qmode: None,
            arch: meta.arch,
            unsupported,
            params: if meta.size_label.is_empty() { params_from_name(&stem) } else { meta.size_label },
            ctx_len: meta.ctx_len,
            vision: mmproj.is_some(),
            mmproj,
            tools: meta.tools,
            thinking: meta.thinking,
        });
    }
}

fn ext_is(p: &Path, ext: &str) -> bool {
    p.extension().map(|e| e.eq_ignore_ascii_case(ext)).unwrap_or(false)
}

fn fname(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// "foo-Q4_K_M-00001-of-00003.gguf" -> "foo-Q4_K_M.gguf"
pub fn split_key(name: &str) -> String {
    let stem = name.trim_end_matches(".gguf");
    if let Some(i) = stem.rfind("-of-") {
        let (a, b) = (&stem[..i], &stem[i + 4..]);
        if b.len() == 5 && b.chars().all(|c| c.is_ascii_digit()) {
            if let Some(j) = a.rfind('-') {
                if a[j + 1..].chars().all(|c| c.is_ascii_digit()) {
                    return format!("{}.gguf", &a[..j]);
                }
            }
        }
    }
    name.to_string()
}

fn is_split_incomplete(parts: &[PathBuf]) -> bool {
    let n = fname(&parts[0]);
    let stem = n.trim_end_matches(".gguf");
    if let Some(i) = stem.rfind("-of-") {
        if let Ok(total) = stem[i + 4..].parse::<usize>() {
            return parts.iter().filter(|p| file_size(p).is_some()).count() != total;
        }
    }
    false
}

/// Extract a quant tag like Q4_K_M, IQ3_XXS, UD-Q4_K_XL, F16, BF16, MXFP4 from a gguf name.
pub fn quant_tag(name: &str) -> String {
    let stem = name.trim_end_matches(".gguf");
    let parts: Vec<&str> = stem.split(['-', '.']).collect();
    for (i, p) in parts.iter().enumerate().rev() {
        let u = p.to_uppercase();
        let is_q = (u.starts_with('Q') || u.starts_with("IQ") || u.starts_with("TQ"))
            && u.chars().nth(if u.starts_with('Q') { 1 } else { 2 }).map(|c| c.is_ascii_digit()).unwrap_or(false);
        let is_f = matches!(u.as_str(), "F16" | "BF16" | "F32" | "FP16" | "FP8" | "MXFP4" | "NVFP4");
        if is_q || is_f {
            if i > 0 && parts[i - 1].eq_ignore_ascii_case("UD") {
                return format!("UD-{u}");
            }
            return u;
        }
    }
    stem.to_string()
}

/// "Qwen3.5-9B-4bit" -> "9B"; "Qwen3-30B-A3B" -> "30B-A3B"
pub fn params_from_name(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let parts: Vec<&str> = base.split(['-', '_']).collect();
    let is_size = |p: &str| {
        let u = p.to_uppercase();
        (u.ends_with('B') || u.ends_with('M'))
            && u.len() > 1
            && u[..u.len() - 1].chars().all(|c| c.is_ascii_digit() || c == '.')
            && u.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
    };
    for (i, p) in parts.iter().enumerate() {
        if is_size(p) {
            let mut s = p.to_uppercase();
            if let Some(n) = parts.get(i + 1) {
                let nu = n.to_uppercase();
                if nu.starts_with('A') && is_size(&nu[1..]) {
                    s = format!("{s}-{nu}");
                }
            }
            return s;
        }
    }
    String::new()
}

// ---------- GGUF header ----------

#[derive(Debug, Default, Clone)]
pub struct GgufMeta {
    pub arch: String,
    pub size_label: String,
    pub ctx_len: Option<u64>,
    pub tools: bool,
    pub thinking: bool,
    /// `general.file_type` (LLAMA_FTYPE_*); > 42 means a fork, not upstream llama.cpp.
    pub file_type: Option<u32>,
    /// The highest `ggml_type` among the tensors.
    pub max_type: Option<i32>,
    /// The file carries `prism.*` metadata (Prism's hadamard-rotated ternary, "PQ2_0").
    pub prism: bool,
}

/// The highest `ggml_type` the bundled llama.cpp knows: build c35b667 (2026-10-08) has
/// `GGML_TYPE_COUNT = 43`, so 0..=42 including TQ1_0 (34) and TQ2_0 (35). Raise it when
/// `llama.git_ref` moves to a build that adds types.
pub const KNOWN_GGML_TYPES: i32 = 42;

/// Why a GGUF cannot be loaded by this llama.cpp, if it cannot. The file's own metadata says
/// it: a type above `KNOWN_GGML_TYPES`, or Prism's `prism.hadamard.*` block, whose weights are
/// rotated before ternary quantization and need Prism's runtime to undo.
pub fn gguf_unsupported(m: &GgufMeta) -> Option<String> {
    let custom = m.max_type.filter(|t| *t > KNOWN_GGML_TYPES);
    let file_type = m.file_type.map(|f| f.to_string()).unwrap_or_else(|| "?".into());
    if m.prism {
        let ty = match custom {
            Some(t) => format!("custom ggml type {t}"),
            None => "a custom ggml type".into(),
        };
        return Some(format!(
            "Prism rotated ternary ({ty}, file type {file_type}): the weights are hadamard-rotated before quantization, \
             so llama.cpp cannot read them (it knows types 0-{KNOWN_GGML_TYPES}). Needs Prism's runtime, or a standard \
             TQ1_0/TQ2_0 build"
        ));
    }
    custom.map(|t| {
        format!(
            "custom ggml tensor type {t} (file type {file_type}): this llama.cpp build knows types 0-{KNOWN_GGML_TYPES}; \
             rebuild with a newer `llama.git_ref` if upstream added it"
        )
    })
}

pub fn gguf_meta(path: &Path) -> Option<GgufMeta> {
    let f = std::fs::File::open(path).ok()?;
    let mut r = BufReader::with_capacity(1 << 16, f);
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).ok()?;
    if &magic != b"GGUF" {
        return None;
    }
    let _version = rd_u32(&mut r)?;
    let tensors = rd_u64(&mut r)?;
    let kvs = rd_u64(&mut r)?;
    let mut m = GgufMeta::default();
    let mut ctx_by_key: Vec<(String, u64)> = Vec::new();
    for _ in 0..kvs.min(100_000) {
        let key = rd_str(&mut r)?;
        let ty = rd_u32(&mut r)?;
        match key.as_str() {
            "general.architecture" if ty == 8 => m.arch = rd_str(&mut r)?,
            "general.size_label" if ty == 8 => m.size_label = rd_str(&mut r)?,
            "tokenizer.chat_template" if ty == 8 => {
                let t = rd_str(&mut r)?;
                m.tools = t.contains("tools");
                m.thinking = t.contains("think");
            }
            k if k.ends_with(".context_length") => {
                if let Some(v) = rd_int(&mut r, ty)? {
                    ctx_by_key.push((k.to_string(), v));
                }
            }
            "general.file_type" => {
                if let Some(Some(v)) = rd_int(&mut r, ty) {
                    m.file_type = Some(v as u32);
                }
            }
            k if k.starts_with("prism.") => {
                m.prism = true;
                skip_val(&mut r, ty)?;
            }
            _ => skip_val(&mut r, ty)?,
        }
    }
    m.ctx_len = ctx_by_key
        .iter()
        .find(|(k, _)| k.starts_with(&format!("{}.", m.arch)))
        .or(ctx_by_key.first())
        .map(|(_, v)| *v);
    m.max_type = gguf_tensor_types(&mut r, tensors);
    Some(m)
}

/// The highest `ggml_type` in the file's tensor infos, right after the metadata block.
/// `None` when the header cannot be read that far (the string/quant checks still work).
fn gguf_tensor_types<R: Read + Seek>(r: &mut BufReader<R>, tensors: u64) -> Option<i32> {
    let mut max = 0i32;
    for _ in 0..tensors.min(1_000_000) {
        let _name = rd_str(r)?;
        let dims = rd_u32(r)?;
        if dims > 8 {
            return None;
        }
        r.seek_relative(i64::from(dims) * 8).ok()?;
        let ty = rd_u32(r)? as i32;
        let _offset = rd_u64(r)?;
        max = max.max(ty);
    }
    Some(max)
}

fn rd_u32<R: Read>(r: &mut R) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b))
}
fn rd_u64<R: Read>(r: &mut R) -> Option<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).ok()?;
    Some(u64::from_le_bytes(b))
}
fn rd_str<R: Read>(r: &mut R) -> Option<String> {
    let n = rd_u64(r)? as usize;
    if n > 64 << 20 {
        return None;
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).ok()?;
    Some(String::from_utf8_lossy(&b).into_owned())
}
fn scalar_size(ty: u32) -> Option<i64> {
    Some(match ty {
        0 | 1 | 7 => 1,
        2 | 3 => 2,
        4 | 5 | 6 => 4,
        10 | 11 | 12 => 8,
        _ => return None,
    })
}
fn rd_int<R: Read + Seek>(r: &mut BufReader<R>, ty: u32) -> Option<Option<u64>> {
    Some(match ty {
        4 => Some(rd_u32(r)? as u64),
        5 => Some(rd_u32(r)? as i32 as u64),
        10 | 11 => Some(rd_u64(r)?),
        _ => {
            skip_val(r, ty)?;
            None
        }
    })
}
fn skip_val<R: Read + Seek>(r: &mut BufReader<R>, ty: u32) -> Option<()> {
    if let Some(n) = scalar_size(ty) {
        return r.seek_relative(n).ok();
    }
    match ty {
        8 => {
            let n = rd_u64(r)? as i64;
            r.seek_relative(n).ok()
        }
        9 => {
            let ety = rd_u32(r)?;
            let len = rd_u64(r)?;
            if let Some(n) = scalar_size(ety) {
                r.seek_relative(n * len as i64).ok()
            } else {
                for _ in 0..len {
                    skip_val(r, ety)?;
                }
                Some(())
            }
        }
        _ => None,
    }
}

// ---------- name resolution ----------

pub fn normalize(name: &str) -> String {
    let n = name.trim();
    let n = n
        .strip_prefix("https://")
        .unwrap_or(n)
        .trim_start_matches("hf.co/")
        .trim_start_matches("huggingface.co/");
    n.strip_suffix(":latest").unwrap_or(n).to_string()
}

pub fn resolve<'a>(name: &str, models: &'a [LocalModel], cfg: &Config) -> Option<&'a LocalModel> {
    let mut n = normalize(name);
    if let Some(a) = cfg.aliases.get(&n) {
        n = normalize(a);
    }
    let lower = n.to_lowercase();
    if let Some(m) = models.iter().find(|m| m.name.to_lowercase() == lower) {
        return Some(m);
    }
    let (base, tag) = match lower.rsplit_once(':') {
        Some((b, t)) => (b.to_string(), Some(t.to_string())),
        None => (lower.clone(), None),
    };
    let repo_matches = |m: &LocalModel| {
        let r = m.repo.as_deref().unwrap_or(&m.name).to_lowercase();
        r == base || r.rsplit('/').next() == Some(base.as_str()) || m.name.to_lowercase().split(':').next() == Some(base.as_str())
    };
    let cands: Vec<&LocalModel> = models
        .iter()
        .filter(|m| repo_matches(m))
        .filter(|m| match &tag {
            Some(t) => m.quant.to_lowercase() == *t || m.quant.to_lowercase().contains(t.as_str()),
            None => true,
        })
        .collect();
    if cands.len() == 1 {
        return Some(cands[0]);
    }
    // Several quants and no tag: prefer MLX, then the configured default quant.
    if tag.is_none() && !cands.is_empty() {
        if let Some(m) = cands.iter().find(|m| m.format == Format::Mlx) {
            return Some(m);
        }
        let dq = cfg.download.gguf_default_quant.to_lowercase();
        return cands.iter().find(|m| m.quant.to_lowercase() == dq).copied().or(Some(cands[0]));
    }
    None
}

/// Delete a model's files from the HF cache (or a loose gguf).
pub fn delete(m: &LocalModel, cfg: &Config) -> std::io::Result<()> {
    let cache = cfg.hf_cache();
    let repo_dir = m.repo.as_ref().map(|r| cache.join(format!("models--{}", r.replace('/', "--"))));
    match (m.format, &repo_dir) {
        (Format::Mlx, Some(dir)) => {
            // Keep GGUFs that may live in the same repo.
            let has_gguf = m.files.iter().any(|p| ext_is(p, "gguf"));
            if !has_gguf {
                return std::fs::remove_dir_all(dir);
            }
            for f in m.files.iter().filter(|p| !ext_is(p, "gguf")) {
                remove_link_and_blob(f);
            }
            Ok(())
        }
        (Format::Gguf, Some(dir)) => {
            for f in &m.files {
                remove_link_and_blob(f);
            }
            // If no gguf/safetensors remain, drop the whole repo dir.
            let mut rest = Vec::new();
            walk(&dir.join("snapshots"), &mut rest);
            if !rest.iter().any(|p| ext_is(p, "gguf") && !fname(p).to_lowercase().starts_with("mmproj") || ext_is(p, "safetensors")) {
                std::fs::remove_dir_all(dir)?;
            }
            Ok(())
        }
        _ => {
            for f in &m.files {
                std::fs::remove_file(f)?;
            }
            Ok(())
        }
    }
}

fn remove_link_and_blob(link: &Path) {
    if let Ok(target) = std::fs::canonicalize(link) {
        if target != link {
            let _ = std::fs::remove_file(&target);
        }
    }
    let _ = std::fs::remove_file(link);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quant_tags() {
        assert_eq!(quant_tag("Qwen3-8B-Q4_K_M.gguf"), "Q4_K_M");
        assert_eq!(quant_tag("gemma-3-12b-it-UD-Q4_K_XL.gguf"), "UD-Q4_K_XL");
        assert_eq!(quant_tag("model.IQ3_XXS.gguf"), "IQ3_XXS");
        assert_eq!(quant_tag("llama-bf16.gguf"), "BF16");
        assert_eq!(quant_tag("weird.gguf"), "weird");
    }

    #[test]
    fn splits() {
        assert_eq!(split_key("big-Q8_0-00001-of-00003.gguf"), "big-Q8_0.gguf");
        assert_eq!(split_key("small-Q8_0.gguf"), "small-Q8_0.gguf");
    }

    fn put_str(b: &mut Vec<u8>, s: &str) {
        b.extend_from_slice(&(s.len() as u64).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    }

    /// One tensor of `tensor_type`, plus Prism's metadata when `prism`.
    fn synthetic_gguf(file_type: u32, tensor_type: i32, prism: bool) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&(if prism { 3u64 } else { 2u64 }).to_le_bytes());
        put_str(&mut b, "general.architecture");
        b.extend_from_slice(&8u32.to_le_bytes());
        put_str(&mut b, "qwen35");
        put_str(&mut b, "general.file_type");
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&file_type.to_le_bytes());
        if prism {
            put_str(&mut b, "prism.hadamard.version");
            b.extend_from_slice(&4u32.to_le_bytes());
            b.extend_from_slice(&1u32.to_le_bytes());
        }
        put_str(&mut b, "output.weight");
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&16u64.to_le_bytes());
        b.extend_from_slice(&16u64.to_le_bytes());
        b.extend_from_slice(&tensor_type.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b
    }

    fn meta_of(bytes: Vec<u8>, tag: &str) -> GgufMeta {
        let dir = std::env::temp_dir().join(format!("lmdeck-gguf-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.gguf");
        std::fs::write(&p, bytes).unwrap();
        let m = gguf_meta(&p).expect("header parses");
        std::fs::remove_dir_all(&dir).ok();
        m
    }

    #[test]
    fn prism_rotated_ternary_is_flagged() {
        // The Huihui PQ2_0 file: file type 141, 348 tensors as ggml type 142, prism.hadamard.* keys.
        let m = meta_of(synthetic_gguf(141, 142, true), "prism");
        assert_eq!(m.arch, "qwen35");
        assert_eq!(m.file_type, Some(141));
        assert_eq!(m.max_type, Some(142));
        assert!(m.prism);
        let why = gguf_unsupported(&m).expect("flagged");
        assert!(why.contains("Prism") && why.contains("142"), "{why}");
    }

    #[test]
    fn standard_ternary_gguf_is_supported() {
        // llama.cpp's own ternary type TQ2_0 = 35, file type 36.
        let m = meta_of(synthetic_gguf(36, 35, false), "tq2");
        assert_eq!(m.max_type, Some(35));
        assert!(!m.prism);
        assert!(gguf_unsupported(&m).is_none());
    }

    #[test]
    fn unknown_custom_type_is_flagged() {
        let m = meta_of(synthetic_gguf(141, 142, false), "custom");
        let why = gguf_unsupported(&m).expect("flagged");
        assert!(why.contains("142") && why.contains("llama.cpp"), "{why}");
    }

    #[test]
    fn params() {
        assert_eq!(params_from_name("mlx-community/Qwen3.5-9B-4bit"), "9B");
        assert_eq!(params_from_name("Qwen/Qwen3-30B-A3B-GGUF"), "30B-A3B");
        assert_eq!(params_from_name("x/no-size"), "");
    }

    #[test]
    fn durations() {
        use crate::config::parse_duration;
        assert_eq!(parse_duration("5m").unwrap().as_secs(), 300);
        assert_eq!(parse_duration("30").unwrap().as_secs(), 30);
        assert!(parse_duration("-1").is_none());
        assert_eq!(parse_duration("0").unwrap().as_secs(), 0);
    }
}
