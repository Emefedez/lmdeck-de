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
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Mlx => "mlx",
            Format::Gguf => "gguf",
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
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
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
    let _tensors = rd_u64(&mut r)?;
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
            _ => skip_val(&mut r, ty)?,
        }
    }
    m.ctx_len = ctx_by_key
        .iter()
        .find(|(k, _)| k.starts_with(&format!("{}.", m.arch)))
        .or(ctx_by_key.first())
        .map(|(_, v)| *v);
    Some(m)
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
