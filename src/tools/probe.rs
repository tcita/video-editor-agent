use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{describe_serde, invalid_args};
use crate::media::Session;
use crate::tool::{Tool, ToolOutcome, ToolSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {}

pub struct ProbeTool;

#[async_trait::async_trait]
impl Tool for ProbeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "probe",
            description: "查看当前素材的技术信息（时长、分辨率、帧率、音轨情况）。无参数；若状态中已有则无需调用。",
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn run(&self, session: &mut Session, args: Value) -> Result<ToolOutcome> {
        if let Err(e) = serde_json::from_value::<Args>(args.clone()) {
            return Err(invalid_args("probe", describe_serde(&e), &args));
        }
        let n = session.assets.len();
        Ok(ToolOutcome {
            summary: format!("probe: 当前共 {n} 个素材（详情见状态）"),
        })
    }
}
