use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::error::invalid;
use crate::guard::{self, MAX_CLIPS};

#[derive(Debug, Clone)]
pub struct Metadata {
    pub duration_sec: f64,
    pub width: i64,
    pub height: i64,
    pub fps: f64,
    pub video_codec: String,
    pub has_audio: bool,
}

#[derive(Debug, Deserialize)]
struct FfprobeOut {
    format: Option<Format>,
    streams: Vec<Stream>,
}

#[derive(Debug, Deserialize)]
struct Format {
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Stream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    avg_frame_rate: Option<String>,
    duration: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub id: String,
    pub path: PathBuf,
    pub file: String,
    pub meta: Metadata,
}

#[derive(Debug, Clone, Serialize)]
pub struct Clip {
    pub id: String,
    pub asset_id: String,
    pub start_sec: f64,
    pub end_sec: f64,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub volume_db: f64,
}

impl Clip {
    pub fn duration(&self) -> f64 {
        (self.end_sec - self.start_sec).max(0.0)
    }

    /// ffmpeg `volume` filter fragment. Default (unmuted, 0dB) returns None so
    /// callers skip the filter entirely.
    pub fn audio_volume_filter(&self) -> Option<String> {
        if self.muted {
            Some("volume=0".to_string())
        } else if self.volume_db.abs() < f64::EPSILON {
            None
        } else {
            Some(format!("volume={:.2}dB", self.volume_db))
        }
    }

    pub fn audio_label(&self) -> String {
        if self.muted {
            "（静音）".to_string()
        } else if self.volume_db.abs() < f64::EPSILON {
            String::new()
        } else {
            format!("（{:+.1}dB）", self.volume_db)
        }
    }
}

pub struct Session {
    pub outdir: PathBuf,
    pub assets: HashMap<String, Asset>,
    pub clips: HashMap<String, Clip>,
    /// Playback order. `cut` appends, `concat` may reorder.
    pub timeline: Vec<String>,
    pub outputs: Vec<PathBuf>,
    next_asset: u64,
    next_clip: u64,
}

pub fn ffmpeg_bin() -> PathBuf {
    bundled_or("ffmpeg.exe", "ffmpeg")
}

pub fn ffprobe_bin() -> PathBuf {
    bundled_or("ffprobe.exe", "ffprobe")
}

fn bundled_or(exe_name: &str, fallback: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(exe_name);
            if p.exists() {
                return p;
            }
        }
    }
    PathBuf::from(fallback)
}

fn parse_fps(s: &str) -> f64 {
    if let Some((a, b)) = s.split_once('/') {
        let (a, b): (f64, f64) = (a.parse().unwrap_or(0.0), b.parse().unwrap_or(1.0));
        if b != 0.0 {
            return a / b;
        }
    }
    s.parse().unwrap_or(0.0)
}

pub async fn probe_path(input: &Path) -> Result<Metadata> {
    anyhow::ensure!(input.exists(), "输入文件不存在：{}", input.display());
    let out = Command::new(ffprobe_bin())
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(input)
        .output()
        .await
        .context(
            "ffprobe 执行失败（请确认 PATH 中包含 ffprobe，或将 ffprobe.exe 与本程序置于同一目录）",
        )?;
    anyhow::ensure!(
        out.status.success(),
        "ffprobe 执行失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: FfprobeOut =
        serde_json::from_slice(&out.stdout).context("ffprobe 输出的 JSON 解析失败")?;

    let duration_sec = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            parsed
                .streams
                .iter()
                .find_map(|s| s.duration.as_deref().and_then(|d| d.parse().ok()))
        })
        .unwrap_or(0.0);

    let v = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .context("未检测到视频流")?;

    let audio = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    Ok(Metadata {
        duration_sec,
        width: v.width.unwrap_or(0),
        height: v.height.unwrap_or(0),
        fps: parse_fps(&v.avg_frame_rate.clone().unwrap_or_default()),
        video_codec: v.codec_name.clone().unwrap_or_default(),
        has_audio: audio.is_some(),
    })
}

impl Session {
    pub fn new(input: &Path, meta: Metadata) -> Result<Self> {
        // `absolute` rather than `canonicalize`: the latter adds a `\\?\` prefix
        // on Windows. ffmpeg copes with it, but it is ugly in logs and previews.
        let input = std::path::absolute(input)
            .with_context(|| format!("解析输入路径失败：{}", input.display()))?;
        let workdir = input
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let outdir = workdir.join("_out");
        let file = input
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("input")
            .to_string();

        let mut session = Self {
            outdir,
            assets: HashMap::new(),
            clips: HashMap::new(),
            timeline: Vec::new(),
            outputs: Vec::new(),
            next_asset: 0,
            next_clip: 0,
        };
        let id = format!("asset_{}", session.next_asset);
        session.next_asset += 1;
        session.assets.insert(
            id.clone(),
            Asset {
                id,
                path: input,
                file,
                meta,
            },
        );
        Ok(session)
    }

