//! Terrain elevation for the 3D view.
//!
//! Heights come from the free AWS Terrain Tiles (Mapzen "Terrarium" PNG encoding: SRTM,
//! GMTED2010, ETOPO1 and national DEMs; ~30 m to ~1 km depending on zoom). Tiles are
//! fetched on demand, decoded to metres and sampled bilinearly; while a tile is loading
//! the nearest loaded ancestor (coarser zoom) stands in, so the relief sharpens instead of
//! popping in. Sea and anything below it is flattened to 0 m, the datum of the wind levels
//! and the barometric altitudes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

const URL: &str = "https://s3.amazonaws.com/elevation-tiles-prod/terrarium";
pub const ATTRIBUTION: &str = "Terrain: AWS Terrain Tiles (SRTM, GMTED2010, ETOPO1)";
/// Finest zoom requested (~75 m pixels at the equator) — plenty for a basemap grid.
pub const MAX_ZOOM: u8 = 11;
pub(crate) const SIZE: usize = 256;
/// Parallel downloads.
const MAX_IN_FLIGHT: usize = 16;

type Key = (u8, u32, u32);

enum Slot {
    Loading,
    Ready(Vec<f32>),
    Failed,
}

#[derive(Clone, Default)]
pub struct Terrain {
    tiles: Arc<Mutex<HashMap<Key, Slot>>>,
}

/// Decode a Terrarium PNG to a 256×256 row-major grid of metres (sea clamped to 0).
pub fn decode_terrarium(bytes: &[u8]) -> Option<Vec<f32>> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => return None,
    };
    if w != SIZE || h != SIZE {
        return None;
    }
    let mut out = Vec::with_capacity(SIZE * SIZE);
    for y in 0..h {
        let row = &buf[y * info.line_size..];
        for x in 0..w {
            let p = &row[x * ch..x * ch + 3];
            let e = p[0] as f32 * 256.0 + p[1] as f32 + p[2] as f32 / 256.0 - 32768.0;
            out.push(e.max(0.0));
        }
    }
    Some(out)
}

pub(crate) fn tile_xy(lat: f64, lon: f64, z: u8) -> (f64, f64) {
    let n = (1u32 << z) as f64;
    let lat = lat.clamp(-85.05, 85.05).to_radians();
    (
        (lon + 180.0) / 360.0 * n,
        (1.0 - lat.tan().asinh() / std::f64::consts::PI) / 2.0 * n,
    )
}

fn bilinear(g: &[f32], px: f64, py: f64) -> f32 {
    let m = (SIZE - 1) as f64;
    let (px, py) = (px.clamp(0.0, m), py.clamp(0.0, m));
    let (x0, y0) = (px.floor() as usize, py.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(SIZE - 1), (y0 + 1).min(SIZE - 1));
    let (fx, fy) = ((px - x0 as f64) as f32, (py - y0 as f64) as f32);
    let r0 = g[y0 * SIZE + x0] * (1.0 - fx) + g[y0 * SIZE + x1] * fx;
    let r1 = g[y1 * SIZE + x0] * (1.0 - fx) + g[y1 * SIZE + x1] * fx;
    r0 * (1.0 - fy) + r1 * fy
}

/// One frame's view of the loaded tiles. Records the tiles it was asked for but lacked;
/// [`Terrain::finish`] requests them.
pub struct Sampler<'a> {
    tiles: MutexGuard<'a, HashMap<Key, Slot>>,
    zoom: u8,
    want: Vec<Key>,
}

impl Sampler<'_> {
    /// Ground height (m above sea level) at a point, from the best loaded tile.
    pub fn elev(&mut self, lat: f64, lon: f64) -> f32 {
        let (fx, fy) = tile_xy(lat, lon, self.zoom);
        let max = (1u32 << self.zoom) - 1;
        let (tx, ty) = ((fx.floor() as u32).min(max), (fy.floor() as u32).min(max));
        let key = (self.zoom, tx, ty);
        if !self.tiles.contains_key(&key) && !self.want.contains(&key) {
            self.want.push(key);
        }
        // finest loaded tile at or above the wanted zoom
        for dz in 0..=self.zoom.min(8) {
            let z = self.zoom - dz;
            let s = (1u32 << dz) as f64;
            let (x, y) = (fx / s, fy / s);
            let (kx, ky) = (x.floor() as u32, y.floor() as u32);
            if let Some(Slot::Ready(g)) = self.tiles.get(&(z, kx, ky)) {
                let px = (x - kx as f64) * SIZE as f64 - 0.5;
                let py = (y - ky as f64) * SIZE as f64 - 0.5;
                return bilinear(g, px, py);
            }
        }
        0.0
    }
}

