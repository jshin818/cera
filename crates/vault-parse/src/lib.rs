//! Markdown extraction for the index. Pure functions: no I/O.

mod chunk;
mod extract;
mod frontmatter;
mod resolve;

pub use chunk::{Chunk, chunk_note};
pub use extract::{parse, title_from_path};
pub use resolve::{LinkResolver, Resolved};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LinkKind {
    /// `[[note]]` or `[text](note.md)`
    Link,
    /// `![[note]]` / `![[image.png]]`
    Embed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Subpath {
    Heading(String),
    Block(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Target as written, without `#heading` / `^block` and without the alias. May be empty for
    /// same-note links like `[[#Heading]]`.
    pub target: String,
    pub subpath: Option<Subpath>,
    pub display: Option<String>,
    pub kind: LinkKind,
    /// Byte range of the whole link syntax in the source.
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub done: bool,
    pub text: String,
    pub offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParsedNote {
    /// Obsidian semantics: the file name without extension.
    pub title: String,
    pub aliases: Vec<String>,
    /// Lower-cased, without `#`, deduplicated, in first-seen order. Nested tags keep their `/`.
    pub tags: Vec<String>,
    pub frontmatter: Option<serde_json::Value>,
    pub headings: Vec<Heading>,
    pub links: Vec<Link>,
    pub block_ids: Vec<(String, u32)>,
    pub tasks: Vec<Task>,
    pub word_count: u32,
    /// Markup-free text for full-text indexing and embeddings.
    pub body_text: String,
    /// Byte offset where the body starts (after frontmatter).
    pub body_offset: u32,
}
