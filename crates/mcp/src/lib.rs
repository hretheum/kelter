//! MCP tool substrate: client (spawns tool servers, feeds the kernel's
//! action space) and the bundled reference tool server logic.
//!
//! Zero custom plugin API — every tool is an MCP server (ADR 0002).

pub mod client;
pub mod code_tools;
pub mod extensions;
pub mod memory_client;
pub mod memory_server;
pub mod server;
pub mod tools;

pub use client::{McpToolSource, ServerSpec};
pub use memory_client::McpMemoryProvider;
