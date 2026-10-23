//! Movebank CSV exports (hyphenated `location-lat` or R/move2 underscored `location_lat`).
//!
//! * [`MovebankSigfox`] — studies uploaded via `wildcloud_to_movebank()`: `sigfox-*` and
//!   `tinyfox-*` attributes, location rows plus separate sensor rows (VeDBA, pressure, temperature).
//! * [`MovebankGps`] — ordinary GPS studies (`height-above-*`, `ground-speed`, `gps-hdop`, ...).

use super::*;
use crate::model::LocationSource;
use crate::table::parse_time;

pub struct MovebankSigfox;
pub struct MovebankGps;

fn is_sigfox(t: &Table) -> bool {
    t.norm
        .iter()
        .any(|n| n.starts_with("sigfox_") || n.starts_with("tinyfox_") || n.starts_with("nanofox_"))
}

fn has_movebank_core(t: &Table) -> bool {
    t.has(&["location_lat"]) && t.has(&["location_long", "location_lon"]) && t.has(&["timestamp"])
}

/// Time of a Movebank row: `timestamp`, else midpoint of `start-timestamp`/`end-timestamp`.
fn row_time(
    t: &Table,
    row: &[String],
    c_ts: Option<usize>,
    c_s: Option<usize>,
    c_e: Option<usize>,
) -> Option<i64> {
    parse_time(t.get(row, c_ts)).or_else(|| {
        let s = parse_time(t.get(row, c_s))?;
        let e = parse_time(t.get(row, c_e)).unwrap_or(s);
        Some((s + e) / 2)
    })
}

fn id_of(
    t: &Table,
    row: &[String],
    c_ind: Option<usize>,
    c_tag: Option<usize>,
    fallback: &str,
) -> String {
    let a = t.get(row, c_ind).trim();
    if !a.is_empty() {
        return a.to_string();
    }
    let b = t.get(row, c_tag).trim();
    if !b.is_empty() {
        b.to_string()
    } else {
        fallback.to_string()
    }
}

