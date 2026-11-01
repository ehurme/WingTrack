//! Wind turbines and other tall man-made structures, and how close the animals came to them.
//!
//! **Data.** OpenStreetMap through the Overpass API (`power=generator` +
//! `generator:source=wind`; `man_made=mast|tower|chimney`), downloaded only for a corridor
//! of 0.5° tiles around the tracks and cached per tile. Heights come from the `height`,
//! `height:hub` and `rotor:diameter` tags; where they are missing a typical modern turbine
//! (hub 100 m, rotor 100 m) is assumed and flagged as estimated. National registers with
//! better metadata (Germany's Marktstammdatenregister, the US Wind Turbine Database, any
//! lat/lon/height table) can be imported as CSV instead or in addition.
//!
//! **Encounters.** Each step between consecutive fixes is taken as a straight line (the
//! only defensible path for sparse Sigfox fixes — the location error and step duration are
//! exported so the uncertainty can be filtered on). For every structure within the radius,
//! the closest approach, its time, the interpolated flight altitude and — when the tag's
//! pressure and ERA5 surface pressure allow — the height above ground and whether that was
//! inside the rotor-swept zone.

use crate::model::Track;
use crate::table::Table;
use std::collections::{BTreeMap, HashMap, HashSet};

pub const OVERPASS: [&str; 2] = [
    "https://overpass-api.de/api/interpreter",
    "https://overpass.private.coffee/api/interpreter",
];
/// Download tile size (degrees).
pub const TILE_DEG: f64 = 0.5;
/// Tiles per Overpass request.
pub const TILES_PER_QUERY: usize = 4;
/// Assumed when OSM has no dimensions (typical onshore turbine of the last decade).
pub const DEFAULT_HUB_M: f32 = 100.0;
pub const DEFAULT_ROTOR_M: f32 = 100.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    WindTurbine,
    Mast,
    Tower,
    Chimney,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::WindTurbine => "wind turbine",
            Kind::Mast => "mast",
            Kind::Tower => "tower",
            Kind::Chimney => "chimney",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Structure {
    /// `n123` / `w456` for OSM, `csv:<row>` for imports.
    pub id: String,
    pub kind: Kind,
    pub lat: f64,
    pub lon: f64,
    /// Total height (tip of the upper blade for turbines), metres.
    pub height_m: Option<f32>,
    pub hub_m: Option<f32>,
    pub rotor_m: Option<f32>,
    pub name: Option<String>,
    pub power_kw: Option<f32>,
    pub source: &'static str,
}

impl Structure {
    /// (hub height, rotor diameter, total height, any estimated) with defaults for gaps.
    pub fn dims(&self) -> (f32, f32, f32, bool) {
        if self.kind != Kind::WindTurbine {
            let h = self.height_m.unwrap_or(50.0);
            return (h, 0.0, h, self.height_m.is_none());
        }
        let mut est = false;
        let rotor = self.rotor_m.unwrap_or_else(|| {
            est = true;
            match (self.height_m, self.hub_m) {
                (Some(t), Some(h)) if t > h => 2.0 * (t - h),
                _ => DEFAULT_ROTOR_M,
            }
        });
        let hub = self.hub_m.unwrap_or_else(|| {
            est = true;
            self.height_m
                .map_or(DEFAULT_HUB_M, |t| (t - rotor / 2.0).max(rotor / 2.0))
        });
        let total = self.height_m.unwrap_or(hub + rotor / 2.0);
        (hub, rotor, total, est)
    }
}

// ---------------------------------------------------------------- download planning

pub type Tile = (i32, i32);

pub fn tile_of(lat: f64, lon: f64) -> Tile {
    (
        (lat / TILE_DEG).floor() as i32,
        (lon / TILE_DEG).floor() as i32,
    )
}

/// (south, west, north, east)
pub fn tile_bbox(t: Tile) -> (f64, f64, f64, f64) {
    let (s, w) = (t.0 as f64 * TILE_DEG, t.1 as f64 * TILE_DEG);
    (s, w, s + TILE_DEG, w + TILE_DEG)
}

