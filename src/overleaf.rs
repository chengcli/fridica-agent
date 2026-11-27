//! LaTeX papers on Overleaf (or any git remote) through the bundled interleaf MCP server.
mod git;
pub use git::probe_git_access;

use crate::McpServer;
use anyhow::{ensure, Context, Result};
pub use interleaf::{
    login, native_operation, native_operations, native_tool_is_mutation, NativeSession,
};

/// Variables interleaf reads on the execution target.
const ENV_VARS: &[&str] = &[
    "OVERLEAF_GIT_TOKEN",
    "ADS_TOKEN",
    "OVERLEAF_PROJECT_ID",
    "INTERLEAF_CONFIG",
    "INTERLEAF_WORKSPACE",
];

pub fn overleaf(command: impl Into<String>) -> McpServer {
    McpServer {
        name: "overleaf".into(),
        command: command.into(),
        args: vec![],
        env_vars: ENV_VARS.iter().map(|s| s.to_string()).collect(),
        auto_approve: false,
    }
}

/// Serve MCP on inherited stdio in this process.
pub fn run() -> Result<()> {
    interleaf::serve(std::io::stdin().lock(), std::io::stdout().lock())
}

/// Serve native collaboration with the host's per-call authorization policy.
pub fn run_native(
    session: NativeSession,
    authorize: impl Fn(&str, &serde_json::Map<String, serde_json::Value>) -> Result<()>,
) -> Result<()> {
    interleaf::serve_native(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        session,
        authorize,
    )
}

/// Credential-free native tool discovery, without contacting Overleaf.
pub fn probe_native() -> Result<usize> {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n"
    );
    let session = NativeSession::from_reader(
        br#"{"baseUrl":"https://www.overleaf.com","cookie":"overleaf_session2=inert-probe"}"#
            .as_slice(),
        std::env::temp_dir().join("fridica-native-probe-unused"),
    )?;
    let mut output = Vec::new();
    interleaf::serve_native(input.as_bytes(), &mut output, session, |_, _| {
        anyhow::bail!("discovery only")
    })?;
    let last = output
        .split(|&b| b == b'\n')
        .rfind(|l| !l.is_empty())
        .context("no reply from native Interleaf")?;
    let reply: serde_json::Value = serde_json::from_slice(last)?;
    let tools = reply["result"]["tools"]
        .as_array()
        .context("native tools missing")?;
    ensure!(
        tools.len() == native_operations().len() + 2
            && native_operations().len() == 33
            && tools
                .iter()
                .all(|t| t["name"].as_str().is_some_and(|n| n.starts_with("native_")))
            && tools.iter().any(|t| t["name"] == "native_suggest_edit")
            && tools.iter().any(|t| t["name"] == "native_reply_comment"),
        "native tool inventory invalid"
    );
    Ok(tools.len())
}

/// Check the worker's Overleaf credential and list the bundled tools, without cloning a
/// project or contacting Overleaf.
pub async fn probe() -> Result<usize> {
    ensure!(
        std::env::var("OVERLEAF_GIT_TOKEN").is_ok_and(|t| !t.trim().is_empty()),
        "set OVERLEAF_GIT_TOKEN on this worker"
    );
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"doctor"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        "\n"
    );
    let mut out = Vec::new();
    interleaf::serve(input.as_bytes(), &mut out)?;
    let last = out
        .split(|&b| b == b'\n')
        .rfind(|l| !l.is_empty())
        .context("no reply from interleaf")?;
    let reply: serde_json::Value = serde_json::from_slice(last)?;
    reply["result"]["tools"]
        .as_array()
        .map(Vec::len)
        .context("interleaf listed no tools")
}
