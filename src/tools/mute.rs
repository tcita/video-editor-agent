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
}

pub struct MuteTool;

#[async_trait::async_trait]
impl Tool for MuteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "mute",
            description: "将指定片段静音（仅影响音频，画面不变）。如需恢复声音，请调用 set_volume（会自动取消静音）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "clip_id": {
                        "type": "string",
                        "description": "片段 ID，例如 clip_0（见状态）"
                    }
                },
                "required": ["clip_id"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        let a: Args = match serde_json::from_value(args.clone()) {
            Ok(a) => a,
            Err(e) => return Err(invalid_args("mute", describe_serde(&e), &args)),
        };
        match session.set_clip_muted(&a.clip_id, true) {
            Ok(()) => (),
            Err(e) => return Err(invalid_args("mute", e.to_string(), &args)),
        };
        Ok(ToolOutcome {
            summary: format!("mute: {} 已静音（画面不变，导出时该段无声）", a.clip_id),
        })
    }
}
