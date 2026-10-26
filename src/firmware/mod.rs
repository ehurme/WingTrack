//! Firmware / data-format profiles.
//!
//! Each tag family and firmware writes different columns with different meanings
//! (e.g. TinyFox sends a *cumulative* VeDBA total and a 24 h minimum pressure, NanoFox
//! "30Days" sends five 36‑min VeDBA and temperature bins plus a 3 h minimum pressure,
//! NanoFox "FineScalePressure" swaps the temperature bins for five instantaneous
//! pressure readings, GPS tags deliver precise fixes with HDOP and height…).
//!
//! A [`Firmware`] knows how to recognise its own export (`detect`) and how to turn it into
//! [`Track`]s (`parse`). Adding a new tag type means adding one struct to [`registry`].

mod ctt;
mod generic;
mod movebank;
mod wildcloud;

use crate::model::{AltitudeSource, Fix, ParseOutput, SensorKind, SensorSample, Track};
use crate::table::Table;
use std::collections::BTreeMap;

pub use ctt::CttGps;
pub use generic::Generic;
pub use movebank::{MovebankGps, MovebankSigfox};
pub use wildcloud::{Wildcloud, WildcloudVariant};

pub trait Firmware: Send + Sync {
    /// Stable identifier (used for overrides and written to exports).
    fn id(&self) -> &'static str;
    /// Short human-readable name.
    fn label(&self) -> &'static str;
    /// One-line description shown in the UI.
    fn description(&self) -> &'static str;
    /// Confidence 0..1 that this table was written by this firmware/exporter.
    fn detect(&self, t: &Table) -> f32;
    /// Parse the table into tracks.
    fn parse(&self, t: &Table) -> Result<ParseOutput, String>;
}

/// All known profiles. Order only matters for ties in auto-detection.
pub fn registry() -> Vec<Box<dyn Firmware>> {
    vec![
        Box::new(Wildcloud::new(WildcloudVariant::TinyFox)),
        Box::new(Wildcloud::new(WildcloudVariant::NanoFox30Days)),
        Box::new(Wildcloud::new(WildcloudVariant::NanoFoxFineScalePressure)),
        Box::new(MovebankSigfox),
        Box::new(CttGps),
        Box::new(MovebankGps),
        Box::new(Generic),
    ]
}

/// Ranked detection results (best first).
pub fn rank(t: &Table) -> Vec<(&'static str, f32)> {
    let mut v: Vec<_> = registry().iter().map(|f| (f.id(), f.detect(t))).collect();
    v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    v
}

/// Parse with the best-matching profile, or with `force` (a firmware id) if given.
pub fn parse(t: &Table, force: Option<&str>) -> Result<(ParseOutput, &'static str), String> {
    let reg = registry();
    let fw = match force {
        Some(id) => reg
            .iter()
            .find(|f| f.id() == id)
            .ok_or_else(|| format!("unknown firmware profile '{id}'"))?,
        None => {
            let best = reg
                .iter()
                .map(|f| (f, f.detect(t)))
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .ok_or("no firmware profiles")?;
            if best.1 <= 0.0 {
                return Err(format!(
                    "{}: could not recognise the format (no latitude/longitude/time columns found). \
                     Columns: {}",
                    t.name,
                    t.headers.join(", ")
                ));
            }
            best.0
        }
    };
    let mut out = fw.parse(t)?;
    for tr in &mut out.tracks {
        tr.firmware = fw.id().to_string();
        tr.finalize();
    }
    out.tracks
        .retain(|tr| !tr.fixes.is_empty() || !tr.sensors.is_empty());
    if out.tracks.is_empty() {
        return Err(format!(
            "{}: parsed with '{}' but no usable rows were found",
            t.name,
            fw.label()
        ));
    }
    Ok((out, fw.id()))
}

pub fn label_for(id: &str) -> &'static str {
    registry()
        .iter()
        .find(|f| f.id() == id)
        .map(|f| f.label())
        .unwrap_or("unknown")
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

pub(crate) const ID_ALIASES: &[&str] = &[
    "individual_local_identifier",
    "individual_id",
    "animal_id",
    "animal",
    "individual",
    "bird_id",
    "bat_id",
    "id",
    "track_id",
    "tag_local_identifier",
    "tag_id",
    "device",
    "device_id",
    "imei",
    "alias",
];
pub(crate) const LAT_ALIASES: &[&str] = &[
    "location_lat",
    "latitude",
    "lat",
    "gps_latitude",
    "latitude_decimal",
    "y",
    "lat_deg",
];
pub(crate) const LON_ALIASES: &[&str] = &[
    "location_long",
    "location_lon",
    "longitude",
    "lon",
    "long",
    "lng",
    "gps_longitude",
    "longitude_decimal",
    "x",
    "lon_deg",
];
pub(crate) const TIME_ALIASES: &[&str] = &[
    "timestamp",
    "time_utc",
    "datetime_utc",
    "utc_datetime",
    "datetime",
    "date_time",
    "time",
    "gps_time",
    "time_of_fix",
    "t",
];

/// File stem used as a fallback track id.
pub(crate) fn file_stem(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(base)
        .to_string()
}

/// Group fixes/sensors into tracks keyed by id, preserving first-seen order.
pub(crate) struct TrackBuilder {
    order: Vec<String>,
    tracks: BTreeMap<String, Track>,
    tag: String,
}

impl TrackBuilder {
    pub fn new(tag: &str) -> Self {
        TrackBuilder {
            order: Vec::new(),
            tracks: BTreeMap::new(),
            tag: tag.to_string(),
        }
    }
    pub fn track(&mut self, id: &str) -> &mut Track {
        if !self.tracks.contains_key(id) {
            self.order.push(id.to_string());
            self.tracks
                .insert(id.to_string(), Track::new(id, self.tag.clone(), ""));
        }
        self.tracks.get_mut(id).expect("inserted above")
    }
    pub fn finish(mut self) -> Vec<Track> {
        self.order
            .iter()
            .filter_map(|id| self.tracks.remove(id))
            .collect()
    }
}

