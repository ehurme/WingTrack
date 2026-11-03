//! Weather field layers from the ERA5 cells already downloaded around the animals:
//! colour fields (2 m temperature, precipitation, cloud cover, wind speed, MSL pressure),
//! isobars, and pressure centres (lows / highs) with a simple storm flag.
//!
//! The wind-field windows fetch every surface variable at a regular spacing (the
//! "wind-field spacing", in 0.25° ERA5 cells), so the cells on that lattice form a grid
//! for the hour under the cursor.
//!
//! Storms: a low (local MSL minimum over a ~2-step neighbourhood, ≥ 1 hPa below its
//! surroundings) is flagged when its central pressure is ≤ 995 hPa or the 10 m wind
//! within ~3 steps reaches gale force (≥ 17.2 m/s, Beaufort 8). A screening aid for
//! "did the animal fly into a storm", not a cyclone-tracking algorithm.

use crate::weather::{CellKey, GRID, Values, WeatherStore};
use egui::Color32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldVar {
    Off,
    Temp,
    Precip,
    Cloud,
    Wind,
    Pressure,
    CloudBase,
}

impl FieldVar {
    pub const ALL: [FieldVar; 7] = [
        FieldVar::Off,
        FieldVar::Temp,
        FieldVar::Precip,
        FieldVar::Cloud,
        FieldVar::Wind,
        FieldVar::Pressure,
        FieldVar::CloudBase,
    ];
    pub fn label(self) -> &'static str {
        match self {
            FieldVar::Off => "none",
            FieldVar::Temp => "2 m temperature",
            FieldVar::Precip => "precipitation",
            FieldVar::Cloud => "cloud cover",
            FieldVar::Wind => "10 m wind speed",
            FieldVar::Pressure => "MSL pressure",
            FieldVar::CloudBase => "cloud base (LCL, where low cloud)",
        }
    }
    /// (legend title, low label, high label)
    pub fn legend(self) -> (&'static str, &'static str, &'static str) {
        match self {
            FieldVar::Off => ("", "", ""),
            FieldVar::Temp => ("ERA5 2 m temperature (°C)", "-10", "35"),
            FieldVar::Precip => ("ERA5 precipitation (mm/h)", "0.1", "5+"),
            FieldVar::Cloud => ("ERA5 cloud cover (%)", "0", "100"),
            FieldVar::Wind => ("ERA5 10 m wind speed (m/s)", "0", "20+"),
            FieldVar::Pressure => ("ERA5 MSL pressure (hPa)", "980", "1040"),
            FieldVar::CloudBase => ("cloud base, LCL (m ASL)", "0", "3000+"),
        }
    }
    pub fn value(self, v: &Values) -> Option<f32> {
        let x = match self {
            FieldVar::Off => return None,
            FieldVar::Temp => v[4],
            FieldVar::Precip => v[7],
            FieldVar::Cloud => v[8],
            FieldVar::Wind => v[0],
            FieldVar::Pressure => v[5],
            // only where ERA5 reports some low cloud (otherwise it's just where cloud *would* form)
            FieldVar::CloudBase => {
                let low = v[crate::weather::CLOUD_LOW];
                if low.is_finite() && low < 10.0 {
                    return None;
                }
                crate::weather::cloud_base_m(v)?
            }
        };
        x.is_finite().then_some(x)
    }
    /// Position on the colour scale (0..1).
    pub fn norm(self, x: f32) -> f32 {
        match self {
            FieldVar::Temp => (x + 10.0) / 45.0,
            FieldVar::Precip => ((x.max(0.1) / 0.1).ln() / 50f32.ln()).clamp(0.0, 1.0),
            FieldVar::Cloud => x / 100.0,
            FieldVar::Wind => x / 20.0,
            FieldVar::Pressure => (x - 980.0) / 60.0,
            FieldVar::CloudBase => x / 3000.0,
            FieldVar::Off => 0.0,
        }
        .clamp(0.0, 1.0)
    }
    /// Colour at scale position `t` (alpha included; dry / clear cells fade out).
    pub fn color(self, t: f32, raw: f32) -> Color32 {
        let lerp = |stops: &[(f32, [u8; 3])], t: f32| {
            let t = t.clamp(0.0, 1.0);
            for w in stops.windows(2) {
                if t <= w[1].0 {
                    let f = (t - w[0].0) / (w[1].0 - w[0].0).max(1e-6);
                    let c = |k: usize| {
                        (w[0].1[k] as f32 + (w[1].1[k] as f32 - w[0].1[k] as f32) * f) as u8
                    };
                    return [c(0), c(1), c(2)];
                }
            }
            stops[stops.len() - 1].1
        };
        match self {
            FieldVar::Temp => {
                let c = lerp(
                    &[
                        (0.0, [49, 54, 149]),
                        (0.25, [116, 173, 209]),
                        (0.45, [224, 243, 248]),
                        (0.6, [254, 224, 144]),
                        (0.8, [244, 109, 67]),
                        (1.0, [165, 0, 38]),
                    ],
                    t,
                );
                Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 120)
            }
            FieldVar::Precip => {
                if raw < 0.1 {
                    return Color32::TRANSPARENT;
                }
                let c = lerp(
                    &[
                        (0.0, [160, 210, 255]),
                        (0.35, [40, 120, 230]),
                        (0.6, [60, 200, 90]),
                        (0.8, [250, 210, 40]),
                        (1.0, [220, 30, 30]),
                    ],
                    t,
                );
                Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 170)
            }
            FieldVar::Cloud => {
                let a = (t * 170.0) as u8;
                Color32::from_rgba_unmultiplied(235, 238, 245, a)
            }
            FieldVar::Wind => {
                let c = lerp(
                    &[
                        (0.0, [40, 40, 120]),
                        (0.3, [40, 150, 180]),
                        (0.55, [120, 200, 80]),
                        (0.8, [250, 200, 40]),
                        (1.0, [230, 40, 40]),
                    ],
                    t,
                );
                Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 120)
            }
            FieldVar::Pressure => {
                let c = lerp(
                    &[
                        (0.0, [120, 40, 160]),
                        (0.55, [235, 235, 235]),
                        (1.0, [230, 140, 30]),
                    ],
                    t,
                );
                Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 90)
            }
            FieldVar::CloudBase => {
                // low base: dark grey; high base: pale
                let c = lerp(
                    &[
                        (0.0, [70, 80, 100]),
                        (0.5, [150, 160, 180]),
                        (1.0, [235, 240, 250]),
                    ],
                    t,
                );
                Color32::from_rgba_unmultiplied(c[0], c[1], c[2], 150)
            }
            FieldVar::Off => Color32::TRANSPARENT,
        }
    }
}

