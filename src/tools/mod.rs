//! The MCP surface: tool definitions, descriptions and server instructions.

pub mod args;

use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use rusqlite::Connection;

use crate::store::{self, ToolResult};
use args::{AddArgs, GetArgs, NextArgs, SetStatusArgs};

/// The one SQLite connection, shared by every request.
pub type Db = Arc<Mutex<Connection>>;

/// Conventions every model gets on connect. Keep it to a few lines.
pub const INSTRUCTIONS: &str = "\
Spokes is one tree of to-dos. IDs look like SPOKE-12 (a bare 12 also works). The root is SPOKE-1 \"To do\".
Areas (kind area: Home, Work) hold work forever and never finish; tasks get done or dropped. Nothing is deleted.
- \"What should I do?\" -> next. Browse with get (start at SPOKE-1).
- Look before adding (get the likely parent) so you don't create duplicates.
- Give new nodes 2 to 5 aliases. Put long detail in notes, not the title.
- Pass your agent name on writes. Ask the user rather than guess.";

/// One MCP server instance. Built per request (the transport is stateless), so it only
/// holds the shared database handle and a clone of the prebuilt tool router.
#[derive(Clone)]
pub struct Spokes {
    db: Db,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Spokes {
    /// The tool router, built once at startup and cloned into each request's server.
    pub fn router() -> ToolRouter<Self> {
        Self::tool_router()
    }

    pub fn new(db: Db, tool_router: ToolRouter<Self>) -> Self {
        Self { db, tool_router }
    }

    /// Runs a store operation on the blocking pool so SQLite never stalls the runtime.
    async fn run<F>(&self, op: F) -> ToolResult
    where
        F: FnOnce(&mut Connection) -> ToolResult + Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            // A panic mid-call rolls its transaction back, so a poisoned lock is safe to reuse.
            let mut conn = db.lock().unwrap_or_else(|p| p.into_inner());
            op(&mut conn)
        })
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "tool task failed");
            Err("Internal error. Try again; if it persists, check the server log.".to_string())
        })
    }

    #[tool(
        description = "What to work on next: questions waiting for the user, then open tasks by priority, one line each. Use scope to look under one node."
    )]
    async fn next(&self, Parameters(args): Parameters<NextArgs>) -> ToolResult {
        self.run(move |c| store::next(c, args)).await
    }

    #[tool(
        description = "Show one node and an outline of its children. Omit id for the root. Notes are only included with notes: true."
    )]
    async fn get(&self, Parameters(args): Parameters<GetArgs>) -> ToolResult {
        self.run(move |c| store::get(c, args)).await
    }

    #[tool(
        description = "Create a task or area under a parent (default: the root). Returns the new id. Include 2 to 5 aliases."
    )]
    async fn add(&self, Parameters(args): Parameters<AddArgs>) -> ToolResult {
        self.run(move |c| store::add(c, args)).await
    }

    #[tool(
        description = "Mark nodes done, dropped (won't do; there is no delete) or open (reopen). Areas can be dropped but never done."
    )]
    async fn set_status(&self, Parameters(args): Parameters<SetStatusArgs>) -> ToolResult {
        self.run(move |c| store::set_status(c, args)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Spokes {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("spokes", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
