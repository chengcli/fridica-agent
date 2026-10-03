//! The agent against a scripted in-memory backend: no processes, no network.
use fridica_agent::*;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

/// Replies to each message the driver writes with scripted lines.
type Script = Arc<dyn Fn(&Value) -> Vec<Value> + Send + Sync>;
struct Scripted {
    stdin: Option<DuplexStream>,
    stdout: Option<DuplexStream>,
    stderr: Option<DuplexStream>,
}
impl Scripted {
    fn start(script: Script) -> Self {
        let (stdin, mut input) = duplex(1 << 20);
        let (mut output, stdout) = duplex(1 << 20);
        let (_, stderr) = duplex(64);
        tokio::spawn(async move {
            let (read, _) = tokio::io::split(&mut input);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                for reply in script(&message) {
                    // `"__eof"` closes the output, as when the CLI exits.
                    if reply == json!("__eof") {
                        return;
                    }
                    let mut bytes = serde_json::to_vec(&reply).unwrap();
                    bytes.push(b'\n');
                    output.write_all(&bytes).await.unwrap();
                }
            }
        });
        Self {
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
        }
    }
}
impl Child for Scripted {
    fn take_stdin(&mut self) -> Option<Stdin> {
        self.stdin.take().map(|s| Box::new(s) as Stdin)
    }
    fn take_stdout(&mut self) -> Option<fridica_agent::Output> {
        self.stdout
            .take()
            .map(|s| Box::new(s) as fridica_agent::Output)
    }
    fn take_stderr(&mut self) -> Option<fridica_agent::Output> {
        self.stderr
            .take()
            .map(|s| Box::new(s) as fridica_agent::Output)
    }
    fn try_exit(&mut self) -> io::Result<Option<i32>> {
        Ok(None)
    }
    fn eof_status(&mut self, _: Duration) -> BoxFuture<'_, io::Result<Option<i32>>> {
        Box::pin(async { Ok(Some(0)) })
    }
    fn terminate(&mut self, _: Duration) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
struct Launch {
    script: Script,
    commands: Mutex<Vec<Vec<String>>>,
}
impl Launcher for Launch {
    fn launch(&self, command: Vec<String>) -> Result<Box<dyn Child>, LaunchError> {
        self.commands.lock().unwrap().push(command);
        Ok(Box::new(Scripted::start(self.script.clone())))
    }
}
struct Ids;
impl SessionIds for Ids {
    fn new_session_id(&self) -> String {
        "00000000-0000-4000-8000-000000000001".into()
    }
}
struct Decide(ApprovalDecision, Mutex<Vec<ApprovalRequest>>);
impl Approver for Decide {
    fn request(&self, request: ApprovalRequest) -> BoxFuture<'_, ApprovalDecision> {
        self.1.lock().unwrap().push(request);
        Box::pin(async move { self.0 })
    }
}
#[derive(Default)]
struct Log(Mutex<Vec<Value>>);
impl Recorder for Log {
    fn record(&self, _: Value, event: Value) -> BoxFuture<'_, Result<(), Error>> {
        self.0.lock().unwrap().push(event);
        Box::pin(async { Ok(()) })
    }
}
fn spec(backend: Backend) -> Spec {
    Spec {
        backend,
        instructions: "Be brief.".into(),
        model: String::new(),
        reasoning_effort: String::new(),
        cwd: "/work".into(),
        policy: Policy {
            mode: "write".into(),
            approvals: "on-request".into(),
            network: vec![],
            claude_prompts: "host".into(),
        },
        confined: false,
        forbid_mcp: false,
        job_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        approval_timeout: Duration::from_secs(5),
    }
}
fn turn(prompt: &str, approver: Arc<dyn Approver>, output: Option<OutputFormat>) -> Turn {
    Turn {
        prompt: prompt.into(),
        resume: String::new(),
        fork_from: String::new(),
        context: json!({"job":"j1"}),
        approver,
        output,
    }
}
fn json_answer() -> OutputFormat {
    OutputFormat {
        schema: json!({"type":"object"}),
        note: "Answer in JSON.".into(),
        repair: "Return only the JSON.".into(),
        accept: Arc::new(|text| serde_json::from_str::<Value>(text).is_ok_and(|v| v.is_object())),
    }
}

