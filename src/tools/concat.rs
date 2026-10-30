use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{describe_serde, invalid_args};
use crate::media::Session;
use crate::tool::{Tool, ToolOutcome, ToolSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    clip_ids: Option<Vec<String>>,
}

pub struct ConcatTool;

#[async_trait::async_trait]
impl Tool for ConcatTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "concat",
            description: "按 clip_ids 的顺序重排时间线。省略 clip_ids 时保持当前顺序。clip_id 须已通过 cut 创建。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "clip_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "按播放顺序排列的 clip_id 列表，可省略"
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        let a: Args = match serde_json::from_value(args.clone()) {
            Ok(a) => a,
            Err(e) => return Err(invalid_args("concat", describe_serde(&e), &args)),
        };
        let ids = match a.clip_ids {
            Some(ids) => {
                if let Err(e) = session.set_timeline(ids.clone()) {
                    return Err(invalid_args("concat", e.to_string(), &args));
                }
                ids
            }
            None => {
                if session.timeline.is_empty() {
                    return Err(invalid_args(
                        "concat",
                        "时间线为空：请先使用 cut 添加片段",
                        &args,
                    ));
                }
                session.timeline.clone()
            }
        };
        Ok(ToolOutcome {
            summary: format!(
                "concat: 时间线 = {}（{} 段，共 {:.1}s）",
                ids.join(" -> "),
                ids.len(),
                session.timeline_duration()
            ),
        })
    }
}
