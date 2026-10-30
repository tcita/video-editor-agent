use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use crate::error::InvalidArg;
use crate::guard::{max_tool_steps, MAX_ARGS_BYTES};
use crate::media::Session;
use crate::runlog::RunLog;
use crate::tool::{ToolContext, ToolRegistry};

pub trait AgentHost: Send + Sync {
    fn log(&self, msg: String);
    fn state(&self, _summary: String) {}
}

pub struct LlmClient {
    endpoint: String,
    key: String,
    model: String,
    http_client: reqwest::Client,
    /// Used to persist raw model responses when `VIDEO_AGENT_DEBUG` is set.
    log: Arc<RunLog>,
}

impl LlmClient {
    pub fn from_env(key: String, log: Arc<RunLog>) -> Result<Self> {
        let base = std::env::var("VIDEO_AGENT_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
        let endpoint = if base.ends_with("/chat/completions") {
            base
        } else {
            format!("{}/chat/completions", base.trim_end_matches('/'))
        };
        let model =
            std::env::var("VIDEO_AGENT_MODEL").unwrap_or_else(|_| "deepseek-flash".to_string());
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .context("初始化 HTTP 客户端失败")?;
        Ok(Self {
            endpoint,
            key,
            model,
            http_client,
            log,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// `tool_choice`: "auto" lets tools through; "none" keeps the definitions
    /// but has the server forbid calls. The closing step uses "none" rather than
    /// dropping `tools` so the request prefix is unchanged and the provider's
    /// cache still hits.
    async fn chat(&self, messages: &[Value], tools: &[Value], tool_choice: &str) -> Result<Value> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": tool_choice,
            "temperature": 0.2,
            "stream": false,
            "thinking": {"type": "disabled"},
        });
        let resp = self
            .http_client
            .post(&self.endpoint)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .context("请求模型失败")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if std::env::var("VIDEO_AGENT_DEBUG").is_ok() {
            eprintln!("--- model raw ---\n{text}\n--- end raw ---");
            self.log.line("RAW", &text);
        }
        if !status.is_success() {
            bail!("模型接口 {status}: {}", truncate(&text, 2000));
        }
        let v: Value = serde_json::from_str(&text).context("模型响应不是合法 JSON")?;
        v.pointer("/choices/0/message")
            .cloned()
            .with_context(|| format!("模型响应缺少 choices[0].message: {}", truncate(&text, 1000)))
    }
}

/// Once the step budget is gone: disable tools and force a text-only summary.
const MAX_STEPS_PROMPT: &str = "\
本轮任务允许的工具调用次数已耗尽，工具已禁用。
仅输出一段中文总结，不再调用任何工具。总结须包含：
1. 已完成的工作
2. 尚未完成的部分
3. 建议的后续步骤
本指令覆盖其他所有要求。";

pub async fn run_turn(
    client: &LlmClient,
    registry: &ToolRegistry,
    host: &dyn AgentHost,
    session: &mut Session,
    user_request: &str,
    log: &RunLog,
) -> Result<String> {
    // On-screen output and the log always happen together, so neither side can
    // miss an entry.
    let say = |tag: &str, msg: String| {
        log.line(tag, &msg);
        host.log(msg);
    };

    let mut messages = vec![
        json!({"role": "system", "content": system_prompt(registry, session)}),
        json!({"role": "user", "content": user_request}),
    ];
    host.state(session.summary());

    let mut finalization_checked = false;
    let max_steps = max_tool_steps();
    // Same as opencode: a step is one model round trip, and the last step is
    // reserved for the text-only wrap-up.
    let tool_steps = max_steps - 1;

    log.line(
        "TURN",
        &format!("---------- 新一轮 · 预算 {max_steps} 步 ----------"),
    );
    log.line("PROMPT", user_request);

    for step in 0..max_steps {
        // Step numbers go to the log only; the TUI scroll area is too noisy
        // for them.
        log.line("STEP", &format!("{}/{}", step + 1, max_steps));
        // Closing step: the tool definitions are still sent (prefix unchanged,
        // provider cache still hits) but tool_choice="none" makes the server
        // forbid calls. That produces no dangling tool_calls, and therefore no
        // illegal history of "assistant.tool_calls with no tool reply"
        // (DeepSeek answers 400 to that).
        //
        // The instruction is not stuffed in as an assistant message the way
        // opencode does it: in thinking mode DeepSeek requires
        // reasoning_content to be echoed back, and a trailing assistant
        // message is a straight 400. A user message is the safe shape.
        if step >= tool_steps {
            say(
                "BUDGET",
                format!("步骤预算已耗尽（{tool_steps} 轮工具调用与 1 轮收尾），转为文字总结"),
            );
            let mut prompt = MAX_STEPS_PROMPT.to_string();
            if !session.timeline.is_empty() && session.outputs.is_empty() {
                prompt.push_str(&format!(
                    "\n补充说明：时间线已有 {} 段（合计 {:.1}s），但尚未导出任何文件。请在总结中说明是否需要继续导出。",
                    session.timeline.len(),
                    session.timeline_duration()
                ));
            }
            messages.push(json!({"role": "user", "content": prompt}));
            let msg = client.chat(&messages, &registry.specs(), "none").await?;
            // The caller (main / TUI) renders the final text; do not print a
            // second copy here.
            let out = msg
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            log.line("FINAL", &out);
            return Ok(out);
        }

        let msg = client.chat(&messages, &registry.specs(), "auto").await?;

        if let Some(r) = msg
            .get("reasoning_content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            say("THINK", format!("思考：{r}"));
        }
        let calls = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text = msg
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // This round's content is logged only when this function does not hand
        // it back:
        //   - with tool calls -> it is narration, log it
        //   - interrupted by the export reminder -> not returned, so log it
        //   - the real final answer -> the caller renders it (CLI println /
        //     TUI checkmark), so do not duplicate it
        if !calls.is_empty() {
            if let Some(c) = text {
                say("MODEL", format!("模型：{c}"));
            }
        }

        // Echo back only role/content/tool_calls; strip reasoning_content.
        let mut assistant = json!({
            "role": "assistant",
            "content": msg.get("content").cloned().unwrap_or(Value::String(String::new())),
        });
        if !calls.is_empty() {
            assistant["tool_calls"] = Value::Array(calls.clone());
        }
        messages.push(assistant);

        if calls.is_empty() {
            // Finalization check: clips exist but nothing was exported. Remind
            // once only, to avoid spinning. There must still be a tool round
            // left afterwards (step+1 < tool_steps) or the reminder is useless.
            if !finalization_checked
                && step + 1 < tool_steps
                && !session.timeline.is_empty()
                && session.outputs.is_empty()
            {
                finalization_checked = true;
                // Interrupted by the reminder and never returned as the final
                // answer, so log it here.
                if let Some(c) = text {
                    say("MODEL", format!("模型：{c}"));
                }
                messages.push(json!({
                    "role": "user",
                    "content": format!(
                        "时间线当前包含 {} 段（合计 {:.1}s），但尚未导出任何文件。\n\
                         若用户需要成片，请调用 export 导出；若用户仅需方案或暂不导出，请用一句话说明原因后结束。",
                        session.timeline.len(),
                        session.timeline_duration()
                    )
                }));
                continue;
            }
            let out = text.unwrap_or_default().to_string();
            log.line("FINAL", &out);
            return Ok(out);
        }

        for (idx, call) in calls.iter().enumerate() {
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args_str = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}")
                .to_string();
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            say(
                "TOOL",
                format!("tool_call: {name}({})", truncate(&args_str, 300)),
            );

            let (content, fatal) =
                match run_one(registry, host, session, &name, &args_str, log).await {
                    Ok(outcome) => {
                        say("RESULT", format!("  -> {}", outcome.summary));
                        (outcome.summary, None)
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if e.downcast_ref::<InvalidArg>().is_some() {
                            say("INVALID", format!("参数不合法：{msg}"));
                            (format!("错误（参数不合法，修正参数后可重试）：{msg}"), None)
                        } else {
                            say("ERROR", format!("致命错误：{msg}"));
                            (format!("错误（已停止，请勿重试）：{msg}"), Some(msg))
                        }
                    }
                };
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content,
            }));
            host.state(session.summary());

            if let Some(reason) = fatal {
                // Every later tool_call in this round still needs a result, or
                // the message history is invalid.
                for rest in &calls[idx + 1..] {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": rest.get("id").and_then(Value::as_str).unwrap_or_default(),
                        "content": "已跳过：上一条工具调用发生致命错误，本轮已终止。",
                    }));
                }
                let out = format!("因工具发生致命错误而终止：{reason}");
                log.line("FINAL", &out);
                return Ok(out);
            }
        }
    }

    Ok("已达到最大工具调用次数。".into())
}

