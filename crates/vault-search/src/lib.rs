//! Search: BM25F full-text (tantivy), the quick switcher (nucleo + frizbee), frecency and RRF.

mod frecency;
mod fusion;
mod query;
mod switcher;
mod text;

pub use frecency::{Frecency, HALF_LIFE_MS};
pub use fusion::{rrf, RRF_K};
pub use query::{parse_query, Clause, ParsedQuery};
pub use switcher::{SwitchHit, SwitchItem, Switcher};
pub use text::{IndexDoc, TextHit, TextIndex};

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("tantivy: {0}")]
    Tantivy(#[from] tantivy::TantivyError),
    #[error("open directory: {0}")]
    Dir(#[from] tantivy::directory::error::OpenDirectoryError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SearchError>;
