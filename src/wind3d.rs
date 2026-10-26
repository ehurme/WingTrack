//! Pressure-level ("3D") wind: download, storage, interpolation and flight-altitude annotation.
//!
//! Two sources fill the same [`ProfileStore`]:
//! * **Open-Meteo Historical Forecast API** — archived operational analyses (GFS, ICON, IFS…)
//!   at 1000–700 hPa with wind speed/direction, geopotential height and temperature. Works in
//!   the browser without a key; available from about 2021–2022 onwards.
//! * **ERA5 wind cube file** — a long CSV (optionally gzipped) exported from your own ERA5
//!   pressure-level downloads with `tools/era5_wind_cube.py`. Any set of levels (e.g. 1000–500 hPa).
//!
//! Each 0.25° cell holds an hourly series of [`Profile`]s (u, v, geopotential height,
//! temperature per level, plus surface pressure so underground levels can be masked).
//! Flight-level matching follows `SigfoxTagPrep::annotate_era5()`: the animal's pressure is
//! matched to the nearest level, and the best level is the one with maximum wind support —
//! here restricted to levels above the ground.

use crate::geo;
use crate::model::{AltitudeSource, Fix, FlightWind, Track};
use crate::table::{normalize_header, parse_num, parse_time};
use crate::weather::{CellKey, GRID, cell_center, cell_of, day_of, day_str, plan_cell_days};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const OM_API: &str = "https://historical-forecast-api.open-meteo.com/v1/forecast";
/// Pressure levels requested from Open-Meteo (≈ 110 m … 3 km).
pub const OM_LEVELS: [u16; 8] = [1000, 975, 950, 925, 900, 850, 800, 700];
/// Locations per request: each location asks for 33 variables, so keep requests modest.
const CELLS_PER_REQUEST: usize = 10;
const G0: f32 = 9.80665;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LevelVal {
    /// Wind towards east / north (m/s).
    pub u: f32,
    pub v: f32,
    /// Geopotential height (m above mean sea level).
    pub z: f32,
    /// Temperature (°C).
    pub t: f32,
}

impl LevelVal {
    const NAN: LevelVal = LevelVal {
        u: f32::NAN,
        v: f32::NAN,
        z: f32::NAN,
        t: f32::NAN,
    };
    fn ok(&self) -> bool {
        self.u.is_finite() && self.v.is_finite()
    }
    pub fn speed(&self) -> f32 {
        (self.u * self.u + self.v * self.v).sqrt()
    }
}

/// One vertical wind profile; `lv[i]` belongs to `ProfileStore::levels[i]`.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Surface pressure (hPa); NaN if unknown.
    pub sp: f32,
    pub lv: Vec<LevelVal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmModel {
    BestMatch,
    Gfs,
    Icon,
    EcmwfIfs,
}

impl OmModel {
    pub const ALL: [OmModel; 4] = [
        OmModel::Gfs,
        OmModel::BestMatch,
        OmModel::Icon,
        OmModel::EcmwfIfs,
    ];
    pub fn label(self) -> &'static str {
        match self {
            OmModel::BestMatch => "best match (regional models)",
            OmModel::Gfs => "NOAA GFS 0.25° (all levels)",
            OmModel::Icon => "DWD ICON",
            OmModel::EcmwfIfs => "ECMWF IFS 0.25° (fewer levels)",
        }
    }
    pub fn param(self) -> Option<&'static str> {
        match self {
            OmModel::BestMatch => None,
            OmModel::Gfs => Some("gfs_seamless"),
            OmModel::Icon => Some("icon_seamless"),
            OmModel::EcmwfIfs => Some("ecmwf_ifs025"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    None,
    OpenMeteo(OmModel),
    File(String),
    /// Artificial test field (WINGTRACK_SYNTHETIC_WIND) — never real data.
    Synthetic,
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::None => "none".into(),
            Source::OpenMeteo(m) => format!(
                "Open-Meteo historical forecast, {}",
                m.param().unwrap_or("best_match")
            ),
            Source::File(n) => format!("ERA5 file {n}"),
            Source::Synthetic => "SYNTHETIC test field (not real data)".into(),
        }
    }
}

pub struct ProfileStore {
    /// Pressure levels (hPa), highest pressure (lowest altitude) first.
    pub levels: Vec<f32>,
    pub source: Source,
    pub cells: HashMap<CellKey, BTreeMap<i64, Profile>>,
    /// (cell, day) already downloaded or attempted (Open-Meteo only).
    pub days: HashSet<(CellKey, i64)>,
    /// Spacing (in 0.25° cells) of the regional field used for interpolation between cells.
    pub node_step: i32,
    /// Bumped whenever data changes, so the UI can invalidate cached curtains/slices.
    pub generation: u64,
}

impl Default for ProfileStore {
    fn default() -> Self {
        ProfileStore {
            levels: Vec::new(),
            source: Source::None,
            cells: HashMap::new(),
            days: HashSet::new(),
            node_step: 4,
            generation: 0,
        }
    }
}

fn lerp(a: f32, b: f32, w: f32) -> f32 {
    a + (b - a) * w
}

fn lerp_lv(a: &LevelVal, b: &LevelVal, w: f32) -> LevelVal {
    LevelVal {
        u: lerp(a.u, b.u, w),
        v: lerp(a.v, b.v, w),
        z: lerp(a.z, b.z, w),
        t: lerp(a.t, b.t, w),
    }
}

impl ProfileStore {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn hours(&self) -> usize {
        self.cells.values().map(|c| c.len()).sum()
    }

    pub fn clear(&mut self) {
        let step = self.node_step;
        *self = ProfileStore {
            node_step: step,
            generation: self.generation + 1,
            ..Default::default()
        };
    }

    /// Switch to Open-Meteo with the standard levels (clears data from another source/model).
    pub fn use_open_meteo(&mut self, model: OmModel) {
        if self.source != Source::OpenMeteo(model) {
            self.clear();
            self.source = Source::OpenMeteo(model);
            self.levels = OM_LEVELS.iter().map(|&l| l as f32).collect();
        }
    }

