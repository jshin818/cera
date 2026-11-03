use crate::Result;
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::Path;

type Bytes = &'static [u8];

const FILES: TableDefinition<u32, Bytes> = TableDefinition::new("files");
const PATHS: TableDefinition<&str, u32> = TableDefinition::new("paths");
const NOTES: TableDefinition<u32, Bytes> = TableDefinition::new("notes");
const LINKS: TableDefinition<(u32, u32, u8), Bytes> = TableDefinition::new("links");
const UNRESOLVED: TableDefinition<&str, Bytes> = TableDefinition::new("unresolved");
const BACKLINKS: TableDefinition<u32, Bytes> = TableDefinition::new("backlinks");
const CHUNKS: TableDefinition<u64, Bytes> = TableDefinition::new("chunks");
const EVENTS: TableDefinition<u32, Bytes> = TableDefinition::new("events");
const GRAPH: TableDefinition<u32, Bytes> = TableDefinition::new("graph_metrics");
const META: TableDefinition<&str, Bytes> = TableDefinition::new("meta");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileRec {
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub xxh3: u64,
    pub markdown: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteRec {
    pub title: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub frontmatter_json: String,
    pub headings: Vec<(u8, String, u32)>,
    pub word_count: u32,
}

/// (src_id, dst_id, kind) where kind is 0 link, 1 embed, 2 heading link, 3 block link.
pub type LinkKey = (u32, u32, u8);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkRec {
    pub count: u32,
    pub offsets: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkRec {
    pub file_id: u32,
    pub start: u32,
    pub end: u32,
    pub heading_path: String,
    pub model: String,
    pub xxh3: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FrecencyRec {
    pub score: f32,
    pub last_access_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GraphRec {
    pub pagerank: f32,
    pub cluster_id: u32,
}

fn enc<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    Ok(postcard::to_allocvec(v)?)
}

fn dec<T: DeserializeOwned>(b: &[u8]) -> Result<T> {
    Ok(postcard::from_bytes(b)?)
}

/// A set of writes applied in one redb transaction (the engine batches ~250 ms of changes).
#[derive(Default, Debug)]
pub struct StoreBatch {
    pub files: Vec<(u32, Option<FileRec>)>,
    pub notes: Vec<(u32, Option<NoteRec>)>,
    /// Replace all outgoing links of a source.
    pub links: Vec<(u32, Vec<(LinkKey, LinkRec)>)>,
    pub unresolved: Vec<(String, Vec<u32>)>,
    pub backlinks: Vec<(u32, Vec<u32>)>,
    /// Replace all chunks of a file. Chunk ids are `(file_id << 32) | index`.
    pub chunks: Vec<(u32, Vec<ChunkRec>)>,
    pub events: Vec<(u32, FrecencyRec)>,
    pub graph: Vec<(u32, GraphRec)>,
    pub meta: Vec<(String, Vec<u8>)>,
}

impl StoreBatch {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.notes.is_empty()
            && self.links.is_empty()
            && self.unresolved.is_empty()
            && self.backlinks.is_empty()
            && self.chunks.is_empty()
            && self.events.is_empty()
            && self.graph.is_empty()
            && self.meta.is_empty()
    }
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        // redb's page cache defaults to 1 GiB; reads are rare here (it is a durable mirror of
        // in-memory state), so a small cache keeps idle memory low.
        let db = redb::Builder::new().set_cache_size(16 << 20).create(path)?;
        let w = db.begin_write()?;
        {
            w.open_table(FILES)?;
            w.open_table(PATHS)?;
            w.open_table(NOTES)?;
            w.open_table(LINKS)?;
            w.open_table(UNRESOLVED)?;
            w.open_table(BACKLINKS)?;
            w.open_table(CHUNKS)?;
            w.open_table(EVENTS)?;
            w.open_table(GRAPH)?;
            w.open_table(META)?;
        }
        w.commit()?;
        Ok(Store { db })
    }

    pub fn apply(&self, b: &StoreBatch) -> Result<()> {
        if b.is_empty() {
            return Ok(());
        }
        let w = self.db.begin_write()?;
        {
            let mut files = w.open_table(FILES)?;
            let mut paths = w.open_table(PATHS)?;
            for (id, rec) in &b.files {
                let old: Option<FileRec> = files.get(*id)?.map(|v| dec(v.value())).transpose()?;
                if let Some(old) = old {
                    paths.remove(old.path.as_str())?;
                }
                match rec {
                    Some(r) => {
                        files.insert(*id, enc(r)?.as_slice())?;
                        paths.insert(r.path.as_str(), *id)?;
                    }
                    None => {
                        files.remove(*id)?;
                    }
                }
            }
            let mut notes = w.open_table(NOTES)?;
            for (id, rec) in &b.notes {
                match rec {
                    Some(r) => notes.insert(*id, enc(r)?.as_slice())?,
                    None => notes.remove(*id)?,
                };
            }
            let mut links = w.open_table(LINKS)?;
            for (src, list) in &b.links {
                let stale: Vec<LinkKey> =
                    links.range((*src, 0u32, 0u8)..=(*src, u32::MAX, u8::MAX))?.map(|r| r.map(|(k, _)| k.value())).collect::<std::result::Result<_, _>>()?;
                for k in stale {
                    links.remove(k)?;
                }
                for (k, v) in list {
                    links.insert(*k, enc(v)?.as_slice())?;
                }
            }
            let mut unresolved = w.open_table(UNRESOLVED)?;
            for (t, srcs) in &b.unresolved {
                if srcs.is_empty() {
                    unresolved.remove(t.as_str())?;
                } else {
                    unresolved.insert(t.as_str(), enc(srcs)?.as_slice())?;
                }
            }
            let mut back = w.open_table(BACKLINKS)?;
            for (dst, srcs) in &b.backlinks {
                if srcs.is_empty() {
                    back.remove(*dst)?;
                } else {
                    back.insert(*dst, enc(srcs)?.as_slice())?;
                }
            }
            let mut chunks = w.open_table(CHUNKS)?;
            for (fid, list) in &b.chunks {
                let lo = (*fid as u64) << 32;
                let stale: Vec<u64> = chunks.range(lo..=lo | 0xFFFF_FFFF)?.map(|r| r.map(|(k, _)| k.value())).collect::<std::result::Result<_, _>>()?;
                for k in stale {
                    chunks.remove(k)?;
                }
                for (i, c) in list.iter().enumerate() {
                    chunks.insert(lo | i as u64, enc(c)?.as_slice())?;
                }
            }
            let mut events = w.open_table(EVENTS)?;
            for (id, e) in &b.events {
                events.insert(*id, enc(e)?.as_slice())?;
            }
            let mut graph = w.open_table(GRAPH)?;
            for (id, g) in &b.graph {
                graph.insert(*id, enc(g)?.as_slice())?;
            }
            let mut meta = w.open_table(META)?;
            for (k, v) in &b.meta {
                meta.insert(k.as_str(), v.as_slice())?;
            }
        }
        w.commit()?;
        Ok(())
    }

    pub fn file(&self, id: u32) -> Result<Option<FileRec>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(FILES)?;
        t.get(id)?.map(|v| dec(v.value())).transpose()
    }

    pub fn id_of(&self, path: &str) -> Result<Option<u32>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(PATHS)?;
        Ok(t.get(path)?.map(|v| v.value()))
    }

    pub fn all_files(&self) -> Result<Vec<(u32, FileRec)>> {
        self.scan(FILES)
    }

    pub fn all_notes(&self) -> Result<Vec<(u32, NoteRec)>> {
        self.scan(NOTES)
    }

    pub fn all_events(&self) -> Result<Vec<(u32, FrecencyRec)>> {
        self.scan(EVENTS)
    }

    pub fn all_graph(&self) -> Result<Vec<(u32, GraphRec)>> {
        self.scan(GRAPH)
    }

    fn scan<T: DeserializeOwned>(&self, def: TableDefinition<u32, Bytes>) -> Result<Vec<(u32, T)>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(def)?;
        let mut out = Vec::new();
        for item in t.iter()? {
            let (k, v) = item?;
            out.push((k.value(), dec(v.value())?));
        }
        Ok(out)
    }

    pub fn links_from(&self, src: u32) -> Result<Vec<(LinkKey, LinkRec)>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(LINKS)?;
        let mut out = Vec::new();
        for item in t.range((src, 0u32, 0u8)..=(src, u32::MAX, u8::MAX))? {
            let (k, v) = item?;
            out.push((k.value(), dec(v.value())?));
        }
        Ok(out)
    }

    pub fn backlinks(&self, dst: u32) -> Result<Vec<u32>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(BACKLINKS)?;
        Ok(t.get(dst)?.map(|v| dec(v.value())).transpose()?.unwrap_or_default())
    }

    pub fn unresolved(&self, target: &str) -> Result<Vec<u32>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(UNRESOLVED)?;
        Ok(t.get(target)?.map(|v| dec(v.value())).transpose()?.unwrap_or_default())
    }

    pub fn chunks_of(&self, fid: u32) -> Result<Vec<ChunkRec>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(CHUNKS)?;
        let lo = (fid as u64) << 32;
        let mut out = Vec::new();
        for item in t.range(lo..=lo | 0xFFFF_FFFF)? {
            out.push(dec(item?.1.value())?);
        }
        Ok(out)
    }

    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(META)?;
        Ok(t.get(key)?.map(|v| v.value().to_vec()))
    }

    pub fn counts(&self) -> Result<[(&'static str, u64); 6]> {
        let r = self.db.begin_read()?;
        Ok([
            ("files", r.open_table(FILES)?.len()?),
            ("notes", r.open_table(NOTES)?.len()?),
            ("links", r.open_table(LINKS)?.len()?),
            ("unresolved", r.open_table(UNRESOLVED)?.len()?),
            ("chunks", r.open_table(CHUNKS)?.len()?),
            ("events", r.open_table(EVENTS)?.len()?),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_replace_links() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("meta.redb")).unwrap();
        let mut b = StoreBatch::default();
        b.files.push((1, Some(FileRec { path: "a.md".into(), size: 3, mtime_ns: 9, xxh3: 7, markdown: true })));
        b.links.push((1, vec![((1, 2, 0), LinkRec { count: 1, offsets: vec![4] }), ((1, 3, 1), LinkRec { count: 2, offsets: vec![1, 9] })]));
        b.backlinks.push((2, vec![1]));
        b.chunks.push((1, vec![ChunkRec { file_id: 1, start: 0, end: 3, heading_path: String::new(), model: "m".into(), xxh3: 1 }]));
        s.apply(&b).unwrap();
        assert_eq!(s.id_of("a.md").unwrap(), Some(1));
        assert_eq!(s.links_from(1).unwrap().len(), 2);
        assert_eq!(s.backlinks(2).unwrap(), vec![1]);
        let mut b = StoreBatch::default();
        b.links.push((1, vec![((1, 5, 0), LinkRec { count: 1, offsets: vec![0] })]));
        b.files.push((1, Some(FileRec { path: "b.md".into(), size: 3, mtime_ns: 9, xxh3: 7, markdown: true })));
        s.apply(&b).unwrap();
        assert_eq!(s.links_from(1).unwrap().len(), 1);
        assert_eq!(s.id_of("a.md").unwrap(), None);
        assert_eq!(s.id_of("b.md").unwrap(), Some(1));
        assert_eq!(s.chunks_of(1).unwrap().len(), 1);
    }
}