impl Terrain {
    /// Borrow the tiles for one frame, sampling at `zoom` (clamped to [`MAX_ZOOM`]).
    pub fn sampler(&self, zoom: u8) -> Sampler<'_> {
        Sampler {
            tiles: self.tiles.lock().unwrap_or_else(|e| e.into_inner()),
            zoom: zoom.min(MAX_ZOOM),
            want: Vec::new(),
        }
    }

    /// Release the frame's sampler and download the tiles it was missing.
    pub fn finish(&self, mut s: Sampler<'_>, ctx: &egui::Context) {
        let in_flight = s
            .tiles
            .values()
            .filter(|v| matches!(v, Slot::Loading))
            .count();
        let want: Vec<Key> = s
            .want
            .drain(..)
            .take(MAX_IN_FLIGHT.saturating_sub(in_flight))
            .collect();
        for k in &want {
            s.tiles.insert(*k, Slot::Loading);
        }
        drop(s);
        for (z, x, y) in want {
            let tiles = self.tiles.clone();
            let ctx = ctx.clone();
            let req = ehttp::Request::get(format!("{URL}/{z}/{x}/{y}.png"));
            ehttp::fetch(req, move |res| {
                let slot = match res {
                    Ok(r) if r.ok => decode_terrarium(&r.bytes).map_or(Slot::Failed, Slot::Ready),
                    _ => Slot::Failed,
                };
                if let Ok(mut t) = tiles.lock() {
                    t.insert((z, x, y), slot);
                }
                ctx.request_repaint();
            });
        }
    }

    /// Tiles still downloading (for the status line).
    pub fn loading(&self) -> usize {
        self.tiles
            .lock()
            .map(|t| t.values().filter(|v| matches!(v, Slot::Loading)).count())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn insert(&self, k: Key, g: Vec<f32>) {
        self.tiles.lock().unwrap().insert(k, Slot::Ready(g));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_rgb(f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, SIZE as u32, SIZE as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let mut data = Vec::with_capacity(SIZE * SIZE * 3);
            for y in 0..SIZE {
                for x in 0..SIZE {
                    data.extend_from_slice(&f(x, y));
                }
            }
            w.write_image_data(&data).unwrap();
        }
        out
    }

    #[test]
    fn terrarium_decoding() {
        // 32768 + 1000.5 m → R=131 (33536), G=232 (+232 = 33768), B=128 (+0.5)
        let bytes = png_rgb(|x, _| {
            if x < 128 {
                [131, 232, 128]
            } else {
                [127, 0, 0]
            }
        });
        let g = decode_terrarium(&bytes).unwrap();
        assert_eq!(g.len(), SIZE * SIZE);
        assert!((g[0] - 1000.5).abs() < 1e-3);
        assert_eq!(g[200], 0.0, "below sea level is flattened");
        assert!(decode_terrarium(b"not a png").is_none());
    }

    #[test]
    fn sampling_falls_back_to_coarser_tiles_and_records_wants() {
        let t = Terrain::default();
        // zoom-3 tile over central Europe, a west→east ramp 0..2550 m
        let (fx, fy) = tile_xy(47.7, 9.0, 3);
        let (x3, y3) = (fx as u32, fy as u32);
        t.insert(
            (3, x3, y3),
            (0..SIZE * SIZE).map(|i| (i % SIZE) as f32 * 10.0).collect(),
        );
        let mut s = t.sampler(10);
        let e = s.elev(47.7, 9.0);
        let expect = ((fx - x3 as f64) * SIZE as f64 - 0.5) as f32 * 10.0;
        assert!((e - expect).abs() < 1.0, "{e} vs {expect}");
        assert_eq!(s.want.len(), 1);
        assert_eq!(s.want[0].0, 10);
        // nothing loaded anywhere near → sea level
        assert_eq!(s.elev(-40.0, -120.0), 0.0);
        assert_eq!(s.want.len(), 2);
    }
}