    /// Time-interpolated profile of one cell.
    pub fn sample_cell(&self, k: CellKey, t: i64) -> Option<Profile> {
        let series = self.cells.get(&k)?;
        let h0 = t.div_euclid(3600) * 3600;
        // Nearest available hour at or before t (files may be 3-hourly), and the next one.
        let (ta, a) = series
            .range(..=h0)
            .next_back()
            .filter(|(ta, _)| h0 - **ta <= 6 * 3600)?;
        let Some((tb, b)) = series
            .range(h0 + 1..)
            .next()
            .filter(|(tb, _)| **tb - *ta <= 6 * 3600)
        else {
            return Some(a.clone());
        };
        let w = ((t - ta) as f32 / (tb - ta) as f32).clamp(0.0, 1.0);
        Some(Profile {
            sp: if a.sp.is_finite() && b.sp.is_finite() {
                lerp(a.sp, b.sp, w)
            } else {
                a.sp
            },
            lv: a
                .lv
                .iter()
                .zip(&b.lv)
                .map(|(x, y)| lerp_lv(x, y, w))
                .collect(),
        })
    }

    /// Profile at a location: the exact cell if downloaded, otherwise bilinear interpolation
    /// between the surrounding regional-field nodes (missing corners are skipped).
    pub fn sample(&self, lat: f64, lon: f64, t: i64) -> Option<Profile> {
        if let Some(p) = self.sample_cell(cell_of(lat, lon), t) {
            return Some(p);
        }
        let s = self.node_step.max(1);
        let fi = lat / GRID / s as f64;
        let fj = lon / GRID / s as f64;
        let (i0, j0) = (fi.floor() as i32, fj.floor() as i32);
        let (wi, wj) = ((fi - i0 as f64) as f32, (fj - j0 as f64) as f32);
        let mut acc: Option<(Profile, f32)> = None;
        for (di, dj, w) in [
            (0, 0, (1.0 - wi) * (1.0 - wj)),
            (1, 0, wi * (1.0 - wj)),
            (0, 1, (1.0 - wi) * wj),
            (1, 1, wi * wj),
        ] {
            if w <= 1e-6 {
                continue;
            }
            let Some(p) = self.sample_cell(((i0 + di) * s, (j0 + dj) * s), t) else {
                continue;
            };
            match &mut acc {
                None => {
                    let mut q = p.clone();
                    for l in &mut q.lv {
                        l.u *= w;
                        l.v *= w;
                        l.z *= w;
                        l.t *= w;
                    }
                    q.sp *= w;
                    acc = Some((q, w));
                }
                Some((q, wsum)) => {
                    for (a, b) in q.lv.iter_mut().zip(&p.lv) {
                        a.u += b.u * w;
                        a.v += b.v * w;
                        a.z += b.z * w;
                        a.t += b.t * w;
                    }
                    q.sp += p.sp * w;
                    *wsum += w;
                }
            }
        }
        let (mut q, wsum) = acc?;
        for l in &mut q.lv {
            l.u /= wsum;
            l.v /= wsum;
            l.z /= wsum;
            l.t /= wsum;
        }
        q.sp /= wsum;
        Some(q)
    }

    /// Is level `i` above the ground in this profile?
    pub fn above_ground(&self, p: &Profile, i: usize) -> bool {
        p.lv[i].ok() && !(p.sp.is_finite() && self.levels[i] > p.sp + 1.0)
    }