impl Firmware for MovebankSigfox {
    fn id(&self) -> &'static str {
        "movebank-sigfox"
    }
    fn label(&self) -> &'static str {
        "Movebank · Sigfox (TinyFox/NanoFox/uWasp)"
    }
    fn description(&self) -> &'static str {
        "Movebank export of ICARUS Sigfox tags: Atlas locations with radius, separate VeDBA/pressure/temperature rows"
    }
    fn detect(&self, t: &Table) -> f32 {
        if has_movebank_core(t) && is_sigfox(t) {
            0.92
        } else {
            0.0
        }
    }

    fn parse(&self, t: &Table) -> Result<ParseOutput, String> {
        let c_ts = t.col(&["timestamp"]);
        let c_start = t.col(&["start_timestamp"]);
        let c_end = t.col(&["end_timestamp"]);
        let c_lat = t.col(&["location_lat"]);
        let c_lon = t.col(&["location_long", "location_lon"]);
        let c_ind = t.col(&["individual_local_identifier"]);
        let c_tag = t.col(&["tag_local_identifier"]);
        let c_sp = t.col(&["individual_taxon_canonical_name", "taxon_canonical_name"]);
        let c_rad = t.col(&[
            "sigfox_computed_location_radius",
            "location_error_numerical",
        ]);
        let c_model = t.col(&["tag_model", "model", "tag_type", "tag_manufacturer_name"]);
        let c_sensor = t.col(&["sensor_type", "sensor_type_id"]);
        let c_baro = t.col(&["barometric_pressure"]);
        let c_temp = t.col(&["external_temperature", "temperature"]);
        let c_vedba = t.col(&["vedba", "acceleration_vedba"]);
        let c_tf_total = t.col(&["tinyfox_total_vedba"]);
        let c_tf_act = t.col(&["tinyfox_activity_percent_last_24h"]);
        let c_tf_pmin = t.col(&["tinyfox_pressure_min_last_24h"]);
        let c_tf_tmin = t.col(&["tinyfox_temperature_min_last_24h"]);
        let c_tf_tmax = t.col(&["tinyfox_temperature_max_last_24h"]);
        let c_min3 = t.col(&["min_3h_pressure", "minimum_pressure", "min_pressure"]);
        let c_seq = t.col(&["sequence_number"]);
        let c_lqi = t.col(&["sigfox_lqi", "sigfox_link_quality"]);

        let tinyfox = c_tf_total.is_some() || c_tf_pmin.is_some();
        let default_tag = if tinyfox { "TinyFox" } else { "Sigfox" };
        let fallback = file_stem(&t.name);
        let mut b = TrackBuilder::new(default_tag);
        let mut out = ParseOutput::default();
        let mut skipped = 0;

        for row in &t.rows {
            let Some(ts) = row_time(t, row, c_ts, c_start, c_end) else {
                skipped += 1;
                continue;
            };
            let id = id_of(t, row, c_ind, c_tag, &fallback);
            let tr = b.track(&id);
            if tr.species.is_none() {
                let sp = t.get(row, c_sp).trim();
                if !sp.is_empty() {
                    tr.species = Some(sp.to_string());
                }
            }
            if let Some(m) = c_model
                .map(|c| t.get(row, Some(c)).trim())
                .filter(|s| !s.is_empty())
            {
                let ml = m.to_lowercase();
                tr.tag = if ml.contains("nano") {
                    "NanoFox".into()
                } else if ml.contains("two") {
                    "TinyFoxTwo".into()
                } else if ml.contains("tiny") {
                    "TinyFox".into()
                } else if ml.contains("wasp") {
                    "uWasp".into()
                } else {
                    m.to_string()
                };
            }
            let sensor = t.get(row, c_sensor).to_lowercase();
            let lat = t.num(row, c_lat);
            let lon = t.num(row, c_lon);

            // Sensor values on any row.
            let baro = t.num(row, c_baro);
            let temp = t.num(row, c_temp);
            let vedba = t.num(row, c_vedba);
            push_sensor(tr, ts, SensorKind::Pressure, baro);
            push_sensor(tr, ts, SensorKind::Temperature, temp);
            push_sensor(tr, ts, SensorKind::Vedba, vedba);
            push_sensor(tr, ts, SensorKind::Activity, t.num(row, c_tf_act));

            match (lat, lon) {
                (Some(la), Some(lo)) if !(la == 0.0 && lo == 0.0) && !sensor.contains("accel") => {
                    let mut f = Fix::new(ts, la, lo, LocationSource::SigfoxAtlas);
                    f.error_m = t.num(row, c_rad).map(|v| v as f32);
                    if let Some(p) = t.num(row, c_tf_pmin) {
                        set_pressure_alt(&mut f, p, AltitudeSource::PressureWindowMin);
                        push_sensor(tr, ts, SensorKind::Pressure, Some(p));
                    } else if let Some(p) = t.num(row, c_min3) {
                        set_pressure_alt(&mut f, p, AltitudeSource::PressureWindowMin);
                    } else if let Some(p) = baro {
                        set_pressure_alt(&mut f, p, AltitudeSource::PressureInstant);
                    }
                    f.temp_c = t.num(row, c_tf_tmax).or(temp).map(|v| v as f32);
                    if let Some(v) = t.num(row, c_tf_tmin) {
                        f.extra.push(("temp_min_24h".into(), format!("{v}")));
                    }
                    if let Some(v) = t.num(row, c_tf_total) {
                        f.extra.push(("tinyfox_total_vedba".into(), format!("{v}")));
                        f.activity = Some(v as f32);
                    } else {
                        f.activity = vedba.map(|v| v as f32);
                    }
                    if let Some(s) = t.num(row, c_seq) {
                        f.extra.push(("sequence_number".into(), format!("{s}")));
                    }
                    if let Some(q) = t.num(row, c_lqi) {
                        f.extra.push(("lqi".into(), format!("{q}")));
                    }
                    import_weather(t, row, &mut f);
                    tr.fixes.push(f);
                }
                _ => {}
            }
        }

        let mut tracks = b.finish();
        for tr in &mut tracks {
            tr.fixes.sort_by_key(|f| f.t);
            if tr.tag == "TinyFox" {
                let mut prev: Option<f32> = None;
                for f in &mut tr.fixes {
                    let cur = f.activity;
                    f.activity = match (prev, cur) {
                        (Some(p), Some(c)) if c >= p => Some(c - p),
                        _ => None,
                    };
                    if cur.is_some() {
                        prev = cur;
                    }
                }
            } else {
                // NanoFox: 3 h minimum pressure from separate sensor rows.
                pressure_from_samples(tr, 3 * 3600);
            }
        }
        if skipped > 0 {
            out.warnings.push(format!(
                "{}: skipped {skipped} rows without a timestamp",
                t.name
            ));
        }
        out.tracks = tracks;
        Ok(out)
    }
}

