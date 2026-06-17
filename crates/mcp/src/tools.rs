//! The five MCP tools, wired to the SessionManager and the vsock exec client.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::{
    ErrorData, handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters, model::*, tool, tool_handler, tool_router,
};
use tokio::sync::Mutex;

use crate::session::{BootSpawner, SessionConfig, SessionManager};
use crate::vsock_client::{self, ExecRequest};

#[derive(Clone)]
pub struct Mcp {
    mgr: Arc<Mutex<SessionManager>>,
    pub tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionId {
    #[schemars(description = "session id from open_session")]
    pub session_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunArgs {
    pub session_id: String,
    #[schemars(description = "shell command run via sh -c in the sandbox")]
    pub command: String,
    #[schemars(description = "seconds before the command is killed (default 30)")]
    pub timeout_s: Option<f64>,
    pub cwd: Option<String>,
    pub stdin: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileArgs {
    pub session_id: String,
    #[schemars(description = "absolute path inside the sandbox")]
    pub path: String,
    #[schemars(description = "file contents, base64-encoded")]
    pub content_base64: String,
}

fn mcp_err(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

impl Mcp {
    pub fn new(cfg: SessionConfig) -> Self {
        Self {
            mgr: Arc::new(Mutex::new(SessionManager::new(cfg))),
            tool_router: Self::tool_router(),
        }
    }

    pub fn manager(&self) -> Arc<Mutex<SessionManager>> {
        self.mgr.clone()
    }

    // Poll the guest exec agent until it answers a no-op, or time out.
    async fn wait_ready(&self, uds: std::path::PathBuf) -> Result<(), ErrorData> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let u = uds.clone();
            let probe = tokio::task::spawn_blocking(move || {
                vsock_client::exec(
                    &u,
                    &ExecRequest { cmd: ":".into(), stdin: None, cwd: None, timeout: Some(5.0) },
                    Duration::from_millis(500),
                )
            })
            .await
            .map_err(mcp_err)?;
            if let Ok(r) = probe
                && r.exit == 0
            {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(mcp_err("sandbox exec agent did not become ready"));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[tool_router]
impl Mcp {
    #[tool(description = "Open a sandbox session (a fresh microVM clone). Returns a session_id.")]
    async fn open_session(&self) -> Result<String, ErrorData> {
        let (id, uds) = {
            let mut mgr = self.mgr.lock().await;
            let id = mgr.open(&BootSpawner).map_err(mcp_err)?;
            let uds = mgr.get_uds(&id).map_err(mcp_err)?;
            (id, uds)
        };
        if let Err(e) = self.wait_ready(uds).await {
            let _ = self.mgr.lock().await.close(&id);
            return Err(e);
        }
        Ok(id)
    }

    #[tool(description = "Run a shell command in a session. Returns stdout, stderr, exit_code, timed_out as JSON.")]
    async fn run(&self, Parameters(a): Parameters<RunArgs>) -> Result<String, ErrorData> {
        let uds = self.mgr.lock().await.get_uds(&a.session_id).map_err(mcp_err)?;
        // Clamp the agent-supplied timeout to a sane range (avoids a u64-cast
        // overflow panic on pathological values and a nonsensical negative).
        let timeout_s = a.timeout_s.unwrap_or(30.0).clamp(0.0, 3600.0);
        let req = ExecRequest { cmd: a.command, stdin: a.stdin, cwd: a.cwd, timeout: Some(timeout_s) };
        let op = Duration::from_secs((timeout_s as u64) + 10);
        let resp = tokio::task::spawn_blocking(move || vsock_client::exec(&uds, &req, op))
            .await
            .map_err(mcp_err)?
            .map_err(mcp_err)?;
        Ok(serde_json::to_string(&serde_json::json!({
            "stdout": resp.stdout, "stderr": resp.stderr,
            "exit_code": resp.exit, "timed_out": resp.timed_out,
        })).unwrap())
    }

    #[tool(description = "Write a base64-encoded file into the session at an absolute path.")]
    async fn write_file(&self, Parameters(a): Parameters<WriteFileArgs>) -> Result<String, ErrorData> {
        let uds = self.mgr.lock().await.get_uds(&a.session_id).map_err(mcp_err)?;
        let cmd = format!("base64 -d > {}", shell_quote(&a.path));
        let req = ExecRequest { cmd, stdin: Some(a.content_base64), cwd: None, timeout: Some(30.0) };
        let resp = tokio::task::spawn_blocking(move || {
            vsock_client::exec(&uds, &req, Duration::from_secs(40))
        })
        .await
        .map_err(mcp_err)?
        .map_err(mcp_err)?;
        if resp.exit != 0 {
            return Err(mcp_err(format!("write_file failed: {}", resp.stderr)));
        }
        Ok("ok".into())
    }

    #[tool(description = "Reset a session: discard its state and roll back to the warm base.")]
    async fn reset(&self, Parameters(a): Parameters<SessionId>) -> Result<String, ErrorData> {
        let uds = {
            let mut mgr = self.mgr.lock().await;
            mgr.reset(&a.session_id, &BootSpawner).map_err(mcp_err)?;
            mgr.get_uds(&a.session_id).map_err(mcp_err)?
        };
        self.wait_ready(uds).await?;
        Ok("ok".into())
    }

    #[tool(description = "Close a session and discard its microVM.")]
    async fn close(&self, Parameters(a): Parameters<SessionId>) -> Result<String, ErrorData> {
        self.mgr.lock().await.close(&a.session_id).map_err(mcp_err)?;
        Ok("ok".into())
    }
}

/// Minimal single-quote shell escaping for a path argument.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[tool_handler]
impl rmcp::ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "ignition microVM sandboxes. open_session -> run/write_file -> reset/close.".into(),
            ),
            ..Default::default()
        }
    }
}
