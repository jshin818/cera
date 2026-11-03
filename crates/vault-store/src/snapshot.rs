//! The startup snapshot: one rkyv archive, memory-mapped. Layout:
//! `[magic "VSNP" | version u32 | payload_len u64 | xxh3 u64 | pad to 32] [rkyv payload]`.
//! The XXH3 check replaces bytecheck validation so that opening a 100k-note snapshot costs
//! a hash pass instead of a full structural walk.

use crate::{Result, StoreError};
use rkyv::rancor;
use std::io::Write;
use std::path::Path;

pub const SNAPSHOT_VERSION: u32 = 4;
const MAGIC: &[u8; 4] = b"VSNP";
const HEADER: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapHeading {
    pub level: u8,
    pub text: String,
    pub offset: u32,
}

/// A link with interned strings (indexes into `LinksData::symbols`); 25 bytes, no heap.
#[derive(Debug, Clone, Copy, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapLink {
    pub target: u32,
    pub sub: u32,
    pub display: u32,
    pub start: u32,
    pub end: u32,
    /// Resolved destination file id, `u32::MAX` if unresolved.
    pub dst: u32,
    /// bit 0 embed, bit 1 heading subpath, bit 2 block subpath
    pub flags: u8,
}

/// The file table: everything the sidebar and quick switcher need. Small, verified on the critical path.
#[derive(Debug, Clone, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapEntry {
    pub path: String,
    pub markdown: bool,
    pub size: u64,
    pub mtime_ns: i64,
    pub hash: u64,
    pub title: String,
    pub aliases: Vec<String>,
    pub tags: Vec<String>,
    pub word_count: u32,
    pub tasks_open: u32,
    pub tasks_done: u32,
}

/// Per-note structure (links, headings, frontmatter), loaded after the first frame.
#[derive(Debug, Clone, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapDetail {
    pub headings: Vec<SnapHeading>,
    pub links: Vec<SnapLink>,
    pub block_ids: Vec<String>,
    pub frontmatter_json: String,
}

#[derive(Debug, Clone, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapshotData {
    pub generation: u64,
    /// Indexed by file id; `None` for freed ids.
    pub entries: Vec<Option<SnapEntry>>,
    pub dirs: Vec<String>,
    pub pagerank: Vec<f32>,
    pub clusters: Vec<u32>,
    /// (score, last_access_ms) per file id.
    pub frecency: Vec<(f32, i64)>,
}

#[derive(Debug, Clone, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct LinksData {
    /// Must equal the snapshot's generation, else the links file is stale and ignored.
    pub generation: u64,
    pub symbols: Vec<String>,
    pub details: Vec<Option<SnapDetail>>,
}

