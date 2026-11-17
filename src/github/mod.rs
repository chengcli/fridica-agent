//! Optional GitHub capabilities and stdio MCP transport for agent hosts.
//! No shell execution, automatic write retries, merge or force-push tools.
mod api;
pub mod tools;

use anyhow::{bail, Context, Result};
use octocrab::Octocrab;
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Debug)]
pub struct Options {
    /// Restrict the server to read tools, enforced when dispatching calls.
    pub read_only: bool,
    /// Name of the target-local environment variable holding the GitHub token.
    pub token_env: String,
    /// Read authenticated identity and exit; does not publish anything.
    pub probe: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            read_only: false,
            token_env: "GH_TOKEN".into(),
            probe: false,
        }
    }
}

async fn authenticated(token_env: &str) -> Result<Octocrab> {
    let token = match std::env::var(token_env).ok().filter(|t| !t.is_empty()) {
        Some(token) => token,
        None => {
            let mut command = tokio::process::Command::new("gh");
            command
                .args(["auth", "token", "--hostname", "github.com"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            // An alternate ambient token must not override the configured source.
            command
                .env_remove("GH_TOKEN")
                .env_remove("GITHUB_TOKEN")
                .env_remove("GH_HOST");
            let output = tokio::time::timeout(Duration::from_secs(10), command.output())
                .await
                .context("GitHub authentication timed out")?
                .context("GitHub authentication unavailable")?;
            if !output.status.success() {
                bail!("GitHub authentication unavailable");
            }
            String::from_utf8(output.stdout)
                .context("invalid GitHub credential")?
                .trim()
                .to_owned()
        }
    };
    if token.is_empty() || token.contains(['\r', '\n', '\0']) {
        bail!("invalid GitHub credential");
    }
    // Features intentionally exclude retry and follow-redirect. Credentials are
    // sent only to GitHub; an uncertain write is never automatically replayed.
    Octocrab::builder()
        .personal_token(token)
        .set_connect_timeout(Some(Duration::from_secs(10)))
        .set_read_timeout(Some(Duration::from_secs(30)))
        .set_write_timeout(Some(Duration::from_secs(30)))
        .build()
        .context("GitHub client setup failed")
}

pub async fn run(options: Options) -> Result<()> {
    if options.token_env.is_empty()
        || !options
            .token_env
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
    {
        bail!("invalid token environment variable name");
    }
    if options.probe {
        let client = authenticated(&options.token_env).await?;
        client
            .get::<Value, _, Value>("/user", None)
            .await
            .map_err(|_| anyhow::anyhow!("GitHub MCP identity probe failed"))?;
        println!("fridica-github-mcp:ready");
        return Ok(());
    }
    let workspace = std::env::current_dir()?;
    let mut server = Server::new(options.read_only, workspace);
    serve(
        &mut server,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        &options.token_env,
    )
    .await
}

pub struct Server {
    client: Option<Octocrab>,
    read_only: bool,
    workspace: PathBuf,
    initialized: bool,
    ready: bool,
}
impl Server {
    pub fn new(read_only: bool, workspace: PathBuf) -> Self {
        Self {
            client: None,
            read_only,
            workspace,
            initialized: false,
            ready: false,
        }
    }
    /// Dependency injection for tests/embedded hosts. CLI always fixes api.github.com.
    pub fn with_client(mut self, client: Octocrab) -> Self {
        self.client = Some(client);
        self
    }
    pub async fn handle(&mut self, request: Value, token_env: &str) -> Option<Value> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let error = |code, message: &str| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}});
        if request["jsonrpc"] != "2.0"
            || !request["method"].is_string()
            || !(id.is_null() || id.is_string() || id.is_number())
        {
            return Some(error(-32600, "Invalid request"));
        }
        let method = request["method"].as_str().unwrap();
        if request.get("id").is_none() {
            if method == "notifications/initialized" && self.initialized {
                self.ready = true;
            }
            // Never execute a side effect in a notification.
            return None;
        }
        let result = match method {
            "initialize" if !self.initialized => {
                self.initialized = true;
                let requested = request["params"]["protocolVersion"].as_str().unwrap_or("");
                let version = if ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"]
                    .contains(&requested)
                {
                    requested
                } else {
                    "2024-11-05"
                };
                json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"fridica-github-mcp","version":env!("CARGO_PKG_VERSION")},"instructions":"GitHub writes are not retried automatically. Inspect GitHub after uncertain responses. Reviews require the inspected commitID; push_files requires expectedHead. No merge, release or force-push tools are exposed."})
            }
            "ping" => json!({}),
            _ if !self.ready => return Some(error(-32002, "Server not initialized")),
            "tools/list" => {
                if request["params"]
                    .get("cursor")
                    .is_some_and(|c| !c.is_null())
                {
                    return Some(error(-32602, "Invalid cursor"));
                }
                json!({"tools":tools::catalog(self.read_only)})
            }
            "tools/call" => {
                let name = request["params"]["name"].as_str().unwrap_or("");
                let catalog = tools::catalog(self.read_only);
                let Some(tool) = catalog.iter().find(|t| t["name"] == name) else {
                    return Some(error(-32602, "Unknown or disabled tool"));
                };
                let args = request["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                if !valid(&args, &tool["inputSchema"]) {
                    return Some(error(-32602, "Invalid tool arguments"));
                }
                if name == "git_context" {
                    let result = match api::git_context(&args, &self.workspace) {
                        Ok(value) => {
                            json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                        }
                        Err(e) => tool_error(&e.to_string()),
                    };
                    return Some(json!({"jsonrpc":"2.0","id":id,"result":result}));
                }
                if self.client.is_none() {
                    match authenticated(token_env).await {
                        Ok(client) => self.client = Some(client),
                        Err(_) => {
                            return Some(
                                json!({"jsonrpc":"2.0","id":id,"result":tool_error("GitHub authentication unavailable on this worker")}),
                            )
                        }
                    }
                }
                match tokio::time::timeout(Duration::from_secs(60),api::call(self.client.as_ref().unwrap(),name,&args,&self.workspace)).await {
                    Ok(Ok(value))=>json!({"content":[{"type":"text","text":value.to_string()}],"isError":false}),
                    Ok(Err(e))=> {
                        if e.downcast_ref::<octocrab::Error>().is_some() {
                            tool_error("GitHub request failed. For writes, inspect GitHub before retrying; the outcome may be uncertain.")
                        } else { tool_error(&e.to_string()) }
                    }
                    Err(_)=>tool_error("GitHub operation timed out. For writes, inspect GitHub before retrying; the outcome may be uncertain."),
                }
            }
            _ => return Some(error(-32601, "Method not found")),
        };
        Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }
}
fn tool_error(message: &str) -> Value {
    json!({"isError":true,"content":[{"type":"text","text":message}]})
}
fn valid(value: &Value, schema: &Value) -> bool {
    if let Some(values) = schema["enum"].as_array() {
        if !values.contains(value) {
            return false;
        }
    }
    match schema["type"].as_str() {
        Some("object") => {
            let Some(map) = value.as_object() else {
                return false;
            };
            if schema["required"]
                .as_array()
                .is_some_and(|keys| keys.iter().any(|k| !map.contains_key(k.as_str().unwrap())))
            {
                return false;
            }
            map.iter()
                .all(|(k, v)| schema["properties"].get(k).is_some_and(|s| valid(v, s)))
        }
        Some("array") => value.as_array().is_some_and(|items| {
            items.len() >= schema["minItems"].as_u64().unwrap_or(0) as usize
                && items.len() <= schema["maxItems"].as_u64().unwrap_or(100) as usize
                && items.iter().all(|v| valid(v, &schema["items"]))
        }),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_u64().is_some_and(|n| {
            n >= schema["minimum"].as_u64().unwrap_or(0)
                && n <= schema["maximum"].as_u64().unwrap_or(u64::MAX)
        }),
        None => true,
        _ => false,
    }
}
/// Newline-delimited MCP stdio; bound each incoming frame to 2 MiB.
pub async fn serve(
    server: &mut Server,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    token_env: &str,
) -> Result<()> {
    const LIMIT: u64 = 2 * 1024 * 1024;
    loop {
        let mut line = Vec::new();
        let n = (&mut input)
            .take(LIMIT + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            break;
        }
        if n as u64 > LIMIT {
            bail!("MCP frame exceeds limit");
        }
        let response = match serde_json::from_slice(&line) {
            Ok(request) => server.handle(request, token_env).await,
            Err(_) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            ),
        };
        if let Some(value) = response {
            output.write_all(value.to_string().as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
    }
    Ok(())
}
