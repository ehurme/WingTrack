//! Raw Wildcloud (ICARUS / MPI-AB Sigfox) CSV exports.
//!
//! Common columns: `Device`, `Time (UTC)` ("14.06.2025, 17:56:24"), `Position` ("lat, lon"),
//! `Radius (m) (Source/Status)` ("7000 (2/1)"), `Sequence Number`, `LQI`, `Link Quality`,
//! `Base Stations (ID, RSSI, Reps)`, plus firmware-specific sensor columns:
//!
//! | Variant | Sensor columns |
//! |---|---|
//! | TinyFox (V13/V13P/V14P/TV1/BBV6/BBX5) | `Total VeDBA` (cumulative), `24h Min./Max. Temperature`, `24h Active (%)`, `24h Min. Pressure (mbar)` |
//! | NanoFox 30Days / DailyVeDBA / 10Day | `VeDBA sum N min ago` ×5, `Average temperature N min ago` ×5, `Min pressure of last 3 hrs (mbar)` |
//! | NanoFox 30DaysFineScalePressure | `VeDBA sum N min ago` ×5, `Pressure N min ago` ×5, `Min/Max temperature of last 3 hrs (°C)` |
//!
//! Column-name variants are matched the same way as `harmonize_wc_columns()` in SigfoxTagPrep.

use super::*;
use crate::model::LocationSource;
use crate::table::{parse_num, parse_time};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WildcloudVariant {
    TinyFox,
    NanoFox30Days,
    NanoFoxFineScalePressure,
}

pub struct Wildcloud {
    v: WildcloudVariant,
}

impl Wildcloud {
    pub fn new(v: WildcloudVariant) -> Self {
        Wildcloud { v }
    }
}

/// Length of a NanoFox VeDBA / temperature bin (minutes).
const NANOFOX_BIN_MIN: i64 = 36;

fn is_wildcloud(t: &Table) -> bool {
    let has_time = t.has(&["time_utc", "timestamp_utc"]);
    let has_wc_cols = t.has(&["position", "radius_m_source_status", "radius_m"])
        || t.has_prefix("base_stations")
        || t.has(&["raw_data"]);
    has_time && has_wc_cols
}

fn bins(t: &Table, prefix: &str) -> Vec<(usize, i64)> {
    // e.g. "vedba_sum_36_min_ago" or, with a unit suffix,
    // "VeDBA sum 36 min ago (m/s²)" → "vedba_sum_36_min_ago_m_s" → (col, 36)
    t.cols_where(|n| n.starts_with(prefix) && n.contains("_min_ago"))
        .into_iter()
        .filter_map(|c| {
            let mins = t.norm[c]
                .split('_')
                .find_map(|tok| tok.parse::<i64>().ok())?;
            Some((c, mins))
        })
        .collect()
}

fn markers(t: &Table) -> (bool, bool, bool) {
    let tiny = t.has(&["total_vedba"]) || t.cols_where(|n| n.starts_with("24h_")).len() >= 2;
    let vedba_bins = !bins(t, "vedba").is_empty();
    let pres_bins = !bins(t, "pressure").is_empty();
    (tiny, vedba_bins, pres_bins)
}

/// "7000 (2/1)" → 7000
fn parse_radius(s: &str) -> Option<f64> {
    let lead: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    parse_num(&lead)
}

/// Strongest RSSI from "id,rssi,reps;id,rssi,reps" (same logic as `extract_max_rssi()`).
fn max_rssi(s: &str) -> Option<f64> {
    s.replace('|', ";")
        .split(';')
        .filter_map(|st| st.split(',').nth(1).and_then(parse_num))
        .reduce(f64::max)
}

