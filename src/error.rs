//! Two levels of tool errors:
//! - InvalidArg: bad arguments or input. The details go back to the model so it
//!   can fix the arguments and retry.
//! - FatalError: environment-level failure (ffmpeg died, disk full). Ends the
//!   turn instead of letting the model guess its way forward.

use anyhow::Error as AnyError;
use serde_json::Value;

#[derive(Debug)]
pub struct InvalidArg {
    pub message: String,
}

impl std::fmt::Display for InvalidArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InvalidArg {}

#[derive(Debug)]
pub struct FatalError {
    pub message: String,
}

impl std::fmt::Display for FatalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FatalError {}

pub fn invalid(message: impl Into<String>) -> AnyError {
    AnyError::new(InvalidArg {
        message: message.into(),
    })
}

pub fn fatal(message: impl Into<String>) -> AnyError {
    AnyError::new(FatalError {
        message: message.into(),
    })
}

/// Canonical format for argument errors: which field is wrong, what the model
/// sent, and how to fix it. This text is the model's only way to self-correct,
/// so the format needs to stay stable. User/LLM-facing, so Chinese.
pub fn invalid_args(tool: &str, issue: impl AsRef<str>, args: &Value) -> AnyError {
    invalid(format!(
        "工具 \"{tool}\" 参数不合法：\n- {}\n\n实际传入的参数：\n{}\n\n请修正参数后重新调用该工具。",
        issue.as_ref(),
        serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string())
    ))
}

/// Squash a serde error into a one-line issue, naming the offending field when
/// possible.
pub fn describe_serde(err: &serde_json::Error) -> String {
    let msg = err.to_string();
    if let Some(rest) = msg.strip_prefix("unknown field `") {
        if let Some((field, tail)) = rest.split_once('`') {
            let expected = tail
                .trim_start_matches(", expected ")
                .trim()
                .trim_start_matches("one of ")
                .trim();
            return format!("unknown field `{field}` (allowed: {expected})");
        }
    }
    msg
}