    /// Wind (u, v) at pressure `p_hpa`, interpolated in ln p between valid levels
    /// (clamped to the lowest / highest level outside the range).
    pub fn wind_at_pressure(&self, prof: &Profile, p_hpa: f32) -> Option<(f32, f32)> {
        let lp = p_hpa.max(1.0).ln();
        let valid: Vec<usize> = (0..self.levels.len())
            .filter(|&i| prof.lv[i].ok())
            .collect();
        let first = *valid.first()?;
        let last = *valid.last()?;
        if p_hpa >= self.levels[first] {
            return Some((prof.lv[first].u, prof.lv[first].v));
        }
        if p_hpa <= self.levels[last] {
            return Some((prof.lv[last].u, prof.lv[last].v));
        }
        for w in valid.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (pa, pb) = (self.levels[a], self.levels[b]);
            if p_hpa <= pa && p_hpa >= pb {
                let f = (lp - pa.ln()) / (pb.ln() - pa.ln());
                return Some((
                    lerp(prof.lv[a].u, prof.lv[b].u, f),
                    lerp(prof.lv[a].v, prof.lv[b].v, f),
                ));
            }
        }
        None
    }

    /// Height (m ASL) of pressure `p_hpa` from the profile's geopotential heights (ln p interpolation;
    /// standard-atmosphere extrapolation outside the levels).
    pub fn height_of_pressure(&self, prof: &Profile, p_hpa: f32) -> Option<f32> {
        let valid: Vec<usize> = (0..self.levels.len())
            .filter(|&i| prof.lv[i].z.is_finite())
            .collect();
        let (&first, &last) = (valid.first()?, valid.last()?);
        let isa = |p: f32| geo::pressure_to_altitude_m(p as f64, 1013.25) as f32;
        if p_hpa >= self.levels[first] {
            return Some(prof.lv[first].z - (isa(self.levels[first]) - isa(p_hpa)));
        }
        if p_hpa <= self.levels[last] {
            return Some(prof.lv[last].z + (isa(p_hpa) - isa(self.levels[last])));
        }
        for w in valid.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (pa, pb) = (self.levels[a], self.levels[b]);
            if p_hpa <= pa && p_hpa >= pb {
                let f = (p_hpa.ln() - pa.ln()) / (pb.ln() - pa.ln());
                return Some(lerp(prof.lv[a].z, prof.lv[b].z, f));
            }
        }
        None
    }

    /// Pressure (hPa) at height `h` m ASL, inverting the geopotential-height profile.
    pub fn pressure_of_height(&self, prof: &Profile, h: f32) -> Option<f32> {
        let valid: Vec<usize> = (0..self.levels.len())
            .filter(|&i| prof.lv[i].z.is_finite())
            .collect();
        let (&first, &last) = (valid.first()?, valid.last()?);
        let inv_isa = |h: f32| (1013.25 * (1.0 - h as f64 / 44330.0).powf(5.255)) as f32;
        let isa = |p: f32| geo::pressure_to_altitude_m(p as f64, 1013.25) as f32;
        if h <= prof.lv[first].z {
            return Some(inv_isa(isa(self.levels[first]) - (prof.lv[first].z - h)));
        }
        if h >= prof.lv[last].z {
            return Some(inv_isa(isa(self.levels[last]) + (h - prof.lv[last].z)));
        }
        for w in valid.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (za, zb) = (prof.lv[a].z, prof.lv[b].z);
            if h >= za && h <= zb && zb > za {
                let f = (h - za) / (zb - za);
                return Some(
                    (self.levels[a].ln() + (self.levels[b].ln() - self.levels[a].ln()) * f).exp(),
                );
            }
        }
        None
    }

    // ------------------------------------------------------------------ downloading

    /// Requests covering `points` (lat, lon, t) at full 0.25° resolution.
    pub fn plan(&self, points: &[(f64, f64, i64)], latest_day: i64) -> Vec<ProfileRequest> {
        let Source::OpenMeteo(model) = self.source else {
            return Vec::new();
        };
        plan_cell_days(points, &self.days, latest_day, CELLS_PER_REQUEST)
            .into_iter()
            .map(|(cells, a, b)| ProfileRequest::new(cells, a, b, model, &self.levels))
            .collect()
    }

    /// Regional field nodes (every `node_step` cells) covering a lat/lon box.
    pub fn field_nodes(&self, s: f64, n: f64, w: f64, e: f64) -> Vec<CellKey> {
        let st = self.node_step.max(1);
        let k0 = cell_of(s, w);
        let k1 = cell_of(n, e);
        let mut out = Vec::new();
        let mut i = k0.0.div_euclid(st) * st;
        while i <= k1.0 + st {
            let mut j = k0.1.div_euclid(st) * st;
            while j <= k1.1 + st {
                out.push((i, j));
                j += st;
            }
            i += st;
        }
        out
    }

    /// Requests for the regional field on day `day` (and the next, for interpolation at midnight).
    pub fn plan_field_day(
        &self,
        nodes: &[CellKey],
        day: i64,
        latest_day: i64,
    ) -> Vec<ProfileRequest> {
        let pts: Vec<(f64, f64, i64)> = nodes
            .iter()
            .map(|&k| {
                let (la, lo) = cell_center(k);
                (la, lo, day * 86400 + 82800) // 23:00 → covers `day` and `day + 1`
            })
            .collect();
        self.plan(&pts, latest_day)
    }

    pub fn ingest(&mut self, req: &ProfileRequest, body: &str) -> Result<usize, String> {
        let json: serde_json::Value =
            serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
        if json.get("error").and_then(|e| e.as_bool()) == Some(true) {
            return Err(json
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("API error")
                .to_string());
        }
        let items: Vec<&serde_json::Value> = match &json {
            serde_json::Value::Array(a) => a.iter().collect(),
            o => vec![o],
        };
        if items.len() != req.cells.len() {
            return Err(format!(
                "expected {} locations, got {}",
                req.cells.len(),
                items.len()
            ));
        }
        let col = |h: &serde_json::Value, name: &str, n: usize| -> Vec<f32> {
            h.get(name)
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .map(|v| v.as_f64().map(|x| x as f32).unwrap_or(f32::NAN))
                        .collect()
                })
                .unwrap_or_else(|| vec![f32::NAN; n])
        };
        let mut n_ok = 0;
        for (k, item) in req.cells.iter().zip(items) {
            let hourly = item.get("hourly").ok_or("response without 'hourly'")?;
            let times: Vec<i64> = hourly
                .get("time")
                .and_then(|t| t.as_array())
                .ok_or("response without hourly.time")?
                .iter()
                .filter_map(|v| v.as_i64())
                .collect();
            let n = times.len();
            let sp = col(hourly, "surface_pressure", n);
            let per_level: Vec<[Vec<f32>; 4]> = self
                .levels
                .iter()
                .map(|&l| {
                    let l = l as i32;
                    [
                        col(hourly, &format!("wind_speed_{l}hPa"), n),
                        col(hourly, &format!("wind_direction_{l}hPa"), n),
                        col(hourly, &format!("geopotential_height_{l}hPa"), n),
                        col(hourly, &format!("temperature_{l}hPa"), n),
                    ]
                })
                .collect();
            let series = self.cells.entry(*k).or_default();
            for (i, &t) in times.iter().enumerate() {
                let lv: Vec<LevelVal> = per_level
                    .iter()
                    .map(|c| {
                        let (ws, wd) = (c[0][i], c[1][i]);
                        if ws.is_finite() && wd.is_finite() {
                            let (u, v) = geo::wind_uv(ws as f64, wd as f64);
                            LevelVal {
                                u: u as f32,
                                v: v as f32,
                                z: c[2][i],
                                t: c[3][i],
                            }
                        } else {
                            LevelVal {
                                z: c[2][i],
                                t: c[3][i],
                                ..LevelVal::NAN
                            }
                        }
                    })
                    .collect();
                if lv.iter().any(|l| l.ok()) {
                    series.insert(t, Profile { sp: sp[i], lv });
                    n_ok += 1;
                }
            }
        }
        self.mark_done(req);
        self.generation += 1;
        Ok(n_ok)
    }

    pub fn mark_done(&mut self, req: &ProfileRequest) {
        for k in &req.cells {
            for d in req.start_day..=req.end_day {
                self.days.insert((*k, d));
            }
        }
    }

    // ------------------------------------------------------------------ ERA5 file import

    /// Load a long-format wind cube (CSV, optionally gzip-compressed). Replaces current data.
    ///
    /// Required columns (names are matched loosely): `time`, `latitude`, `longitude`,
    /// `level` (hPa), `u`, `v`. Optional: `z` (geopotential, m²/s²) **or** `gh`/`height_m`
    /// (geopotential height, m), `t` (K or °C), `sp` (surface pressure, Pa or hPa).
    pub fn import_cube(&mut self, name: &str, bytes: &[u8]) -> Result<String, String> {
        let raw = decompress(bytes)?;
        let mut rdr = csv::ReaderBuilder::new()
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(raw.as_slice());
        let headers: Vec<String> = rdr
            .headers()
            .map_err(|e| format!("{name}: {e}"))?
            .iter()
            .map(normalize_header)
            .collect();
        let find = |aliases: &[&str]| {
            aliases
                .iter()
                .find_map(|a| headers.iter().position(|h| h == a))
        };
        let c_t =
            find(&["time", "valid_time", "timestamp", "datetime"]).ok_or("missing time column")?;
        let c_lat = find(&["latitude", "lat"]).ok_or("missing latitude column")?;
        let c_lon = find(&["longitude", "lon", "long"]).ok_or("missing longitude column")?;
        let c_lev = find(&[
            "level",
            "pressure_level",
            "level_hpa",
            "isobaricinhpa",
            "plev",
        ])
        .ok_or("missing level column")?;
        let c_u = find(&["u", "u_ms", "u_component_of_wind"]).ok_or("missing u column")?;
        let c_v = find(&["v", "v_ms", "v_component_of_wind"]).ok_or("missing v column")?;
        let c_z = find(&["z", "geopotential"]);
        let c_gh = find(&["gh", "geopotential_height", "height_m", "z_m"]);
        let c_tk = find(&["t", "temperature", "t_k", "t_c"]);
        let c_sp = find(&["sp", "surface_pressure", "sp_hpa", "sp_pa"]);

        // First pass into a temporary map: (cell, time) -> level -> values.
        let mut tmp: HashMap<(CellKey, i64), (f32, BTreeMap<i32, LevelVal>)> = HashMap::new();
        let mut levels: HashSet<i32> = HashSet::new();
        let mut rows = 0usize;
        let mut bad = 0usize;
        let mut rec = csv::StringRecord::new();
        while rdr
            .read_record(&mut rec)
            .map_err(|e| format!("{name}: {e}"))?
        {
            let get = |c: usize| rec.get(c).unwrap_or("");
            let (Some(t), Some(la), Some(lo), Some(lev), Some(u), Some(v)) = (
                parse_time(get(c_t)),
                parse_num(get(c_lat)),
                parse_num(get(c_lon)),
                parse_num(get(c_lev)),
                parse_num(get(c_u)),
                parse_num(get(c_v)),
            ) else {
                bad += 1;
                continue;
            };
            let lev = if lev > 2000.0 { lev / 100.0 } else { lev }; // Pa → hPa
            let lo = if lo > 180.0 { lo - 360.0 } else { lo };
            let z = c_gh
                .and_then(|c| parse_num(get(c)))
                .or_else(|| c_z.and_then(|c| parse_num(get(c))).map(|z| z / G0 as f64))
                .unwrap_or_else(|| geo::pressure_to_altitude_m(lev, 1013.25));
            let tc = c_tk
                .and_then(|c| parse_num(get(c)))
                .map(|t| if t > 150.0 { t - 273.15 } else { t });
            let sp = c_sp
                .and_then(|c| parse_num(get(c)))
                .map(|p| if p > 2000.0 { p / 100.0 } else { p });
            let key = (cell_of(la, lo), t);
            let e = tmp.entry(key).or_insert((f32::NAN, BTreeMap::new()));
            if let Some(sp) = sp {
                e.0 = sp as f32;
            }
            e.1.insert(
                lev.round() as i32,
                LevelVal {
                    u: u as f32,
                    v: v as f32,
                    z: z as f32,
                    t: tc.map(|x| x as f32).unwrap_or(f32::NAN),
                },
            );
            levels.insert(lev.round() as i32);
            rows += 1;
        }
        if rows == 0 {
            return Err(format!(
                "{name}: no valid rows (need time, latitude, longitude, level, u, v)"
            ));
        }
        let mut lv_sorted: Vec<i32> = levels.into_iter().collect();
        lv_sorted.sort_by(|a, b| b.cmp(a));
        self.clear();
        self.source = Source::File(name.to_string());
        self.levels = lv_sorted.iter().map(|&l| l as f32).collect();
        let mut keys = HashSet::new();
        for ((k, t), (sp, m)) in tmp {
            keys.insert(k);
            let lv = lv_sorted
                .iter()
                .map(|l| m.get(l).copied().unwrap_or(LevelVal::NAN))
                .collect();
            self.cells
                .entry(k)
                .or_default()
                .insert(t, Profile { sp, lv });
        }
        self.node_step = detect_step(&keys);
        self.generation += 1;
        Ok(format!(
            "{name}: {rows} rows, {} cells (grid step {:.2}°), {} levels {}–{} hPa{}{}",
            self.cells.len(),
            self.node_step as f64 * GRID,
            self.levels.len(),
            lv_sorted.last().unwrap_or(&0),
            lv_sorted.first().unwrap_or(&0),
            if c_z.is_none() && c_gh.is_none() {
                "; no geopotential — heights from standard atmosphere"
            } else {
                ""
            },
            if bad > 0 {
                format!("; {bad} rows skipped")
            } else {
                String::new()
            }
        ))
    }
}