/// Tiles within `buffer_m` of any visible track's path (sampled every ~2 km).
pub fn corridor_tiles(tracks: &[Track], buffer_m: f64) -> Vec<Tile> {
    let mut set: HashSet<Tile> = HashSet::new();
    let dlat = buffer_m / 111_320.0;
    for tr in tracks.iter().filter(|t| t.visible) {
        let mut add = |lat: f64, lon: f64| {
            let dlon = dlat / lat.to_radians().cos().max(0.05);
            for (a, b) in [
                (-dlat, -dlon),
                (-dlat, dlon),
                (dlat, -dlon),
                (dlat, dlon),
                (0.0, 0.0),
            ] {
                set.insert(tile_of(lat + a, lon + b));
            }
        };
        for (i, f) in tr.fixes.iter().enumerate() {
            add(f.lat, f.lon);
            if i > 0 {
                let p = &tr.fixes[i - 1];
                let d = crate::geo::haversine_m(p.lat, p.lon, f.lat, f.lon);
                let n = (d / 2000.0).ceil() as usize;
                for k in 1..n {
                    let (la, lo) =
                        crate::geo::interpolate(p.lat, p.lon, f.lat, f.lon, k as f64 / n as f64);
                    add(la, lo);
                }
            }
        }
    }
    let mut v: Vec<Tile> = set.into_iter().collect();
    v.sort_unstable();
    v
}