/// Values on the regular lattice of cells (every `step` 0.25° cells), NaN where missing.
#[derive(Clone, Debug)]
pub struct Grid {
    pub i0: i32,
    pub j0: i32,
    pub step: i32,
    pub ni: usize,
    pub nj: usize,
    pub v: Vec<f32>,
}

impl Grid {
    pub fn at(&self, a: usize, b: usize) -> f32 {
        self.v[a * self.nj + b]
    }
    pub fn latlon(&self, a: f64, b: f64) -> (f64, f64) {
        (
            (self.i0 as f64 + a * self.step as f64) * GRID,
            (self.j0 as f64 + b * self.step as f64) * GRID,
        )
    }
}

/// Lattice for the hour `t` with values from `f` (None when no data or the grid would be huge).
pub fn grid(
    store: &WeatherStore,
    t: i64,
    step: i32,
    f: impl Fn(&Values) -> Option<f32>,
) -> Option<Grid> {
    let st = step.max(1);
    let keys: Vec<CellKey> = store
        .cells
        .keys()
        .copied()
        .filter(|k| k.0.rem_euclid(st) == 0 && k.1.rem_euclid(st) == 0)
        .collect();
    if keys.len() < 4 {
        return None;
    }
    let (i0, i1) = keys
        .iter()
        .fold((i32::MAX, i32::MIN), |a, k| (a.0.min(k.0), a.1.max(k.0)));
    let (j0, j1) = keys
        .iter()
        .fold((i32::MAX, i32::MIN), |a, k| (a.0.min(k.1), a.1.max(k.1)));
    let ni = ((i1 - i0) / st + 1) as usize;
    let nj = ((j1 - j0) / st + 1) as usize;
    if ni * nj > 250_000 {
        return None;
    }
    let mut v = vec![f32::NAN; ni * nj];
    let mut any = false;
    for k in keys {
        if let Some(x) = store.sample_cell(k, t).and_then(|vals| f(&vals)) {
            let a = ((k.0 - i0) / st) as usize;
            let b = ((k.1 - j0) / st) as usize;
            v[a * nj + b] = x;
            any = true;
        }
    }
    any.then_some(Grid {
        i0,
        j0,
        step: st,
        ni,
        nj,
        v,
    })
}