fn detect_step(keys: &HashSet<CellKey>) -> i32 {
    let mut lats: Vec<i32> = keys.iter().map(|k| k.0).collect();
    lats.sort_unstable();
    lats.dedup();
    let step = lats
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 0)
        .min()
        .unwrap_or(1);
    step.max(1)
}

fn decompress(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() > 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes)
            .read_to_end(&mut out)
            .map_err(|e| format!("gzip: {e}"))?;
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}

#[derive(Clone, Debug)]
pub struct ProfileRequest {
    pub cells: Vec<CellKey>,
    pub start_day: i64,
    pub end_day: i64,
    pub url: String,
}

impl ProfileRequest {
    pub fn new(
        cells: Vec<CellKey>,
        start_day: i64,
        end_day: i64,
        model: OmModel,
        levels: &[f32],
    ) -> Self {
        let lats: Vec<String> = cells
            .iter()
            .map(|k| format!("{:.2}", cell_center(*k).0))
            .collect();
        let lons: Vec<String> = cells
            .iter()
            .map(|k| format!("{:.2}", cell_center(*k).1))
            .collect();
        let mut vars = vec!["surface_pressure".to_string()];
        for &l in levels {
            let l = l as i32;
            for v in [
                "wind_speed",
                "wind_direction",
                "geopotential_height",
                "temperature",
            ] {
                vars.push(format!("{v}_{l}hPa"));
            }
        }
        let mut url = format!(
            "{OM_API}?latitude={}&longitude={}&start_date={}&end_date={}&hourly={}&wind_speed_unit=ms&timezone=GMT&timeformat=unixtime",
            lats.join(","),
            lons.join(","),
            day_str(start_day),
            day_str(end_day),
            vars.join(",")
        );
        if let Some(m) = model.param() {
            url.push_str("&models=");
            url.push_str(m);
        }
        ProfileRequest {
            cells,
            start_day,
            end_day,
            url,
        }
    }
}