fn write_file(path: &Path, magic: &[u8; 4], payload: &[u8]) -> Result<()> {
    let mut header = [0u8; HEADER];
    header[..4].copy_from_slice(magic);
    header[4..8].copy_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    header[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[16..24].copy_from_slice(&xxhash_rust::xxh3::xxh3_64(payload).to_le_bytes());
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("rkyv.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&header)?;
        f.write_all(payload)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Serialize and atomically write the file table and the links file.
pub fn save(dir: &Path, data: &SnapshotData, links: &LinksData) -> Result<()> {
    let payload = rkyv::to_bytes::<rancor::Error>(links).map_err(|e| StoreError::Encode(e.to_string()))?;
    write_file(&dir.join("links.rkyv"), LINKS_MAGIC, &payload)?;
    let payload = rkyv::to_bytes::<rancor::Error>(data).map_err(|e| StoreError::Encode(e.to_string()))?;
    write_file(&dir.join("snapshot.rkyv"), MAGIC, &payload)
}

const LINKS_MAGIC: &[u8; 4] = b"VLNK";

struct Mapped {
    map: memmap2::Mmap,
    verified: bool,
}

impl Mapped {
    fn open(path: &Path, magic: &[u8; 4], verify: bool) -> Result<Mapped> {
        let f = std::fs::File::open(path)?;
        // SAFETY: snapshot files are only ever replaced via rename (never modified in place).
        let map = unsafe { memmap2::Mmap::map(&f)? };
        if map.len() < HEADER || &map[..4] != magic {
            return Err(StoreError::BadSnapshot);
        }
        let ver = u32::from_le_bytes(map[4..8].try_into().unwrap());
        let len = u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;
        if ver != SNAPSHOT_VERSION || map.len() != HEADER + len {
            return Err(StoreError::BadSnapshot);
        }
        let mut m = Mapped { map, verified: false };
        if verify {
            m.verify()?;
        }
        Ok(m)
    }

    fn verify(&mut self) -> Result<()> {
        if !self.verified {
            let sum = u64::from_le_bytes(self.map[16..24].try_into().unwrap());
            if xxhash_rust::xxh3::xxh3_64(&self.map[HEADER..]) != sum {
                return Err(StoreError::BadSnapshot);
            }
            self.verified = true;
        }
        Ok(())
    }
}

/// The memory-mapped, checksum-verified file table. Access is zero-copy.
pub struct MappedSnapshot {
    m: Mapped,
}

impl MappedSnapshot {
    pub fn open(path: &Path) -> Result<MappedSnapshot> {
        Ok(MappedSnapshot { m: Mapped::open(path, MAGIC, true)? })
    }

    pub fn archived(&self) -> &ArchivedSnapshotData {
        // SAFETY: produced by `save` for this type and version; checksum verified in `open`.
        unsafe { rkyv::access_unchecked::<ArchivedSnapshotData>(&self.m.map[HEADER..]) }
    }

    pub fn to_owned(&self) -> Result<SnapshotData> {
        rkyv::deserialize::<SnapshotData, rancor::Error>(self.archived()).map_err(|e| StoreError::Encode(e.to_string()))
    }

    pub fn len_bytes(&self) -> usize {
        self.m.map.len()
    }
}

/// The links file, mapped at startup and verified lazily (off the critical path).
pub struct MappedLinks {
    m: Mapped,
}

impl MappedLinks {
    pub fn open(path: &Path) -> Result<MappedLinks> {
        Ok(MappedLinks { m: Mapped::open(path, LINKS_MAGIC, false)? })
    }

    /// Verify the checksum (once) and return the archive.
    pub fn archived(&mut self) -> Result<&ArchivedLinksData> {
        self.m.verify()?;
        // SAFETY: produced by `save` for this type and version; checksum verified above.
        Ok(unsafe { rkyv::access_unchecked::<ArchivedLinksData>(&self.m.map[HEADER..]) })
    }

    pub fn len_bytes(&self) -> usize {
        self.m.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_open_roundtrip_and_corruption() {
        let d = tempfile::tempdir().unwrap();
        let data = SnapshotData {
            generation: 7,
            entries: vec![None, Some(SnapEntry { path: "a.md".into(), title: "a".into(), tags: vec!["x".into()], ..Default::default() })],
            pagerank: vec![0.5, 0.5],
            ..Default::default()
        };
        let links = LinksData {
            generation: 7,
            symbols: vec!["b".into()],
            details: vec![None, Some(SnapDetail { links: vec![SnapLink { target: 0, dst: 1, ..Default::default() }], ..Default::default() })],
        };
        save(d.path(), &data, &links).unwrap();
        let m = MappedSnapshot::open(&d.path().join("snapshot.rkyv")).unwrap();
        assert_eq!(m.archived().entries.len(), 2);
        assert_eq!(m.archived().entries[1].as_ref().unwrap().path.as_str(), "a.md");
        assert_eq!(m.to_owned().unwrap(), data);
        let mut l = MappedLinks::open(&d.path().join("links.rkyv")).unwrap();
        assert_eq!(l.archived().unwrap().symbols.len(), 1);
        let p = d.path().join("snapshot.rkyv");
        let mut bytes = std::fs::read(&p).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xff;
        std::fs::write(&p, bytes).unwrap();
        assert!(MappedSnapshot::open(&p).is_err());
        let lp = d.path().join("links.rkyv");
        let mut bytes = std::fs::read(&lp).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xff;
        std::fs::write(&lp, bytes).unwrap();
        assert!(MappedLinks::open(&lp).unwrap().archived().is_err());
    }
}
