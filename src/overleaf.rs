//! LaTeX papers on Overleaf (or any git remote) through the bundled interleaf MCP server.
use crate::McpServer;
use anyhow::{ensure, Context, Result};

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