    pub fn asset(&self, id: &str) -> Result<&Asset> {
        self.assets
            .get(id)
            .with_context(|| format!("未知素材：{id}"))
    }

    pub fn add_clip(&mut self, asset_id: &str, start: f64, end: f64) -> Result<String> {
        guard::check_id("asset_id", asset_id)?;
        guard::check_time("start_sec", start)?;
        guard::check_time("end_sec", end)?;
        if end <= start {
            return Err(invalid(format!(
                "end_sec 须大于 start_sec（实际为 {start} -> {end}）"
            )));
        }
        if end - start < 0.05 {
            return Err(invalid("片段时长过短（最短为 0.05 秒）"));
        }
        if self.timeline.len() >= MAX_CLIPS {
            return Err(invalid(format!("时间线片段数量已达上限 {MAX_CLIPS}")));
        }

        let duration = self.asset(asset_id)?.meta.duration_sec;
        if start > duration {
            return Err(invalid(format!(
                "start_sec（{start}）超出素材时长（{duration:.2}s）"
            )));
        }
        if end > duration + 0.5 {
            return Err(invalid(format!(
                "end_sec（{end}）超出素材时长（{duration:.2}s）"
            )));
        }

        let id = format!("clip_{}", self.next_clip);
        self.next_clip += 1;
        self.clips.insert(
            id.clone(),
            Clip {
                id: id.clone(),
                asset_id: asset_id.to_string(),
                start_sec: start,
                end_sec: end,
                muted: false,
                volume_db: 0.0,
            },
        );
        self.timeline.push(id.clone());
        Ok(id)
    }

    pub fn set_clip_muted(&mut self, clip_id: &str, muted: bool) -> Result<()> {
        guard::check_id("clip_id", clip_id)?;
        let clip = self
            .clips
            .get_mut(clip_id)
            .with_context(|| format!("未知片段：{clip_id}"))?;
        clip.muted = muted;
        Ok(())
    }

    pub fn set_clip_volume(&mut self, clip_id: &str, db: f64) -> Result<()> {
        guard::check_id("clip_id", clip_id)?;
        guard::check_volume("db", db)?;
        let clip = self
            .clips
            .get_mut(clip_id)
            .with_context(|| format!("未知片段：{clip_id}"))?;
        clip.volume_db = db;
        // Setting volume unmutes: otherwise the model would need an extra
        // unmute round trip.
        clip.muted = false;
        Ok(())
    }

    pub fn set_timeline(&mut self, ids: Vec<String>) -> Result<()> {
        if ids.is_empty() {
            return Err(invalid("clip_ids 不允许为空"));
        }
        if ids.len() > MAX_CLIPS {
            return Err(invalid(format!("片段数量超出上限 {MAX_CLIPS}")));
        }
        for id in &ids {
            guard::check_id("clip_id", id)?;
            if !self.clips.contains_key(id) {
                return Err(invalid(format!("未知片段：{id}")));
            }
        }
        self.timeline = ids;
        Ok(())
    }

    pub fn split_clip(&mut self, clip_id: &str, offset_sec: f64) -> Result<(String, String)> {
        guard::check_id("clip_id", clip_id)?;
        guard::check_time("offset_sec", offset_sec)?;
        let src = self
            .clips
            .get(clip_id)
            .with_context(|| format!("未知片段：{clip_id}"))?
            .clone();
        let duration = src.end_sec - src.start_sec;
        if offset_sec < 0.05 || duration - offset_sec < 0.05 {
            return Err(invalid(format!(
                "offset_sec 须在 0.05 秒至片段时长减 0.05 秒之间（片段时长为 {duration:.2}s，实际为 {offset_sec}）"
            )));
        }
        if self.timeline.len() + 1 > MAX_CLIPS {
            return Err(invalid(format!("片段数量超出上限 {MAX_CLIPS}")));
        }
        let pos = self
            .timeline
            .iter()
            .position(|id| id == clip_id)
            .with_context(|| format!("片段不在时间线上：{clip_id}"))?;

        let mid = src.start_sec + offset_sec;
        let first_id = format!("clip_{}", self.next_clip);
        self.next_clip += 1;
        let second_id = format!("clip_{}", self.next_clip);
        self.next_clip += 1;
        self.clips.insert(
            first_id.clone(),
            Clip {
                id: first_id.clone(),
                asset_id: src.asset_id.clone(),
                start_sec: src.start_sec,
                end_sec: mid,
                muted: src.muted,
                volume_db: src.volume_db,
            },
        );
        self.clips.insert(
            second_id.clone(),
            Clip {
                id: second_id.clone(),
                asset_id: src.asset_id.clone(),
                start_sec: mid,
                end_sec: src.end_sec,
                muted: src.muted,
                volume_db: src.volume_db,
            },
        );
        self.timeline
            .splice(pos..pos + 1, [first_id.clone(), second_id.clone()]);
        self.clips.remove(clip_id);
        Ok((first_id, second_id))
    }

