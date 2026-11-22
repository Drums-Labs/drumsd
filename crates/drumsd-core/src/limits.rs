//! The step-argument limit, stated once. The same numbers hold for an action (`POST /v1/slots/{id}/step`)
//! and for a task's setup and postconfig steps (`config`, `evaluator.postconfig`), and they are the Drums gateway's:
//!
//! * one command string (a `command` given as a string, or one element of a `command` list): at most
//!   [`STEP_ARG_MAX_CHARS`] characters;
//! * a `command` list: at most [`STEP_ARGV_MAX_ITEMS`] elements;
//! * pyautogui code: at most [`PYAUTOGUI_CODE_MAX_CHARS`] characters.
//!
//! A file larger than one argument is written in chunks (the task factory writes 8,000 base64 characters per step).

use serde_json::{json, Value};

pub const STEP_ARG_MAX_CHARS: usize = 8192;
pub const STEP_ARGV_MAX_ITEMS: usize = 256;
pub const PYAUTOGUI_CODE_MAX_CHARS: usize = 16384;

#[derive(Debug, Clone, PartialEq)]
pub struct LimitError {
    /// Where, as a dotted path (`action.command.5`, `task.config.3.parameters.command.2`).
    pub field: String,
    pub limit: usize,
    pub length: usize,
    /// "characters" or "items".
    pub unit: &'static str,
}

impl LimitError {
    pub fn message(&self) -> String {
        format!(
            "{} is {} {}; the step-argument limit is {} {} (docs: README, API, step-argument limit; write larger files in chunks)",
            self.field, self.length, self.unit, self.limit, self.unit
        )
    }
    /// The fields of the 400 body.
    pub fn body(&self) -> Value {
        json!({"code": "step_arg_too_long", "field": self.field, "limit": self.limit, "length": self.length, "unit": self.unit})
    }
}

fn chars(s: &str) -> usize {
    s.chars().count()
}

fn check_command(cmd: &Value, field: &str) -> Result<(), LimitError> {
    match cmd {
        Value::String(s) if chars(s) > STEP_ARG_MAX_CHARS => {
            Err(LimitError { field: field.into(), limit: STEP_ARG_MAX_CHARS, length: chars(s), unit: "characters" })
        }
        Value::Array(items) => {
            if items.len() > STEP_ARGV_MAX_ITEMS {
                return Err(LimitError { field: field.into(), limit: STEP_ARGV_MAX_ITEMS, length: items.len(), unit: "items" });
            }
            for (i, it) in items.iter().enumerate() {
                if let Some(s) = it.as_str() {
                    if chars(s) > STEP_ARG_MAX_CHARS {
                        return Err(LimitError { field: format!("{field}.{i}"), limit: STEP_ARG_MAX_CHARS, length: chars(s), unit: "characters" });
                    }
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// An action as `POST /v1/slots/{id}/step` takes it: a pyautogui string, or an action object.
pub fn check_action(a: &Value, field: &str) -> Result<(), LimitError> {
    let code = |s: &str, f: String| {
        if chars(s) > PYAUTOGUI_CODE_MAX_CHARS {
            Err(LimitError { field: f, limit: PYAUTOGUI_CODE_MAX_CHARS, length: chars(s), unit: "characters" })
        } else {
            Ok(())
        }
    };
    match a {
        Value::String(s) => code(s, field.into()),
        Value::Object(o) => match o.get("type").and_then(|t| t.as_str()).unwrap_or("execute") {
            "pyautogui" => code(o.get("code").and_then(|c| c.as_str()).unwrap_or(""), format!("{field}.code")),
            _ => o.get("command").map(|c| check_command(c, &format!("{field}.command"))).unwrap_or(Ok(())),
        },
        _ => Ok(()),
    }
}

/// A task's setup (`config`) and postconfig steps: every step with a `command` parameter (OSWorld's `execute`,
/// `launch`, `command`, ...) is held to the same limit as an action.
pub fn check_task_steps(task: &Value) -> Result<(), LimitError> {
    let lists = [("task.config", task.get("config")), ("task.evaluator.postconfig", task.get("evaluator").and_then(|e| e.get("postconfig")))];
    for (name, list) in lists {
        for (i, step) in list.and_then(|l| l.as_array()).into_iter().flatten().enumerate() {
            if let Some(c) = step.get("parameters").and_then(|p| p.get("command")) {
                check_command(c, &format!("{name}.{i}.parameters.command"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_name_the_field_the_limit_and_the_length() {
        let long = "x".repeat(STEP_ARG_MAX_CHARS + 1);
        let e = check_action(&json!({"type": "execute", "command": ["bash", "-c", "a", "b", "c", long]}), "action").unwrap_err();
        assert_eq!((e.field.as_str(), e.limit, e.length), ("action.command.5", 8192, 8193));
        assert!(e.message().contains("action.command.5 is 8193 characters; the step-argument limit is 8192 characters"));
        assert_eq!(e.body()["code"], "step_arg_too_long");
        assert!(check_action(&json!({"type": "launch", "command": "x".repeat(8192)}), "action").is_ok(), "the limit itself is allowed");
        assert!(check_action(&json!({"type": "launch", "command": long}), "action").is_err());
        let many: Vec<String> = (0..257).map(|i| i.to_string()).collect();
        assert_eq!(check_action(&json!({"command": many}), "action").unwrap_err().unit, "items");
        assert!(check_action(&json!("y".repeat(16384)), "action").is_ok());
        assert_eq!(check_action(&json!({"type": "pyautogui", "code": "y".repeat(16385)}), "action").unwrap_err().field, "action.code");
        assert!(check_action(&json!({"type": "sleep", "seconds": 1}), "action").is_ok());
        // characters, not bytes
        assert!(check_action(&json!({"command": ["é".repeat(8192)]}), "action").is_ok());
    }

    #[test]
    fn setup_and_postconfig_steps_have_the_same_limit() {
        let ok = "a".repeat(8000);
        let long = "a".repeat(60_000);
        let t = json!({"config": [{"type": "execute", "parameters": {"command": ["python3", "-c", "w", "/p", ok]}},
                                  {"type": "sleep", "parameters": {"seconds": 1}}]});
        assert!(check_task_steps(&t).is_ok());
        let t =
            json!({"config": [{"type": "sleep", "parameters": {"seconds": 1}}, {"type": "execute", "parameters": {"command": ["python3", "-c", "w", long]}}]});
        let e = check_task_steps(&t).unwrap_err();
        assert_eq!((e.field.as_str(), e.length), ("task.config.1.parameters.command.3", 60_000));
        let t = json!({"config": [], "evaluator": {"postconfig": [{"type": "command", "parameters": {"command": long}}]}});
        assert_eq!(check_task_steps(&t).unwrap_err().field, "task.evaluator.postconfig.0.parameters.command");
    }
}
