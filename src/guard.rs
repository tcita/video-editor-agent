//! The last gate between the model and ffmpeg:
//! - Only arguments of a known shape get through (each tool tightens this
//!   further with `deny_unknown_fields`)
//! - Numbers must be finite and in range
//! - Ids may only contain `[A-Za-z0-9_]`
//! - Output is always `<workdir>/_out/<sanitized-name>.mp4`; no path is accepted
//!
//! Nothing here evaluates a command or an expression. That is what makes "no
//! code written online" a structural guarantee rather than a policy.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::invalid;

/// Step budget per user turn (one step = one model round trip, which may carry
/// several parallel tool calls).
///
/// Modelled on opencode's `steps`: configurable, with a default. An env var
/// rather than a config file because this project has no config layer yet. Must
/// be >= 2 so there is at least one execution round plus one text-only round.
pub fn max_tool_steps() -> usize {
    const DEFAULT: usize = 12;
    std::env::var("VIDEO_AGENT_MAX_STEPS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n >= 2)
        .unwrap_or(DEFAULT)
}

pub const MAX_CLIPS: usize = 50;
pub const MAX_ARGS_BYTES: usize = 8 * 1024;
pub const MAX_RES: u32 = 7680;

pub fn check_time(name: &str, v: f64) -> Result<()> {
    if !v.is_finite() || v < 0.0 {
        return Err(invalid(format!("{name} 须为非负有限数字（实际为 {v}）")));
    }
    Ok(())
}

/// Volume gain in dB. Below -60dB is effectively inaudible; use mute instead.
pub fn check_volume(name: &str, v: f64) -> Result<()> {
    if !v.is_finite() || !(-60.0..=20.0).contains(&v) {
        return Err(invalid(format!(
            "{name} 须为 -60 至 +20 之间的有限数字（单位 dB，实际为 {v}）"
        )));
    }
    Ok(())
}

pub fn check_id(name: &str, v: &str) -> Result<()> {
    let ok =
        !v.is_empty() && v.len() <= 40 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !ok {
        return Err(invalid(format!(
            "{name} 不合法（仅允许字母、数字与下划线，长度为 1..=40）"
        )));
    }
    Ok(())
}

pub fn parse_resolution(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .with_context(|| format!("分辨率格式须为 宽x高（例如 1920x1080），实际为 {s}"))?;
    let w: u32 = w
        .trim()
        .parse()
        .with_context(|| format!("分辨率宽度不合法: {s}"))?;
    let h: u32 = h
        .trim()
        .parse()
        .with_context(|| format!("分辨率高度不合法: {s}"))?;
    if !(16..=MAX_RES).contains(&w) || !(16..=MAX_RES).contains(&h) {
        return Err(invalid(format!("分辨率超出允许范围 16..={MAX_RES}")));
    }
    Ok((w & !1, h & !1))
}

fn sanitize_stem(raw: &str) -> Result<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let stem = match base.rsplit_once('.') {
        Some((a, _)) if !a.is_empty() => a,
        _ => base,
    };
    let cleaned: String = stem
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        return Err(invalid("输出文件名不合法（清理后为空）"));
    }
    Ok(cleaned.chars().take(60).collect())
}

/// Build an output path that never overwrites an existing file. Rust owns the
/// path; the model only supplies a name. Computes the path without creating any
/// directory, so the preview step has no side effects.
pub fn output_path(outdir: &Path, raw_name: Option<&str>) -> Result<PathBuf> {
    output_path_with_ext(outdir, raw_name, "mp4")
}

/// Variant of [`output_path`] with an explicit extension (e.g. audio-only
/// outputs). The extension is sanitized to ASCII alphanumerics.
pub fn output_path_with_ext(outdir: &Path, raw_name: Option<&str>, ext: &str) -> Result<PathBuf> {
    let ext = {
        let e: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        if e.is_empty() {
            "bin".to_string()
        } else {
            e.to_lowercase()
        }
    };
    let stem = match raw_name {
        Some(n) => sanitize_stem(n)?,
        None => format!(
            "edit_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
    };
    for i in 0..100 {
        let name = if i == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}_{i}.{ext}")
        };
        let candidate = outdir.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(crate::error::fatal("同名输出文件过多，请更换名称后重试"))
}
