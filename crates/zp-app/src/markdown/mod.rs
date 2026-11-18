//! Markdown rendering for assistant messages.

pub mod highlight;
mod parse;
mod render;

pub use parse::{Block, parse};
pub use render::{IdGen, MarkdownStyle, render_markdown};
