use crate::agent::AgentHost;
use crate::media::Session;
use crate::runlog::RunLog;
use anyhow::Result;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    pub fn to_openai(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}

/// A finished tool reports exactly one sentence:
/// - `summary` is what the human reads (log / TUI)
/// - the same sentence goes back to the model; facts live in the Session
///   projection and are not repeated here
pub struct ToolOutcome {
    pub summary: String,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Human-readable plan, written to the log only. Safety comes from never
    /// overwriting an existing file; there is no confirmation step.
    fn plan(&self, _session: &Session, _args: &serde_json::Value) -> Result<String> {
        Ok(String::new())
    }

    async fn run(&self, session: &mut Session, args: serde_json::Value) -> Result<ToolOutcome>;
}

pub struct ToolContext<'a> {
    pub session: &'a mut Session,
    pub host: &'a dyn AgentHost,
    /// Plan lines are persisted too, so what the model intended can be checked
    /// against what actually ran.
    pub log: &'a RunLog,
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self { tools }
    }

    pub fn specs(&self) -> Vec<serde_json::Value> {
        self.tools.iter().map(|t| t.spec().to_openai()).collect()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.spec().name == name)
    }

    fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.as_ref())
    }

    pub async fn dispatch(
        &self,
        ctx: &mut ToolContext<'_>,
        name: &str,
        args: serde_json::Value,
    ) -> Result<ToolOutcome> {
        let Some(tool) = self.get(name) else {
            return Err(crate::error::invalid(format!(
                "未知工具「{name}」，可用工具为：{}",
                self.names().join(", ")
            )));
        };
        let plan = tool.plan(ctx.session, &args)?;
        if !plan.is_empty() {
            ctx.log.line("PLAN", &plan);
            ctx.host.log(format!("计划：{plan}"));
        }
        tool.run(ctx.session, args).await
    }
}
