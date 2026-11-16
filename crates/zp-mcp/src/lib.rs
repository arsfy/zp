//! Model Context Protocol support for ZP: server configuration, a client for stdio and
//! streamable HTTP servers, and their tools as agent tools.

pub mod client;
pub mod config;
pub mod hub;

pub use client::{Client, ToolInfo};
pub use config::{Server, ServerConfig, Transport};
pub use hub::{McpHub, ServerState, ServerStatus, tool_name};
