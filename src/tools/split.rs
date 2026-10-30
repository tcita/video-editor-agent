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
    offset_sec: f64,
}

pub struct SplitTool;

#[async_trait::async_trait]
impl Tool for SplitTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "split",
            description: "将一个片段按偏移量一分为二，并按原位置保留在时间线上。偏移量为距片段起始的秒数，两段长度均须不小于 0.05 秒。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "clip_id": {
                        "type": "string",
                        "description": "待拆分的片段 ID，例如 clip_0"
                    },
                    "offset_sec": {
                        "type": "number",
                        "description": "距片段起始的偏移秒，须大于 0 且小于片段时长"
                    }
                },
                "required": ["clip_id", "offset_sec"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        let a: Args = match serde_json::from_value(args.clone()) {
            Ok(a) => a,
            Err(e) => return Err(invalid_args("split", describe_serde(&e), &args)),
        };
        let (first, second) = match session.split_clip(&a.clip_id, a.offset_sec) {
            Ok(ids) => ids,
            Err(e) => return Err(invalid_args("split", e.to_string(), &args)),
        };
        Ok(ToolOutcome {
            summary: format!(
                "split: 已将 {} 拆分为 {first} 与 {second}（时间线现有 {} 段，共 {:.1}s）",
                a.clip_id,
                session.timeline.len(),
                session.timeline_duration()
            ),
        })
    }
}
