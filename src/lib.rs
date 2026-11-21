#![doc = include_str!("../README.md")]
pub mod agent;
#[cfg(feature = "alphaxiv")]
pub mod alphaxiv;
pub mod claude;
pub mod codex;
pub mod doctor;
#[cfg(feature = "github")]
pub mod github;
#[cfg(feature = "scix")]
pub mod scix;
mod session;

pub use agent::{Agent, OutputFormat, Reply, Turn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, io, path::PathBuf, pin::Pin, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Maximum size of one protocol line or message.
pub const LINE_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Claude,
    Codex,
}
impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Execution,
    Refusal,
    Cancelled,
    Interrupted,
    /// The backend's usage limit stopped the turn. Temporary: retry after
    /// `retry_at` (Unix seconds) when the backend said, never at once.
    RateLimited {
        #[serde(default)]
        retry_at: Option<u64>,
    },
}
/// A failure with a stable machine-readable code and, when known, the backend
/// session it happened in.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Error {
    pub kind: Failure,
    pub code: String,
    #[serde(default)]
    pub backend_session_id: String,
}
impl Error {
    pub fn new(kind: Failure, code: &str, session: &str) -> Self {
        Self {
            kind,
            code: code.into(),
            backend_session_id: session.into(),
        }
    }
}

/// Sandbox and approval policy, in the vocabulary both backends share.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// `read-only`, `write` or `full`.
    pub mode: String,
    /// `never`, `on-request`, `untrusted` or `auto`.
    pub approvals: String,
    /// Allowed network domains; empty means no network.
    pub network: Vec<String>,
    /// Claude only: `host` lets Claude ask for permission, `none` never asks.
    pub claude_prompts: String,
}
/// An explicit stdio MCP server. Values of inherited variables never enter the spec.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// Preauthorize tools from this explicitly configured server. Default false.
    #[serde(default)]
    pub auto_approve: bool,
}
impl McpServer {
    pub(crate) fn valid(&self) -> bool {
        !self.name.is_empty()
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            && !self.command.is_empty()
            && !self.command.contains('\0')
            && self.args.iter().all(|s| !s.contains('\0'))
            && self.env_vars.iter().all(|s| {
                !s.is_empty()
                    && !s.as_bytes()[0].is_ascii_digit()
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
    }
}
/// Everything a driver needs to start and run one backend session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spec {
    pub backend: Backend,
    /// Appended to Claude's system prompt; Codex developer instructions.
    pub instructions: String,
    pub model: String,
    pub reasoning_effort: String,
    /// Working directory sent to Codex; a path starting with `~` is not sent.
    pub cwd: PathBuf,
    pub policy: Policy,
    /// An external sandbox already confines the process, so the backend's own
    /// sandbox is turned off.
    pub confined: bool,
    /// Refuse explicit MCP servers on either backend. Codex also verifies that
    /// its effective configuration contains no enabled ambient server.
    pub forbid_mcp: bool,
    /// Only explicitly supplied servers are enabled by the driver.
    #[serde(default)]
    pub mcp_servers: Vec<McpServer>,
    pub job_timeout: Duration,
    pub idle_timeout: Duration,
    pub approval_timeout: Duration,
}

/// How the driver names itself on the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct ClientInfo {
    /// Prefix of Claude control request IDs and the Codex client name.
    pub name: String,
    /// Shown in messages to the backend, e.g. "not supported by `title`".
    pub title: String,
    pub version: String,
}
#[derive(Clone)]
pub struct Options {
    pub client: ClientInfo,
    /// Codex MCP servers to disable at startup.
    pub disabled_mcp_servers: Vec<String>,
    /// How long to wait for an exit status after output closes.
    pub eof_wait: Duration,
    /// Grace period between asking the process to stop and killing it.
    pub close_grace: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            client: ClientInfo {
                name: "fridica-agent".into(),
                title: "fridica-agent".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            disabled_mcp_servers: vec![],
            eof_wait: Duration::from_secs(1),
            close_grace: Duration::from_secs(1),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// `command`, `tool`, `file_change` or `permissions`.
    pub kind: String,
    pub summary: String,
    #[serde(default)]
    pub detail: Value,
    #[serde(default)]
    pub backend_request_id: String,
    /// Requests with the same non-empty key share a `Session` approval.
    #[serde(default)]
    pub cache_key: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Once,
    Session,
    Deny,
}
pub trait Approver: Send + Sync {
    fn request(&self, request: ApprovalRequest) -> BoxFuture<'_, ApprovalDecision>;
}

/// Receives every wire event before it takes effect. A recording failure stops
/// the turn, so a caller that needs a complete record can rely on one.
pub trait Recorder: Send + Sync {
    fn record(&self, context: Value, event: Value) -> BoxFuture<'_, Result<(), Error>>;
}
pub struct DiscardWire;
impl Recorder for DiscardWire {
    fn record(&self, _: Value, _: Value) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}

pub trait SessionIds: Send + Sync {
    /// A new Claude session ID (a UUID).
    fn new_session_id(&self) -> String;
}

pub type Stdin = Box<dyn AsyncWrite + Send + Sync + Unpin>;
pub type Output = Box<dyn AsyncRead + Send + Sync + Unpin>;
/// A started backend process. Dropping it must stop the process.
pub trait Child: Send + Sync {
    fn take_stdin(&mut self) -> Option<Stdin>;
    fn take_stdout(&mut self) -> Option<Output>;
    fn take_stderr(&mut self) -> Option<Output>;
    /// The exit code if the process has already exited.
    fn try_exit(&mut self) -> io::Result<Option<i32>>;
    /// After output closed: the exit code, waiting at most `wait`.
    fn eof_status(&mut self, wait: Duration) -> BoxFuture<'_, io::Result<Option<i32>>>;
    /// Stop the process, killing it after `grace`, and reap it.
    fn terminate(&mut self, grace: Duration) -> BoxFuture<'_, io::Result<()>>;
    /// A refusal code for an exit the launcher recognizes (e.g. its own setup
    /// wrapper refusing to start), given the exit code and stderr tail.
    fn refusal(&self, _status: Option<i32>, _stderr: &[u8]) -> Option<String> {
        None
    }
}
pub enum LaunchError {
    /// The launcher declined; reported as is.
    Refused(Error),
    /// The process could not be started.
    StartFailed,
}
pub trait Launcher: Send + Sync {
    fn launch(&self, command: Vec<String>) -> Result<Box<dyn Child>, LaunchError>;
}