/// Historical-forecast data lag only a day or two.
pub fn latest_available_day() -> i64 {
    day_of(chrono::Utc::now().timestamp()) - 1
}

// ---------------------------------------------------------------------------
// Annotation
// ---------------------------------------------------------------------------

/// Pressure the animal flew at: the tag's own barometer, or GPS altitude converted with the profile.
pub fn flight_pressure(store: &ProfileStore, prof: &Profile, f: &Fix) -> Option<f32> {
    match (f.pressure_hpa, f.alt_source) {
        (Some(p), Some(AltitudeSource::PressureInstant | AltitudeSource::PressureWindowMin)) => {
            Some(p)
        }
        _ => {
            let h = f.alt_m?;
            store.pressure_of_height(prof, h)
        }
    }
}

/// Heading used for wind components: reported heading, else the bearing of the step to this fix
/// (only if the step is larger than the location error).
fn heading(f: &Fix) -> Option<f32> {
    f.reported_heading.or(if f.step_significant {
        f.bearing_deg
    } else {
        None
    })
}

/// Annotate every fix with flight-level and best-level wind. Returns the number of fixes annotated.
pub fn annotate(tracks: &mut [Track], store: &ProfileStore) -> usize {
    let mut n = 0;
    let src = store.source.label();
    for tr in tracks.iter_mut() {
        for f in &mut tr.fixes {
            f.weather.flight = None;
            let Some(prof) = store.sample(f.lat, f.lon, f.t) else {
                continue;
            };
            let Some(fp) = flight_pressure(store, &prof, f) else {
                continue;
            };
            // Nearest valid level (as annotate_era5's matched_pressure_level).
            let Some(mi) = (0..store.levels.len())
                .filter(|&i| prof.lv[i].ok())
                .min_by(|&a, &b| {
                    (store.levels[a] - fp)
                        .abs()
                        .total_cmp(&(store.levels[b] - fp).abs())
                })
            else {
                continue;
            };
            let m = prof.lv[mi];
            let hd = heading(f);
            let comps =
                |l: &LevelVal| hd.map(|h| geo::wind_components(l.u as f64, l.v as f64, h as f64));
            let (ws, cw) = match comps(&m) {
                Some((s, c)) => (Some(s as f32), Some(c as f32)),
                None => (None, None),
            };
            let airspeed = match (hd, f.ground_speed_ms) {
                (Some(h), Some(gs)) if f.step_significant || f.reported_speed_ms.is_some() => {
                    Some(geo::airspeed(gs as f64, h as f64, m.u as f64, m.v as f64) as f32)
                }
                _ => None,
            };
            let best = hd.and_then(|h| {
                (0..store.levels.len())
                    .filter(|&i| store.above_ground(&prof, i))
                    .map(|i| {
                        (
                            i,
                            geo::wind_components(
                                prof.lv[i].u as f64,
                                prof.lv[i].v as f64,
                                h as f64,
                            )
                            .0,
                        )
                    })
                    .max_by(|a, b| a.1.total_cmp(&b.1))
            });
            f.weather.flight = Some(FlightWind {
                flight_pressure_hpa: fp,
                flight_height_m: store.height_of_pressure(&prof, fp),
                matched_level_hpa: store.levels[mi],
                u: m.u,
                v: m.v,
                wind_speed: m.speed(),
                wind_support: ws,
                crosswind: cw,
                airspeed,
                best_level_hpa: best.map(|b| store.levels[b.0]),
                best_level_height_m: best.map(|b| prof.lv[b.0].z).filter(|z| z.is_finite()),
                best_wind_support: best.map(|b| b.1 as f32),
                at_best_wind: best.map(|b| b.0 == mi),
                source: src.clone(),
            });
            n += 1;
        }
    }
    n
}

/// Points to download at full resolution for annotation: every fix with an altitude (or all fixes
/// if `all_fixes`), plus hourly points along each step when the curtain chart needs them.
pub fn annotation_points(
    tracks: &[Track],
    along_steps: bool,
    max_step_h: f32,
) -> Vec<(f64, f64, i64)> {
    let mut pts = Vec::new();
    for tr in tracks.iter().filter(|t| t.visible) {
        for (i, f) in tr.fixes.iter().enumerate() {
            pts.push((f.lat, f.lon, f.t));
            if along_steps && i > 0 {
                let a = &tr.fixes[i - 1];
                let dt = f.t - a.t;
                if dt > 3600 && (dt as f32) <= max_step_h * 3600.0 {
                    let mut h = (a.t.div_euclid(3600) + 1) * 3600;
                    while h < f.t {
                        let w = (h - a.t) as f64 / dt as f64;
                        let (la, lo) = geo::interpolate(a.lat, a.lon, f.lat, f.lon, w);
                        pts.push((la, lo, h));
                        h += 3 * 3600; // every 3 h is plenty for a 0.25° grid
                    }
                }
            }
        }
    }
    pts
}

// ---------------------------------------------------------------------------
// Time–height curtain along a track
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurtainMetric {
    WindSupport,
    Crosswind,
    WindSpeed,
}

impl CurtainMetric {
    pub const ALL: [CurtainMetric; 3] = [
        CurtainMetric::WindSupport,
        CurtainMetric::Crosswind,
        CurtainMetric::WindSpeed,
    ];
    pub fn label(self) -> &'static str {
        match self {
            CurtainMetric::WindSupport => "wind support",
            CurtainMetric::Crosswind => "crosswind",
            CurtainMetric::WindSpeed => "wind speed",
        }
    }
}

