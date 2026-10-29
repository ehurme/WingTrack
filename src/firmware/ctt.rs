//! Cellular Tracking Technologies (CTT) GPS units (e.g. CTT Series GPS-GSM/LTE tags),
//! as downloaded from the CTT web portal ("Download" tab) or the older CTT exports.
//!
//! The portal's data grid lists: Time of Fix, Lat, Lng, Altitude (m), Speed (knots),
//! COG, HDOP/VDOP/PDOP, Satellite Count, Fix Time (s), Error, Nav Type, Activity Index,
//! Battery (mV), Charge, Solar Current, Temp (°C), Cellular Connection, id (imei), alias.
//! Older exports used `GPS_YYYY-MM-DD_HH:MM:SS` or separate `UTC_Date`/`UTC_Time` columns.
//! Column matching is tolerant; speed is assumed to be in knots unless the header says km/h or m/s.

use super::*;
use crate::model::LocationSource;
use crate::table::parse_time;

pub struct CttGps;

const CTT_TIME: &[&str] = &[
    "gps_yyyy_mm_dd_hh_mm_ss",
    "time_of_fix",
    "gps_time",
    "gps_datetime",
    "utc_datetime",
    "fix_time_utc",
    "datetime",
    "timestamp",
];
const CTT_ID: &[&str] = &[
    "imei",
    "id_imei",
    "id",
    "alias",
    "device_id",
    "transmitter",
    "serial",
    "tag_id",
];

fn ctt_markers(t: &Table) -> usize {
    let mut n = 0;
    for m in [
        &["hdop"][..],
        &["vdop"],
        &["pdop"],
        &["cog", "course"],
        &["satellite_count", "satellites", "sats", "nsats"],
        &["nav_type"],
        &["activity_index"],
        &["imei", "id_imei"],
        &["alias"],
        &["fix_time"],
        &["solar_current"],
        &["cellular_connection"],
    ] {
        if t.has(m) {
            n += 1;
        }
    }
    if t.has(&["gps_yyyy_mm_dd_hh_mm_ss"]) {
        n += 5;
    }
    n
}