/// Overpass QL for a set of tiles: turbines and tall structures (nodes and ways, way
/// centres), tags included.
pub fn overpass_query(tiles: &[Tile], other: bool) -> String {
    let mut q = String::from("[out:json][timeout:180];(");
    for &t in tiles {
        let (s, w, n, e) = tile_bbox(t);
        let bb = format!("({s:.3},{w:.3},{n:.3},{e:.3})");
        q.push_str(&format!(
            r#"nwr["power"="generator"]["generator:source"="wind"]{bb};"#
        ));
        if other {
            q.push_str(&format!(r#"nwr["man_made"~"^(mast|tower|chimney)$"]{bb};"#));
        }
    }
    q.push_str(");out center tags;");
    q
}

/// Form-encoded POST to an Overpass server.
pub fn overpass_request(server: &str, tiles: &[Tile], other: bool) -> ehttp::Request {
    let q = overpass_query(tiles, other);
    let body = format!("data={}", urlencode(&q));
    let mut r = ehttp::Request::post(server, body.into_bytes());
    r.headers
        .insert("Content-Type", "application/x-www-form-urlencoded");
    r
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Metres from an OSM length tag: `150`, `150 m`, `150.5m`, `492'` / `492 ft`.
pub fn parse_len(s: &str) -> Option<f32> {
    let s = s.trim().to_lowercase().replace(',', ".");
    let (num, ft) = if let Some(x) = s.strip_suffix("ft") {
        (x.trim().to_string(), true)
    } else if let Some(x) = s.strip_suffix('\'') {
        (x.trim().to_string(), true)
    } else {
        (s.trim_end_matches('m').trim().to_string(), false)
    };
    let v: f32 = num.split(';').next()?.trim().parse().ok()?;
    let v = if ft { v * 0.3048 } else { v };
    (v.is_finite() && v > 0.0 && v < 1000.0).then_some(v)
}

/// kW from `generator:output:electricity` (`2.3 MW`, `850 kW`, `3000`).
fn parse_power(s: &str) -> Option<f32> {
    let s = s.trim().to_lowercase().replace(',', ".");
    let (num, mult) = if let Some(x) = s.strip_suffix("mw") {
        (x, 1000.0)
    } else if let Some(x) = s.strip_suffix("kw") {
        (x, 1.0)
    } else {
        (s.as_str(), 1.0)
    };
    let v: f32 = num.trim().parse().ok()?;
    Some(v * mult)
}

/// Parse an Overpass JSON response.
pub fn parse_overpass(body: &str) -> Result<Vec<Structure>, String> {
    let j: serde_json::Value = serde_json::from_str(body).map_err(|_| overpass_error(body))?;
    if let Some(r) = j.get("remark").and_then(|r| r.as_str())
        && (r.contains("error") || r.contains("timed out"))
    {
        return Err(format!("Overpass: {r}"));
    }
    let els = j
        .get("elements")
        .and_then(|e| e.as_array())
        .ok_or("Overpass response without elements")?;
    let mut out = Vec::new();
    for e in els {
        let ty = e.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let id = e.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
        let (lat, lon) = match (e.get("lat"), e.get("lon")) {
            (Some(a), Some(b)) => (a.as_f64(), b.as_f64()),
            _ => (
                e.pointer("/center/lat").and_then(|v| v.as_f64()),
                e.pointer("/center/lon").and_then(|v| v.as_f64()),
            ),
        };
        let (Some(lat), Some(lon)) = (lat, lon) else {
            continue;
        };
        let tag = |k: &str| {
            e.get("tags")
                .and_then(|t| t.get(k))
                .and_then(|v| v.as_str())
        };
        let kind = if tag("power") == Some("generator") {
            Kind::WindTurbine
        } else {
            match tag("man_made") {
                Some("mast") => Kind::Mast,
                Some("tower") => Kind::Tower,
                Some("chimney") => Kind::Chimney,
                _ => continue,
            }
        };
        let rotor = tag("rotor:diameter")
            .or(tag("generator:rotor:diameter"))
            .and_then(parse_len);
        let hub = tag("height:hub")
            .or(tag("hub_height"))
            .or(tag("generator:hub_height"))
            .and_then(parse_len);
        out.push(Structure {
            id: format!("{}{id}", ty.chars().next().unwrap_or('x')),
            kind,
            lat,
            lon,
            height_m: tag("height").and_then(parse_len),
            hub_m: hub,
            rotor_m: rotor,
            name: tag("name").or(tag("ref")).map(str::to_string),
            power_kw: tag("generator:output:electricity").and_then(parse_power),
            source: "OpenStreetMap",
        });
    }
    Ok(out)
}

fn overpass_error(body: &str) -> String {
    // Overpass reports errors as HTML.
    let txt: String = body
        .split('<')
        .filter_map(|p| p.split_once('>').map(|x| x.1.trim()))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let t = if txt.is_empty() { body } else { &txt };
    format!("Overpass: {}", t.chars().take(200).collect::<String>())
}

/// Import a structure table: OSM-style, the German Marktstammdatenregister export, the
/// US Wind Turbine Database, or any CSV with latitude / longitude (+ optional heights).
pub fn from_table(t: &Table) -> Result<Vec<Structure>, String> {
    let c_lat = t.col(&[
        "lat",
        "latitude",
        "ylat",
        "y",
        "koordinate_breitengrad_wgs84",
        "breitengrad",
        "location_lat",
    ]);
    let c_lon = t.col(&[
        "lon",
        "lng",
        "long",
        "longitude",
        "xlong",
        "x",
        "koordinate_laengengrad_wgs84",
        "laengengrad",
        "location_long",
    ]);
    let (Some(_), Some(_)) = (c_lat, c_lon) else {
        return Err("no latitude / longitude columns".into());
    };
    let c_h = t.col(&[
        "height",
        "total_height",
        "height_m",
        "t_ttlh",
        "gesamthoehe",
    ]);
    let c_hub = t.col(&[
        "hub_height",
        "height_hub",
        "hub_height_m",
        "t_hh",
        "nabenhoehe_der_windenergieanlage",
        "nabenhoehe",
    ]);
    let c_rot = t.col(&[
        "rotor_diameter",
        "rotor_diameter_m",
        "t_rd",
        "rotordurchmesser_der_windenergieanlage",
        "rotordurchmesser",
    ]);
    let c_kind = t.col(&["kind", "type", "man_made"]);
    let c_id = t.col(&[
        "id",
        "case_id",
        "mastr_nummer_der_einheit",
        "einheitmastrnummer",
        "osm_id",
    ]);
    let c_name = t.col(&[
        "name",
        "anzeige_name_der_einheit",
        "p_name",
        "name_des_windparks",
    ]);
    let c_kw = t.col(&[
        "power_kw",
        "bruttoleistung_der_einheit",
        "t_cap",
        "nettonennleistung_der_einheit",
    ]);
    // decimal commas ("121,5") in German exports
    let num = |row: &[String], c: Option<usize>| {
        let s = t.get(row, c).trim();
        crate::table::parse_num(s).or_else(|| crate::table::parse_num(&s.replace(',', ".")))
    };
    let mut out = Vec::new();
    for (i, row) in t.rows.iter().enumerate() {
        let (Some(lat), Some(lon)) = (num(row, c_lat), num(row, c_lon)) else {
            continue;
        };
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        let kind = match t.get(row, c_kind).to_lowercase().as_str() {
            "mast" => Kind::Mast,
            "tower" => Kind::Tower,
            "chimney" => Kind::Chimney,
            _ => Kind::WindTurbine,
        };
        let pos = |v: Option<f64>| v.map(|x| x as f32).filter(|x| *x > 0.0 && *x < 1000.0);
        let id = t.get(row, c_id).trim();
        out.push(Structure {
            id: if id.is_empty() {
                format!("csv:{i}")
            } else {
                id.to_string()
            },
            kind,
            lat,
            lon,
            height_m: pos(num(row, c_h)),
            hub_m: pos(num(row, c_hub)),
            rotor_m: pos(num(row, c_rot)),
            name: Some(t.get(row, c_name).trim().to_string()).filter(|s| !s.is_empty()),
            power_kw: num(row, c_kw).map(|x| x as f32),
            source: "imported",
        });
    }
    if out.is_empty() {
        return Err("no rows with valid coordinates".into());
    }
    Ok(out)
}

// ---------------------------------------------------------------- store

#[derive(Default)]
pub struct StructureStore {
    pub items: Vec<Structure>,
    ids: HashSet<String>,
    /// Tiles downloaded from OSM (or attempted).
    pub tiles: HashSet<Tile>,
    /// 0.05° grid → indices into `items`.
    index: HashMap<(i32, i32), Vec<usize>>,
    /// Bumped on every change (analysis cache key).
    pub version: u64,
}

const IDX_DEG: f64 = 0.05;

fn idx_key(lat: f64, lon: f64) -> (i32, i32) {
    (
        (lat / IDX_DEG).floor() as i32,
        (lon / IDX_DEG).floor() as i32,
    )
}

impl StructureStore {
    /// Add structures, skipping ids already present. Returns how many were new.
    pub fn add(&mut self, v: Vec<Structure>) -> usize {
        let mut n = 0;
        for s in v {
            if self.ids.insert(s.id.clone()) {
                self.index
                    .entry(idx_key(s.lat, s.lon))
                    .or_default()
                    .push(self.items.len());
                self.items.push(s);
                n += 1;
            }
        }
        self.version += 1;
        n
    }

    pub fn clear(&mut self) {
        *self = StructureStore {
            version: self.version + 1,
            ..Default::default()
        };
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Indices of structures whose index cell intersects the box.
    fn candidates(&self, s: f64, w: f64, n: f64, e: f64) -> impl Iterator<Item = usize> + '_ {
        let (a, b) = idx_key(s, w);
        let (c, d) = idx_key(n, e);
        (a..=c).flat_map(move |i| {
            (b..=d).flat_map(move |j| self.index.get(&(i, j)).into_iter().flatten().copied())
        })
    }
}

// ---------------------------------------------------------------- encounter analysis

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub radius_m: f32,
    /// Ignore masts/towers/chimneys lower than this (they are numerous; unknown height = keep).
    pub min_other_height_m: f32,
    pub include_other: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            radius_m: 1000.0,
            min_other_height_m: 50.0,
            include_other: true,
        }
    }
}

