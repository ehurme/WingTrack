//! ERA5 reanalysis download (via the Open-Meteo Historical Weather API) and wind annotation.
//!
//! Fixes (and, optionally, hourly points interpolated along each step) are binned into
//! 0.25° ERA5 grid cells. Cells are requested month by month, many locations per request,
//! and cached so panning the time slider or re-annotating never re-downloads.
//!
//! Derived per fix: wind u/v at 10 m or 100 m (chosen from height above ground),
//! wind support / crosswind relative to the step heading, airspeed, 2 m temperature,
//! MSL & surface pressure, precipitation, cloud cover, an ERA5‑MSLP‑corrected barometric
//! altitude, and mean along-step (optionally night-only) wind support — the quantity that
//! matters for Sigfox tags whose fixes are hours apart and taken at the roost.

use crate::geo;
use crate::model::{AltitudeSource, Track};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const API: &str = "https://archive-api.open-meteo.com/v1/archive";

/// Hourly variables requested (order = index into a cell's value array).
pub const VARS: [&str; 9] = [
    "wind_speed_10m",
    "wind_direction_10m",
    "wind_speed_100m",
    "wind_direction_100m",
    "temperature_2m",
    "pressure_msl",
    "surface_pressure",
    "precipitation",
    "cloud_cover",
];
pub const NV: usize = VARS.len();
const WS10: usize = 0;
const WD10: usize = 1;
const WS100: usize = 2;
const WD100: usize = 3;
const T2M: usize = 4;
const MSL: usize = 5;
const SP: usize = 6;
const PRECIP: usize = 7;
const CLOUD: usize = 8;

/// Locations per request (keeps URLs short and responses small).
const CELLS_PER_REQUEST: usize = 25;

pub type CellKey = (i32, i32);
pub type Values = [f32; NV];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Era5,
    BestMatch,
}

