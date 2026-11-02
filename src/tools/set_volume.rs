use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{describe_serde, invalid_args};
use crate::media::Session;
use crate::tool::{Tool, ToolOutcome, ToolSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    clip_id: String,
    db: f64,
}

pub struct SetVolumeTool;

#[async_trait::async_trait]
impl Tool for SetVolumeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_volume",
            description: "调整指定片段的音量（单位 dB，范围 -60 至 +20，例如 -6 为减半响度，+6 为增强）。调用后自动取消静音。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "clip_id": {
                        "type": "string",
                        "description": "片段 ID，例如 clip_0（见状态）"
                    },
                    "db": {
                        "type": "number",
                        "description": "音量增益（dB），须为 -60 至 +20 之间的有限数字"
                    }
                },
                "required": ["clip_id", "db"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        let a: Args = match serde_json::from_value(args.clone()) {
            Ok(a) => a,
            Err(e) => return Err(invalid_args("set_volume", describe_serde(&e), &args)),
        };
        match session.set_clip_volume(&a.clip_id, a.db) {
            Ok(()) => (),
            Err(e) => return Err(invalid_args("set_volume", e.to_string(), &args)),
        };
        Ok(ToolOutcome {
            summary: format!(
                "set_volume: {} 音量已设为 {:+.1}dB（已取消静音）",
                a.clip_id, a.db
            ),
        })
    }
}