/// Wind along a track's (interpolated) path: one column per time step, one cell per level.
pub struct Curtain {
    pub times: Vec<i64>,
    pub step_s: i64,
    /// [time][level] geopotential height (m ASL), NaN if missing.
    pub z: Vec<Vec<f32>>,
    /// [time][level] metric value, NaN if missing or below ground.
    pub val: Vec<Vec<f32>>,
    /// Height of the best (max support) level per time.
    pub best_z: Vec<f32>,
    /// Ground height estimate per time (m ASL).
    pub ground_z: Vec<f32>,
}

pub fn curtain(
    store: &ProfileStore,
    tr: &Track,
    metric: CurtainMetric,
    max_cols: usize,
) -> Option<Curtain> {
    let (t0, t1) = tr.time_range()?;
    if t1 <= t0 || store.levels.is_empty() {
        return None;
    }
    let span = t1 - t0;
    let step = ((span / max_cols.max(1) as i64).max(3600) / 3600) * 3600;
    let nl = store.levels.len();
    let mut c = Curtain {
        times: Vec::new(),
        step_s: step,
        z: Vec::new(),
        val: Vec::new(),
        best_z: Vec::new(),
        ground_z: Vec::new(),
    };
    let mut t = (t0 / 3600) * 3600 + step / 2;
    while t <= t1 {
        let Some((la, lo, idx)) = tr.position_at(t as f64) else {
            t += step;
            continue;
        };
        // Direction of travel in the step that contains t.
        let hd = tr.fixes.get(idx + 1).map(|b| {
            let a = &tr.fixes[idx];
            geo::bearing_deg(a.lat, a.lon, b.lat, b.lon) as f32
        });
        let mut zc = vec![f32::NAN; nl];
        let mut vc = vec![f32::NAN; nl];
        let (mut best, mut ground) = (f32::NAN, f32::NAN);
        if let Some(p) = store.sample(la, lo, t) {
            let mut best_v = f32::NEG_INFINITY;
            for i in 0..nl {
                zc[i] = p.lv[i].z;
                if !store.above_ground(&p, i) {
                    continue;
                }
                let l = p.lv[i];
                let comps = hd.map(|h| geo::wind_components(l.u as f64, l.v as f64, h as f64));
                vc[i] = match metric {
                    CurtainMetric::WindSpeed => l.speed(),
                    CurtainMetric::WindSupport => comps.map(|c| c.0 as f32).unwrap_or(f32::NAN),
                    CurtainMetric::Crosswind => comps.map(|c| c.1 as f32).unwrap_or(f32::NAN),
                };
                if let Some((s, _)) = comps
                    && (s as f32) > best_v
                {
                    best_v = s as f32;
                    best = l.z;
                }
            }
            if p.sp.is_finite() {
                ground = store.height_of_pressure(&p, p.sp).unwrap_or(f32::NAN);
            }
        }
        c.times.push(t);
        c.z.push(zc);
        c.val.push(vc);
        c.best_z.push(best);
        c.ground_z.push(ground);
        t += step;
    }
    if c.times.is_empty() { None } else { Some(c) }
}

// ---------------------------------------------------------------------------
// Regional slice at one time (for the 3D view)
// ---------------------------------------------------------------------------

/// Gridded wind at one time over a lat/lon box, for chosen levels. Cheap to sample per frame.
pub struct FieldSlice {
    pub t: i64,
    pub lat0: f64,
    pub lon0: f64,
    pub step_deg: f64,
    pub nlat: usize,
    pub nlon: usize,
    /// Level indices into `ProfileStore::levels`.
    pub levels: Vec<usize>,
    /// [level][i * nlon + j] → (u, v), NaN where missing.
    pub uv: Vec<Vec<(f32, f32)>>,
    /// Mean geopotential height per slice level (m).
    pub mean_z: Vec<f32>,
}

impl FieldSlice {
    pub fn build(
        store: &ProfileStore,
        levels: &[usize],
        bbox: (f64, f64, f64, f64),
        t: i64,
    ) -> Option<FieldSlice> {
        let (s, n, w, e) = bbox;
        let step_deg = store.node_step.max(1) as f64 * GRID;
        let lat0 = (s / step_deg).floor() * step_deg;
        let lon0 = (w / step_deg).floor() * step_deg;
        let nlat = (((n - lat0) / step_deg).ceil() as usize + 1).min(400);
        let nlon = (((e - lon0) / step_deg).ceil() as usize + 1).min(400);
        let mut uv = vec![vec![(f32::NAN, f32::NAN); nlat * nlon]; levels.len()];
        let mut zsum = vec![0.0f64; levels.len()];
        let mut zn = vec![0usize; levels.len()];
        let mut any = false;
        for i in 0..nlat {
            for j in 0..nlon {
                let (la, lo) = (lat0 + i as f64 * step_deg, lon0 + j as f64 * step_deg);
                let Some(p) = store.sample_cell(cell_of(la, lo), t) else {
                    continue;
                };
                for (k, &li) in levels.iter().enumerate() {
                    if li < p.lv.len() && store.above_ground(&p, li) {
                        uv[k][i * nlon + j] = (p.lv[li].u, p.lv[li].v);
                        any = true;
                    }
                    if li < p.lv.len() && p.lv[li].z.is_finite() {
                        zsum[k] += p.lv[li].z as f64;
                        zn[k] += 1;
                    }
                }
            }
        }
        if !any {
            return None;
        }
        let mean_z = levels
            .iter()
            .enumerate()
            .map(|(k, &li)| {
                if zn[k] > 0 {
                    (zsum[k] / zn[k] as f64) as f32
                } else {
                    geo::pressure_to_altitude_m(store.levels[li] as f64, 1013.25) as f32
                }
            })
            .collect();
        Some(FieldSlice {
            t,
            lat0,
            lon0,
            step_deg,
            nlat,
            nlon,
            levels: levels.to_vec(),
            uv,
            mean_z,
        })
    }

