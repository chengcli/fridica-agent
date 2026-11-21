//! Overleaf project tools through the upstream stdio MCP server.
use crate::{npm_server, McpServer};
use anyhow::{anyhow, ensure, Context, Result};
use rmcp::{model::CallToolRequestParams, ServiceExt};
use std::process::{Command, Stdio};

// Claude and Codex resolve these names on the execution target.
const CREDENTIAL_SOURCES: &[&str] = &[
    "OVERLEAF_GIT_TOKEN",
    "OVERLEAF_GIT_TOKEN_FILE",
    "OVERLEAF_MCP_ENV_FILE",
    "OVERLEAF_PROJECTS_CONFIG",
    "OVERLEAF_SESSION_COOKIE",
    "OVERLEAF_SESSION_COOKIE_FILE",
];
const SETTINGS: &[&str] = &[
    "OVERLEAF_PROJECT_ID",
    "OVERLEAF_PROJECT_NAME",
    "OVERLEAF_PROJECTS",
    "OVERLEAF_DEFAULT_PROJECT",
    "OVERLEAF_MCP_WORKSPACE_DIR",
    "OVERLEAF_GIT_BASE_URL",
    "OVERLEAF_WEB_BASE_URL",
    "OVERLEAF_MCP_CHECKOUT_MODE",
    "OVERLEAF_MCP_COMPILE_TIMEOUT_MS",
];
const MISSING_CREDENTIAL: &str = "configure OVERLEAF_GIT_TOKEN, OVERLEAF_MCP_ENV_FILE, or another Overleaf credential source on this worker";

pub fn overleaf(command: impl Into<String>) -> McpServer {
    McpServer {
        name: "overleaf".into(),
        command: command.into(),
        args: vec![],
        env_vars: CREDENTIAL_SOURCES
            .iter()
            .chain(SETTINGS)
            .map(|s| s.to_string())
            .collect(),
        auto_approve: false,
    }
}

fn configured(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.to_string_lossy().trim().is_empty())
}

fn server_command() -> Result<Command> {
    ensure!(
        CREDENTIAL_SOURCES.iter().any(|name| configured(name)),
        MISSING_CREDENTIAL
    );
    let mut command =
        npm_server::command("mcp-server-overleaf", "--package=mcp-server-overleaf@0.4.1")?;
    // Empty values injected by Claude must not shadow the upstream env file.
    for name in CREDENTIAL_SOURCES.iter().chain(SETTINGS) {
        if !configured(name) {
            command.env_remove(name);
        }
    }
    command.arg("--stdio");
    Ok(command)
}

pub fn run() -> Result<()> {
    npm_server::run(server_command()?).context("could not start Overleaf MCP; check Node.js and the worker PATH for mcp-server-overleaf or npm")
}

/// Inspect the upstream tool catalog and local project configuration without
/// cloning a project or contacting the user's Overleaf account.
pub async fn probe() -> Result<usize> {
    let mut child = tokio::process::Command::from(server_command()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context(
            "could not launch Overleaf MCP; check the worker PATH for mcp-server-overleaf or npm",
        )?;
    let server = ().serve((child.stdout.take().unwrap(), child.stdin.take().unwrap())).await
        .map_err(|_| anyhow!("Overleaf MCP initialization failed; check Node.js, package availability and worker configuration"))?;
    let tools = server
        .list_all_tools()
        .await
        .map_err(|_| anyhow!("Overleaf tool discovery failed"))?;
    let result = server
        .call_tool(CallToolRequestParams::new("list_projects"))
        .await
        .map_err(|_| anyhow!("Overleaf project configuration check failed"))?;
    ensure!(
        result.is_error != Some(true),
        "Overleaf project configuration check failed"
    );
    let text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .context("missing Overleaf project configuration response")?;
    // Upstream reports missing credentials as a successful text tool response.
    ensure!(
        !text.text.starts_with("OVERLEAF_GIT_TOKEN is not set"),
        MISSING_CREDENTIAL
    );
    Ok(tools.len())
}
