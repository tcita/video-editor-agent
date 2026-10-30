use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{describe_serde, invalid_args};
use crate::media::Session;
use crate::tool::{Tool, ToolOutcome, ToolSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    clip_ids: Vec<String>,
}

pub struct RemoveTool;

#[async_trait::async_trait]
impl Tool for RemoveTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remove",
            description: "从时间线中移除一个或多个片段。移除后片段 ID 即失效，请勿继续引用。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "clip_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "待移除的 clip_id 列表，不允许为空"
                    }
                },
                "required": ["clip_ids"],
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        let a: Args = match serde_json::from_value(args.clone()) {
            Ok(a) => a,
            Err(e) => return Err(invalid_args("remove", describe_serde(&e), &args)),
        };
        let n = match session.remove_clips(&a.clip_ids) {
            Ok(n) => n,
            Err(e) => return Err(invalid_args("remove", e.to_string(), &args)),
        };
        Ok(ToolOutcome {
            summary: format!(
                "remove: 已移除 {n} 段（{}，时间线剩余 {} 段，共 {:.1}s）",
                a.clip_ids.join(", "),
                session.timeline.len(),
                session.timeline_duration()
            ),
        })
    }
}