impl Model {
    pub fn label(self) -> &'static str {
        match self {
            Model::Era5 => "ERA5 (0.25°)",
            Model::BestMatch => "best match (ERA5 + ERA5-Land/ERA5T)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindLevel {
    /// 10 m below ~55 m above ground, otherwise 100 m (unknown altitude → 100 m).
    Auto,
    M10,
    M100,
}

impl WindLevel {
    pub fn label(self) -> &'static str {
        match self {
            WindLevel::Auto => "auto (by height above ground)",
            WindLevel::M10 => "10 m",
            WindLevel::M100 => "100 m",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AnnotateOptions {
    pub level: WindLevel,
    pub model: Model,
    /// Also sample hourly along each step (needed for Sigfox / sparse fixes).
    pub segments: bool,
    /// Only average segment hours when the sun is > 6° below the horizon (bats, nocturnal migrants).
    pub night_only: bool,
    /// Ignore steps longer than this when sampling segments.
    pub max_segment_h: f32,
}

impl Default for AnnotateOptions {
    fn default() -> Self {
        AnnotateOptions {
            level: WindLevel::Auto,
            model: Model::Era5,
            segments: true,
            night_only: true,
            max_segment_h: 72.0,
        }
    }
}

#[derive(Default)]
pub struct WeatherStore {
    /// cell → hour (unix s, multiple of 3600) → values (NaN = missing)
    pub cells: HashMap<CellKey, BTreeMap<i64, Values>>,
    /// (cell, day index) already downloaded (or attempted).
    pub days: HashSet<(CellKey, i64)>,
}

pub const GRID: f64 = 0.25;

pub fn cell_of(lat: f64, lon: f64) -> CellKey {
    ((lat / GRID).round() as i32, (lon / GRID).round() as i32)
}

pub fn cell_center(k: CellKey) -> (f64, f64) {
    (k.0 as f64 * GRID, k.1 as f64 * GRID)
}

fn day_of(t: i64) -> i64 {
    t.div_euclid(86400)
}

fn day_str(d: i64) -> String {
    DateTime::<Utc>::from_timestamp(d * 86400, 0)
        .map(|x| x.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

impl WeatherStore {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn hours(&self) -> usize {
        self.cells.values().map(|c| c.len()).sum()
    }

    /// Values at a location and time, linearly interpolated between the bracketing hours.
    pub fn sample(&self, lat: f64, lon: f64, t: i64) -> Option<Values> {
        self.sample_cell(cell_of(lat, lon), t)
    }

    pub fn sample_cell(&self, k: CellKey, t: i64) -> Option<Values> {
        let series = self.cells.get(&k)?;
        let h0 = t.div_euclid(3600) * 3600;
        let a = series.get(&h0)?;
        let Some(b) = series.get(&(h0 + 3600)) else {
            return Some(*a);
        };
        let w = (t - h0) as f32 / 3600.0;
        let mut out = [f32::NAN; NV];
        for i in 0..NV {
            out[i] = if i == WD10 || i == WD100 {
                interp_angle(a[i], b[i], w)
            } else {
                a[i] + (b[i] - a[i]) * w
            };
        }
        Some(out)
    }

    /// Plan the requests needed to cover `points` (lat, lon, t), skipping cached cell-days.
    pub fn plan(&self, points: &[(f64, f64, i64)], model: Model, latest_day: i64) -> Vec<Request> {
        // Needed (cell, day), including the next day for interpolation across midnight.
        let mut need: HashSet<(CellKey, i64)> = HashSet::new();
        for &(lat, lon, t) in points {
            let k = cell_of(lat, lon);
            for d in [day_of(t), day_of(t + 3600)] {
                if d <= latest_day && !self.days.contains(&(k, d)) {
                    need.insert((k, d));
                }
            }
        }
        // Group by calendar month.
        let mut by_month: BTreeMap<(i32, u32), BTreeMap<CellKey, (i64, i64)>> = BTreeMap::new();
        for (k, d) in need {
            let date = DateTime::<Utc>::from_timestamp(d * 86400, 0)
                .expect("valid day")
                .date_naive();
            let e = by_month
                .entry((date.year(), date.month()))
                .or_default()
                .entry(k)
                .or_insert((d, d));
            e.0 = e.0.min(d);
            e.1 = e.1.max(d);
        }
        let mut reqs = Vec::new();
        for (_, cells) in by_month {
            let cells: Vec<_> = cells.into_iter().collect();
            for chunk in cells.chunks(CELLS_PER_REQUEST) {
                let start = chunk.iter().map(|c| c.1.0).min().expect("non-empty chunk");
                let end = chunk.iter().map(|c| c.1.1).max().expect("non-empty chunk");
                let keys: Vec<CellKey> = chunk.iter().map(|c| c.0).collect();
                reqs.push(Request::new(keys, start, end, model));
            }
        }
        reqs
    }

    /// Store a JSON response for `req`. Returns the number of hours ingested.
    pub fn ingest(&mut self, req: &Request, body: &str) -> Result<usize, String> {
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
        let mut n = 0;
        for (k, item) in req.cells.iter().zip(items) {
            let hourly = item.get("hourly").ok_or("response without 'hourly'")?;
            let times: Vec<i64> = hourly
                .get("time")
                .and_then(|t| t.as_array())
                .ok_or("response without hourly.time")?
                .iter()
                .filter_map(|v| v.as_i64())
                .collect();
            let cols: Vec<Vec<f32>> = VARS
                .iter()
                .map(|name| {
                    hourly
                        .get(*name)
                        .and_then(|a| a.as_array())
                        .map(|a| {
                            a.iter()
                                .map(|v| v.as_f64().map(|x| x as f32).unwrap_or(f32::NAN))
                                .collect()
                        })
                        .unwrap_or_else(|| vec![f32::NAN; times.len()])
                })
                .collect();
            let series = self.cells.entry(*k).or_default();
            for (i, &t) in times.iter().enumerate() {
                let mut v = [f32::NAN; NV];
                for j in 0..NV {
                    v[j] = cols[j].get(i).copied().unwrap_or(f32::NAN);
                }
                if v[WS10].is_nan() && v[WS100].is_nan() {
                    continue;
                }
                series.insert(t, v);
                n += 1;
            }
        }
        self.mark_done(req);
        Ok(n)
    }

    /// Mark a request's cell-days as attempted (also on failure, to avoid hammering the API).
    pub fn mark_done(&mut self, req: &Request) {
        for k in &req.cells {
            for d in req.start_day..=req.end_day {
                self.days.insert((*k, d));
            }
        }
    }
}

fn interp_angle(a: f32, b: f32, w: f32) -> f32 {
    let d = ((b - a + 540.0) % 360.0) - 180.0;
    (a + d * w).rem_euclid(360.0)
}

#[derive(Clone, Debug)]
pub struct Request {
    pub cells: Vec<CellKey>,
    pub start_day: i64,
    pub end_day: i64,
    pub url: String,
}

impl Request {
    pub fn new(cells: Vec<CellKey>, start_day: i64, end_day: i64, model: Model) -> Self {
        let lats: Vec<String> = cells
            .iter()
            .map(|k| format!("{:.2}", cell_center(*k).0))
            .collect();
        let lons: Vec<String> = cells
            .iter()
            .map(|k| format!("{:.2}", cell_center(*k).1))
            .collect();
        let mut url = format!(
            "{API}?latitude={}&longitude={}&start_date={}&end_date={}&hourly={}&wind_speed_unit=ms&timezone=GMT&timeformat=unixtime",
            lats.join(","),
            lons.join(","),
            day_str(start_day),
            day_str(end_day),
            VARS.join(",")
        );
        if model == Model::Era5 {
            url.push_str("&models=era5");
        }
        Request {
            cells,
            start_day,
            end_day,
            url,
        }
    }
}

/// Latest day ERA5 is expected to be available (≈ 6 days behind real time).
pub fn latest_available_day() -> i64 {
    day_of(Utc::now().timestamp()) - 6
}

/// Points to download: every fix, plus hourly points along each step if `opts.segments`.
pub fn sample_points(tracks: &[Track], opts: &AnnotateOptions) -> Vec<(f64, f64, i64)> {
    let mut pts = Vec::new();
    for tr in tracks {
        for (i, f) in tr.fixes.iter().enumerate() {
            pts.push((f.lat, f.lon, f.t));
            if opts.segments && i > 0 {
                for (lat, lon, t) in segment_hours(&tr.fixes[i - 1], f, opts) {
                    pts.push((lat, lon, t));
                }
            }
        }
    }
    pts
}

fn segment_hours(
    a: &crate::model::Fix,
    b: &crate::model::Fix,
    opts: &AnnotateOptions,
) -> Vec<(f64, f64, i64)> {
    let dt = b.t - a.t;
    if dt <= 3600 || dt as f32 > opts.max_segment_h * 3600.0 || !b.step_significant {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut h = (a.t.div_euclid(3600) + 1) * 3600;
    while h < b.t {
        let w = (h - a.t) as f64 / dt as f64;
        let (lat, lon) = geo::interpolate(a.lat, a.lon, b.lat, b.lon, w);
        if !opts.night_only || geo::is_night(h, lat, lon) {
            out.push((lat, lon, h));
        }
        h += 3600;
    }
    out
}

fn uv_at(v: &Values, use100: bool) -> Option<(f64, f64)> {
    let (ws, wd) = if use100 {
        (v[WS100], v[WD100])
    } else {
        (v[WS10], v[WD10])
    };
    if ws.is_nan() || wd.is_nan() {
        // fall back to the other level
        let (ws, wd) = if use100 {
            (v[WS10], v[WD10])
        } else {
            (v[WS100], v[WD100])
        };
        if ws.is_nan() || wd.is_nan() {
            return None;
        }
        return Some(geo::wind_uv(ws as f64, wd as f64));
    }
    Some(geo::wind_uv(ws as f64, wd as f64))
}

fn opt(v: f32) -> Option<f32> {
    if v.is_nan() { None } else { Some(v) }
}

/// Annotate every fix in `tracks` from the cached ERA5 data. Fixes with no ERA5 data keep
/// whatever (possibly imported) annotation they already have.
pub fn annotate(tracks: &mut [Track], store: &WeatherStore, opts: &AnnotateOptions) -> usize {
    let mut n = 0;
    for tr in tracks.iter_mut() {
        let len = tr.fixes.len();
        for i in 0..len {
            let (prev, rest) = tr.fixes.split_at_mut(i);
            let f = &mut rest[0];
            let Some(v) = store.sample(f.lat, f.lon, f.t) else {
                continue;
            };
            n += 1;
            let w = &mut f.weather;
            w.temp_2m = opt(v[T2M]);
            w.msl_hpa = opt(v[MSL]);
            w.surface_pressure_hpa = opt(v[SP]);
            w.precipitation_mm = opt(v[PRECIP]);
            w.cloud_cover = opt(v[CLOUD]);

            // ERA5-corrected barometric altitude.
            w.alt_msl_corrected = match (f.pressure_hpa, w.msl_hpa) {
                (Some(p), Some(msl))
                    if matches!(
                        f.alt_source,
                        Some(AltitudeSource::PressureInstant | AltitudeSource::PressureWindowMin)
                    ) =>
                {
                    Some(geo::pressure_to_altitude_m(p as f64, msl as f64) as f32)
                }
                _ => None,
            };

            // Height above ground → wind level.
            let agl: Option<f64> =
                match (f.pressure_hpa, w.surface_pressure_hpa, f.alt_m, w.msl_hpa) {
                    (Some(p), Some(sp), _, _) => {
                        Some(geo::pressure_to_altitude_m(p as f64, sp as f64))
                    }
                    (None, Some(sp), Some(alt), Some(msl)) => {
                        Some(alt as f64 - geo::pressure_to_altitude_m(sp as f64, msl as f64))
                    }
                    _ => None,
                };
            let use100 = match opts.level {
                WindLevel::M10 => false,
                WindLevel::M100 => true,
                WindLevel::Auto => agl.is_none_or(|h| h >= 55.0),
            };
            let Some((u, vv)) = uv_at(&v, use100) else {
                continue;
            };
            w.u = Some(u as f32);
            w.v = Some(vv as f32);
            w.level = Some(if use100 { "100m" } else { "10m" });
            w.wind_speed = Some((u * u + vv * vv).sqrt() as f32);
            w.wind_from_deg = Some(((u.atan2(vv).to_degrees() + 180.0).rem_euclid(360.0)) as f32);

            w.wind_support = None;
            w.crosswind = None;
            w.airspeed = None;
            if let (Some(hd), true) = (
                f.bearing_deg,
                f.step_significant || f.reported_heading.is_some(),
            ) {
                let (s, c) = geo::wind_components(u, vv, hd as f64);
                w.wind_support = Some(s as f32);
                w.crosswind = Some(c as f32);
                if let Some(gs) = f.ground_speed_ms {
                    w.airspeed = Some(geo::airspeed(gs as f64, hd as f64, u, vv) as f32);
                }
            }

            // Along-step mean wind (segment that ended at this fix).
            w.seg_wind_support = None;
            w.seg_crosswind = None;
            w.seg_hours = None;
            if opts.segments && i > 0 {
                let a = &prev[i - 1];
                let hd = geo::bearing_deg(a.lat, a.lon, f.lat, f.lon);
                let (mut ss, mut cs, mut k) = (0.0, 0.0, 0u16);
                for (lat, lon, t) in segment_hours(a, f, opts) {
                    if let Some(sv) = store.sample(lat, lon, t) {
                        let seg100 = !matches!(opts.level, WindLevel::M10);
                        if let Some((su, svv)) = uv_at(&sv, seg100) {
                            let (s, c) = geo::wind_components(su, svv, hd);
                            ss += s;
                            cs += c;
                            k += 1;
                        }
                    }
                }
                let w = &mut f.weather;
                if k > 0 {
                    w.seg_wind_support = Some((ss / k as f64) as f32);
                    w.seg_crosswind = Some((cs / k as f64) as f32);
                    w.seg_hours = Some(k);
                }
            }
        }
    }
    n
}

/// **Testing aid only** (env `WINGTRACK_SYNTHETIC_WIND=1`): fill the store with an artificial,
/// slowly rotating wind field over the tracks so the UI can be exercised offline. Not ERA5.
pub fn synthetic_field(store: &mut WeatherStore, tracks: &[Track]) {
    let fixes = tracks.iter().flat_map(|t| t.fixes.iter());
    let (mut a, mut b, mut c, mut d, mut t0, mut t1) =
        (90.0f64, -90.0f64, 180.0f64, -180.0f64, i64::MAX, i64::MIN);
    for f in fixes {
        a = a.min(f.lat);
        b = b.max(f.lat);
        c = c.min(f.lon);
        d = d.max(f.lon);
        t0 = t0.min(f.t);
        t1 = t1.max(f.t);
    }
    if a > b {
        return;
    }
    let (k0, k1) = (cell_of(a - 0.5, c - 0.5), cell_of(b + 0.5, d + 0.5));
    for i in k0.0..=k1.0 {
        for j in k0.1..=k1.1 {
            let series = store.cells.entry((i, j)).or_default();
            let mut h = t0.div_euclid(3600) * 3600 - 3600;
            while h <= t1 + 7200 {
                let ph = (h - t0) as f32 / 86400.0;
                let dir = (30.0 + 90.0 * (ph * 0.7 + i as f32 * 0.05).sin() + j as f32 * 1.5)
                    .rem_euclid(360.0);
                let ws = 6.0 + 4.0 * (ph * 1.3 + j as f32 * 0.08).sin();
                series.insert(h, [ws * 0.6, dir, ws, dir, 15.0, 1015.0, 965.0, 0.0, 40.0]);
                h += 3600;
            }
        }
    }
}

/// Validate a YYYY-MM-DD date (used by tests/UI).
pub fn parse_day(s: &str) -> Option<i64> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| {
        d.and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
            .timestamp()
            / 86400
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fix, LocationSource};

    fn fake_response(n_loc: usize, t0: i64, hours: usize, ws: f32, wd: f32) -> String {
        let one = |_: usize| {
            let times: Vec<String> = (0..hours)
                .map(|h| (t0 + h as i64 * 3600).to_string())
                .collect();
            let rep = |x: f32| {
                (0..hours)
                    .map(|_| format!("{x}"))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            format!(
                r#"{{"latitude":47.0,"longitude":9.0,"hourly":{{"time":[{}],"wind_speed_10m":[{}],"wind_direction_10m":[{}],"wind_speed_100m":[{}],"wind_direction_100m":[{}],"temperature_2m":[{}],"pressure_msl":[{}],"surface_pressure":[{}],"precipitation":[{}],"cloud_cover":[{}]}}}}"#,
                times.join(","),
                rep(ws * 0.6),
                rep(wd),
                rep(ws),
                rep(wd),
                rep(15.0),
                rep(1020.0),
                rep(960.0),
                rep(0.0),
                rep(50.0)
            )
        };
        if n_loc == 1 {
            one(0)
        } else {
            format!("[{}]", (0..n_loc).map(one).collect::<Vec<_>>().join(","))
        }
    }

    #[test]
    fn plan_groups_by_month_and_skips_cached() {
        let store = WeatherStore::default();
        let t = 1_756_684_800; // 2025-09-01
        let pts = vec![
            (47.0, 9.0, t),
            (47.0, 9.0, t + 86400 * 3),
            (46.0, 8.0, t + 86400 * 40),
        ];
        let reqs = store.plan(&pts, Model::Era5, day_of(t) + 1000);
        assert_eq!(reqs.len(), 2, "September and October");
        assert!(reqs[0].url.contains("start_date=2025-09-01"));
        assert!(reqs[0].url.contains("end_date=2025-09-04"));
        assert!(reqs[0].url.contains("models=era5"));
        assert!(reqs[0].url.contains("wind_speed_100m"));
        let mut store = store;
        store.mark_done(&reqs[0]);
        store.mark_done(&reqs[1]);
        assert!(store.plan(&pts, Model::Era5, day_of(t) + 1000).is_empty());
    }

    #[test]
    fn ingest_and_annotate_tailwind() {
        let t0 = 1_756_684_800;
        let mut store = WeatherStore::default();
        let a = (47.0, 9.0);
        let b = (46.7, 8.6); // south-west
        let mut tr = Track::new("x", "NanoFox", "test");
        let mut f1 = Fix::new(t0 + 3600 * 20, a.0, a.1, LocationSource::SigfoxAtlas);
        f1.error_m = Some(1000.0);
        let mut f2 = Fix::new(t0 + 3600 * 26, b.0, b.1, LocationSource::SigfoxAtlas);
        f2.error_m = Some(1000.0);
        f2.pressure_hpa = Some(900.0);
        f2.alt_source = Some(AltitudeSource::PressureWindowMin);
        f2.alt_m = Some(988.0);
        tr.fixes = vec![f1, f2];
        tr.finalize();
        let mut tracks = vec![tr];
        let opts = AnnotateOptions {
            night_only: false,
            ..Default::default()
        };
        // Wind FROM north-east (45°) at 10 m/s → tailwind for a SW-bound animal.
        let pts = sample_points(&tracks, &opts);
        let reqs = store.plan(&pts, Model::Era5, day_of(t0) + 1000);
        assert!(!reqs.is_empty());
        for req in &reqs {
            assert_eq!(req.start_day, day_of(t0));
            let body = fake_response(req.cells.len(), t0, 48, 10.0, 45.0);
            assert_eq!(store.ingest(req, &body).unwrap(), 48 * req.cells.len());
        }
        assert_eq!(annotate(&mut tracks, &store, &opts), 2);
        let w = &tracks[0].fixes[1].weather;
        assert_eq!(w.level, Some("100m"), "high above ground → 100 m wind");
        assert!(w.wind_support.unwrap() > 9.0, "{:?}", w.wind_support);
        assert!(w.crosswind.unwrap().abs() < 3.0);
        assert!(w.seg_wind_support.unwrap() > 9.0);
        assert_eq!(w.seg_hours, Some(5));
        // 900 hPa with MSLP 1020 → higher than with standard 1013.25
        assert!(w.alt_msl_corrected.unwrap() > 988.0);
        assert!((w.wind_from_deg.unwrap() - 45.0).abs() < 0.5);
    }

    #[test]
    fn api_error_is_reported() {
        let mut store = WeatherStore::default();
        let req = Request::new(vec![(188, 36)], 0, 0, Model::Era5);
        let e = store.ingest(
            &req,
            r#"{"error":true,"reason":"Parameter 'start_date' is out of allowed range"}"#,
        );
        assert!(e.unwrap_err().contains("out of allowed range"));
    }

    #[test]
    fn angle_interp() {
        assert!(
            (interp_angle(350.0, 10.0, 0.5) - 0.0).abs() < 1e-4
                || (interp_angle(350.0, 10.0, 0.5) - 360.0).abs() < 1e-4
        );
        assert_eq!(parse_day("1970-01-02"), Some(1));
    }
}