impl Params {
    pub fn counts(&self, s: &Structure) -> bool {
        match s.kind {
            Kind::WindTurbine => true,
            _ => self.include_other && s.height_m.is_none_or(|h| h >= self.min_other_height_m),
        }
    }
}

/// Closest approach of one track to one structure.
#[derive(Clone, Debug)]
pub struct Encounter {
    pub track: usize,
    pub structure: usize,
    pub min_dist_m: f32,
    /// Time of closest approach (straight-line, constant-speed interpolation).
    pub t: i64,
    /// Fix index at the end of the step of closest approach.
    pub fix: usize,
    /// Steps of this track that came within the radius of this structure.
    pub n_steps: usize,
    pub step_hours: f32,
    /// Larger location error of the two fixes bounding the step.
    pub loc_error_m: Option<f32>,
    pub alt_msl_m: Option<f32>,
    pub alt_agl_m: Option<f32>,
    /// Height above ground within [hub − rotor/2, hub + rotor/2] (turbines with AGL only).
    pub in_rotor_zone: Option<bool>,
}

#[derive(Default)]
pub struct Analysis {
    pub encounters: Vec<Encounter>,
    /// Per (track, fix): structures within the radius of the step ending at that fix,
    /// (turbines, other), and the nearest turbine distance within the search distance.
    pub per_fix: HashMap<(usize, usize), (u32, u32, Option<f32>)>,
    pub params: Params,
}