impl Firmware for MovebankGps {
    fn id(&self) -> &'static str {
        "movebank-gps"
    }
    fn label(&self) -> &'static str {
        "Movebank · GPS"
    }
    fn description(&self) -> &'static str {
        "Movebank GPS export: location-lat/long, height-above-msl/ellipsoid, ground-speed, heading, gps-hdop"
    }
    fn detect(&self, t: &Table) -> f32 {
        if !has_movebank_core(t) {
            return 0.0;
        }
        if is_sigfox(t) {
            return 0.2;
        }
        let gpsish = t
            .norm
            .iter()
            .any(|n| n.starts_with("gps_") || n.starts_with("height_above"));
        if gpsish { 0.85 } else { 0.7 }
    }

    fn parse(&self, t: &Table) -> Result<ParseOutput, String> {
        let c_ts = t.col(&["timestamp"]);
        let c_lat = t.col(&["location_lat"]);
        let c_lon = t.col(&["location_long", "location_lon"]);
        let c_ind = t.col(&["individual_local_identifier"]);
        let c_tag = t.col(&["tag_local_identifier"]);
        let c_sp = t.col(&["individual_taxon_canonical_name"]);
        let c_alt = t.col(&[
            "height_above_msl",
            "height_above_mean_sea_level",
            "height_above_ellipsoid",
            "height_raw",
        ]);
        let c_speed = t.col(&["ground_speed"]);
        let c_head = t.col(&["heading"]);
        let c_hdop = t.col(&["gps_hdop"]);
        let c_err = t.col(&[
            "location_error_numerical",
            "gps_horizontal_accuracy_estimate",
        ]);
        let c_sats = t.col(&["gps_satellite_count"]);
        let c_temp = t.col(&["external_temperature", "eobs_temperature", "temperature"]);
        let c_baro = t.col(&["barometric_pressure", "barometric_height"]);
        let c_vis = t.col(&["visible"]);
        let c_outlier = t.col(&["manually_marked_outlier", "algorithm_marked_outlier"]);

        let fallback = file_stem(&t.name);
        let mut b = TrackBuilder::new("GPS");
        let mut out = ParseOutput::default();
        let mut dropped = 0;
        for row in &t.rows {
            if t.get(row, c_vis).eq_ignore_ascii_case("false")
                || t.get(row, c_outlier).eq_ignore_ascii_case("true")
            {
                dropped += 1;
                continue;
            }
            let (Some(ts), Some(la), Some(lo)) = (
                parse_time(t.get(row, c_ts)),
                t.num(row, c_lat),
                t.num(row, c_lon),
            ) else {
                continue;
            };
            let id = id_of(t, row, c_ind, c_tag, &fallback);
            let tr = b.track(&id);
            if tr.species.is_none() {
                let sp = t.get(row, c_sp).trim();
                if !sp.is_empty() {
                    tr.species = Some(sp.to_string());
                }
            }
            let mut f = Fix::new(ts, la, lo, LocationSource::Gps);
            if let Some(a) = t.num(row, c_alt) {
                f.alt_m = Some(a as f32);
                f.alt_source = Some(AltitudeSource::Gps);
            }
            if let Some(p) = t.num(row, c_baro)
                && f.alt_m.is_none()
            {
                set_pressure_alt(&mut f, p, AltitudeSource::PressureInstant);
            }
            f.reported_speed_ms = t.num(row, c_speed).map(|v| v as f32);
            f.reported_heading = t.num(row, c_head).map(|v| v as f32);
            // ~5 m user-equivalent range error per unit HDOP.
            f.error_m = t
                .num(row, c_err)
                .or_else(|| t.num(row, c_hdop).map(|h| h * 5.0))
                .map(|v| v as f32);
            f.temp_c = t.num(row, c_temp).map(|v| v as f32);
            if let Some(s) = t.num(row, c_sats) {
                f.extra.push(("gps_satellite_count".into(), format!("{s}")));
            }
            import_weather(t, row, &mut f);
            tr.fixes.push(f);
        }
        if dropped > 0 {
            out.warnings.push(format!(
                "{}: dropped {dropped} rows flagged invisible/outlier",
                t.name
            ));
        }
        out.tracks = b.finish();
        Ok(out)
    }
}
