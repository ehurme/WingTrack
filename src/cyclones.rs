//! Tropical cyclones (hurricanes, typhoons) from IBTrACS — NOAA NCEI's International Best
//! Track Archive for Climate Stewardship, the global best-track record (positions every
//! 3–6 h, maximum wind, central pressure, Saffir–Simpson category).
//!
//! Two ways in: a subset for the tracks' time window and area from NOAA AOML's ERDDAP
//! server (`IBTrACS_since1980_1`, CSV), or an IBTrACS CSV downloaded from NCEI
//! (`ibtracs.last3years.list.v04r01.csv`, per-basin or since-1980 files) and imported.
//! Both have a units row under the header, which is skipped.

use crate::table::Table;
use egui::Color32;

pub const ERDDAP: &str = "https://erddap.aoml.noaa.gov/hdb/erddap/tabledap/IBTrACS_since1980_1.csv";
pub const NCEI_PAGE: &str = "https://www.ncei.noaa.gov/products/international-best-track-archive";
const KT: f32 = 0.514_444;

#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub t: i64,
    pub lat: f64,
    pub lon: f64,
    /// Maximum sustained wind (kt): US agencies' value, else WMO.
    pub wind_kt: Option<f32>,
    pub pres_hpa: Option<f32>,
    /// Saffir–Simpson: −5…−1 not a hurricane (−1 TD, 0 TS), 1–5 category.
    pub sshs: Option<i32>,
    pub nature: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Storm {
    pub sid: String,
    pub name: String,
    pub basin: String,
    pub pts: Vec<Point>,
}

impl Storm {
    pub fn time_range(&self) -> (i64, i64) {
        (self.pts[0].t, self.pts[self.pts.len() - 1].t)
    }
    pub fn max_wind_kt(&self) -> Option<f32> {
        self.pts.iter().filter_map(|p| p.wind_kt).reduce(f32::max)
    }
    pub fn peak_category(&self) -> Option<i32> {
        self.pts.iter().filter_map(|p| p.sshs).max()
    }
    pub fn label(&self) -> String {
        let n = if self.name.is_empty() || self.name == "NOT_NAMED" {
            "unnamed".to_string()
        } else {
            title_case(&self.name)
        };
        let y = self
            .pts
            .first()
            .map(|p| crate::table::fmt_date(p.t)[..4].to_string())
            .unwrap_or_default();
        format!("{n} {y}")
    }
    /// Interpolated position and intensity at `t` (inside the storm's lifetime).
    pub fn at(&self, t: i64) -> Option<Point> {
        let (a, b) = self.time_range();
        if t < a || t > b {
            return None;
        }
        let i = self
            .pts
            .partition_point(|p| p.t <= t)
            .max(1)
            .min(self.pts.len() - 1);
        let (p, q) = (&self.pts[i - 1], &self.pts[i]);
        let w = if q.t > p.t {
            (t - p.t) as f64 / (q.t - p.t) as f64
        } else {
            0.0
        };
        let (lat, lon) = crate::geo::interpolate(p.lat, p.lon, q.lat, q.lon, w);
        let mix = |x: Option<f32>, y: Option<f32>| match (x, y) {
            (Some(x), Some(y)) => Some(x + (y - x) * w as f32),
            (x, y) => x.or(y),
        };
        Some(Point {
            t,
            lat,
            lon,
            wind_kt: mix(p.wind_kt, q.wind_kt),
            pres_hpa: mix(p.pres_hpa, q.pres_hpa),
            sshs: if w < 0.5 { p.sshs } else { q.sshs },
            nature: if w < 0.5 {
                p.nature.clone()
            } else {
                q.nature.clone()
            },
        })
    }
}

