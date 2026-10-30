use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

use crate::error::{describe_serde, fatal, invalid, invalid_args};
use crate::guard;
use crate::media::{ffmpeg_bin, probe_path, Clip, Session};
use crate::tool::{Tool, ToolOutcome, ToolSpec};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Quality {
    High,
    Balanced,
    Small,
}

impl Quality {
    fn crf(self) -> u8 {
        match self {
            Quality::High => 18,
            Quality::Balanced => 20,
            Quality::Small => 26,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Quality::High => "high",
            Quality::Balanced => "balanced",
            Quality::Small => "small",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    output_name: Option<String>,
    #[serde(default)]
    quality: Option<Quality>,
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    include_audio: Option<bool>,
}

struct RenderPlan {
    clips: Vec<Clip>,
    width: u32,
    height: u32,
    fps: f64,
    include_audio: bool,
    quality: Quality,
    out_path: PathBuf,
}

pub struct ExportTool;

impl ExportTool {
    fn build_plan(session: &Session, a: &Args) -> Result<RenderPlan> {
        if session.timeline.is_empty() {
            return Err(invalid("时间线为空：请先使用 cut 添加片段"));
        }
        if session.timeline.len() > guard::MAX_CLIPS {
            return Err(invalid(format!(
                "时间线片段数量超出上限 {}",
                guard::MAX_CLIPS
            )));
        }

        let mut clips = Vec::with_capacity(session.timeline.len());
        for id in &session.timeline {
            let c = session
                .clips
                .get(id)
                .with_context(|| format!("时间线引用了不存在的片段：{id}"))?;
            if !session.assets.contains_key(&c.asset_id) {
                return Err(invalid(format!(
                    "片段 {id} 引用的素材不存在：{}",
                    c.asset_id
                )));
            }
            clips.push(c.clone());
        }

        let first = session.assets.get(&clips[0].asset_id).context("素材缺失")?;

        let (mut width, mut height) = match a.resolution.as_deref() {
            Some(r) => guard::parse_resolution(r)?,
            None => {
                let w = if first.meta.width > 0 {
                    first.meta.width as u32
                } else {
                    1280
                };
                let h = if first.meta.height > 0 {
                    first.meta.height as u32
                } else {
                    720
                };
                (w, h)
            }
        };
        width = (width.max(2)) & !1;
        height = (height.max(2)) & !1;

        let mut fps = first.meta.fps;
        if !fps.is_finite() || fps <= 0.0 {
            fps = 30.0;
        }
        let fps = fps.clamp(1.0, 120.0);

        let out_path = guard::output_path(&session.outdir, a.output_name.as_deref())?;

        Ok(RenderPlan {
            clips,
            width,
            height,
            fps,
            include_audio: a.include_audio.unwrap_or(true),
            quality: a.quality.unwrap_or(Quality::Balanced),
            out_path,
        })
    }

    fn parse(session: &Session, args: &Value) -> Result<(Args, RenderPlan)> {
        let a: Args = serde_json::from_value(args.clone())
            .map_err(|e| invalid_args("export", describe_serde(&e), args))?;
        let plan = Self::build_plan(session, &a)?;
        Ok((a, plan))
    }
}

#[async_trait::async_trait]
impl Tool for ExportTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "export",
            description: "把当前时间线一次性渲染成 mp4（单次编码，不重复转码）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "output_name": {
                        "type": "string",
                        "description": "输出文件名（将自动规范化并置于 _out 目录，可省略）"
                    },
                    "quality": {
                        "type": "string",
                        "enum": ["high", "balanced", "small"],
                        "description": "画质与体积的取舍，默认为 balanced"
                    },
                    "resolution": {
                        "type": "string",
                        "description": "输出分辨率，格式为 宽x高，例如 1920x1080；省略则沿用首个片段"
                    },
                    "include_audio": {
                        "type": "boolean",
                        "description": "是否保留音轨，默认为 true；缺失音轨的片段将自动补充静音"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn plan(&self, session: &Session, args: &Value) -> Result<String> {
        let (_, p) = Self::parse(session, args)?;
        let total: f64 = p.clips.iter().map(Clip::duration).sum();
        Ok(format!(
            "导出 {} 个片段，总时长 {:.1}s，{}x{} @ {:.0}fps，质量 {}，{}，输出 {}",
            p.clips.len(),
            total,
            p.width,
            p.height,
            p.fps,
            p.quality.label(),
            if p.include_audio { "保留音轨" } else { "移除音轨" },
            p.out_path.display()
        ))
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        let (_, plan) = Self::parse(session, &args)?;
        std::fs::create_dir_all(&session.outdir)
            .with_context(|| format!("创建输出目录失败：{}", session.outdir.display()))?;

        let n = plan.clips.len();
        let mut cmd = Command::new(ffmpeg_bin());
        cmd.arg("-y").arg("-nostdin");

        for c in &plan.clips {
            let asset = session
                .assets
                .get(&c.asset_id)
                .with_context(|| format!("素材缺失：{}", c.asset_id))?;
            cmd.arg("-ss")
                .arg(format!("{:.3}", c.start_sec))
                .arg("-t")
                .arg(format!("{:.3}", c.duration()))
                .arg("-i")
                .arg(&asset.path);
        }

        let mut chains: Vec<String> = Vec::new();
        for (i, c) in plan.clips.iter().enumerate() {
            let asset = session
                .assets
                .get(&c.asset_id)
                .with_context(|| format!("素材缺失：{}", c.asset_id))?;
            chains.push(format!(
                "[{i}:v]scale={w}:{h}:force_original_aspect_ratio=decrease,\
                 pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,\
                 setsar=1,fps={fps},format=yuv420p,setpts=PTS-STARTPTS,settb=AVTB[v{i}]",
                w = plan.width,
                h = plan.height,
                fps = plan.fps,
            ));
            if plan.include_audio {
                if asset.meta.has_audio {
                    chains.push(format!(
                        "[{i}:a]aresample=48000,\
                         aformat=sample_fmts=fltp:channel_layouts=stereo,\
                         asetpts=PTS-STARTPTS[a{i}]"
                    ));
                } else {
                    chains.push(format!(
                        "anullsrc=channel_layout=stereo:sample_rate=48000,\
                         atrim=start=0:end={d:.3},asetpts=PTS-STARTPTS[a{i}]",
                        d = c.duration()
                    ));
                }
            }
        }

        let (vmap, amap) = if n == 1 {
            (
                "[v0]".to_string(),
                plan.include_audio.then(|| "[a0]".to_string()),
            )
        } else {
            let mut inputs = String::new();
            for i in 0..n {
                inputs.push_str(&format!("[v{i}]"));
                if plan.include_audio {
                    inputs.push_str(&format!("[a{i}]"));
                }
            }
            if plan.include_audio {
                chains.push(format!("{inputs}concat=n={n}:v=1:a=1[v][a]"));
                ("[v]".to_string(), Some("[a]".to_string()))
            } else {
                chains.push(format!("{inputs}concat=n={n}:v=1:a=0[v]"));
                ("[v]".to_string(), None)
            }
        };

        cmd.arg("-filter_complex").arg(chains.join(";"));
        cmd.arg("-map").arg(&vmap);
        if let Some(am) = &amap {
            cmd.arg("-map").arg(am);
        }
        cmd.arg("-c:v")
            .arg("libx264")
            .arg("-preset")
            .arg("veryfast")
            .arg("-crf")
            .arg(plan.quality.crf().to_string())
            .arg("-pix_fmt")
            .arg("yuv420p");
        if plan.include_audio {
            cmd.args(["-c:a", "aac", "-b:a", "192k"]);
        } else {
            cmd.arg("-an");
        }
        cmd.args(["-movflags", "+faststart"]);
        cmd.arg(&plan.out_path);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let out = cmd.output().await.context("启动 ffmpeg 失败")?;
        if !out.status.success() {
            // Environment-level failure: retrying with different arguments is
            // pointless.
            return Err(fatal(format!(
                "ffmpeg 执行失败：\n{}",
                ffmpeg_error(&out.stderr)
            )));
        }
        if !plan.out_path.exists() {
            return Err(fatal(format!(
                "ffmpeg 返回成功，但未生成文件：{}",
                plan.out_path.display()
            )));
        }

        let actual = probe_path(&plan.out_path)
            .await
            .ok()
            .map(|m| m.duration_sec);
        let duration = actual.unwrap_or_else(|| plan.clips.iter().map(Clip::duration).sum());
        session.outputs.push(plan.out_path.clone());

        Ok(ToolOutcome {
            summary: format!(
                "export: 已导出 {}（{n} 段，{}x{}，{duration:.1}s）",
                plan.out_path.display(),
                plan.width,
                plan.height
            ),
        })
    }
}

/// Pick the real error lines out of ffmpeg's stderr.
/// A plain tail would be buried under the build-configuration banner (dozens of
/// `--enable-xxx` lines).
fn ffmpeg_error(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let mut lines: Vec<&str> = s
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.contains("--enable-")
                || t.starts_with("built with")
                || (t.starts_with("lib") && t.contains(" / ")))
        })
        .collect();
    if lines.len() > 20 {
        lines = lines.split_off(lines.len() - 20);
    }
    let text = lines.join("\n");
    if text.chars().count() > 800 {
        let cut = text
            .chars()
            .skip(text.chars().count() - 800)
            .collect::<String>();
        return format!("…{cut}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Asset;
    use std::path::Path;

    fn run_ffmpeg(args: &[&str]) -> bool {
        std::process::Command::new(ffmpeg_bin())
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn export_mixed_clips_with_missing_audio() {
        if !run_ffmpeg(&["-version"]) {
            eprintln!("skip: ffmpeg not available");
            return;
        }
        let dir = std::env::temp_dir().join(format!("video_agent_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.mp4");
        let b = dir.join("b.mp4");

        assert!(run_ffmpeg(&[
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x180:rate=30:duration=2",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            a.to_str().unwrap(),
        ]));
        assert!(run_ffmpeg(&[
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x360:rate=25:duration=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            b.to_str().unwrap(),
        ]));

        let meta_a = probe_path(&a).await.unwrap();
        let meta_b = probe_path(&b).await.unwrap();
        let mut session = Session::new(&a, meta_a).unwrap();
        session.assets.insert(
            "asset_1".into(),
            Asset {
                id: "asset_1".into(),
                path: b,
                file: "b.mp4".into(),
                meta: meta_b,
            },
        );
        session.add_clip("asset_0", 0.2, 1.2).unwrap();
        session.add_clip("asset_1", 0.0, 1.0).unwrap();

        let out = ExportTool
            .run(&mut session, json!({ "output_name": "test_out" }))
            .await
            .unwrap();
        assert!(out.summary.starts_with("export: 已导出"));

        let path = session.outputs.last().unwrap();
        assert!(Path::new(path).exists(), "输出文件不存在：{}", path.display());
        let m = probe_path(Path::new(path)).await.unwrap();
        assert!(
            (m.duration_sec - 2.0).abs() < 0.3,
            "时长异常：{}",
            m.duration_sec
        );
        assert_eq!(m.width, 320);
        assert_eq!(m.height, 180);
        assert!(m.has_audio);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