impl Firmware for CttGps {
    fn id(&self) -> &'static str {
        "ctt-gps"
    }
    fn label(&self) -> &'static str {
        "CTT · cellular GPS"
    }
    fn description(&self) -> &'static str {
        "Cellular Tracking Technologies GPS export: Time of Fix, Lat/Lng, Altitude, Speed (kn), COG, HDOP, satellites"
    }
    fn detect(&self, t: &Table) -> f32 {
        let has_pos = t.has(&["lat", "latitude"]) && t.has(&["lng", "lon", "long", "longitude"]);
        if !has_pos {
            return 0.0;
        }
        match ctt_markers(t) {
            0 | 1 => 0.0,
            2 => 0.5,
            3 => 0.75,
            _ => 0.9,
        }
    }

    fn parse(&self, t: &Table) -> Result<ParseOutput, String> {
        let c_time = t.col(CTT_TIME);
        let c_date = t.col(&["utc_date", "date", "gps_date"]);
        let c_tod = t.col(&["utc_time", "time", "gps_time_of_day"]);
        let c_lat = t.col(&["lat", "latitude"]);
        let c_lon = t.col(&["lng", "lon", "long", "longitude"]);
        let c_alt = t.col(&["altitude_m", "altitude", "alt", "height"]);
        let c_speed_kmh = t.col(&["speed_km_h", "speed_kmh"]);
        let c_speed_ms = t.col(&["speed_m_s", "speed_ms"]);
        let c_speed_kn = t.col(&["speed_knots", "speed_kn", "speed"]);
        let c_cog = t.col(&["cog", "course", "heading"]);
        let c_hdop = t.col(&["hdop"]);
        let c_sats = t.col(&["satellite_count", "satellites", "sats", "nsats"]);
        let c_temp = t.col(&["temp", "temperature", "temperature_c", "temp_c"]);
        let c_batt = t.col(&["battery", "battery_mv", "battery_v", "voltage"]);
        let c_act = t.col(&["activity_index", "activity"]);
        let c_nav = t.col(&["nav_type"]);
        let c_err = t.col(&["error"]);
        let c_id = t.col(CTT_ID);

        if c_time.is_none() && c_date.is_none() {
            return Err(format!("{}: no CTT time column found", t.name));
        }
        let fallback = file_stem(&t.name);
        let mut b = TrackBuilder::new("CTT GPS");
        let mut out = ParseOutput::default();
        let (mut no_fix, mut bad) = (0, 0);
        for row in &t.rows {
            let ts = parse_time(t.get(row, c_time)).or_else(|| {
                let s = format!("{} {}", t.get(row, c_date), t.get(row, c_tod));
                parse_time(&s)
            });
            let Some(ts) = ts else {
                bad += 1;
                continue;
            };
            // Nav type 1 = no fix; error codes ≠ 0 are failed fixes.
            if t.num(row, c_nav) == Some(1.0) || t.num(row, c_err).is_some_and(|e| e != 0.0) {
                no_fix += 1;
                continue;
            }
            let (Some(la), Some(lo)) = (t.num(row, c_lat), t.num(row, c_lon)) else {
                no_fix += 1;
                continue;
            };
            if la == 0.0 && lo == 0.0 {
                no_fix += 1;
                continue;
            }
            let id = {
                let s = t.get(row, c_id).trim();
                if s.is_empty() {
                    fallback.clone()
                } else {
                    s.to_string()
                }
            };
            let tr = b.track(&id);
            let mut f = Fix::new(ts, la, lo, LocationSource::Gps);
            if let Some(a) = t.num(row, c_alt) {
                f.alt_m = Some(a as f32);
                f.alt_source = Some(AltitudeSource::Gps);
            }
            f.reported_speed_ms = t
                .num(row, c_speed_ms)
                .or_else(|| t.num(row, c_speed_kmh).map(|v| v / 3.6))
                .or_else(|| t.num(row, c_speed_kn).map(|v| v * 0.514_444))
                .map(|v| v as f32);
            f.reported_heading = t.num(row, c_cog).map(|v| v as f32);
            f.error_m = t.num(row, c_hdop).map(|h| (h * 5.0) as f32);
            f.temp_c = t.num(row, c_temp).map(|v| v as f32);
            f.activity = t.num(row, c_act).map(|v| v as f32);
            if let Some(s) = t.num(row, c_sats) {
                f.extra.push(("satellites".into(), format!("{s}")));
            }
            if let Some(bv) = t.num(row, c_batt) {
                f.extra.push(("battery".into(), format!("{bv}")));
            }
            import_weather(t, row, &mut f);
            tr.fixes.push(f);
        }
        if no_fix + bad > 0 {
            out.warnings.push(format!(
                "{}: skipped {no_fix} rows without a valid fix, {bad} with bad time",
                t.name
            ));
        }
        out.tracks = b.finish();
        Ok(out)
    }
}

/// CTT detection exports (`*_detections_export*.csv`): one row per time a tag was heard,
/// located at the receiver — a CTT node / SensorStation (`receiver_id`) or a crowd-sourced
/// phone (`source = crowd_sourced`). Columns: `source, receiver_id, tag_id, time_utc, lat, lon,
/// conf, rssi, payload`.
///
/// The positions are where the *receiver* was, so fixes are marked as station locations.
/// Detections of the same tag in the same second (heard by several receivers) are merged into
/// one fix at their mean position. `conf`, `rssi`, `source`, `receiver_id` and `payload` are kept
/// as extra columns; `conf` is not interpreted as a location error.
pub struct CttDetections;