fn title_case(s: &str) -> String {
    s.split(['_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Category label and colour (Saffir–Simpson; wind used when the code is missing).
pub fn category(sshs: Option<i32>, wind_kt: Option<f32>, nature: &str) -> (String, Color32) {
    let s = sshs.or_else(|| {
        wind_kt.map(|w| match w {
            w if w >= 137.0 => 5,
            w if w >= 113.0 => 4,
            w if w >= 96.0 => 3,
            w if w >= 83.0 => 2,
            w if w >= 64.0 => 1,
            w if w >= 34.0 => 0,
            _ => -1,
        })
    });
    if nature == "ET" && s.is_none_or(|s| s < 1) {
        return ("extratropical".into(), Color32::from_rgb(170, 170, 200));
    }
    match s {
        Some(5) => ("Cat 5".into(), Color32::from_rgb(196, 100, 217)),
        Some(4) => ("Cat 4".into(), Color32::from_rgb(255, 96, 96)),
        Some(3) => ("Cat 3".into(), Color32::from_rgb(255, 143, 32)),
        Some(2) => ("Cat 2".into(), Color32::from_rgb(255, 216, 33)),
        Some(1) => ("Cat 1".into(), Color32::from_rgb(255, 255, 204)),
        Some(0) => ("tropical storm".into(), Color32::from_rgb(0, 250, 244)),
        Some(-1) => (
            "tropical depression".into(),
            Color32::from_rgb(94, 186, 255),
        ),
        _ => (
            "disturbance / other".into(),
            Color32::from_rgb(190, 190, 190),
        ),
    }
}

pub fn kt_to_ms(kt: f32) -> f32 {
    kt * KT
}

/// ERDDAP subset URL: points within the time window (± a margin) and box.
pub fn erddap_url(t0: i64, t1: i64, s: f64, n: f64, w: f64, e: f64) -> String {
    let iso = |t: i64| {
        chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default()
    };
    format!(
        "{ERDDAP}?sid,name,basin,time,latitude,longitude,usa_wind,wmo_wind,usa_pres,wmo_pres,usa_sshs,nature\
         &time%3E={}&time%3C={}&latitude%3E={s:.2}&latitude%3C={n:.2}&longitude%3E={w:.2}&longitude%3C={e:.2}",
        iso(t0),
        iso(t1)
    )
}

/// Parse IBTrACS CSV (NCEI file or ERDDAP subset) into storms, optionally keeping only
/// points within [t0, t1].
pub fn parse(name: &str, bytes: &[u8], window: Option<(i64, i64)>) -> Result<Vec<Storm>, String> {
    let t = Table::from_bytes(name, bytes)?;
    let c_sid = t
        .col(&["sid"])
        .ok_or("not an IBTrACS table (no SID column)")?;
    let c_name = t.col(&["name"]);
    let c_basin = t.col(&["basin"]);
    let c_t = t
        .col(&["iso_time", "time"])
        .ok_or("IBTrACS table without time")?;
    let c_lat = t
        .col(&["lat", "latitude", "usa_lat"])
        .ok_or("IBTrACS table without latitude")?;
    let c_lon = t
        .col(&["lon", "longitude", "usa_lon"])
        .ok_or("IBTrACS table without longitude")?;
    let (c_uw, c_ww) = (t.col(&["usa_wind"]), t.col(&["wmo_wind"]));
    let (c_up, c_wp) = (t.col(&["usa_pres"]), t.col(&["wmo_pres"]));
    let c_ss = t.col(&["usa_sshs"]);
    let c_nat = t.col(&["nature"]);
    let mut by: std::collections::BTreeMap<String, Storm> = Default::default();
    for row in &t.rows {
        // the units row ("degrees_north", "kts") has no numeric latitude and is skipped
        let (Some(lat), Some(lon)) = (t.num(row, Some(c_lat)), t.num(row, Some(c_lon))) else {
            continue;
        };
        let Some(ts) = crate::table::parse_time(t.get(row, Some(c_t))) else {
            continue;
        };
        if window.is_some_and(|(a, b)| ts < a || ts > b) {
            continue;
        }
        let pos = |c: Option<usize>| t.num(row, c).filter(|v| *v > 0.0).map(|v| v as f32);
        let sid = t.get(row, Some(c_sid)).trim().to_string();
        let st = by.entry(sid.clone()).or_insert_with(|| Storm {
            sid,
            name: t.get(row, c_name).trim().to_string(),
            basin: t.get(row, c_basin).trim().to_string(),
            pts: Vec::new(),
        });
        st.pts.push(Point {
            t: ts,
            lat,
            lon: if lon > 180.0 { lon - 360.0 } else { lon },
            wind_kt: pos(c_uw).or(pos(c_ww)),
            pres_hpa: pos(c_up).or(pos(c_wp)),
            sshs: t.num(row, c_ss).filter(|v| *v > -10.0).map(|v| v as i32),
            nature: t.get(row, c_nat).trim().to_string(),
        });
    }
    let mut v: Vec<Storm> = by.into_values().filter(|s| !s.pts.is_empty()).collect();
    for s in &mut v {
        s.pts.sort_by_key(|p| p.t);
        s.pts.dedup_by_key(|p| p.t);
    }
    if v.is_empty() {
        return Err("no cyclone positions in the table (or none in the time window)".into());
    }
    Ok(v)
}

/// Nearest cyclone to (lat, lon) at time `t` within `max_km`: (storm index, km, state).
pub fn nearest(
    storms: &[Storm],
    lat: f64,
    lon: f64,
    t: i64,
    max_km: f64,
) -> Option<(usize, f64, Point)> {
    storms
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let p = s.at(t)?;
            let d = crate::geo::haversine_m(lat, lon, p.lat, p.lon) / 1000.0;
            (d <= max_km).then_some((i, d, p))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NCEI: &str = "SID,SEASON,NUMBER,BASIN,SUBBASIN,NAME,ISO_TIME,NATURE,LAT,LON,WMO_WIND,WMO_PRES,WMO_AGENCY,TRACK_TYPE,DIST2LAND,LANDFALL,IFLAG,USA_AGENCY,USA_ATCF_ID,USA_LAT,USA_LON,USA_RECORD,USA_STATUS,USA_WIND,USA_PRES,USA_SSHS\n\
 ,Year, , , , ,degrees_north,degrees_east,kts,mb, , ,km,km, , , ,degrees_north,degrees_east, , ,kts,mb,1\n\
2024252N13314,2024,74,NA,NA,FRANCINE,2024-09-09 12:00:00,TS,22.0,-94.0,55,990,hurdat_atl,main,100,100,O__,hurdat_atl,AL062024,22.0,-94.0, ,TS,55,990,0\n\
2024252N13314,2024,74,NA,NA,FRANCINE,2024-09-11 18:00:00,TS,28.6,-91.4,90,972,hurdat_atl,main,0,0,O__,hurdat_atl,AL062024,28.6,-91.4,L,HU,90,972,2\n\
2024252N13314,2024,74,NA,NA,FRANCINE,2024-09-12 00:00:00,TS,29.5,-90.9,75,980,hurdat_atl,main,0,0,O__,hurdat_atl,AL062024,29.5,-90.9, ,HU,75,980,1\n";

    #[test]
    fn parse_ncei_and_interpolate() {
        let v = parse("ibtracs.csv", NCEI.as_bytes(), None).unwrap();
        assert_eq!(v.len(), 1);
        let s = &v[0];
        assert_eq!((s.label().as_str(), s.pts.len()), ("Francine 2024", 3));
        assert_eq!(s.peak_category(), Some(2));
        assert_eq!(s.max_wind_kt(), Some(90.0));
        let t = crate::table::parse_time("2024-09-11 21:00:00").unwrap();
        let p = s.at(t).unwrap();
        assert!((p.lat - 29.05).abs() < 0.05 && (p.wind_kt.unwrap() - 82.5).abs() < 0.1);
        assert_eq!(category(Some(2), None, "TS").0, "Cat 2");
        assert_eq!(category(None, Some(40.0), "").0, "tropical storm");
        // a bird 120 km east of the storm at that time
        let (i, d, _) = nearest(&v, 29.05, -89.8, t, 500.0).unwrap();
        assert_eq!(i, 0);
        assert!((d - 120.0).abs() < 15.0, "{d}");
        assert!(
            nearest(&v, 29.05, -89.8, t + 86_400 * 3, 500.0).is_none(),
            "storm over"
        );
        // time window filter
        let w = parse("x.csv", NCEI.as_bytes(), Some((t, t + 86_400))).unwrap();
        assert_eq!(w[0].pts.len(), 1);
    }

    #[test]
    fn parse_erddap_subset() {
        let csv = "sid,name,basin,time,latitude,longitude,usa_wind,wmo_wind,usa_pres,wmo_pres,usa_sshs,nature\n\
                   ,,,UTC,degrees_north,degrees_east,kts,kts,mb,mb,1,\n\
                   2024255N13336,GORDON,NA,2024-09-12T00:00:00Z,17.5,-33.4,35,35,1005,1005,0,TS\n\
                   2024255N13336,GORDON,NA,2024-09-12T06:00:00Z,18.0,-34.6,NaN,35,NaN,1006,0,TS\n";
        let v = parse("erddap.csv", csv.as_bytes(), None).unwrap();
        assert_eq!(v[0].pts.len(), 2);
        assert_eq!(
            v[0].pts[1].wind_kt,
            Some(35.0),
            "WMO wind when US value is missing"
        );
        let u = erddap_url(1_726_000_000, 1_727_000_000, 30.0, 60.0, -20.0, 30.0);
        assert!(u.contains("time%3E=2024-09-10T20:26:40Z") && u.contains("latitude%3C=60.00"));
    }
}
