//! Append-only string interner shared by the engine state and every snapshot. Link targets,
//! displays and subpaths repeat heavily across a vault (the same titles are linked thousands of
//! times), so links store 4-byte symbols instead of strings.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

pub const NONE: u32 = u32::MAX;

#[derive(Default)]
pub struct Interner {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<Arc<str>, u32>,
    strs: Vec<Arc<str>>,
}

impl Interner {
    pub fn from_vec(v: Vec<String>) -> Interner {
        let strs: Vec<Arc<str>> = v.into_iter().map(Arc::from).collect();
        let map = strs.iter().enumerate().map(|(i, s)| (s.clone(), i as u32)).collect();
        Interner { inner: RwLock::new(Inner { map, strs }) }
    }

    pub fn intern(&self, s: &str) -> u32 {
        if let Some(&id) = self.inner.read().map.get(s) {
            return id;
        }
        let mut w = self.inner.write();
        if let Some(&id) = w.map.get(s) {
            return id;
        }
        let id = w.strs.len() as u32;
        let a: Arc<str> = Arc::from(s);
        w.strs.push(a.clone());
        w.map.insert(a, id);
        id
    }

    pub fn opt(&self, s: Option<&str>) -> u32 {
        s.map(|s| self.intern(s)).unwrap_or(NONE)
    }

    pub fn lookup(&self, s: &str) -> Option<u32> {
        self.inner.read().map.get(s).copied()
    }

    pub fn get(&self, id: u32) -> Arc<str> {
        if id == NONE {
            return Arc::from("");
        }
        self.inner.read().strs.get(id as usize).cloned().unwrap_or_else(|| Arc::from(""))
    }

    pub fn get_opt(&self, id: u32) -> Option<Arc<str>> {
        (id != NONE).then(|| self.get(id))
    }

    pub fn to_vec(&self) -> Vec<String> {
        self.inner.read().strs.iter().map(|s| s.to_string()).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().strs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