/// A Claude backend that asks to run `ls` once, then answers each prompt in turn.
fn claude(answers: Vec<&'static str>) -> Script {
    let answers = Arc::new(Mutex::new(VecDeque::from(answers)));
    let asked = Arc::new(Mutex::new(false));
    Arc::new(move |m| {
        let result = |text: &str| json!({"type":"result","session_id":"claude-1","is_error":false,"result":text});
        match m["type"].as_str() {
            Some("user") if !std::mem::replace(&mut *asked.lock().unwrap(), true) => vec![
                json!({"type":"system","subtype":"init","session_id":"claude-1","permissionMode":"acceptEdits"}),
                json!({"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}),
            ],
            Some("user") => vec![result(answers.lock().unwrap().pop_front().unwrap())],
            Some("control_response") => vec![result(answers.lock().unwrap().pop_front().unwrap())],
            _ => vec![],
        }
    })
}

#[tokio::test]
async fn claude_turn_asks_for_approval_and_repairs_an_unstructured_answer() {
    let launch = Arc::new(Launch {
        script: claude(vec!["Done, I listed the files.", "{\"files\":2}"]),
        commands: Mutex::new(vec![]),
    });
    let log = Arc::new(Log::default());
    let agent = Agent::new(
        spec(Backend::Claude),
        Options::default(),
        launch.clone(),
        Arc::new(Ids),
        log.clone(),
    )
    .unwrap();
    let approver = Arc::new(Decide(ApprovalDecision::Once, Mutex::new(vec![])));
    let reply = agent
        .run(turn(
            "List the files.",
            approver.clone(),
            Some(json_answer()),
        ))
        .await
        .unwrap();
    assert_eq!(reply.text, "{\"files\":2}");
    assert!(reply.accepted);
    assert_eq!(reply.backend_session_id, "claude-1");
    let requests = approver.1.lock().unwrap().clone();
    assert_eq!(
        (requests[0].kind.as_str(), requests[0].summary.as_str()),
        ("command", "Bash: ls")
    );
    // A new session with the generated ID; the note is appended for Claude.
    let command = launch.commands.lock().unwrap()[0].clone();
    assert!(command
        .windows(2)
        .any(|w| w == ["--session-id", "00000000-0000-4000-8000-000000000001"]));
    let sent: Vec<_> = log
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e["direction"] == "send_intent")
        .map(|e| e["message"].clone())
        .collect();
    assert_eq!(
        sent[1]["message"]["content"][0]["text"],
        "List the files.\n\nAnswer in JSON."
    );
    assert_eq!(sent[0]["request_id"], "fridica-agent-1");
    assert_eq!(sent[2]["response"]["response"]["behavior"], "allow");
    assert_eq!(
        sent[3]["message"]["content"][0]["text"],
        "Return only the JSON."
    );
    agent.close().await.unwrap();
    assert!(!agent.alive());
}

/// A Codex app-server that answers initialize, thread/start and turn/start.
fn codex(text: &'static str) -> Script {
    Arc::new(move |m| {
        let id = m["id"].clone();
        match m["method"].as_str() {
            Some("initialize") => vec![json!({"id":id,"result":{}})],
            Some("thread/start") => vec![json!({"id":id,"result":{"thread":{"id":"thread-1"}}})],
            Some("turn/start") => vec![
                json!({"id":id,"result":{"turn":{"id":"turn-1"}}}),
                json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{"type":"agentMessage","text":text}}}),
                json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}),
            ],
            _ => vec![],
        }
    })
}

#[tokio::test]
async fn codex_turn_starts_a_thread_and_sends_the_output_schema() {
    let launch = Arc::new(Launch {
        script: codex("{\"ok\":true}"),
        commands: Mutex::new(vec![]),
    });
    let log = Arc::new(Log::default());
    let options = Options {
        disabled_mcp_servers: vec!["me".into()],
        ..Options::default()
    };
    let agent = Agent::new(
        spec(Backend::Codex),
        options,
        launch.clone(),
        Arc::new(Ids),
        log.clone(),
    )
    .unwrap();
    let approver = Arc::new(Decide(ApprovalDecision::Deny, Mutex::new(vec![])));
    let reply = agent
        .run(turn("Check.", approver, Some(json_answer())))
        .await
        .unwrap();
    assert_eq!(
        (
            reply.text.as_str(),
            reply.accepted,
            reply.backend_session_id.as_str()
        ),
        ("{\"ok\":true}", true, "thread-1")
    );
    let command = launch.commands.lock().unwrap()[0].clone();
    assert_eq!(&command[..2], ["codex", "app-server"]);
    assert!(command.contains(&"mcp_servers.\"me\".enabled=false".to_string()));
    let sent: Vec<_> = log
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e["direction"] == "send_intent")
        .map(|e| e["message"].clone())
        .collect();
    assert_eq!(sent[0]["params"]["clientInfo"]["name"], "fridica-agent");
    let start = sent.iter().find(|m| m["method"] == "thread/start").unwrap();
    assert_eq!(
        (
            start["params"]["cwd"].clone(),
            start["params"]["developerInstructions"].clone()
        ),
        (json!("/work"), json!("Be brief."))
    );
    let turn = sent.iter().find(|m| m["method"] == "turn/start").unwrap();
    assert_eq!(turn["params"]["outputSchema"], json!({"type":"object"}));
    agent.close().await.unwrap();
}

