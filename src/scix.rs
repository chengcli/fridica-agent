//! Run the upstream NASA SciX stdio MCP server with the worker's ADS credential.
use anyhow::{anyhow, ensure, Result};
use rmcp::{model::CallToolRequestParams, ServiceExt};
use std::process::{Command, Stdio};

pub const TOKEN_ENV: &str = "ADS_TOKEN";

/// Attach the installed fridica-agent-scix command to either backend.
pub fn scix(command: impl Into<String>) -> crate::McpServer {
    crate::McpServer {
        name: "scix".into(),
        command: command.into(),
        args: vec![],
        env_vars: vec![TOKEN_ENV.into()],
        auto_approve: false,
    }
}

fn server_command() -> Result<Command> {
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    ensure!(
        !token.trim().is_empty() && !token.chars().any(char::is_control),
        "set a valid ADS_TOKEN on this worker"
    );
    let mut command = Command::new("scix-mcp");
    command.env_remove(TOKEN_ENV).env("SCIX_API_TOKEN", token);
    Ok(command)
}

/// Serve MCP on inherited stdio. On Unix, replace the wrapper so the backend
/// supervises the upstream server process directly.
pub fn run() -> Result<()> {
    let mut command = server_command()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        ensure!(
            command.status()?.success(),
            "scix-mcp exited unsuccessfully"
        );
        Ok(())
    }
}

/// Exercise the upstream health_check over the same stdio launch path.
pub async fn probe() -> Result<usize> {
    let mut child = tokio::process::Command::from(server_command()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| anyhow!("install scix-mcp on the worker PATH"))?;
    let transport = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
    let server = ().serve(transport).await.map_err(|_| anyhow!("SciX MCP initialization failed"))?;
    let result = server
        .call_tool(CallToolRequestParams::new("health_check").with_arguments(
            serde_json::Map::from_iter([("response_format".into(), "json".into())]),
        ))
        .await
        .map_err(|_| anyhow!("SciX health_check failed"))?;
    let report: serde_json::Value = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .and_then(|c| serde_json::from_str(&c.text).ok())
        .ok_or_else(|| anyhow!("invalid SciX health_check response"))?;
    ensure!(
        report["probe"]["state"] == "ok",
        "SciX authentication probe failed; check ADS_TOKEN and API connectivity"
    );
    report["tools"]
        .as_array()
        .map(Vec::len)
        .ok_or_else(|| anyhow!("missing SciX tool list"))
}