    pub fn remove_clips(&mut self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Err(invalid("clip_ids 不允许为空"));
        }
        for id in ids {
            guard::check_id("clip_id", id)?;
            if !self.clips.contains_key(id) {
                return Err(invalid(format!("未知片段：{id}")));
            }
        }
        let before = self.timeline.len();
        self.timeline.retain(|id| !ids.contains(id));
        for id in ids {
            self.clips.remove(id);
        }
        Ok(before - self.timeline.len())
    }

    pub fn timeline_clips(&self) -> Vec<Clip> {
        self.timeline
            .iter()
            .filter_map(|id| self.clips.get(id).cloned())
            .collect()
    }

    pub fn timeline_duration(&self) -> f64 {
        self.timeline_clips().iter().map(Clip::duration).sum()
    }

    /// State projection: a compact view for the model and the user.
    ///
    /// Rust builds this from the facts, never the LLM. It keeps what decisions
    /// need and drops what they do not (codec, rotation, absolute paths).
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        let mut ids: Vec<&String> = self.assets.keys().collect();
        ids.sort();
        for id in ids {
            let Some(a) = self.assets.get(id) else {
                continue;
            };
            lines.push(format!(
                "素材 {} ({}): {:.1}s, {}x{}, {:.0}fps, {}",
                a.id,
                a.file,
                a.meta.duration_sec,
                a.meta.width,
                a.meta.height,
                a.meta.fps,
                if a.meta.has_audio {
                    "含音轨"
                } else {
                    "无音轨"
                }
            ));
        }
        if self.timeline.is_empty() {
            lines.push("时间线：空".into());
        } else {
            let items: Vec<String> = self
                .timeline_clips()
                .iter()
                .map(|c| {
                    format!(
                        "{}={}[{:.1}-{:.1}]{}",
                        c.id,
                        c.asset_id,
                        c.start_sec,
                        c.end_sec,
                        c.audio_label()
                    )
                })
                .collect();
            lines.push(format!(
                "时间线（{} 段，{:.1}s）：{}",
                items.len(),
                self.timeline_duration(),
                items.join(" -> ")
            ));
        }
        if self.outputs.is_empty() {
            lines.push("输出：无".into());
        } else {
            lines.push(format!(
                "输出：{}",
                self.outputs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        let meta = Metadata {
            duration_sec: 10.0,
            width: 640,
            height: 360,
            fps: 30.0,
            video_codec: "h264".into(),
            has_audio: true,
        };
        // Session::new only resolves the path to absolute; no ffmpeg involved.
        Session::new(Path::new("test.mp4"), meta).unwrap()
    }

    #[test]
    fn mute_and_volume_flow() {
        let mut s = test_session();
        let id = s.add_clip("asset_0", 0.0, 2.0).unwrap();
        assert!(s.clips[&id].audio_volume_filter().is_none());

        s.set_clip_muted(&id, true).unwrap();
        assert_eq!(
            s.clips[&id].audio_volume_filter().as_deref(),
            Some("volume=0")
        );
        assert!(s.summary().contains("（静音）"));

        // Setting volume unmutes automatically.
        s.set_clip_volume(&id, -6.0).unwrap();
        assert!(!s.clips[&id].muted);
        assert_eq!(s.clips[&id].volume_db, -6.0);
        assert!(s.summary().contains("-6.0dB"));

        // split inherits the audio state.
        let (a, b) = s.split_clip(&id, 1.0).unwrap();
        assert_eq!(s.clips[&a].volume_db, -6.0);
        assert_eq!(s.clips[&b].volume_db, -6.0);
    }

    #[test]
    fn volume_range_rejected() {
        let mut s = test_session();
        let id = s.add_clip("asset_0", 0.0, 1.0).unwrap();
        assert!(s.set_clip_volume(&id, f64::NAN).is_err());
        assert!(s.set_clip_volume(&id, -61.0).is_err());
        assert!(s.set_clip_volume(&id, 21.0).is_err());
        assert!(s.set_clip_volume("clip_999", -6.0).is_err());
    }
}
