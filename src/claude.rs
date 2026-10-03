//! Claude Code `claude -p` with stream-json input/output and stdio control
//! requests (initialize, interrupt, can_use_tool).
use crate::{
    session::{Session, WireError},
    ApprovalDecision, ApprovalRequest, Spec,
};
use serde_json::{json, Value};

/// The permission mode Claude should report for this policy.
pub fn expected_mode(spec: &Spec) -> &'static str {
    let p = &spec.policy;
    if p.approvals == "auto" {
        "auto"
    } else if p.mode == "read-only" {
        "default"
    } else if p.mode == "full" && p.approvals == "never" {
        "bypassPermissions"
    } else if p.approvals == "untrusted" {
        "default"
    } else {
        "acceptEdits"
    }
}
/// The `--settings` JSON: no hooks, connectors, plugins or memory, and Claude's
/// sandbox unless the process is already confined or fully trusted.
pub fn settings(spec: &Spec) -> Value {
    let p = &spec.policy;
    let sandbox = if spec.confined || p.mode == "full" {
        json!({"enabled":false})
    } else {
        json!({"enabled":true,"failIfUnavailable":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":p.approvals!="never","excludedCommands":[],"network":{"allowedDomains":p.network,"allowLocalBinding":false}})
    };
    json!({"disableAllHooks":true,"disableClaudeAiConnectors":true,"enabledPlugins":{},"autoMemoryEnabled":false,"sandbox":sandbox})
}
/// The `claude` command line; `fork_from` copies that session into a new one,
/// `resume` continues a session, otherwise a new session starts with ID
/// `session`.
pub fn command(spec: &Spec, resume: &str, session: &str, fork_from: &str) -> Vec<String> {
    let p = &spec.policy;
    let mut allowed = "Read,Glob,Grep";
    let mut mode = "default";
    let tools = if p.mode == "read-only" {
        "Read,Glob,Grep"
    } else {
        if spec.confined {
            allowed = "Bash,Read,Glob,Grep";
        }
        mode = if p.approvals == "untrusted" {
            "default"
        } else {
            "acceptEdits"
        };
        if p.mode == "full" && p.approvals == "never" {
            mode = "bypassPermissions";
        }
        "Bash,Read,Glob,Grep,Edit,Write"
    };
    if p.approvals == "auto" {
        mode = "auto";
    }
    let mut args = vec![
        "claude".into(),
        "-p".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--setting-sources".into(),
        "".into(),
        "--settings".into(),
        settings(spec).to_string(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        "{\"mcpServers\":{}}".into(),
        "--disable-slash-commands".into(),
        "--no-chrome".into(),
        "--permission-mode".into(),
        mode.into(),
        "--tools".into(),
        tools.into(),
        "--allowedTools".into(),
        allowed.into(),
        "--append-system-prompt".into(),
        spec.instructions.clone(),
    ];
    if p.approvals == "never" || p.claude_prompts == "none" {
        args.extend(["--permission-prompts".into(), "none".into()]);
    } else {
        args.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    }
    if !fork_from.is_empty() {
        args.extend(["--resume".into(), fork_from.into(), "--fork-session".into()]);
    } else if resume.is_empty() {
        args.extend(["--session-id".into(), session.into()]);
    } else {
        args.extend(["--resume".into(), resume.into()]);
    }
    if !spec.model.is_empty() {
        args.extend(["--model".into(), spec.model.clone()]);
    }
    if !spec.reasoning_effort.is_empty() {
        args.extend(["--effort".into(), spec.reasoning_effort.clone()]);
    }
    args
}
pub(crate) async fn handshake(s: &mut Session) -> Result<(), WireError> {
    let id = s.next_id();
    let request_id = s.request_id(id);
    s.send(json!({"type":"control_request","request_id":request_id,"request":{"subtype":"initialize","hooks":null}})).await
}
pub(crate) async fn job(s: &mut Session, prompt: &str) -> Result<String, WireError> {
    s.send(
        json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":prompt}]}}),
    )
    .await?;
    s.in_turn = true;
    if s.interrupted {
        s.send_interrupt().await?;
    }
    // A usage limit: Claude Code says so in a rejected rate_limit_event (with
    // its reset time) and an assistant message with error "rate_limit", then
    // ends the turn with an error result carrying HTTP status 429.
    let mut limited: Option<Option<u64>> = None;
    loop {
        // A turn that ends without a result after the limit was hit (the
        // stream closes, the CLI exits) is still the limit, not a failure to
        // retry at once.
        let m = match s.receive(false).await {
            Ok(m) => m,
            Err(e)
                if limited.is_some()
                    && !matches!(
                        e.kind,
                        crate::Failure::Interrupted | crate::Failure::Cancelled
                    ) =>
            {
                return Err(WireError::rate_limited(
                    "claude_rate_limited",
                    limited.flatten(),
                ));
            }
            Err(e) => return Err(e),
        };
        let kind = m["type"].as_str().unwrap_or("");
        if kind == "rate_limit_event" && m["rate_limit_info"]["status"] == "rejected" {
            // Seconds since the epoch; a fractional value is rounded down.
            let reset = m["rate_limit_info"]["resetsAt"]
                .as_f64()
                .filter(|t| t.is_finite() && *t > 0.)
                .map(|t| t as u64);
            limited = Some(reset.or(limited.flatten()));
        }
        if kind == "assistant" && m["error"] == "rate_limit" {
            limited = Some(limited.flatten());
        }
        if ["system", "result"].contains(&kind) {
            if let Some(id) = m["session_id"].as_str() {
                s.session = id.into();
            }
        }
        if kind == "system" && m["subtype"] == "init" {
            if let Some(actual) = m["permissionMode"].as_str().filter(|s| !s.is_empty()) {
                let expected = expected_mode(&s.spec);
                if actual != expected {
                    s.notice(json!({"code":"claude_permission_mode_fallback","expected":expected,"actual":actual.chars().take(64).collect::<String>()})).await?;
                }
            }
        }
        match kind {
            "control_request" => control(s, m).await?,
            "result" => {
                s.in_turn = false;
                if m["is_error"].as_bool() == Some(true) {
                    if m["api_error_status"] == 429 {
                        limited = Some(limited.flatten());
                    }
                    return Err(if s.interrupted {
                        WireError::interrupted()
                    } else if let Some(retry_at) = limited {
                        WireError::rate_limited("claude_rate_limited", retry_at)
                    } else {
                        WireError::execution("claude_job_failed")
                    });
                }
                return Ok(m["result"].as_str().unwrap_or("").into());
            }
            _ => {}
        }
    }
}
/// The approval request for a `can_use_tool` control request.
pub fn describe(r: &Value, id: &Value) -> (ApprovalRequest, Value) {
    let tool = r["tool_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("tool");
    let input = if r["input"].is_object() {
        r["input"].clone()
    } else {
        json!({})
    };
    let target = ["command", "file_path", "path"]
        .into_iter()
        .find_map(|k| input[k].as_str().filter(|s| !s.is_empty()))
        .unwrap_or("");
    let request = ApprovalRequest {
        kind: if tool == "Bash" { "command" } else { "tool" }.into(),
        summary: if target.is_empty() {
            format!("Use {tool}")
        } else {
            format!("{tool}: {}", target.chars().take(300).collect::<String>())
        },
        detail: json!({"tool":tool,"input":input,"reason":r.get("description").filter(|v|!v.is_null()).unwrap_or(&r["decision_reason"])}),
        backend_request_id: id
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_string()),
        cache_key: if target.is_empty() {
            String::new()
        } else {
            format!("{tool}:{target}")
        },
    };
    (request, input)
}
async fn control(s: &mut Session, m: Value) -> Result<(), WireError> {
    let r = &m["request"];
    let id = &m["request_id"];
    if r["subtype"] != "can_use_tool" {
        let error = format!("not supported by {}", s.options.client.title);
        return s.send(json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":error}})).await;
    }
    let (request, input) = describe(r, id);
    let decision = s.approve(request).await?;
    let response = if decision == ApprovalDecision::Deny {
        json!({"behavior":"deny","message":"The owner declined this action; continue without it."})
    } else {
        json!({"behavior":"allow","updatedInput":input})
    };
    s.send(json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}})).await
}