/// Point (x, y) to segment (0,0)-(bx,by) in metres: (distance, fraction along).
fn seg_dist(px: f64, py: f64, bx: f64, by: f64) -> (f64, f64) {
    let l2 = bx * bx + by * by;
    let s = if l2 > 0.0 {
        ((px * bx + py * by) / l2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (((px - s * bx).powi(2) + (py - s * by).powi(2)).sqrt(), s)
}

/// Nearest-turbine distances are reported up to this far (also beyond the radius).
const NEAREST_SEARCH_M: f64 = 10_000.0;

pub fn analyse(tracks: &[Track], store: &StructureStore, p: &Params) -> Analysis {
    let mut out = Analysis {
        params: *p,
        ..Default::default()
    };
    let r = p.radius_m as f64;
    let search = r.max(NEAREST_SEARCH_M);
    for (ti, tr) in tracks.iter().enumerate() {
        if !tr.visible || tr.fixes.len() < 2 {
            continue;
        }
        let mut best: BTreeMap<usize, Encounter> = BTreeMap::new();
        for fi in 1..tr.fixes.len() {
            let (a, b) = (&tr.fixes[fi - 1], &tr.fixes[fi]);
            let lat0 = (a.lat + b.lat) / 2.0;
            let mx = 111_320.0 * lat0.to_radians().cos();
            let my = 110_574.0;
            let (bx, by) = ((b.lon - a.lon) * mx, (b.lat - a.lat) * my);
            let pad_lat = search / my;
            let pad_lon = search / mx.max(1.0);
            let (s, n) = (a.lat.min(b.lat) - pad_lat, a.lat.max(b.lat) + pad_lat);
            let (w, e) = (a.lon.min(b.lon) - pad_lon, a.lon.max(b.lon) + pad_lon);
            let (mut nt, mut no, mut nearest) = (0u32, 0u32, None::<f32>);
            for si in store.candidates(s, w, n, e) {
                let st = &store.items[si];
                if !p.counts(st) {
                    continue;
                }
                let (d, frac) = seg_dist((st.lon - a.lon) * mx, (st.lat - a.lat) * my, bx, by);
                if st.kind == Kind::WindTurbine && d <= search {
                    nearest = Some(nearest.map_or(d as f32, |x| x.min(d as f32)));
                }
                if d > r {
                    continue;
                }
                if st.kind == Kind::WindTurbine {
                    nt += 1;
                } else {
                    no += 1;
                }
                let t = a.t + ((b.t - a.t) as f64 * frac).round() as i64;
                let lerp = |x: Option<f32>, y: Option<f32>| match (x, y) {
                    (Some(x), Some(y)) => Some(x + (y - x) * frac as f32),
                    (x, y) => {
                        if frac < 0.5 {
                            x.or(y)
                        } else {
                            y.or(x)
                        }
                    }
                };
                let alt = lerp(a.best_alt(), b.best_alt());
                let p_tag = lerp(a.pressure_hpa, b.pressure_hpa);
                let p_sfc = lerp(
                    a.weather.surface_pressure_hpa,
                    b.weather.surface_pressure_hpa,
                );
                let agl = match (p_tag, p_sfc) {
                    (Some(pt), Some(ps)) => {
                        Some(crate::geo::pressure_to_altitude_m(pt as f64, ps as f64) as f32)
                    }
                    _ => None,
                };
                let in_zone = (st.kind == Kind::WindTurbine)
                    .then(|| {
                        let (hub, rotor, _, _) = st.dims();
                        agl.map(|h| h >= hub - rotor / 2.0 && h <= hub + rotor / 2.0)
                    })
                    .flatten();
                let enc = Encounter {
                    track: ti,
                    structure: si,
                    min_dist_m: d as f32,
                    t,
                    fix: fi,
                    n_steps: 1,
                    step_hours: (b.t - a.t) as f32 / 3600.0,
                    loc_error_m: match (a.error_m, b.error_m) {
                        (Some(x), Some(y)) => Some(x.max(y)),
                        (x, y) => x.or(y),
                    },
                    alt_msl_m: alt,
                    alt_agl_m: agl,
                    in_rotor_zone: in_zone,
                };
                match best.get_mut(&si) {
                    Some(e) => {
                        let n = e.n_steps + 1;
                        if enc.min_dist_m < e.min_dist_m {
                            *e = enc;
                        }
                        e.n_steps = n;
                    }
                    None => {
                        best.insert(si, enc);
                    }
                }
            }
            out.per_fix.insert((ti, fi), (nt, no, nearest));
        }
        out.encounters.extend(best.into_values());
    }
    out
}

/// Per-track summary row.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub track: usize,
    pub path_km: f64,
    pub turbines: usize,
    pub other: usize,
    pub per_100km: f64,
    pub nearest_turbine_m: Option<f32>,
    pub steps_near: usize,
    pub with_agl: usize,
    pub in_rotor_zone: usize,
}

pub fn summaries(tracks: &[Track], store: &StructureStore, a: &Analysis) -> Vec<Summary> {
    let mut out = Vec::new();
    for (ti, tr) in tracks.iter().enumerate() {
        if !tr.visible {
            continue;
        }
        let path_km = tr
            .fixes
            .iter()
            .filter_map(|f| f.step_m)
            .map(|d| d as f64)
            .sum::<f64>()
            / 1000.0;
        let enc: Vec<&Encounter> = a.encounters.iter().filter(|e| e.track == ti).collect();
        let is_t = |e: &&&Encounter| store.items[e.structure].kind == Kind::WindTurbine;
        let turbines = enc.iter().filter(is_t).count();
        let steps_near = (1..tr.fixes.len())
            .filter(|&fi| a.per_fix.get(&(ti, fi)).is_some_and(|x| x.0 > 0))
            .count();
        out.push(Summary {
            track: ti,
            path_km,
            turbines,
            other: enc.len() - turbines,
            per_100km: if path_km > 0.0 {
                turbines as f64 / path_km * 100.0
            } else {
                0.0
            },
            nearest_turbine_m: (1..tr.fixes.len())
                .filter_map(|fi| a.per_fix.get(&(ti, fi)).and_then(|x| x.2))
                .reduce(f32::min),
            steps_near,
            with_agl: enc
                .iter()
                .filter(is_t)
                .filter(|e| e.alt_agl_m.is_some())
                .count(),
            in_rotor_zone: enc.iter().filter(|e| e.in_rotor_zone == Some(true)).count(),
        });
    }
    out
}

/// What the map and the 3D view draw: the structures, which ones the animals came within
/// the radius of, and the filter.
pub struct Draw<'a> {
    pub items: &'a [Structure],
    pub hits: &'a HashSet<usize>,
    pub params: Params,
}

