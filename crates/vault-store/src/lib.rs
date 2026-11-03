//! Persistence for the `.vault/` cache directory. Everything here is rebuildable from the Markdown files.
//!
//! * [`Store`]: redb tables (files, paths, notes, links, unresolved, backlinks, chunks, events, graph_metrics).
//! * [`snapshot`]: the rkyv startup snapshot, memory-mapped and checked with XXH3 instead of a full validation pass.

pub mod snapshot;
mod tables;

pub use snapshot::{MappedSnapshot, SnapEntry, SnapHeading, SnapLink, SnapshotData, SNAPSHOT_VERSION};
pub use tables::{ChunkRec, FileRec, FrecencyRec, GraphRec, LinkKey, LinkRec, NoteRec, Store, StoreBatch};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(String),
    #[error("encoding: {0}")]
    Encode(String),
    #[error("snapshot is missing, stale or corrupt")]
    BadSnapshot,
}

macro_rules! db_err {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self { StoreError::Db(e.to_string()) }
        }
    )*};
}
db_err!(redb::Error, redb::DatabaseError, redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError);

impl From<postcard::Error> for StoreError {
    fn from(e: postcard::Error) -> Self {
        StoreError::Encode(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