impl Firmware for Wildcloud {
    fn id(&self) -> &'static str {
        match self.v {
            WildcloudVariant::TinyFox => "wildcloud-tinyfox",
            WildcloudVariant::NanoFox30Days => "wildcloud-nanofox-30days",
            WildcloudVariant::NanoFoxFineScalePressure => "wildcloud-nanofox-fsp",
        }
    }
    fn label(&self) -> &'static str {
        match self.v {
            WildcloudVariant::TinyFox => "Wildcloud · TinyFox",
            WildcloudVariant::NanoFox30Days => "Wildcloud · NanoFox 30Days",
            WildcloudVariant::NanoFoxFineScalePressure => "Wildcloud · NanoFox FineScalePressure",
        }
    }
    fn description(&self) -> &'static str {
        match self.v {
            WildcloudVariant::TinyFox => {
                "Daily Sigfox messages: cumulative VeDBA, 24 h activity %, 24 h min/max temperature, 24 h min pressure"
            }
            WildcloudVariant::NanoFox30Days => {
                "~3 h Sigfox messages: 5×36-min VeDBA and temperature bins, 3 h minimum pressure"
            }
            WildcloudVariant::NanoFoxFineScalePressure => {
                "~3 h Sigfox messages: 5×36-min VeDBA bins and 5 instantaneous pressure readings"
            }
        }
    }

    fn detect(&self, t: &Table) -> f32 {
        if !is_wildcloud(t) {
            return 0.0;
        }
        let (tiny, vedba_bins, pres_bins) = markers(t);
        match self.v {
            WildcloudVariant::TinyFox if tiny => 0.95,
            WildcloudVariant::NanoFoxFineScalePressure if pres_bins => 0.96,
            WildcloudVariant::NanoFox30Days if vedba_bins && !pres_bins => 0.95,
            // Unknown sensor layout: positions still parse with the 30Days profile.
            WildcloudVariant::NanoFox30Days if !tiny && !pres_bins => 0.6,
            _ => 0.05,
        }
    }

    fn parse(&self, t: &Table) -> Result<ParseOutput, String> {
        let c_dev = t.col(&["device", "device_id", "tag_id"]);
        let c_time = t
            .col(&["time_utc", "timestamp_utc"])
            .ok_or("missing 'Time (UTC)' column")?;
        let c_pos = t.col(&["position"]);
        let c_lat = t.col(&["latitude", "lat"]);
        let c_lon = t.col(&["longitude", "lon", "long"]);
        let c_rad = t.col(&["radius_m_source_status", "radius_m", "location_radius"]);
        let c_seq = t.col(&["sequence_number", "seq_number", "seq"]);
        let c_lqi = t.col(&["lqi", "link_quality"]);
        let c_bs = t.col_prefix("base_stations");

        let tag = match self.v {
            WildcloudVariant::TinyFox => "TinyFox",
            _ => "NanoFox",
        };
        let fallback_id = file_stem(&t.name);
        let mut b = TrackBuilder::new(tag);
        let mut out = ParseOutput::default();
        let mut bad_time = 0usize;

        // Variant-specific columns.
        let vedba_bins = bins(t, "vedba");
        let temp_bins: Vec<_> = bins(t, "average_temperature")
            .into_iter()
            .chain(bins(t, "avg_temperature"))
            .chain(bins(t, "temperature"))
            .collect();
        let pres_bins = bins(t, "pressure");
        let c_min_p3 = t.col_prefix("min_pressure");
        let c_min_t3 = t.col_prefix("min_temperature").or(t.col_prefix("min_temp"));
        let c_max_t3 = t.col_prefix("max_temperature").or(t.col_prefix("max_temp"));
        let c_total_vedba = t.col(&["total_vedba"]);
        let c_24_act = t.col_prefix("24h_active");
        let c_24_tmin = t.col_prefix("24h_min_temp");
        let c_24_tmax = t.col_prefix("24h_max_temp");
        let c_24_pmin = t.col_prefix("24h_min_pressure");

        for row in &t.rows {
            let Some(ts) = parse_time(t.get(row, Some(c_time))) else {
                bad_time += 1;
                continue;
            };
            let id = {
                let s = t.get(row, c_dev).trim();
                if s.is_empty() {
                    fallback_id.clone()
                } else {
                    s.to_string()
                }
            };
            // Position: "lat, lon" or separate columns.
            let (lat, lon) = match c_pos.map(|c| t.get(row, Some(c))) {
                Some(p) if p.contains(',') => {
                    let mut it = p.split(',').map(parse_num);
                    (it.next().flatten(), it.next().flatten())
                }
                _ => (t.num(row, c_lat), t.num(row, c_lon)),
            };
            let tr = b.track(&id);
            let mut fix = match (lat, lon) {
                (Some(la), Some(lo))
                    if la.abs() <= 90.0 && lo.abs() <= 180.0 && !(la == 0.0 && lo == 0.0) =>
                {
                    let mut f = Fix::new(ts, la, lo, LocationSource::SigfoxAtlas);
                    f.error_m = parse_radius(t.get(row, c_rad)).map(|r| r as f32);
                    Some(f)
                }
                _ => None,
            };

            match self.v {
                WildcloudVariant::TinyFox => {
                    let total = t.num(row, c_total_vedba);
                    push_sensor(tr, ts, SensorKind::Activity, t.num(row, c_24_act));
                    push_sensor(
                        tr,
                        ts - 43_200,
                        SensorKind::Temperature,
                        t.num(row, c_24_tmin),
                    );
                    push_sensor(
                        tr,
                        ts - 43_200,
                        SensorKind::Temperature,
                        t.num(row, c_24_tmax),
                    );
                    let pmin = t.num(row, c_24_pmin);
                    push_sensor(tr, ts, SensorKind::Pressure, pmin);
                    if let Some(f) = fix.as_mut() {
                        if let Some(p) = pmin {
                            set_pressure_alt(f, p, AltitudeSource::PressureWindowMin);
                        }
                        f.temp_c = t.num(row, c_24_tmax).map(|v| v as f32);
                        if let Some(tot) = total {
                            f.extra
                                .push(("tinyfox_total_vedba".into(), format!("{tot}")));
                            f.activity = Some(tot as f32); // converted to daily diffs below
                        }
                    }
                }
                WildcloudVariant::NanoFox30Days | WildcloudVariant::NanoFoxFineScalePressure => {
                    let mut vsum = 0.0;
                    let mut nv = 0;
                    for &(c, mins) in &vedba_bins {
                        let v = t.num(row, Some(c));
                        if let Some(x) = v {
                            vsum += x;
                            nv += 1;
                        }
                        // centre of the 36-min window that ended `mins` ago
                        push_sensor(
                            tr,
                            ts - mins * 60 - NANOFOX_BIN_MIN * 30,
                            SensorKind::Vedba,
                            v,
                        );
                    }
                    for &(c, mins) in &temp_bins {
                        push_sensor(
                            tr,
                            ts - mins * 60 - NANOFOX_BIN_MIN * 30,
                            SensorKind::Temperature,
                            t.num(row, Some(c)),
                        );
                    }
                    let mut p_now = None;
                    for &(c, mins) in &pres_bins {
                        let p = t.num(row, Some(c));
                        push_sensor(tr, ts - mins * 60, SensorKind::Pressure, p);
                        if mins == 0 {
                            p_now = p;
                        }
                    }
                    let pmin3 = t.num(row, c_min_p3);
                    if pres_bins.is_empty() {
                        push_sensor(tr, ts, SensorKind::Pressure, pmin3);
                    }
                    // ">10" style range labels are not numeric → ignored.
                    push_sensor(tr, ts - 5400, SensorKind::Temperature, t.num(row, c_min_t3));
                    push_sensor(tr, ts - 5400, SensorKind::Temperature, t.num(row, c_max_t3));
                    if let Some(f) = fix.as_mut() {
                        if nv > 0 {
                            f.activity = Some(vsum as f32);
                        }
                        if let Some(p) = p_now {
                            set_pressure_alt(f, p, AltitudeSource::PressureInstant);
                        } else if let Some(p) = pmin3 {
                            set_pressure_alt(f, p, AltitudeSource::PressureWindowMin);
                        }
                        f.temp_c = temp_bins
                            .iter()
                            .min_by_key(|b| b.1)
                            .and_then(|&(c, _)| t.num(row, Some(c)))
                            .or_else(|| t.num(row, c_max_t3))
                            .map(|v| v as f32);
                    }
                }
            }

            if let Some(mut f) = fix {
                if let Some(s) = t.num(row, c_seq) {
                    f.extra.push(("sequence_number".into(), format!("{s}")));
                }
                if let Some(q) = c_lqi.map(|c| t.get(row, Some(c))).filter(|s| !s.is_empty()) {
                    f.extra.push(("lqi".into(), q.to_string()));
                }
                if let Some(r) = max_rssi(t.get(row, c_bs)) {
                    f.extra.push(("max_rssi".into(), format!("{r}")));
                }
                import_weather(t, row, &mut f);
                tr.fixes.push(f);
            }
        }

        let mut tracks = b.finish();
        for tr in &mut tracks {
            tr.fixes.sort_by_key(|f| f.t);
            if self.v == WildcloudVariant::TinyFox {
                // Cumulative total → VeDBA accumulated since the previous message.
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
            }
        }
        if bad_time > 0 {
            out.warnings.push(format!(
                "{}: skipped {bad_time} rows with unparseable time",
                t.name
            ));
        }
        out.tracks = tracks;
        Ok(out)
    }
}
