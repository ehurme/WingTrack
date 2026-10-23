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
