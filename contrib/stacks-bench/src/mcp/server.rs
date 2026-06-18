//! MCP server definition and bootstrap.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{RoleServer, ServerHandler, tool_handler};
use stacks_bench::db::app::AppDb;

use super::resources;

const SERVER_INSTRUCTIONS: &str =
    "Stacks blockchain benchmarking tool. Use list_runs to see benchmark results.";

/// The MCP server handler. Holds a cloneable [`AppDb`] for the session.
#[derive(Clone)]
pub struct StacksBenchServer {
    pub app_db: AppDb,
    tool_router: ToolRouter<Self>,
}

impl StacksBenchServer {
    pub fn new(app_db: AppDb) -> Self {
        Self {
            app_db,
            tool_router: Self::build_tool_router(),
        }
    }
}

#[tool_handler]
impl ServerHandler for StacksBenchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            "stacks-bench",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(resources::list_resources())
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(resources::list_resource_templates())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        resources::read_resource(&request.uri, &self.app_db).await
    }
}