#[tokio::test]
async fn invalid_settings_are_refused() {
    let launch = Arc::new(Launch {
        script: codex("x"),
        commands: Mutex::new(vec![]),
    });
    let mut bad = spec(Backend::Codex);
    bad.job_timeout = Duration::ZERO;
    let error = Agent::new(
        bad,
        Options::default(),
        launch,
        Arc::new(Ids),
        Arc::new(DiscardWire),
    )
    .err()
    .unwrap();
    assert_eq!(
        (error.kind, error.code.as_str()),
        (Failure::Refusal, "invalid_backend_configuration")
    );
}

#[tokio::test]
async fn a_claude_fork_resumes_the_source_with_fork_session() {
    let launch = Arc::new(Launch {
        script: claude(vec!["Forked.", "{\"ok\":true}"]),
        commands: Mutex::new(vec![]),
    });
    let log = Arc::new(Log::default());
    let agent = Agent::new(
        spec(Backend::Claude),
        Options::default(),
        launch.clone(),
        Arc::new(Ids),
        log.clone(),
    )
    .unwrap();
    let approver = Arc::new(Decide(ApprovalDecision::Once, Mutex::new(vec![])));
    let mut t = turn("Continue.", approver, Some(json_answer()));
    t.fork_from = "claude-src".into();
    let reply = agent.run(t).await.unwrap();
    // The fork gets the ID the backend reports, not the source's.
    assert_eq!(reply.backend_session_id, "claude-1");
    let command = launch.commands.lock().unwrap()[0].clone();
    assert!(
        command
            .windows(3)
            .any(|w| w == ["--resume", "claude-src", "--fork-session"]),
        "{command:?}"
    );
    assert!(!command.contains(&"--session-id".to_string()));
    let start = log.0.lock().unwrap()[0].clone();
    assert_eq!(
        (
            start["direction"].clone(),
            start["resume"].clone(),
            start["fork_from"].clone()
        ),
        (json!("start"), json!(""), json!("claude-src"))
    );
    agent.close().await.unwrap();
}

/// A Codex app-server whose thread/fork answers with `fork`: a new thread,
/// or an error naming the missing source.
fn codex_forking(fork: Value) -> Script {
    Arc::new(move |m| {
        let id = m["id"].clone();
        match m["method"].as_str() {
            Some("initialize") => vec![json!({"id":id,"result":{}})],
            Some("thread/start") => vec![json!({"id":id,"result":{"thread":{"id":"thread-1"}}})],
            Some("thread/fork") => {
                let mut reply = fork.clone();
                reply["id"] = id;
                vec![reply]
            }
            Some("turn/start") => vec![
                json!({"id":id,"result":{"turn":{"id":"turn-1"}}}),
                json!({"method":"item/completed","params":{"threadId":"thread-2","turnId":"turn-1","item":{"type":"agentMessage","text":"{\"ok\":true}"}}}),
                json!({"method":"turn/completed","params":{"threadId":"thread-2","turn":{"id":"turn-1","status":"completed"}}}),
            ],
            _ => vec![],
        }
    })
}

#[tokio::test]
async fn a_codex_fork_calls_thread_fork_and_reports_the_new_thread() {
    let launch = Arc::new(Launch {
        script: codex_forking(json!({"result":{"thread":{"id":"thread-2"}}})),
        commands: Mutex::new(vec![]),
    });
    let log = Arc::new(Log::default());
    let agent = Agent::new(
        spec(Backend::Codex),
        Options::default(),
        launch.clone(),
        Arc::new(Ids),
        log.clone(),
    )
    .unwrap();
    let approver = Arc::new(Decide(ApprovalDecision::Deny, Mutex::new(vec![])));
    let mut t = turn("Continue.", approver, Some(json_answer()));
    t.fork_from = "thread-src".into();
    let reply = agent.run(t).await.unwrap();
    assert_eq!(reply.backend_session_id, "thread-2");
    let sent: Vec<_> = log
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e["direction"] == "send_intent")
        .map(|e| e["message"].clone())
        .collect();
    let fork = sent.iter().find(|m| m["method"] == "thread/fork").unwrap();
    assert_eq!(fork["params"]["threadId"], "thread-src");
    assert_eq!(fork["params"]["cwd"], "/work");
    assert!(!sent
        .iter()
        .any(|m| m["method"] == "thread/start" || m["method"] == "thread/resume"));
    agent.close().await.unwrap();
}

