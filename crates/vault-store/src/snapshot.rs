//! The startup snapshot: one rkyv archive, memory-mapped. Layout:
//! `[magic "VSNP" | version u32 | payload_len u64 | xxh3 u64 | pad to 32] [rkyv payload]`.
//! The XXH3 check replaces bytecheck validation so that opening a 100k-note snapshot costs
//! a hash pass instead of a full structural walk.

use crate::{Result, StoreError};
use rkyv::rancor;
use std::io::Write;
use std::path::Path;

pub const SNAPSHOT_VERSION: u32 = 3;
const MAGIC: &[u8; 4] = b"VSNP";
const HEADER: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapHeading {
    pub level: u8,
    pub text: String,
    pub offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SnapLink {
    pub target: String,
    /// 0 none, 1 heading, 2 block
    pub sub_kind: u8,
    pub subpath: String,
    pub display: String,
    /// 0 link, 1 embed
    pub kind: u8,
    pub start: u32,
    pub end: u32,
    /// Resolved destination file id, `u32::MAX` if unresolved.
    pub dst: u32,
}

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
    pub headings: Vec<SnapHeading>,
    pub links: Vec<SnapLink>,
    pub block_ids: Vec<String>,
    pub word_count: u32,
    pub tasks_open: u32,
    pub tasks_done: u32,
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

/// Serialize and atomically write a snapshot.
pub fn save(path: &Path, data: &SnapshotData) -> Result<()> {
    let payload = rkyv::to_bytes::<rancor::Error>(data).map_err(|e| StoreError::Encode(e.to_string()))?;
    let mut header = [0u8; HEADER];
    header[..4].copy_from_slice(MAGIC);
    header[4..8].copy_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    header[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    header[16..24].copy_from_slice(&xxhash_rust::xxh3::xxh3_64(&payload).to_le_bytes());
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("rkyv.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&header)?;
        f.write_all(&payload)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// A memory-mapped, checksum-verified snapshot. Access is zero-copy.
pub struct MappedSnapshot {
    map: memmap2::Mmap,
}

impl MappedSnapshot {
    pub fn open(path: &Path) -> Result<MappedSnapshot> {
        let f = std::fs::File::open(path)?;
        // SAFETY: the file is only replaced via rename (never modified in place) by this process.
        let map = unsafe { memmap2::Mmap::map(&f)? };
        if map.len() < HEADER || &map[..4] != MAGIC {
            return Err(StoreError::BadSnapshot);
        }
        let ver = u32::from_le_bytes(map[4..8].try_into().unwrap());
        let len = u64::from_le_bytes(map[8..16].try_into().unwrap()) as usize;
        let sum = u64::from_le_bytes(map[16..24].try_into().unwrap());
        if ver != SNAPSHOT_VERSION || map.len() != HEADER + len || xxhash_rust::xxh3::xxh3_64(&map[HEADER..]) != sum {
            return Err(StoreError::BadSnapshot);
        }
        Ok(MappedSnapshot { map })
    }

    pub fn archived(&self) -> &ArchivedSnapshotData {
        // SAFETY: the payload was produced by `save` for this exact type and version, and its
        // checksum matched in `open`. The mmap is page-aligned and the header is 32 bytes.
        unsafe { rkyv::access_unchecked::<ArchivedSnapshotData>(&self.map[HEADER..]) }
    }

    pub fn to_owned(&self) -> Result<SnapshotData> {
        rkyv::deserialize::<SnapshotData, rancor::Error>(self.archived()).map_err(|e| StoreError::Encode(e.to_string()))
    }

    pub fn len_bytes(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_open_roundtrip_and_corruption() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("snapshot.rkyv");
        let data = SnapshotData {
            generation: 7,
            entries: vec![None, Some(SnapEntry { path: "a.md".into(), title: "a".into(), tags: vec!["x".into()], ..Default::default() })],
            pagerank: vec![0.5, 0.5],
            ..Default::default()
        };
        save(&p, &data).unwrap();
        let m = MappedSnapshot::open(&p).unwrap();
        assert_eq!(m.archived().entries.len(), 2);
        assert_eq!(m.archived().entries[1].as_ref().unwrap().path.as_str(), "a.md");
        assert_eq!(m.to_owned().unwrap(), data);
        let mut bytes = std::fs::read(&p).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xff;
        std::fs::write(&p, bytes).unwrap();
        assert!(MappedSnapshot::open(&p).is_err());
    }
}