/// Columns written by `SigfoxTagPrep::annotate_era5()` (or similar) — if present, keep them
/// as "imported" weather so files annotated in R display without a re-download.
pub(crate) fn import_weather(t: &Table, row: &[String], f: &mut Fix) {
    let u = t
        .num(row, t.col(&["era5_u_flight", "u_flight"]))
        .or_else(|| t.num(row, t.col(&["era5_u100", "u100", "u_100m"])))
        .or_else(|| t.num(row, t.col(&["era5_u10", "u10", "u_10m"])));
    let v = t
        .num(row, t.col(&["era5_v_flight", "v_flight"]))
        .or_else(|| t.num(row, t.col(&["era5_v100", "v100", "v_100m"])))
        .or_else(|| t.num(row, t.col(&["era5_v10", "v10", "v_10m"])));
    if let (Some(u), Some(v)) = (u, v) {
        f.weather.u = Some(u as f32);
        f.weather.v = Some(v as f32);
        f.weather.level = Some("imported");
        f.weather.wind_speed = Some(((u * u + v * v).sqrt()) as f32);
        f.weather.wind_from_deg =
            Some(((u.atan2(v).to_degrees() + 180.0).rem_euclid(360.0)) as f32);
    }
    if let Some(ws) = t.num(
        row,
        t.col(&["wind_support_flight", "wind_support", "wind_support_100"]),
    ) {
        f.weather.wind_support = Some(ws as f32);
    }
    if let Some(cw) = t.num(
        row,
        t.col(&["crosswind_flight", "crosswind", "crosswind_100"]),
    ) {
        f.weather.crosswind = Some(cw as f32);
    }
    if let Some(v) = t.num(row, t.col(&["era5_t2m", "t2m"])) {
        // ERA5 NetCDF is in Kelvin.
        f.weather.temp_2m = Some(if v > 150.0 {
            (v - 273.15) as f32
        } else {
            v as f32
        });
    }
    if let Some(v) = t.num(row, t.col(&["era5_msl", "msl"])) {
        f.weather.msl_hpa = Some(if v > 5000.0 {
            (v / 100.0) as f32
        } else {
            v as f32
        });
    }
    // Flight-level (pressure-level) wind from annotate_era5() or a WingTrack export.
    if let (Some(lvl), Some(fp)) = (
        t.num(row, t.col(&["matched_pressure_level"])),
        t.num(row, t.col(&["flight_pressure_hpa"])),
    ) {
        let num = |names: &[&str]| t.num(row, t.col(names)).map(|v| v as f32);
        let u = num(&["u_flight", "era5_u_flight"]).unwrap_or(f32::NAN);
        let v = num(&["v_flight", "era5_v_flight"]).unwrap_or(f32::NAN);
        let at_best = match t
            .get(row, t.col(&["at_best_wind"]))
            .to_ascii_lowercase()
            .as_str()
        {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        };
        f.weather.flight = Some(crate::model::FlightWind {
            flight_pressure_hpa: fp as f32,
            flight_height_m: num(&["flight_height_m"]),
            matched_level_hpa: lvl as f32,
            u,
            v,
            wind_speed: num(&["wind_speed_flight"]).unwrap_or((u * u + v * v).sqrt()),
            wind_support: num(&["wind_support_flight"]),
            crosswind: num(&["crosswind_flight"]),
            airspeed: num(&["airspeed_flight"]),
            best_level_hpa: num(&["best_wind_level"]),
            best_level_height_m: num(&["best_wind_level_height_m"]),
            best_wind_support: num(&["best_wind_support"]),
            at_best_wind: at_best,
            source: "imported".into(),
        });
    }
}

/// Attach barometric altitude to fixes lacking it, from the minimum pressure sample in
/// the `window_s` before each fix (mirrors `add_min_pressure_to_locations()`).
pub(crate) fn pressure_from_samples(tr: &mut Track, window_s: i64) {
    let mut ps: Vec<(i64, f32)> = tr
        .sensors
        .iter()
        .filter(|s| s.kind == SensorKind::Pressure)
        .map(|s| (s.t, s.value))
        .collect();
    if ps.is_empty() {
        return;
    }
    ps.sort_by_key(|p| p.0);
    for f in tr.fixes.iter_mut().filter(|f| f.pressure_hpa.is_none()) {
        let lo = ps.partition_point(|p| p.0 < f.t - window_s);
        let hi = ps.partition_point(|p| p.0 <= f.t);
        if let Some(pmin) = ps[lo..hi].iter().map(|p| p.1).reduce(f32::min) {
            set_pressure_alt(f, pmin as f64, AltitudeSource::PressureWindowMin);
        }
    }
}

/// Plausible tag pressure (hPa / mbar) → set pressure + ISA altitude on a fix.
pub(crate) fn set_pressure_alt(f: &mut Fix, p: f64, src: AltitudeSource) {
    if (300.0..1100.0).contains(&p) {
        f.pressure_hpa = Some(p as f32);
        f.alt_m = Some(crate::geo::pressure_to_altitude_m(p, 1013.25) as f32);
        f.alt_source = Some(src);
    }
}

pub(crate) fn push_sensor(tr: &mut Track, t: i64, kind: SensorKind, v: Option<f64>) {
    if let Some(v) = v {
        tr.sensors.push(SensorSample {
            t,
            kind,
            value: v as f32,
        });
    }
}

#[cfg(test)]
mod tests;
