//! ignition-mcp: stdio MCP server exposing ignition microVM sandboxes.

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};

#[derive(Clone)]
struct Mcp {
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PingRequest {
    #[schemars(description = "text to echo back")]
    text: String,
}

#[tool_router]
impl Mcp {
    fn new() -> Self {
        Self { tool_router: Self::tool_router() }
    }

    #[tool(description = "Health check; echoes the supplied text")]
    async fn ping(&self, Parameters(PingRequest { text }): Parameters<PingRequest>) -> String {
        format!("pong: {text}")
    }
}

#[tool_handler]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "ignition microVM sandboxes: open_session, run, write_file, reset, close.".into(),
            ),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let service = Mcp::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
