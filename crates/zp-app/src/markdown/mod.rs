//! Markdown rendering for assistant messages.

mod parse;
mod render;

pub use parse::{Block, parse};
pub use render::{IdGen, MarkdownStyle, render_markdown};
