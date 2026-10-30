use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{describe_serde, invalid_args};
use crate::media::Session;
use crate::tool::{Tool, ToolOutcome, ToolSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    asset_id: String,
    start_sec: f64,
    end_sec: f64,
}

pub struct CutTool;

#[async_trait::async_trait]
impl Tool for CutTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "cut",
            description: "从素材中剪取一个片段，并追加至时间线末尾。时间单位为秒，须满足 0 <= start_sec < end_sec <= 素材时长。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "asset_id": {
                        "type": "string",
                        "description": "素材 ID，例如 asset_0（见状态）"
                    },
                    "start_sec": {
                        "type": "number",
                        "description": "起始时间（秒），须 >= 0"
                    },
                    "end_sec": {
                        "type": "number",
                        "description": "结束时间（秒），须大于 start_sec"
                    }
                },
                "required": ["asset_id", "start_sec", "end_sec"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        let a: Args = match serde_json::from_value(args.clone()) {
            Ok(a) => a,
            Err(e) => return Err(invalid_args("cut", describe_serde(&e), &args)),
        };
        let clip_id = match session.add_clip(&a.asset_id, a.start_sec, a.end_sec) {
            Ok(id) => id,
            Err(e) => return Err(invalid_args("cut", e.to_string(), &args)),
        };
        let n = session.timeline.len();
        let total = session.timeline_duration();
        Ok(ToolOutcome {
            summary: format!(
                "cut: 已追加 {clip_id} = {}[{:.1}-{:.1}]（时间线现有 {n} 段，共 {total:.1}s）",
                a.asset_id, a.start_sec, a.end_sec
            ),
        })
    }
}