impl Firmware for CttDetections {
    fn id(&self) -> &'static str {
        "ctt-detections"
    }
    fn label(&self) -> &'static str {
        "CTT · detections (receivers / crowd-sourced)"
    }
    fn description(&self) -> &'static str {
        "CTT detection export: tag_id, time_utc, receiver/phone lat & lon, conf, rssi, source, receiver_id"
    }
    fn detect(&self, t: &Table) -> f32 {
        let core = t.has(&["tag_id"])
            && t.has(&["time_utc", "time"])
            && t.has(&["lat", "latitude"])
            && t.has(&["lon", "lng", "longitude"]);
        if !core {
            return 0.0;
        }
        let markers = [
            &["conf"][..],
            &["rssi"],
            &["receiver_id"],
            &["payload"],
            &["source"],
        ]
        .iter()
        .filter(|m| t.has(m))
        .count();
        match markers {
            0 => 0.0,
            1 => 0.5,
            2 => 0.8,
            _ => 0.94,
        }
    }

    fn parse(&self, t: &Table) -> Result<ParseOutput, String> {
        let c_tag = t.col(&["tag_id"]);
        let c_time = t
            .col(&["time_utc", "time"])
            .ok_or("missing time_utc column")?;
        let c_lat = t.col(&["lat", "latitude"]);
        let c_lon = t.col(&["lon", "lng", "longitude"]);
        let c_src = t.col(&["source"]);
        let c_rec = t.col(&["receiver_id"]);
        let c_conf = t.col(&["conf"]);
        let c_rssi = t.col(&["rssi"]);
        let c_payload = t.col(&["payload"]);

        struct Det {
            lat: f64,
            lon: f64,
            conf: Option<f64>,
            rssi: Option<f64>,
            source: String,
            receiver: String,
            payload: String,
        }
        // (tag, time) -> detections, in first-seen tag order
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::BTreeMap<(String, i64), Vec<Det>> = Default::default();
        let (mut no_pos, mut bad_time) = (0usize, 0usize);
        let fallback = file_stem(&t.name);
        for row in &t.rows {
            let Some(ts) = parse_time(t.get(row, Some(c_time))) else {
                bad_time += 1;
                continue;
            };
            let (Some(la), Some(lo)) = (t.num(row, c_lat), t.num(row, c_lon)) else {
                no_pos += 1;
                continue;
            };
            if (la == 0.0 && lo == 0.0) || la.abs() > 90.0 || lo.abs() > 180.0 {
                no_pos += 1;
                continue;
            }
            let tag = {
                let s = t.get(row, c_tag).trim();
                if s.is_empty() {
                    fallback.clone()
                } else {
                    s.to_string()
                }
            };
            if !order.contains(&tag) {
                order.push(tag.clone());
            }
            groups.entry((tag, ts)).or_default().push(Det {
                lat: la,
                lon: lo,
                conf: t.num(row, c_conf),
                rssi: t.num(row, c_rssi),
                source: t.get(row, c_src).trim().to_string(),
                receiver: t.get(row, c_rec).trim().to_string(),
                payload: t.get(row, c_payload).trim().to_string(),
            });
        }

        let mut b = TrackBuilder::new("CTT detections");
        for id in &order {
            b.track(id);
        }
        let mut merged = 0usize;
        for ((tag, ts), dets) in groups {
            let n = dets.len();
            merged += n - 1;
            let lat = dets.iter().map(|d| d.lat).sum::<f64>() / n as f64;
            let lon = dets.iter().map(|d| d.lon).sum::<f64>() / n as f64;
            let mut f = Fix::new(ts, lat, lon, LocationSource::Station);
            let join = |v: Vec<String>| {
                let mut v: Vec<String> = v.into_iter().filter(|s| !s.is_empty()).collect();
                v.sort();
                v.dedup();
                v.join(" ")
            };
            let src = join(dets.iter().map(|d| d.source.clone()).collect());
            let rec = join(dets.iter().map(|d| d.receiver.clone()).collect());
            let payload = join(dets.iter().map(|d| d.payload.clone()).collect());
            if !src.is_empty() {
                f.extra.push(("detection_source".into(), src));
            }
            if !rec.is_empty() {
                f.extra.push(("receiver_id".into(), rec));
            }
            if let Some(c) = dets.iter().filter_map(|d| d.conf).reduce(f64::max) {
                f.extra.push(("conf".into(), format!("{c}")));
            }
            if let Some(r) = dets.iter().filter_map(|d| d.rssi).reduce(f64::max) {
                f.extra.push(("rssi_max".into(), format!("{r}")));
            }
            if !payload.is_empty() {
                f.extra.push(("payload".into(), payload));
            }
            if n > 1 {
                f.extra.push(("detections_merged".into(), n.to_string()));
            }
            b.track(&tag).fixes.push(f);
        }
        let mut out = ParseOutput {
            tracks: b.finish(),
            ..Default::default()
        };
        if merged > 0 {
            out.warnings.push(format!(
                "{}: merged {merged} same-second detections (heard by several receivers) into their mean position",
                t.name
            ));
        }
        if no_pos + bad_time > 0 {
            out.warnings.push(format!(
                "{}: skipped {no_pos} detections without a receiver position, {bad_time} with bad time",
                t.name
            ));
        }
        Ok(out)
    }
}