#[tokio::test]
async fn a_fork_whose_source_is_gone_fails_instead_of_starting_fresh() {
    let launch = Arc::new(Launch {
        script: codex_forking(
            json!({"error":{"code":-32600,"message":"no rollout found for thread id thread-src"}}),
        ),
        commands: Mutex::new(vec![]),
    });
    let agent = Agent::new(
        spec(Backend::Codex),
        Options::default(),
        launch.clone(),
        Arc::new(Ids),
        Arc::new(Log::default()),
    )
    .unwrap();
    let approver = Arc::new(Decide(ApprovalDecision::Deny, Mutex::new(vec![])));
    let mut t = turn("Continue.", approver.clone(), Some(json_answer()));
    t.fork_from = "thread-src".into();
    let error = agent.run(t).await.unwrap_err();
    assert_eq!(
        (error.kind, error.code.as_str()),
        (Failure::Execution, "fork_source_missing")
    );
    // One launch only: no silent retry in a fresh session.
    assert_eq!(launch.commands.lock().unwrap().len(), 1);
    assert!(!agent.alive());
    // Forking and resuming at once is contradictory.
    let mut t = turn("Continue.", approver, None);
    t.fork_from = "thread-src".into();
    t.resume = "thread-1".into();
    let error = agent.run(t).await.unwrap_err();
    assert_eq!(
        (error.kind, error.code.as_str()),
        (Failure::Refusal, "fork_with_resume")
    );
    agent.close().await.unwrap();
}

/// A usage limit is a `RateLimited` failure with its reset time when the
/// backend gives one, for Claude (rate_limit_event + 429 result) and Codex
/// (usageLimitExceeded), and is not confused with an ordinary failure.
#[tokio::test]
async fn usage_limits_are_rate_limited_failures() {
    let limited: Script = Arc::new(|m| match m["type"].as_str() {
        Some("user") => vec![
            json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","utilization":0.99,"resetsAt":1790973000u64}}),
            json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790973000u64}}),
            json!({"type":"assistant","error":"rate_limit","message":{"content":[]}}),
            json!({"type":"result","session_id":"claude-1","is_error":true,"api_error_status":429,"result":"You've hit your session limit"}),
        ],
        _ => vec![],
    });
    // The limit hit, then the CLI exits without a result message.
    let cut_off: Script = Arc::new(|m| match m["type"].as_str() {
        Some("user") => vec![
            json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790973000.5}}),
            json!("__eof"),
        ],
        _ => vec![],
    });
    let failing: Script = Arc::new(|m| match m["type"].as_str() {
        Some("user") => {
            vec![json!({"type":"result","session_id":"claude-1","is_error":true,"result":"boom"})]
        }
        _ => vec![],
    });
    let codex_limited: Script = Arc::new(|m| {
        let id = m["id"].clone();
        match m["method"].as_str() {
            Some("initialize") => vec![json!({"id":id,"result":{}})],
            Some("thread/start") => vec![json!({"id":id,"result":{"thread":{"id":"thread-1"}}})],
            Some("turn/start") => vec![
                json!({"id":id,"result":{"turn":{"id":"turn-1"}}}),
                json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"failed","error":{"message":"usage limit","codexErrorInfo":"usageLimitExceeded"}}}}),
            ],
            _ => vec![],
        }
    });
    for (backend, script, kind, code) in [
        (
            Backend::Claude,
            limited,
            Failure::RateLimited {
                retry_at: Some(1790973000),
            },
            "claude_rate_limited",
        ),
        (
            Backend::Claude,
            cut_off,
            Failure::RateLimited {
                retry_at: Some(1790973000),
            },
            "claude_rate_limited",
        ),
        (
            Backend::Claude,
            failing,
            Failure::Execution,
            "claude_job_failed",
        ),
        (
            Backend::Codex,
            codex_limited,
            Failure::RateLimited { retry_at: None },
            "codex_rate_limited",
        ),
    ] {
        let agent = Agent::new(
            spec(backend),
            Options::default(),
            Arc::new(Launch {
                script,
                commands: Mutex::new(vec![]),
            }),
            Arc::new(Ids),
            Arc::new(Log::default()),
        )
        .unwrap();
        let approver = Arc::new(Decide(ApprovalDecision::Deny, Mutex::new(vec![])));
        let error = agent.run(turn("Go.", approver, None)).await.err().unwrap();
        assert_eq!((error.kind, error.code.as_str()), (kind, code));
        let _ = agent.close().await;
    }
}
