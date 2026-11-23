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

## GitHub capabilities

Enable `fridica-agent = { version = "0.4.1", features = ["github"] }` to use
`fridica_agent::github::{Options, run, Server, serve}`. The default feature set
keeps GitHub dependencies disabled. Hosts supply CLI parsing and decide which
workers receive these tools; `Options::default()` uses `GH_TOKEN` with local
`gh auth token` fallback. Set `read_only` to hide and reject writes.

The module supports PR reads, CI checks, reviews (including approvals and change
requests), comments, branches, file changes, and local Git context using octocrab
and gitoxide. See [GitHub module details](src/github/README.md) for authentication,
write semantics, and tests. Fridica packages this as `fridica github-mcp`.

## Literature research with alphaXiv

Version 0.4.3 renames the `search` feature/module to `alphaxiv` and the
`fridica-agent-search` executable to `fridica-agent-alphaxiv`. Update dependency
features, Rust imports, and configured executable names when upgrading.

The optional `alphaxiv` feature adds `fridica_agent::alphaxiv` and the Rust
`fridica-agent-alphaxiv` executable. It bridges stdio MCP to
[alphaXiv's Streamable HTTP service](https://www.alphaxiv.org/docs/mcp) at
`https://api.alphaxiv.org/mcp/v1`; no Node or Python bridge is required.

Install on each execution target (from this checkout before 0.4.3 is published):

```sh
cargo install --path . --features alphaxiv --bin fridica-agent-alphaxiv
# After publication:
# cargo install fridica-agent --version 0.4.3 --features alphaxiv --bin fridica-agent-alphaxiv
```

Create an API key in alphaXiv Settings > API Keys and set `ALPHAXIV_TOKEN` in
the worker's environment. Headless operation uses that key, not browser OAuth.
Allow network access to `api.alphaxiv.org`. Never put the key in the agent's
instructions or MCP arguments. The host must make the named variable available
on the target; this library does not transfer credentials to SSH workers.

Enable `features = ["alphaxiv"]` in your `fridica-agent` dependency, then attach the
server to a worker's specification before creating the agent:

```rust
#[cfg(feature = "alphaxiv")]
fn enable_research(spec: &mut fridica_agent::Spec) {
    spec.mcp_servers.push(fridica_agent::alphaxiv::alphaxiv("fridica-agent-alphaxiv"));
    spec.instructions.push('\n');
    spec.instructions.push_str(fridica_agent::alphaxiv::INSTRUCTIONS);
}
```

This uses the existing explicit MCP path for both Claude Code and Codex.
`forbid_mcp` must be false. Tool approval remains under the host's existing
approval policy (`auto_approve` defaults to false). The feature alone does not
attach tools to sessions; the host controls placement and enablement.

The bridge discovers the upstream schemas and exposes only paper discovery,
paper content, PDF queries, paper codebase reads, and researcher lookup tools.
Library edits and researcher follow/unfollow tools are neither advertised nor
callable. Schemas and returned citations come from alphaXiv. Its default paper
content is an AI-generated report; use `fullText=true` or PDF queries to verify
claims against paper text. Search and analysis consume alphaXiv quota.

Initialization/discovery is bounded to 30 seconds and calls to 180 seconds.
HTTP redirects and automatic POST retries/session replay are disabled; expired
sessions require restarting the bridge. MCP cancellation is forwarded upstream.
The SDK handles JSON/SSE responses and session cleanup. Connection errors use
fixed diagnostics rather than printing credentials or upstream response bodies.

Run `cargo test --locked --features alphaxiv` for mock HTTP/SSE coverage. Live
alphaXiv authentication and quota-dependent searches require your own API key.

## Upgrading the Overleaf and NASA ADS (SciX) servers to 0.4.6

Version 0.4.6 bundles both stdio MCP servers in Rust. Existing worker specs
can keep their `overleaf` and `scix` server names and executable paths. To
replace the npm-backed binaries, run this from the 0.4.6 checkout on each
worker and ensure the installed binaries are on the worker's `PATH`:

```sh
cargo install --locked --path . --features overleaf,scix --bins
```

If the worker also uses the GitHub or alphaXiv doctor checks, include
`github` or `alphaxiv` in the same feature list when reinstalling.

- **Overleaf.** Migrate cookie or token-file setups to `OVERLEAF_GIT_TOKEN`
  in the worker environment. Replace `OVERLEAF_PROJECTS` aliases with a project
  link or ID per request, or set `OVERLEAF_PROJECT_ID` for a default. Use
  `INTERLEAF_WORKSPACE` for a custom checkout directory. The bundled server
  edits Git checkouts and drops the old cookie-based editor tools.
- **ADS.** Keep the worker's `ADS_TOKEN`. Update callers that pass
  `response_format` to omit it. Use `fridica-agent doctor --only scix` in
  place of the old `health_check` tool. The bundled server supports the
  [SciX tools listed below](#literature-research-with-nasa-scix).

## Literature research with NASA SciX

Enable the `scix` Cargo feature. `fridica-agent-scix` calls the
[ADS API](https://ui.adsabs.harvard.edu/help/api/) directly: no Node.js and no
download at first use. Install the Fridica binaries on each worker from this
checkout:

```sh
cargo install --locked --path . --features scix --bins
```

Set `ADS_TOKEN` in the worker environment (ADS, Account → Settings → API Token)
and allow network access to `api.adsabs.harvard.edu`.

```rust
#[cfg(feature = "scix")]
fn enable_scix(spec: &mut fridica_agent::Spec) {
    use fridica_agent::scix;
    spec.mcp_servers.push(scix::scix("fridica-agent-scix"));
}
```

Tools: `search` (ADS query syntax), `get_paper`, `get_citations`,
`get_references`, `get_metrics`, `export` (BibTeX, AASTeX, AGU, MNRAS, RIS and
other formats), and for the owner's ADS libraries `get_libraries`,
`get_library`, `manage_documents` (add or remove papers) and
`add_documents_by_query`. Deleting, sharing or transferring libraries is not
exposed. Tool approvals follow the host's existing policy.

## Overleaf projects

Enable the `overleaf` Cargo feature. The server is
[interleaf](https://github.com/happysky19/interleaf), compiled into
`fridica-agent-overleaf` and into any host built with the feature: no Node.js and
no download at first use. Workers need Git, and TeX Live (`latexmk`, `latexdiff`)
for the compile and review tools.

```sh
cargo install --locked --path . --features overleaf --bins
fridica-agent doctor --only overleaf
```

Set `OVERLEAF_GIT_TOKEN` (Overleaf: Account Settings > Git Integration) in the
worker environment, and `ADS_TOKEN` for citation checks against NASA ADS. Allow
network access to `git.overleaf.com` and `api.adsabs.harvard.edu`. A request
names its project by Overleaf link or id, or by any git URL (GitHub and GitLab
use the worker's own git login); `OVERLEAF_PROJECT_ID` sets a default. Clones go
in the session's working folder (`INTERLEAF_WORKSPACE` changes it), so the agent
edits them with its own tools.

Edits reach the shared copy as suggestions (`\sgadd`, `\sgdel`, `\sgrep`) that
co-authors accept or reject. Push dry-runs by default, pulls first, merges edits
on adjacent lines and never forces. Other tools check references, citations
(against ADS), wording and journal limits, compile with errors at file:line, tie
numbers and figures to their result files, and build a latexdiff review PDF.

```rust
#[cfg(feature = "overleaf")]
fn enable_overleaf(spec: &mut fridica_agent::Spec) {
    spec.mcp_servers.push(fridica_agent::overleaf::overleaf("fridica-agent-overleaf"));
}
```

The host handles tool approvals, including pushes.

## MCP doctor

Install the operator command on the worker machine with all four capabilities:

```sh
cargo install --path . --features github,alphaxiv,scix,overleaf --bins
fridica-agent doctor
fridica-agent doctor --json --timeout 60
fridica-agent doctor --only github --github-token-env WORKER_GITHUB_TOKEN
fridica-agent doctor --only alphaxiv
fridica-agent doctor --only scix
fridica-agent doctor --only overleaf
```

The command checks the local execution user's settings, not an SSH target or
Fridica daemon configuration. Run it on each worker as that worker's user.
GitHub uses `GH_TOKEN` (or the named variable), falling back to local `gh auth token`;
it reads authenticated identity and reports the bundled tool count. alphaXiv
uses `ALPHAXIV_TOKEN`, authenticates MCP, and lists the available research tools.
SciX requests zero results from ADS to verify that ADS accepts `ADS_TOKEN`.
Overleaf checks that `OVERLEAF_GIT_TOKEN` is set and lists the bundled tools
locally. To verify remote access, pull the intended project as the worker user.
An identity check does not prove write permission to any particular repository.

The checks run concurrently with a per-service timeout (default 30 seconds,
maximum 120). Exit code 0 means all selected checks passed, 1 means a check failed,
timed out, or its Cargo feature was not built; 2 means invalid command arguments.
JSON has a `checks` array with `service`, `status` (`ok`, `failed`, `timeout`, or
`not_built`), and a credential-free `message`. Missing features explain how to
reinstall; failures identify the credential source and endpoint to check.