// ---------------------------------------------------------------- CSV export

fn o<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}
fn r1(v: Option<f32>) -> String {
    v.map(|x| format!("{x:.1}")).unwrap_or_default()
}

fn write(rows: Vec<Vec<String>>) -> String {
    let mut w = csv::Writer::from_writer(Vec::new());
    for r in rows {
        let _ = w.write_record(&r);
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

fn s(v: &str) -> String {
    v.to_string()
}

/// One row per (track, structure) within the radius: closest approach and context.
pub fn encounters_csv(tracks: &[Track], store: &StructureStore, a: &Analysis) -> String {
    let mut rows = vec![
        [
            "individual_local_identifier",
            "tag_type",
            "species",
            "structure_id",
            "structure_kind",
            "structure_source",
            "structure_lat",
            "structure_long",
            "hub_height_m",
            "rotor_diameter_m",
            "total_height_m",
            "dimensions_estimated",
            "power_kw",
            "structure_name",
            "radius_m",
            "min_distance_m",
            "time_closest_utc",
            "n_steps_within_radius",
            "step_hours",
            "location_error_m",
            "animal_alt_msl_m",
            "animal_alt_agl_m",
            "in_rotor_zone",
        ]
        .map(s)
        .to_vec(),
    ];
    for e in &a.encounters {
        let tr = &tracks[e.track];
        let st = &store.items[e.structure];
        let (hub, rotor, total, est) = st.dims();
        let turb = st.kind == Kind::WindTurbine;
        rows.push(vec![
            tr.id.clone(),
            tr.tag.clone(),
            o(tr.species.clone()),
            st.id.clone(),
            s(st.kind.label()),
            s(st.source),
            format!("{:.6}", st.lat),
            format!("{:.6}", st.lon),
            if turb {
                format!("{hub:.1}")
            } else {
                String::new()
            },
            if turb {
                format!("{rotor:.1}")
            } else {
                String::new()
            },
            format!("{total:.1}"),
            est.to_string(),
            o(st.power_kw),
            o(st.name.clone()),
            format!("{:.0}", a.params.radius_m),
            format!("{:.0}", e.min_dist_m),
            crate::table::fmt_time(e.t),
            e.n_steps.to_string(),
            format!("{:.2}", e.step_hours),
            r1(e.loc_error_m),
            r1(e.alt_msl_m),
            r1(e.alt_agl_m),
            o(e.in_rotor_zone),
        ]);
    }
    write(rows)
}

/// One row per visible track.
pub fn summary_csv(tracks: &[Track], store: &StructureStore, a: &Analysis) -> String {
    let mut rows = vec![
        [
            "individual_local_identifier",
            "tag_type",
            "species",
            "first_fix_utc",
            "last_fix_utc",
            "n_fixes",
            "path_km",
            "radius_m",
            "turbines_within_radius",
            "other_structures_within_radius",
            "turbines_per_100km",
            "nearest_turbine_m",
            "steps_near_turbines",
            "turbine_encounters_with_agl",
            "turbine_encounters_in_rotor_zone",
            "mean_lat",
            "mean_long",
        ]
        .map(s)
        .to_vec(),
    ];
    for sm in summaries(tracks, store, a) {
        let tr = &tracks[sm.track];
        let (t0, t1) = tr.time_range().unwrap_or((0, 0));
        let n = tr.fixes.len().max(1) as f64;
        rows.push(vec![
            tr.id.clone(),
            tr.tag.clone(),
            o(tr.species.clone()),
            crate::table::fmt_time(t0),
            crate::table::fmt_time(t1),
            tr.fixes.len().to_string(),
            format!("{:.1}", sm.path_km),
            format!("{:.0}", a.params.radius_m),
            sm.turbines.to_string(),
            sm.other.to_string(),
            format!("{:.2}", sm.per_100km),
            r1(sm.nearest_turbine_m),
            sm.steps_near.to_string(),
            sm.with_agl.to_string(),
            sm.in_rotor_zone.to_string(),
            format!("{:.5}", tr.fixes.iter().map(|f| f.lat).sum::<f64>() / n),
            format!("{:.5}", tr.fixes.iter().map(|f| f.lon).sum::<f64>() / n),
        ]);
    }
    write(rows)
}

/// All loaded structures (re-importable with [`from_table`]).
pub fn structures_csv(store: &StructureStore) -> String {
    let mut rows = vec![
        [
            "id",
            "kind",
            "source",
            "lat",
            "lon",
            "height",
            "hub_height",
            "rotor_diameter",
            "total_height_used",
            "dimensions_estimated",
            "power_kw",
            "name",
        ]
        .map(s)
        .to_vec(),
    ];
    for st in &store.items {
        let (_, _, total, est) = st.dims();
        rows.push(vec![
            st.id.clone(),
            s(st.kind.label()),
            s(st.source),
            format!("{:.6}", st.lat),
            format!("{:.6}", st.lon),
            r1(st.height_m),
            r1(st.hub_m),
            r1(st.rotor_m),
            format!("{total:.1}"),
            est.to_string(),
            o(st.power_kw),
            o(st.name.clone()),
        ]);
    }
    write(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fix, LocationSource};

    const OVERPASS_JSON: &str = r#"{"version":0.6,"elements":[
      {"type":"node","id":1,"lat":47.70,"lon":9.05,"tags":{"power":"generator","generator:source":"wind","height:hub":"135","rotor:diameter":"131 m","generator:output:electricity":"3.6 MW"}},
      {"type":"node","id":2,"lat":47.75,"lon":9.20,"tags":{"power":"generator","generator:source":"wind","height":"200"}},
      {"type":"node","id":3,"lat":47.75,"lon":9.10,"tags":{"power":"generator","generator:source":"wind"}},
      {"type":"way","id":9,"center":{"lat":47.71,"lon":9.051},"tags":{"man_made":"chimney","height":"120"}},
      {"type":"node","id":10,"lat":47.71,"lon":9.052,"tags":{"man_made":"mast","height":"20"}}
    ]}"#;

    #[test]
    fn overpass_parse_and_dims() {
        let v = parse_overpass(OVERPASS_JSON).unwrap();
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].dims(), (135.0, 131.0, 200.5, false));
        assert_eq!(v[0].power_kw, Some(3600.0));
        // total height only → assumed rotor, hub derived
        let (hub, rotor, total, est) = v[1].dims();
        assert_eq!((hub, rotor, total, est), (150.0, 100.0, 200.0, true));
        assert_eq!(v[2].dims(), (DEFAULT_HUB_M, DEFAULT_ROTOR_M, 150.0, true));
        assert_eq!((v[3].id.as_str(), v[3].kind), ("w9", Kind::Chimney));
        assert_eq!(parse_len("492 ft").map(|x| x.round()), Some(150.0));
        assert!(
            parse_overpass("<html><body><p>rate_limited</p></body></html>")
                .unwrap_err()
                .contains("rate_limited")
        );
        let q = overpass_query(&[(95, 18)], true);
        assert!(q.contains("(47.500,9.000,48.000,9.500)") && q.contains("chimney"));
    }

    fn track() -> Track {
        // west → east along 47.70 N, passing turbine 1 at ~0 m and the chimney ~1.1 km north
        let mut tr = Track::new("bat", "NanoFox", "t");
        for (i, lon) in [9.00, 9.04, 9.08].into_iter().enumerate() {
            let mut f = Fix::new(i as i64 * 3600, 47.70, lon, LocationSource::SigfoxAtlas);
            f.error_m = Some(500.0);
            f.pressure_hpa = Some(1000.0);
            f.weather.surface_pressure_hpa = Some(1015.0);
            f.alt_m = Some(400.0);
            tr.fixes.push(f);
        }
        tr.finalize();
        tr
    }

    #[test]
    fn encounters_along_steps() {
        let mut st = StructureStore::default();
        st.add(parse_overpass(OVERPASS_JSON).unwrap());
        assert_eq!(
            st.add(parse_overpass(OVERPASS_JSON).unwrap()),
            0,
            "ids dedupe"
        );
        let tracks = vec![track()];
        let p = Params {
            radius_m: 1500.0,
            ..Default::default()
        };
        let a = analyse(&tracks, &st, &p);
        let kinds: Vec<Kind> = a
            .encounters
            .iter()
            .map(|e| st.items[e.structure].kind)
            .collect();
        // turbine 1 and the 120 m chimney; the 20 m mast is below the height filter
        assert_eq!(kinds, vec![Kind::WindTurbine, Kind::Chimney]);
        let e = &a.encounters[0];
        assert!(e.min_dist_m < 5.0, "{}", e.min_dist_m);
        // turbine at lon 9.05 lies on the second step, a quarter of the way
        assert_eq!(e.fix, 2);
        assert_eq!(e.t, 3600 + 900);
        // 1000 vs 1015 hPa → ~126 m above ground, inside 135 ± 65.5 m
        assert!((e.alt_agl_m.unwrap() - 126.0).abs() < 3.0);
        assert_eq!(e.in_rotor_zone, Some(true));
        assert_eq!(a.per_fix[&(0, 2)].0, 1);
        assert!(
            a.per_fix[&(0, 1)].2.unwrap() > 700.0,
            "nearest turbine beyond step 1's end"
        );
        let sm = summaries(&tracks, &st, &a);
        assert_eq!(
            (sm[0].turbines, sm[0].other, sm[0].in_rotor_zone),
            (1, 1, 1)
        );
        assert!((sm[0].path_km - 6.0).abs() < 0.1);
        let csv = encounters_csv(&tracks, &st, &a);
        assert!(
            csv.lines()
                .nth(1)
                .unwrap()
                .starts_with("bat,NanoFox,,n1,wind turbine,OpenStreetMap")
        );
        let csv = summary_csv(&tracks, &st, &a);
        assert!(csv.contains("bat,NanoFox"));
        // structures CSV re-imports
        let t = Table::from_str("s.csv", &structures_csv(&st)).unwrap();
        assert_eq!(from_table(&t).unwrap().len(), 5);
    }

    #[test]
    fn corridor_and_registry_import() {
        let tiles = corridor_tiles(&[track()], 1000.0);
        assert!(tiles.contains(&tile_of(47.70, 9.04)));
        assert!(tiles.len() <= 4, "{tiles:?}");
        // Marktstammdatenregister-style export (semicolons, decimal commas)
        let mastr = "EinheitMastrNummer;Koordinate: Breitengrad (WGS84);Koordinate: Längengrad (WGS84);Nabenhöhe der Windenergieanlage;Rotordurchmesser der Windenergieanlage\n\
                     SEE900;47,7;9,05;135,5;131\n";
        let t = Table::from_str("mastr.csv", mastr).unwrap();
        let v = from_table(&t).unwrap();
        assert_eq!(v[0].id, "SEE900");
        assert_eq!((v[0].hub_m, v[0].rotor_m), (Some(135.5), Some(131.0)));
        // USWTDB
        let t = Table::from_str(
            "us.csv",
            "case_id,xlong,ylat,t_hh,t_rd,t_ttlh\n3001,-100.1,35.2,80,77,118.5\n",
        )
        .unwrap();
        let v = from_table(&t).unwrap();
        assert_eq!(v[0].height_m, Some(118.5));
    }
}
