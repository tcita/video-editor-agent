use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::process::Command;

use crate::error::{describe_serde, fatal, invalid, invalid_args};
use crate::guard;
use crate::media::{ffmpeg_bin, Clip, Session};
use crate::tool::{Tool, ToolOutcome, ToolSpec};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AudioFormat {
    M4a,
    Mp3,
    Wav,
}

impl AudioFormat {
    fn ext(self) -> &'static str {
        match self {
            AudioFormat::M4a => "m4a",
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Wav => "wav",
        }
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        AudioFormat::M4a
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    output_name: Option<String>,
    #[serde(default)]
    format: Option<AudioFormat>,
}

struct AudioPlan {
    clips: Vec<Clip>,
    format: AudioFormat,
    out_path: std::path::PathBuf,
}

pub struct ExtractAudioTool;

impl ExtractAudioTool {
    fn build_plan(session: &Session, a: &Args) -> Result<AudioPlan> {
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
        let format = a.format.unwrap_or_default();
        let out_path =
            guard::output_path_with_ext(&session.outdir, a.output_name.as_deref(), format.ext())?;
        Ok(AudioPlan {
            clips,
            format,
            out_path,
        })
    }

    fn parse(session: &Session, args: &Value) -> Result<(Args, AudioPlan)> {
        let a: Args = serde_json::from_value(args.clone())
            .map_err(|e| invalid_args("extract_audio", describe_serde(&e), args))?;
        let plan = Self::build_plan(session, &a)?;
        Ok((a, plan))
    }
}

#[async_trait::async_trait]
impl Tool for ExtractAudioTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "extract_audio",
            description: "将当前时间线的音频导出为独立音频文件（默认 m4a）。时间线本身不变，可重复调用以输出不同格式。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "output_name": {
                        "type": "string",
                        "description": "输出文件名（将自动规范化并置于 _out 目录，可省略）"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["m4a", "mp3", "wav"],
                        "description": "音频格式，默认为 m4a"
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
            "提取音频 {} 个片段，总时长 {:.1}s，格式 {}，输出 {}",
            p.clips.len(),
            total,
            p.format.ext(),
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

        let amap = if n == 1 {
            "[a0]".to_string()
        } else {
            let mut inputs = String::new();
            for i in 0..n {
                inputs.push_str(&format!("[a{i}]"));
            }
            chains.push(format!("{inputs}concat=n={n}:v=0:a=1[a]"));
            "[a]".to_string()
        };

        cmd.arg("-filter_complex").arg(chains.join(";"));
        cmd.arg("-map").arg(&amap);
        match plan.format {
            AudioFormat::M4a => {
                cmd.args(["-c:a", "aac", "-b:a", "192k"]);
            }
            AudioFormat::Mp3 => {
                cmd.args(["-c:a", "libmp3lame", "-b:a", "192k"]);
            }
            AudioFormat::Wav => {
                cmd.args(["-c:a", "pcm_s16le"]);
            }
        }
        cmd.arg(&plan.out_path);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let out = cmd.output().await.context("启动 ffmpeg 失败")?;
        if !out.status.success() {
            return Err(fatal(format!(
                "ffmpeg 执行失败：\n{}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        if !plan.out_path.exists() {
            return Err(fatal(format!(
                "ffmpeg 返回成功，但未生成文件：{}",
                plan.out_path.display()
            )));
        }

        let duration: f64 = plan.clips.iter().map(Clip::duration).sum();
        session.outputs.push(plan.out_path.clone());

        Ok(ToolOutcome {
            summary: format!(
                "extract_audio: 已提取 {}（{n} 段，{}，{duration:.1}s）",
                plan.out_path.display(),
                plan.format.ext(),
            ),
        })
    }
}
