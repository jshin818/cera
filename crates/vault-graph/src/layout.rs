//! Force-directed layout: Barnes-Hut repulsion (quadtree, theta 0.8), springs along links,
//! centering gravity and a soft collision radius. Parallel over nodes with rayon.
//! The converged layout persists to `layout.bin`; reopening starts from it (no "explosion"), and the
//! simulation freezes when kinetic energy falls below a threshold.

use crate::Graph;
use rayon::prelude::*;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct LayoutParams {
    pub repulsion: f32,
    pub spring_len: f32,
    pub spring_k: f32,
    pub gravity: f32,
    pub damping: f32,
    pub theta: f32,
    pub collide: f32,
}

impl Default for LayoutParams {
    fn default() -> Self {
        LayoutParams { repulsion: 60.0, spring_len: 30.0, spring_k: 0.04, gravity: 0.004, damping: 0.85, theta: 0.8, collide: 4.0 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub pos: Vec<[f32; 2]>,
    pub vel: Vec<[f32; 2]>,
    pub frozen: bool,
}

struct Quad {
    cx: f32,
    cy: f32,
    half: f32,
    mass: f32,
    mx: f32,
    my: f32,
    children: Option<Box<[Quad; 4]>>,
    point: Option<usize>,
}

impl Quad {
    fn new(cx: f32, cy: f32, half: f32) -> Quad {
        Quad { cx, cy, half, mass: 0.0, mx: 0.0, my: 0.0, children: None, point: None }
    }

    fn insert(&mut self, i: usize, p: [f32; 2], pos: &[[f32; 2]], depth: u32) {
        self.mx = (self.mx * self.mass + p[0]) / (self.mass + 1.0);
        self.my = (self.my * self.mass + p[1]) / (self.mass + 1.0);
        self.mass += 1.0;
        if self.mass == 1.0 && self.children.is_none() {
            self.point = Some(i);
            return;
        }
        if depth > 40 {
            return; // coincident points: keep as aggregated mass
        }
        if self.children.is_none() {
            let h = self.half / 2.0;
            self.children = Some(Box::new([
                Quad::new(self.cx - h, self.cy - h, h),
                Quad::new(self.cx + h, self.cy - h, h),
                Quad::new(self.cx - h, self.cy + h, h),
                Quad::new(self.cx + h, self.cy + h, h),
            ]));
            if let Some(j) = self.point.take() {
                let q = self.child_index(pos[j]);
                self.children.as_mut().unwrap()[q].insert(j, pos[j], pos, depth + 1);
            }
        }
        let q = self.child_index(p);
        self.children.as_mut().unwrap()[q].insert(i, p, pos, depth + 1);
    }

    fn child_index(&self, p: [f32; 2]) -> usize {
        (if p[0] >= self.cx { 1 } else { 0 }) + (if p[1] >= self.cy { 2 } else { 0 })
    }

    fn force(&self, i: usize, p: [f32; 2], theta: f32, k: f32, collide: f32, out: &mut [f32; 2]) {
        if self.mass == 0.0 || self.point == Some(i) && self.children.is_none() {
            return;
        }
        let dx = p[0] - self.mx;
        let dy = p[1] - self.my;
        let d2 = dx * dx + dy * dy + 0.01;
        let d = d2.sqrt();
        if self.children.is_none() || (self.half * 2.0) / d < theta {
            let mut f = k * self.mass / d2;
            if d < collide {
                f += (collide - d) * 0.5;
            }
            out[0] += dx / d * f;
            out[1] += dy / d * f;
            return;
        }
        for c in self.children.as_ref().unwrap().iter() {
            c.force(i, p, theta, k, collide, out);
        }
    }
}

impl Layout {
    /// Initialise positions: keep `prev` where available, place new nodes near a linked neighbour
    /// (or on a golden-angle spiral) so the existing picture does not explode.
    pub fn seeded(g: &Graph, active: &[bool], prev: Option<&Layout>) -> Layout {
        let n = g.n;
        let mut pos = vec![[0f32; 2]; n];
        let mut known = vec![false; n];
        if let Some(p) = prev {
            for i in 0..n.min(p.pos.len()) {
                if active[i] && p.pos[i] != [0.0, 0.0] {
                    pos[i] = p.pos[i];
                    known[i] = true;
                }
            }
        }
        let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        let count = active.iter().filter(|a| **a).count().max(1) as f32;
        for i in 0..n {
            if !active[i] || known[i] {
                continue;
            }
            if let Some(&nb) = g.neighbors(i as u32).iter().find(|&&u| known[u as usize]) {
                let a = i as f32 * golden;
                pos[i] = [pos[nb as usize][0] + 8.0 * a.cos(), pos[nb as usize][1] + 8.0 * a.sin()];
            } else {
                let r = 12.0 * (i as f32 + 1.0).sqrt() * (count / 1000.0).max(1.0).sqrt().min(4.0);
                let a = i as f32 * golden;
                pos[i] = [r * a.cos(), r * a.sin()];
            }
            known[i] = true;
        }
        Layout { pos, vel: vec![[0.0; 2]; n], frozen: false }
    }

    /// One simulation step. Returns kinetic energy per node.
    pub fn step(&mut self, g: &Graph, active: &[bool], p: &LayoutParams) -> f32 {
        let n = g.n;
        let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for i in 0..n {
            if active[i] {
                minx = minx.min(self.pos[i][0]);
                miny = miny.min(self.pos[i][1]);
                maxx = maxx.max(self.pos[i][0]);
                maxy = maxy.max(self.pos[i][1]);
            }
        }
        if minx > maxx {
            return 0.0;
        }
        let half = ((maxx - minx).max(maxy - miny) / 2.0 + 1.0).max(1.0);
        let mut root = Quad::new((minx + maxx) / 2.0, (miny + maxy) / 2.0, half);
        for i in 0..n {
            if active[i] {
                root.insert(i, self.pos[i], &self.pos, 0);
            }
        }
        let pos = &self.pos;
        let forces: Vec<[f32; 2]> = (0..n)
            .into_par_iter()
            .map(|i| {
                if !active[i] {
                    return [0.0, 0.0];
                }
                let mut f = [0f32; 2];
                root.force(i, pos[i], p.theta, p.repulsion, p.collide, &mut f);
                for (&j, &w) in g.neighbors(i as u32).iter().zip(g.weights(i as u32)) {
                    let dx = pos[j as usize][0] - pos[i][0];
                    let dy = pos[j as usize][1] - pos[i][1];
                    let d = (dx * dx + dy * dy).sqrt().max(0.01);
                    let s = p.spring_k * (d - p.spring_len) * w.min(3.0) / (g.degree(i as u32) as f32).sqrt().max(1.0);
                    f[0] += dx / d * s;
                    f[1] += dy / d * s;
                }
                f[0] -= pos[i][0] * p.gravity;
                f[1] -= pos[i][1] * p.gravity;
                f
            })
            .collect();
        let mut energy = 0.0f32;
        let mut cnt = 0.0f32;
        for i in 0..n {
            if !active[i] {
                continue;
            }
            for k in 0..2 {
                let f = forces[i][k].clamp(-50.0, 50.0);
                self.vel[i][k] = (self.vel[i][k] + f) * p.damping;
                self.pos[i][k] += self.vel[i][k];
            }
            energy += self.vel[i][0] * self.vel[i][0] + self.vel[i][1] * self.vel[i][1];
            cnt += 1.0;
        }
        energy / cnt.max(1.0)
    }

    /// Run until energy < threshold or `max_steps`. Marks the layout frozen when converged.
    pub fn run(&mut self, g: &Graph, active: &[bool], p: &LayoutParams, max_steps: usize, threshold: f32) -> usize {
        for s in 0..max_steps {
            if self.step(g, active, p) < threshold {
                self.frozen = true;
                return s + 1;
            }
        }
        max_steps
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        w.write_all(b"VLAY")?;
        w.write_all(&(self.pos.len() as u64).to_le_bytes())?;
        for p in &self.pos {
            w.write_all(&p[0].to_le_bytes())?;
            w.write_all(&p[1].to_le_bytes())?;
        }
        w.into_inner()?.sync_all()?;
        std::fs::rename(tmp, path)
    }

    pub fn load(path: &Path) -> std::io::Result<Layout> {
        let mut r = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut m = [0u8; 4];
        r.read_exact(&mut m)?;
        if &m != b"VLAY" {
            return Err(std::io::Error::other("bad layout file"));
        }
        let mut b8 = [0u8; 8];
        r.read_exact(&mut b8)?;
        let n = u64::from_le_bytes(b8) as usize;
        let mut pos = Vec::with_capacity(n);
        let mut b4 = [0u8; 4];
        for _ in 0..n {
            r.read_exact(&mut b4)?;
            let x = f32::from_le_bytes(b4);
            r.read_exact(&mut b4)?;
            pos.push([x, f32::from_le_bytes(b4)]);
        }
        Ok(Layout { vel: vec![[0.0; 2]; n], pos, frozen: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_nodes_end_closer_than_unlinked() {
        let mut e = Vec::new();
        for a in 0..10u32 {
            for b in a + 1..10 {
                e.push((a, b));
                e.push((a + 10, b + 10));
            }
        }
        let g = Graph::new(20, &e);
        let active = vec![true; 20];
        let mut l = Layout::seeded(&g, &active, None);
        let steps = l.run(&g, &active, &LayoutParams::default(), 2000, 0.001);
        assert!(steps < 2000, "converges");
        let d = |a: usize, b: usize| ((l.pos[a][0] - l.pos[b][0]).powi(2) + (l.pos[a][1] - l.pos[b][1]).powi(2)).sqrt();
        let intra = (1..10).map(|j| d(0, j)).sum::<f32>() / 9.0;
        let inter = (10..20).map(|j| d(0, j)).sum::<f32>() / 10.0;
        assert!(intra < inter, "{intra} {inter}");
        let dir = std::env::temp_dir().join(format!("vlay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        l.save(&dir.join("layout.bin")).unwrap();
        let l2 = Layout::load(&dir.join("layout.bin")).unwrap();
        assert_eq!(l2.pos, l.pos);
        let seeded = Layout::seeded(&g, &active, Some(&l2));
        assert_eq!(seeded.pos[3], l.pos[3], "reuses persisted positions");
    }
}