/// Contour line segments at `level` (marching squares), in (lat, lon).
pub fn contour(g: &Grid, level: f32) -> Vec<[(f64, f64); 2]> {
    let mut out = Vec::new();
    for a in 0..g.ni.saturating_sub(1) {
        for b in 0..g.nj.saturating_sub(1) {
            // corners: 0 (a,b) 1 (a,b+1) 2 (a+1,b+1) 3 (a+1,b)
            let c = [
                g.at(a, b),
                g.at(a, b + 1),
                g.at(a + 1, b + 1),
                g.at(a + 1, b),
            ];
            if c.iter().any(|x| !x.is_finite()) {
                continue;
            }
            let pos = [
                (a as f64, b as f64),
                (a as f64, b as f64 + 1.0),
                (a as f64 + 1.0, b as f64 + 1.0),
                (a as f64 + 1.0, b as f64),
            ];
            let mut pts: Vec<(f64, f64)> = Vec::with_capacity(4);
            for e in 0..4 {
                let (p, q) = (e, (e + 1) % 4);
                let (vp, vq) = (c[p], c[q]);
                if (vp < level) != (vq < level) {
                    let f = ((level - vp) / (vq - vp)) as f64;
                    let (pa, pb) = pos[p];
                    let (qa, qb) = pos[q];
                    pts.push(g.latlon(pa + (qa - pa) * f, pb + (qb - pb) * f));
                }
            }
            match pts.len() {
                2 => out.push([pts[0], pts[1]]),
                4 => {
                    // saddle: pair by the cell mean
                    let mean = (c[0] + c[1] + c[2] + c[3]) / 4.0;
                    if (mean < level) == (c[0] < level) {
                        out.push([pts[0], pts[3]]);
                        out.push([pts[1], pts[2]]);
                    } else {
                        out.push([pts[0], pts[1]]);
                        out.push([pts[2], pts[3]]);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Isobar levels (hPa) covering the grid, every `every` hPa.
pub fn levels(g: &Grid, every: f32) -> Vec<f32> {
    let (lo, hi) =
        g.v.iter()
            .filter(|x| x.is_finite())
            .fold((f32::INFINITY, f32::NEG_INFINITY), |a, &x| {
                (a.0.min(x), a.1.max(x))
            });
    if !lo.is_finite() {
        return Vec::new();
    }
    let mut l = (lo / every).ceil() * every;
    let mut out = Vec::new();
    while l <= hi && out.len() < 60 {
        out.push(l);
        l += every;
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct Centre {
    pub lat: f64,
    pub lon: f64,
    pub hpa: f32,
    pub low: bool,
    /// Strongest 10 m wind (m/s) within ~3 grid steps.
    pub max_wind: Option<f32>,
    pub storm: bool,
}

/// Lows and highs of an MSL grid; `wind` (10 m speed on the same lattice) flags storms.
pub fn centres(p: &Grid, wind: Option<&Grid>) -> Vec<Centre> {
    const R: usize = 2;
    let mut out = Vec::new();
    if p.ni < 2 * R + 1 || p.nj < 2 * R + 1 {
        return out;
    }
    for a in R..p.ni - R {
        for b in R..p.nj - R {
            let v = p.at(a, b);
            if !v.is_finite() {
                continue;
            }
            let mut ring = Vec::new();
            let (mut is_min, mut is_max, mut complete) = (true, true, true);
            for da in 0..=2 * R {
                for db in 0..=2 * R {
                    if da == R && db == R {
                        continue;
                    }
                    let w = p.at(a + da - R, b + db - R);
                    if !w.is_finite() {
                        complete = false;
                        continue;
                    }
                    is_min &= v < w;
                    is_max &= v > w;
                    if da == 0 || db == 0 || da == 2 * R || db == 2 * R {
                        ring.push(w);
                    }
                }
            }
            if !complete || ring.is_empty() || !(is_min || is_max) {
                continue;
            }
            let mean = ring.iter().sum::<f32>() / ring.len() as f32;
            if (mean - v).abs() < 1.0 {
                continue; // too shallow to be a system
            }
            let max_wind = wind.and_then(|w| {
                let r3 = 3;
                let mut m = f32::NEG_INFINITY;
                for da in a.saturating_sub(r3)..=(a + r3).min(w.ni - 1) {
                    for db in b.saturating_sub(r3)..=(b + r3).min(w.nj - 1) {
                        let x = w.at(da, db);
                        if x.is_finite() {
                            m = m.max(x);
                        }
                    }
                }
                m.is_finite().then_some(m)
            });
            let (lat, lon) = p.latlon(a as f64, b as f64);
            out.push(Centre {
                lat,
                lon,
                hpa: v,
                low: is_min,
                max_wind,
                storm: is_min && (v <= 995.0 || max_wind.is_some_and(|m| m >= 17.2)),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with an idealised low (990 hPa, gale ring) on a 1° lattice for one hour.
    fn store() -> WeatherStore {
        let mut s = WeatherStore::default();
        let t = 1_700_000_000i64 / 3600 * 3600;
        for i in (180..=220).step_by(4) {
            for j in (0..=40).step_by(4) {
                let (lat, lon) = (i as f64 * GRID, j as f64 * GRID);
                let d = ((lat - 50.0).powi(2) + (lon - 5.0).powi(2)).sqrt();
                let mut v = [f32::NAN; crate::weather::NV];
                v[5] = (1015.0 - 25.0 * (-d * d / 8.0).exp()) as f32;
                v[0] = (20.0 * (-(d - 2.0).powi(2)).exp()) as f32;
                v[4] = (lat - 40.0) as f32;
                s.cells.entry((i, j)).or_default().insert(t, v);
            }
        }
        s
    }

    #[test]
    fn grid_contours_and_storm() {
        let s = store();
        let t = 1_700_000_000i64 / 3600 * 3600;
        let p = grid(&s, t, 4, |v| FieldVar::Pressure.value(v)).unwrap();
        assert_eq!((p.ni, p.nj), (11, 11));
        let w = grid(&s, t, 4, |v| FieldVar::Wind.value(v)).unwrap();
        let c = centres(&p, Some(&w));
        let lows: Vec<&Centre> = c.iter().filter(|c| c.low).collect();
        assert_eq!(lows.len(), 1);
        assert!((lows[0].lat - 50.0).abs() < 0.01 && (lows[0].lon - 5.0).abs() < 0.01);
        assert!(lows[0].hpa < 991.0 && lows[0].storm);
        let lv = levels(&p, 4.0);
        assert!(lv.contains(&1000.0) && lv[0] >= 990.0);
        let iso = contour(&p, 1000.0);
        assert!(iso.len() >= 6, "closed ring around the low");
        for seg in &iso {
            for (la, lo) in seg {
                let d = ((la - 50.0).powi(2) + (lo - 5.0).powi(2)).sqrt();
                assert!(d > 1.0 && d < 4.5, "1000 hPa isobar ~2.4° from centre: {d}");
            }
        }
        assert!(FieldVar::Precip.color(0.0, 0.0) == Color32::TRANSPARENT);
        // fix cells off the lattice are ignored
        assert!(grid(&WeatherStore::default(), t, 4, |v| FieldVar::Temp.value(v)).is_none());
    }
}