    /// Bilinear wind at (lat, lon) on slice level `k`.
    pub fn wind(&self, k: usize, lat: f64, lon: f64) -> Option<(f32, f32)> {
        let fi = (lat - self.lat0) / self.step_deg;
        let fj = (lon - self.lon0) / self.step_deg;
        if fi < 0.0 || fj < 0.0 {
            return None;
        }
        let (i0, j0) = (fi.floor() as usize, fj.floor() as usize);
        if i0 + 1 >= self.nlat || j0 + 1 >= self.nlon {
            return None;
        }
        let (wi, wj) = ((fi - i0 as f64) as f32, (fj - j0 as f64) as f32);
        let g = &self.uv[k];
        let (mut u, mut v, mut ws) = (0.0, 0.0, 0.0);
        for (di, dj, w) in [
            (0, 0, (1.0 - wi) * (1.0 - wj)),
            (1, 0, wi * (1.0 - wj)),
            (0, 1, (1.0 - wi) * wj),
            (1, 1, wi * wj),
        ] {
            let (a, b) = g[(i0 + di) * self.nlon + j0 + dj];
            if a.is_finite() && w > 0.0 {
                u += a * w;
                v += b * w;
                ws += w;
            }
        }
        if ws < 0.3 {
            None
        } else {
            Some((u / ws, v / ws))
        }
    }

    pub fn bbox(&self) -> (f64, f64, f64, f64) {
        (
            self.lat0,
            self.lat0 + (self.nlat - 1) as f64 * self.step_deg,
            self.lon0,
            self.lon0 + (self.nlon - 1) as f64 * self.step_deg,
        )
    }
}

