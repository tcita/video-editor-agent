mod agent;
mod error;
mod guard;
mod media;
mod runlog;
mod tool;
mod tools;
mod tui;

use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use agent::{AgentHost, LlmClient};
use media::Session;
use runlog::RunLog;

const USAGE: &str = "\
用法:
  video-agent \"<视频路径>\"                        交互模式（默认）
  video-agent -p \"<自然语言需求>\" \"<视频路径>\"     一次性执行后退出

选项:
  -p, --prompt <需求>   一次性执行后退出（自动化 / 脚本用）
  -h, --help            显示本帮助

注意: 视频路径请用引号包裹，避免空格导致解析错误。
      已禁用 stdin 管道。Windows PowerShell 5.1 会按 ASCII 重编码管道内容，
      中文在到达本程序前已变为 ?，无法检测也无法还原。
      自动化场景请使用 -p，并从文件读取需求：
        video-agent a.mp4 -p (Get-Content req.txt -Raw -Encoding UTF8)
";

/// Read the API key. Falls back to the User-level variable when the process
/// environment has nothing, so setting it does not require a reboot.
fn get_key() -> Result<String> {
    for name in ["deepseek_key", "DEEPSEEK_API_KEY"] {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
    }
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[Environment]::GetEnvironmentVariable('deepseek_key','User')",
        ])
        .output()
        .context("读取用户级环境变量 deepseek_key 失败")?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(
        !v.is_empty(),
        "未找到 deepseek_key（进程级与用户级环境变量均不存在）"
    );
    Ok(v)
}

struct CliHost;

impl AgentHost for CliHost {
    fn log(&self, msg: String) {
        println!("{msg}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // The video is the only positional argument. The request never comes from a
    // bare argv word: it is either given by -p or typed into the TUI.
    let mut video: Option<PathBuf> = None;
    let mut prompt: Option<String> = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            "-p" | "--prompt" => {
                let value = it.next().context("缺少参数：-p 后须提供需求字符串")?;
                anyhow::ensure!(prompt.is_none(), "-p 仅允许提供一次");
                prompt = Some(value);
            }
            other if other.starts_with('-') => anyhow::bail!("未知参数 {other}\n\n{USAGE}"),
            other => {
                if let Some(first) = &video {
                    anyhow::bail!(
                        "当前仅支持单个视频（已指定 {}，又指定 {other}）\n{USAGE}",
                        first.display()
                    );
                }
                video = Some(PathBuf::from(other));
            }
        }
    }

    let Some(input) = video else {
        anyhow::bail!("未指定视频\n\n{USAGE}");
    };

    // No stdin support on purpose: a Windows PowerShell 5.1 pipe re-encodes the
    // text as ASCII before it reaches us, so a Chinese request arrives as "?"
    // with no way to tell it apart from a legitimate question mark.
    let prompt = match prompt {
        Some(p) => Some(p.trim().to_string()),
        None if std::io::stdin().is_terminal() => None,
        None => anyhow::bail!("缺少需求：请使用 -p \"<需求>\" 提供\n\n{USAGE}"),
    };
    if let Some(p) = &prompt {
        anyhow::ensure!(!p.is_empty(), "需求为空（-p 提供的值为空白）");
    }

    let key = get_key()?;
    let meta = media::probe_path(&input).await?;
    println!(
        "[probe] {}  {:.2}s  {}x{}  {:.2}fps  {}{}",
        input.display(),
        meta.duration_sec,
        meta.width,
        meta.height,
        meta.fps,
        meta.video_codec,
        if meta.has_audio { "  +audio" } else { "" }
    );
    let mut session = Session::new(&input, meta)?;
    // The log lives next to the output: one file per session, appended across
    // the TUI's turns.
    let log = Arc::new(RunLog::open(&session.outdir));
    if let Some(path) = log.path() {
        println!("[log] {}", path.display());
    }
    let client = LlmClient::from_env(key, Arc::clone(&log))?;
    let registry = tools::standard_registry();

    // Record how the process was started; each turn logs its own PROMPT.
    log.line(
        "RUN",
        &format!(
            "video={} model={} max_steps={} mode={}",
            input.display(),
            client.model(),
            guard::max_tool_steps(),
            if prompt.is_some() { "one-shot" } else { "tui" }
        ),
    );

    let Some(request) = prompt else {
        return tui::run(session, client, registry, log).await;
    };

    let host = CliHost;
    let result = agent::run_turn(&client, &registry, &host, &mut session, &request, &log).await;
    // run_turn logs FINAL on the success path; only the errors it cannot hand
    // back need recording here.
    if let Err(e) = &result {
        log.line("ERROR", &format!("{e:#}"));
    }
    let final_text = result?;

    if !final_text.trim().is_empty() {
        println!("\n{}", final_text.trim());
    }
    if session.outputs.is_empty() {
        println!("\n（本次未生成文件）");
    } else {
        for p in &session.outputs {
            println!("\n输出: {}", p.display());
        }
    }
    Ok(())
}
