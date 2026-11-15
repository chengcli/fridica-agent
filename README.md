# fridica-agent

Drive [Claude Code](https://docs.claude.com/en/docs/claude-code) and
[Codex](https://github.com/openai/codex) agent sessions from Rust.

`fridica-agent` speaks `claude -p --input-format stream-json --output-format stream-json`
and `codex app-server` (JSON-RPC), and gives both the same interface:

- **One handle per session.** An `Agent` starts its backend on the first turn,
  keeps it for later turns, and stops it on failure, idle timeout or `close()`.
  Dropping a turn's future cancels the turn.
- **Approvals.** Command, tool, file-change and permission requests go to your
  `Approver`; "approve for session" answers are cached per session.
- **Interrupts.** `interrupt()` stops the current turn and keeps the session.
- **Resume.** A turn can resume a backend session; if the backend no longer has
  it, the turn runs once in a fresh session.
- **Fork.** A turn can fork a backend session (`fork_from`): the new session
  starts with a copy of the source's conversation under its own ID (Claude
  `--resume … --fork-session`, Codex `thread/fork`), and the source is left
  as it was. A source the backend no longer has fails the turn with
  `fork_source_missing`.
- **Structured answers.** With an `OutputFormat`, Codex gets your JSON schema
  and Claude gets a format note; if your `accept` check rejects the answer, one
  repair turn asks for it again.

It is the backend driver of [Fridica](https://github.com/chengcli/fridica), a
Slack agent that delegates work to Claude Code and Codex workers on your machines.

## What you supply

| Trait | Purpose |
|---|---|
| `Launcher` / `Child` | Start the backend command (locally, over SSH, in a sandbox) and stop it |
| `Recorder` | Receive every wire event before it takes effect, for replay or audit (`DiscardWire` ignores them) |
| `Approver` | Decide approval requests |
| `SessionIds` | New Claude session IDs (UUIDs) |

Policy (`Spec::policy`) maps onto each backend's own sandbox and permission
modes: `mode` is `read-only`, `write` or `full`; `approvals` is `never`,
`on-request`, `untrusted` or `auto`. Set `confined` when you already sandbox the
process yourself.

## Example

```rust,no_run
use fridica_agent::*;
use std::{io, sync::Arc, time::Duration};

/// Runs the command locally with tokio.
struct Local;
struct Proc(tokio::process::Child);
impl Child for Proc {
    fn take_stdin(&mut self) -> Option<Stdin> { self.0.stdin.take().map(|s| Box::new(s) as Stdin) }
    fn take_stdout(&mut self) -> Option<Output> { self.0.stdout.take().map(|s| Box::new(s) as Output) }
    fn take_stderr(&mut self) -> Option<Output> { self.0.stderr.take().map(|s| Box::new(s) as Output) }
    fn try_exit(&mut self) -> io::Result<Option<i32>> { Ok(self.0.try_wait()?.and_then(|s| s.code())) }
    fn eof_status(&mut self, wait: Duration) -> BoxFuture<'_, io::Result<Option<i32>>> {
        Box::pin(async move {
            Ok(tokio::time::timeout(wait, self.0.wait()).await.ok().transpose()?.and_then(|s| s.code()))
        })
    }
    fn terminate(&mut self, _grace: Duration) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { self.0.kill().await })
    }
}
impl Launcher for Local {
    fn launch(&self, command: Vec<String>) -> Result<Box<dyn Child>, LaunchError> {
        use std::process::Stdio;
        tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map(|c| Box::new(Proc(c)) as Box<dyn Child>)
            .map_err(|_| LaunchError::StartFailed)
    }
}
struct Ids;
impl SessionIds for Ids {
    fn new_session_id(&self) -> String { "4f1c2d8e-0000-4000-8000-000000000000".into() }
}
struct Ask;
impl Approver for Ask {
    fn request(&self, request: ApprovalRequest) -> BoxFuture<'_, ApprovalDecision> {
        println!("approve? {}", request.summary);
        Box::pin(async { ApprovalDecision::Deny })
    }
}

async fn run() -> Result<(), Error> {
let spec = Spec {
    backend: Backend::Codex,
    instructions: "Answer briefly.".into(),
    model: String::new(),
    reasoning_effort: String::new(),
    cwd: "/tmp".into(),
    policy: Policy {
        mode: "read-only".into(),
        approvals: "on-request".into(),
        network: vec![],
        claude_prompts: "host".into(),
    },
    confined: false,
    forbid_mcp: false,
        mcp_servers: vec![],
    job_timeout: Duration::from_secs(600),
    idle_timeout: Duration::from_secs(1800),
    approval_timeout: Duration::from_secs(300),
};
let agent = Agent::new(spec, Options::default(), Arc::new(Local), Arc::new(Ids), Arc::new(DiscardWire))?;
let reply = agent
    .run(Turn {
        prompt: "What files are here?".into(),
        resume: String::new(),
        fork_from: String::new(),
        context: serde_json::json!({}),
        approver: Arc::new(Ask),
        output: None,
    })
    .await?;
println!("{} (session {})", reply.text, reply.backend_session_id);
agent.close().await?;
Ok(())
}
```

A production launcher should also start the backend in its own process group
and kill the whole group on `terminate`, so tools the agent started stop too.

## Compatibility

Tested against the Claude Code and Codex CLIs current at each release; both
protocols change with their CLIs, so pin the CLI versions you deploy.

## License

MIT

## Explicit MCP servers

Set `Spec::mcp_servers` to a list of `McpServer { name, command, args,
env_vars, auto_approve }` for stdio servers. The default deserialized list is empty. Only
variable names belong in `env_vars`; the execution target resolves their values.
Claude uses strict MCP configuration; Codex receives complete server entries.
The host must disable ambient Codex servers before applying these entries.
`forbid_mcp` rejects any explicit server before launch. `auto_approve` defaults to
false. Set it to true only when the host has preauthorized all tools exposed by
that server: this sets Codex's `default_tools_approval_mode = "approve"` and adds
Claude's server-specific `--allowedTools` rule. Other tool permission requests
continue through the existing `Approver`; the driver adds no GitHub-specific policy.
Changing MCP configuration requires a new agent handle, like other `Spec` changes.

Version 0.4 adds `mcp_servers` to Rust `Spec` literals; use `vec![]` to retain
existing behavior.