/// **Testing aid only** (env `WINGTRACK_SYNTHETIC_WIND=1`): an artificial sheared wind field
/// over the tracks — winds veer and strengthen with height. Not real data.
pub fn synthetic_profiles(store: &mut ProfileStore, tracks: &[Track]) {
    let fixes: Vec<&Fix> = tracks.iter().flat_map(|t| t.fixes.iter()).collect();
    if fixes.is_empty() {
        return;
    }
    let (mut s, mut n, mut w, mut e, mut t0, mut t1) =
        (90.0f64, -90.0f64, 180.0f64, -180.0f64, i64::MAX, i64::MIN);
    for f in &fixes {
        s = s.min(f.lat);
        n = n.max(f.lat);
        w = w.min(f.lon);
        e = e.max(f.lon);
        t0 = t0.min(f.t);
        t1 = t1.max(f.t);
    }
    store.clear();
    store.source = Source::Synthetic;
    store.levels = OM_LEVELS.iter().map(|&l| l as f32).collect();
    store.node_step = 2;
    let nodes = store.field_nodes(s - 1.5, n + 1.5, w - 1.5, e + 1.5);
    let mut extra: Vec<CellKey> = fixes.iter().map(|f| cell_of(f.lat, f.lon)).collect();
    extra.sort_unstable();
    extra.dedup();
    for k in nodes.into_iter().chain(extra) {
        let (la, lo) = cell_center(k);
        let series = store.cells.entry(k).or_default();
        let mut h = t0.div_euclid(3600) * 3600 - 3600;
        while h <= t1 + 7200 {
            let ph = (h - t0) as f32 / 86400.0;
            let lv = store
                .levels
                .iter()
                .map(|&p| {
                    let z = geo::pressure_to_altitude_m(p as f64, 1016.0) as f32;
                    let k = z / 1000.0;
                    let dir = (20.0
                        + 70.0 * (ph * 0.6 + la as f32 * 0.4).sin()
                        + 25.0 * k
                        + (lo as f32 - 8.0) * 6.0)
                        .rem_euclid(360.0);
                    let spd = (4.0 + 3.5 * k + 3.0 * (ph * 1.1 + lo as f32 * 0.3).sin()).max(0.5);
                    let (u, v) = geo::wind_uv(spd as f64, dir as f64);
                    LevelVal {
                        u: u as f32,
                        v: v as f32,
                        z,
                        t: 15.0 - 6.5 * k,
                    }
                })
                .collect();
            series.insert(
                h,
                Profile {
                    sp: 1005.0 - (la as f32 - 46.0).max(0.0) * 4.0,
                    lv,
                },
            );
            h += 3600;
        }
    }
    store.generation += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fix, LocationSource};

    fn om_body(
        n_loc: usize,
        t0: i64,
        hours: usize,
        speed_of_level: impl Fn(u16) -> f32,
        dir: f32,
        sp: f32,
    ) -> String {
        let one = || {
            let times: Vec<String> = (0..hours)
                .map(|h| (t0 + h as i64 * 3600).to_string())
                .collect();
            let rep = |x: f32| {
                format!(
                    "[{}]",
                    (0..hours)
                        .map(|_| format!("{x}"))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };
            let mut fields = vec![
                format!("\"time\":[{}]", times.join(",")),
                format!("\"surface_pressure\":{}", rep(sp)),
            ];
            for l in OM_LEVELS {
                let z = geo::pressure_to_altitude_m(l as f64, 1013.25) as f32;
                fields.push(format!("\"wind_speed_{l}hPa\":{}", rep(speed_of_level(l))));
                fields.push(format!("\"wind_direction_{l}hPa\":{}", rep(dir)));
                fields.push(format!("\"geopotential_height_{l}hPa\":{}", rep(z)));
                fields.push(format!("\"temperature_{l}hPa\":{}", rep(10.0)));
            }
            format!(
                "{{\"latitude\":0,\"longitude\":0,\"hourly\":{{{}}}}}",
                fields.join(",")
            )
        };
        if n_loc == 1 {
            one()
        } else {
            format!(
                "[{}]",
                (0..n_loc).map(|_| one()).collect::<Vec<_>>().join(",")
            )
        }
    }

    #[test]
    fn request_url_and_planning() {
        let mut st = ProfileStore::default();
        st.use_open_meteo(OmModel::Gfs);
        let t = 1_756_684_800;
        let reqs = st.plan(&[(47.0, 9.0, t), (46.0, 8.0, t + 86400)], day_of(t) + 100);
        assert_eq!(reqs.len(), 1);
        let u = &reqs[0].url;
        assert!(u.starts_with(OM_API));
        assert!(u.contains("wind_speed_975hPa") && u.contains("geopotential_height_700hPa"));
        assert!(u.contains("models=gfs_seamless") && u.contains("surface_pressure"));
        let nodes = st.field_nodes(46.0, 48.0, 8.0, 10.0);
        assert!(nodes.iter().all(|k| k.0 % 4 == 0 && k.1 % 4 == 0));
        assert!(
            !st.plan_field_day(&nodes, day_of(t), day_of(t) + 100)
                .is_empty()
        );
    }

    #[test]
    fn ingest_interpolate_and_annotate() {
        let t0 = 1_756_684_800;
        let mut st = ProfileStore::default();
        st.use_open_meteo(OmModel::Gfs);
        let a = (47.0, 9.0);
        let b = (46.7, 8.6); // south-west
        let req = ProfileRequest::new(
            vec![cell_of(a.0, a.1), cell_of(b.0, b.1)],
            day_of(t0),
            day_of(t0) + 1,
            OmModel::Gfs,
            &st.levels,
        );
        // Wind from NE (tailwind for SW travel), strengthening with height; ground at 960 hPa.
        let body = om_body(
            2,
            t0,
            48,
            |l| (1000 - l as i32) as f32 / 30.0 + 2.0,
            45.0,
            960.0,
        );
        assert_eq!(st.ingest(&req, &body).unwrap(), 96);

        let p = st.sample(a.0, a.1, t0 + 1800).unwrap();
        assert!(!st.above_ground(&p, 0), "1000 hPa is below 960 hPa ground");
        assert!(st.above_ground(&p, 2));
        // ln-p interpolation between 925 (4.5 m/s) and 900 (5.33 m/s)
        let (u, v) = st.wind_at_pressure(&p, 912.0).unwrap();
        let s = (u * u + v * v).sqrt();
        assert!(s > 4.5 && s < 5.34, "{s}");
        let h = st.height_of_pressure(&p, 900.0).unwrap();
        assert!((h - 988.0).abs() < 5.0, "{h}");
        let back = st.pressure_of_height(&p, h).unwrap();
        assert!((back - 900.0).abs() < 0.5, "{back}");

        let mut tr = Track::new("x", "NanoFox", "t");
        let mut f1 = Fix::new(t0 + 3600 * 20, a.0, a.1, LocationSource::SigfoxAtlas);
        f1.error_m = Some(500.0);
        let mut f2 = Fix::new(t0 + 3600 * 23, b.0, b.1, LocationSource::SigfoxAtlas);
        f2.error_m = Some(500.0);
        f2.pressure_hpa = Some(930.0);
        f2.alt_source = Some(AltitudeSource::PressureInstant);
        f2.alt_m = Some(700.0);
        tr.fixes = vec![f1, f2];
        tr.finalize();
        let mut tracks = vec![tr];
        assert_eq!(annotate(&mut tracks, &st), 1, "first fix has no altitude");
        let fw = tracks[0].fixes[1].weather.flight.clone().unwrap();
        assert_eq!(fw.matched_level_hpa, 925.0);
        assert!(fw.wind_support.unwrap() > 4.0);
        assert_eq!(fw.best_level_hpa, Some(700.0), "strongest tailwind aloft");
        assert_eq!(fw.at_best_wind, Some(false));

        let c = curtain(&st, &tracks[0], CurtainMetric::WindSupport, 100).unwrap();
        assert!(!c.times.is_empty());
        assert!(c.val[0][0].is_nan(), "underground level masked");
        assert!(c.val[0][7] > c.val[0][3]);

        let slice = FieldSlice::build(&st, &[2, 7], (46.5, 47.2, 8.4, 9.2), t0 + 3600).unwrap();
        assert_eq!(slice.levels, vec![2, 7]);
        assert!(slice.mean_z[1] > slice.mean_z[0]);
    }

    #[test]
    fn cube_import_csv_and_gzip() {
        let mut csv = String::from("time,latitude,longitude,level,u,v,z,t,sp\n");
        for (lev, z) in [(1000, 1100.0), (950, 5000.0), (500, 55000.0)] {
            for la in [47.0, 47.25] {
                csv.push_str(&format!(
                    "2025-09-01 00:00:00,{la},9.0,{lev},3.0,-4.0,{z},280.0,97000\n"
                ));
                csv.push_str(&format!(
                    "2025-09-01 01:00:00,{la},9.0,{lev},5.0,-4.0,{z},280.0,97000\n"
                ));
            }
        }
        let mut st = ProfileStore::default();
        let msg = st.import_cube("cube.csv", csv.as_bytes()).unwrap();
        assert!(msg.contains("3 levels"), "{msg}");
        assert_eq!(st.levels, vec![1000.0, 950.0, 500.0]);
        let p = st.sample(47.0, 9.0, 1_756_686_600).unwrap(); // 00:30
        assert!((p.lv[0].u - 4.0).abs() < 1e-4, "time interpolation");
        assert!(
            (p.lv[2].z - 55000.0 / G0).abs() < 1.0,
            "geopotential → metres"
        );
        assert!((p.sp - 970.0).abs() < 1e-3 && (p.lv[0].t - 6.85).abs() < 0.01);
        assert!(!st.above_ground(&p, 0));

        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(csv.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        let mut st2 = ProfileStore::default();
        st2.import_cube("cube.csv.gz", &gz).unwrap();
        assert_eq!(st2.hours(), st.hours());
    }

    /// `WINGTRACK_CUBE=path/to/cube.csv.gz cargo test --lib -- --ignored cube_file`
    #[test]
    #[ignore]
    fn cube_file_from_env() {
        let Ok(path) = std::env::var("WINGTRACK_CUBE") else {
            return;
        };
        let bytes = std::fs::read(&path).unwrap();
        let mut st = ProfileStore::default();
        let msg = st.import_cube(&path, &bytes).unwrap();
        println!("{msg}");
        assert!(st.hours() > 0);
        let p = st
            .sample(47.0, 9.0, 1_756_872_000)
            .expect("profile at 47N 9E");
        println!(
            "levels {:?}\nsp {} z {:?}",
            st.levels,
            p.sp,
            p.lv.iter().map(|l| l.z).collect::<Vec<_>>()
        );
    }
}