async fn run_one(
    registry: &ToolRegistry,
    host: &dyn AgentHost,
    session: &mut Session,
    name: &str,
    args_str: &str,
    log: &RunLog,
) -> Result<crate::tool::ToolOutcome> {
    if args_str.len() > MAX_ARGS_BYTES {
        return Err(crate::error::invalid(format!(
            "参数过长（实际为 {} 字节，上限为 {MAX_ARGS_BYTES} 字节）",
            args_str.len()
        )));
    }
    if !registry.contains(name) {
        return Err(crate::error::invalid(format!(
            "未知工具「{name}」，可用工具为：{}",
            registry.names().join(", ")
        )));
    }
    let args: Value =
        serde_json::from_str(args_str).with_context(|| format!("{name} 的参数不是合法 JSON"))?;
    if !args.is_object() {
        return Err(crate::error::invalid(format!(
            "{name} 的参数必须是 JSON 对象"
        )));
    }
    let mut ctx = ToolContext { session, host, log };
    registry.dispatch(&mut ctx, name, args).await
}

fn system_prompt(registry: &ToolRegistry, session: &Session) -> String {
    format!(
        r#"你是运行在 Windows 上的视频剪辑助手。你的工作只有三件事：理解需求、选择工具、填写参数。

硬性规则（必须遵守）：
- 只能调用下面列出的工具函数。禁止输出 ffmpeg 命令、shell 命令、Python/Rust 等任何代码，禁止尝试其它执行方式。
- 禁止编造文件路径。素材与片段须引用状态中的 asset_id / clip_id。
- 所有时间单位均为秒（允许小数）。参数须符合工具 schema，请勿传递多余字段。
- 典型流程：cut 生成片段 ->（可选）concat 调整顺序 -> export 输出。
- 工具返回“参数不合法”时，请按提示修正参数后重试；其他错误请勿重试，并直接说明情况。
- 只要时间线非空且用户需要成片，最终须调用 export；请勿仅停留于描述计划。
- 完成后请用中文简要说明已执行的操作与输出位置。

当前状态：
{}

可用工具：{}"#,
        session.summary(),
        registry.names().join(", ")
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}
