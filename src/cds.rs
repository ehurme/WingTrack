//! ERA5 from the Copernicus Climate Data Store (CDS) with the user's own API key.
//!
//! Uses the *ERA5 hourly time-series* dataset (`reanalysis-era5-single-levels-timeseries`),
//! which returns one 0.25° grid point's full hourly record as CSV, so each needed cell is one
//! job no matter how long the deployment. Jobs follow the CDS (OGC API Processes) flow:
//! submit → poll status → read the result link → download (a ZIP with CSV inside, or CSV).
//! The request quota is the user's own CDS account, independent of Open-Meteo's per-IP
//! limits. Variables the time-series dataset doesn't carry (100 m wind, cloud cover) stay
//! missing; annotation then falls back to 10 m wind.

use crate::weather::{CellKey, NV, Values, cell_center, day_str};
use std::collections::{BTreeMap, HashSet};

pub const API: &str = "https://cds.climate.copernicus.eu/api/retrieve/v1";
pub const DATASET: &str = "reanalysis-era5-single-levels-timeseries";
pub const DATASET_PAGE: &str = "https://cds.climate.copernicus.eu/datasets/reanalysis-era5-single-levels-timeseries?tab=download";
pub const PROFILE_PAGE: &str = "https://cds.climate.copernicus.eu/profile";
pub const VARS: [&str; 6] = [
    "10m_u_component_of_wind",
    "10m_v_component_of_wind",
    "2m_temperature",
    "mean_sea_level_pressure",
    "surface_pressure",
    "total_precipitation",
];
/// Split a cell's days into separate jobs across gaps longer than this (e.g. two seasons).
const MAX_GAP_DAYS: i64 = 45;

/// One CDS job: a grid cell over a day range.
#[derive(Clone, Debug, PartialEq)]
pub struct CellJob {
    pub cell: CellKey,
    pub start_day: i64,
    pub end_day: i64,
}

/// Group needed (cell, day) pairs into one job per cell and continuous stretch of days.
pub fn plan(
    points: &[(f64, f64, i64)],
    done: &HashSet<(CellKey, i64)>,
    latest_day: i64,
) -> Vec<CellJob> {
    let mut need: BTreeMap<CellKey, Vec<i64>> = BTreeMap::new();
    for &(lat, lon, t) in points {
        let k = crate::weather::cell_of(lat, lon);
        for d in [crate::weather::day_of(t), crate::weather::day_of(t + 3600)] {
            if d <= latest_day && !done.contains(&(k, d)) {
                need.entry(k).or_default().push(d);
            }
        }
    }
    let mut out = Vec::new();
    for (cell, mut days) in need {
        days.sort_unstable();
        days.dedup();
        let mut start = days[0];
        let mut prev = days[0];
        for &d in &days[1..] {
            if d - prev > MAX_GAP_DAYS {
                out.push(CellJob {
                    cell,
                    start_day: start,
                    end_day: prev,
                });
                start = d;
            }
            prev = d;
        }
        out.push(CellJob {
            cell,
            start_day: start,
            end_day: prev,
        });
    }
    out
}

fn with_key(mut r: ehttp::Request, key: &str) -> ehttp::Request {
    r.headers.insert("PRIVATE-TOKEN", key.trim());
    r
}

/// POST the job.
pub fn submit(key: &str, job: &CellJob) -> ehttp::Request {
    let (lat, lon) = cell_center(job.cell);
    let body = serde_json::json!({
        "inputs": {
            "variable": VARS,
            "location": { "latitude": lat, "longitude": lon },
            "date": [format!("{}/{}", day_str(job.start_day), day_str(job.end_day))],
            "data_format": "csv",
        }
    });
    let mut r = ehttp::Request::post(
        format!("{API}/processes/{DATASET}/execution"),
        body.to_string().into_bytes(),
    );
    r.headers.insert("Content-Type", "application/json");
    with_key(r, key)
}

pub fn status(key: &str, job_id: &str) -> ehttp::Request {
    with_key(ehttp::Request::get(format!("{API}/jobs/{job_id}")), key)
}

pub fn results(key: &str, job_id: &str) -> ehttp::Request {
    with_key(
        ehttp::Request::get(format!("{API}/jobs/{job_id}/results")),
        key,
    )
}

#[derive(Clone, Debug, PartialEq)]
pub enum JobState {
    /// Queued or running.
    Waiting,
    Done,
    Failed(String),
}

/// `(job id, state)` from a submit or status response.
pub fn parse_job(body: &str) -> Result<(String, JobState), String> {
    let j: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
    let id = j
        .get("jobID")
        .and_then(|v| v.as_str())
        .ok_or_else(|| error_text(body))?
        .to_string();
    let st = match j.get("status").and_then(|v| v.as_str()).unwrap_or("") {
        "successful" => JobState::Done,
        "failed" | "dismissed" | "deleted" => JobState::Failed(error_text(body)),
        _ => JobState::Waiting,
    };
    Ok((id, st))
}

/// Download link from a `/results` response.
pub fn parse_results(body: &str) -> Result<String, String> {
    let j: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
    j.pointer("/asset/value/href")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| error_text(body))
}

/// Human-readable message from a CDS error body (licence not accepted, bad key, ...).
pub fn error_text(body: &str) -> String {
    let j: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let pick = |k: &str| j.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let mut msg = [pick("title"), pick("detail"), pick("message")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(": ");
    if msg.is_empty() {
        msg = body.chars().take(200).collect();
    }
    if msg.to_lowercase().contains("licen") {
        msg.push_str(&format!(
            " - accept the dataset licence once at {DATASET_PAGE} (bottom of the download tab)"
        ));
    }
    msg
}

/// Extract the CSV from the downloaded payload (a ZIP of CSVs, or plain CSV).
pub fn payload_csv(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(b"PK") {
        let files = unzip(bytes)?;
        let csvs: Vec<String> = files
            .into_iter()
            .filter(|(n, _)| n.to_lowercase().ends_with(".csv"))
            .map(|(_, b)| String::from_utf8_lossy(&b).into_owned())
            .collect();
        if csvs.is_empty() {
            return Err("CDS result has no CSV inside".into());
        }
        // Several CSVs (one per grid) share the time column; merge by concatenating
        // their rows under their own headers — `parse_csv` reads each block.
        return Ok(csvs.join("\n"));
    }
    if bytes.starts_with(b"\x89HDF") || bytes.starts_with(b"CDF") {
        return Err("CDS returned NetCDF, expected CSV".into());
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// Minimal ZIP reader (stored / deflate entries) via the central directory.
fn unzip(b: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    use std::io::Read;
    let u16_at = |i: usize| {
        b.get(i..i + 2)
            .map(|s| u16::from_le_bytes([s[0], s[1]]) as usize)
    };
    let u32_at = |i: usize| {
        b.get(i..i + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    let eocd = (0..b.len().saturating_sub(21))
        .rev()
        .find(|&i| b[i..].starts_with(b"PK\x05\x06"))
        .ok_or("ZIP without central directory")?;
    let n = u16_at(eocd + 10).ok_or("bad ZIP")?;
    let mut p = u32_at(eocd + 16).ok_or("bad ZIP")?;
    let mut out = Vec::new();
    for _ in 0..n {
        if !b.get(p..).is_some_and(|s| s.starts_with(b"PK\x01\x02")) {
            return Err("bad ZIP directory".into());
        }
        let method = u16_at(p + 10).ok_or("bad ZIP")?;
        let csize = u32_at(p + 20).ok_or("bad ZIP")?;
        let (nl, xl, cl) = (
            u16_at(p + 28).ok_or("bad ZIP")?,
            u16_at(p + 30).ok_or("bad ZIP")?,
            u16_at(p + 32).ok_or("bad ZIP")?,
        );
        let lh = u32_at(p + 42).ok_or("bad ZIP")?;
        let name =
            String::from_utf8_lossy(b.get(p + 46..p + 46 + nl).ok_or("bad ZIP")?).into_owned();
        p += 46 + nl + xl + cl;
        let data_at =
            lh + 30 + u16_at(lh + 26).ok_or("bad ZIP")? + u16_at(lh + 28).ok_or("bad ZIP")?;
        let raw = b.get(data_at..data_at + csize).ok_or("truncated ZIP")?;
        let data = match method {
            0 => raw.to_vec(),
            8 => {
                let mut v = Vec::new();
                flate2::read::DeflateDecoder::new(raw)
                    .read_to_end(&mut v)
                    .map_err(|e| format!("ZIP inflate: {e}"))?;
                v
            }
            m => return Err(format!("unsupported ZIP compression {m}")),
        };
        out.push((name, data));
    }
    Ok(out)
}

/// Parse the CDS CSV into hourly values in the Open-Meteo layout and units
/// (wind speed/direction m/s & degrees-from, °C, hPa, mm). Accepts short (`u10`) or long
/// (`10m_u_component_of_wind`) column names; several header blocks are merged by time.
pub fn parse_csv(csv: &str) -> Result<BTreeMap<i64, Values>, String> {
    let mut rows: BTreeMap<i64, [f32; 6]> = BTreeMap::new();
    let mut cols: Option<(usize, [Option<usize>; 6])> = None;
    for line in csv.lines() {
        let f: Vec<&str> = line
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .collect();
        let is_header = f.iter().any(|c| {
            let c = c.to_lowercase();
            c == "valid_time" || c == "time" || c == "date"
        });
        if is_header {
            let names: Vec<String> = f.iter().map(|c| c.to_lowercase()).collect();
            let find = |alts: &[&str]| names.iter().position(|n| alts.contains(&n.as_str()));
            let t = find(&["valid_time", "time", "date"]).ok_or("no time column")?;
            cols = Some((
                t,
                [
                    find(&["u10", "10m_u_component_of_wind"]),
                    find(&["v10", "10m_v_component_of_wind"]),
                    find(&["t2m", "2m_temperature"]),
                    find(&["msl", "mean_sea_level_pressure"]),
                    find(&["sp", "surface_pressure"]),
                    find(&["tp", "total_precipitation"]),
                ],
            ));
            continue;
        }
        let Some((tc, idx)) = cols else { continue };
        let Some(t) = f.get(tc).and_then(|s| crate::table::parse_time(s)) else {
            continue;
        };
        let e = rows
            .entry(t.div_euclid(3600) * 3600)
            .or_insert([f32::NAN; 6]);
        for (j, c) in idx.iter().enumerate() {
            if let Some(v) = c.and_then(|c| f.get(c)).and_then(|s| s.parse::<f32>().ok()) {
                e[j] = v;
            }
        }
    }
    if cols.is_none() {
        return Err("CDS CSV without a header".into());
    }
    let mut out = BTreeMap::new();
    for (t, [u, v, t2, msl, sp, tp]) in rows {
        if !(u.is_finite() && v.is_finite()) {
            continue;
        }
        let mut x = [f32::NAN; NV];
        x[0] = (u * u + v * v).sqrt();
        x[1] = (u.atan2(v).to_degrees() + 180.0).rem_euclid(360.0); // direction it blows from
        // K → °C, Pa → hPa, m → mm (values already in target units are left alone)
        x[4] = if t2 > 150.0 { t2 - 273.15 } else { t2 };
        x[5] = if msl > 2000.0 { msl / 100.0 } else { msl };
        x[6] = if sp > 2000.0 { sp / 100.0 } else { sp };
        x[7] = if tp.is_finite() {
            (tp * 1000.0).max(0.0)
        } else {
            f32::NAN
        };
        out.insert(t, x);
    }
    if out.is_empty() {
        return Err("CDS CSV had no wind values".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_one_job_per_cell_and_season() {
        let d = |s: &str| crate::weather::parse_day(s).unwrap() * 86400 + 7200;
        let pts = [
            (47.76, 8.99, d("2024-09-20")),
            (47.76, 8.99, d("2024-09-28")),
            (47.76, 8.99, d("2025-09-05")),
            (46.0, 7.0, d("2024-09-21")),
        ];
        let jobs = plan(&pts, &HashSet::new(), i64::MAX);
        assert_eq!(jobs.len(), 3);
        let a = jobs
            .iter()
            .find(|j| j.cell == crate::weather::cell_of(47.76, 8.99))
            .unwrap();
        assert_eq!(day_str(a.start_day), "2024-09-20");
        assert_eq!(day_str(a.end_day), "2024-09-28");
        let r = submit("abc", a);
        assert_eq!(r.method, ehttp::Method::POST);
        assert!(
            r.url
                .ends_with("/processes/reanalysis-era5-single-levels-timeseries/execution")
        );
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["inputs"]["date"][0], "2024-09-20/2024-09-28");
        assert_eq!(body["inputs"]["data_format"], "csv");
    }

    #[test]
    fn job_flow_parsing() {
        assert_eq!(
            parse_job(r#"{"processID":"x","jobID":"j1","status":"accepted"}"#).unwrap(),
            ("j1".into(), JobState::Waiting)
        );
        assert_eq!(
            parse_job(r#"{"jobID":"j1","status":"successful"}"#)
                .unwrap()
                .1,
            JobState::Done
        );
        assert_eq!(
            parse_results(
                r#"{"asset":{"value":{"type":"application/zip","href":"https://x/y.zip"}}}"#
            )
            .unwrap(),
            "https://x/y.zip"
        );
        let e = parse_job(
            r#"{"type":"x","title":"required licences not accepted","detail":"accept them"}"#,
        )
        .unwrap_err();
        assert!(e.contains("licences not accepted") && e.contains("download tab"));
    }

    #[test]
    fn csv_zip_and_units() {
        let csv = "valid_time,u10,v10,t2m,msl,sp,tp,latitude,longitude\n\
                   2024-09-20 00:00:00,0.0,-5.0,288.15,101325,95000,0.0002,47.75,9.0\n\
                   2024-09-20 01:00:00,3.0,4.0,287.15,101300,94990,0,47.75,9.0\n";
        // wrap in a deflate ZIP like CDS does
        let mut z = Vec::new();
        {
            use std::io::Write;
            let mut enc =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(csv.as_bytes()).unwrap();
            let comp = enc.finish().unwrap();
            let name = b"data.csv";
            let crc = 0u32;
            // local header
            z.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x00\x08\x00\x00\x00\x00\x00");
            z.extend_from_slice(&crc.to_le_bytes());
            z.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            z.extend_from_slice(&(csv.len() as u32).to_le_bytes());
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
            z.extend_from_slice(&0u16.to_le_bytes());
            z.extend_from_slice(name);
            z.extend_from_slice(&comp);
            let cd = z.len();
            z.extend_from_slice(b"PK\x01\x02\x14\x00\x14\x00\x00\x00\x08\x00\x00\x00\x00\x00");
            z.extend_from_slice(&crc.to_le_bytes());
            z.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            z.extend_from_slice(&(csv.len() as u32).to_le_bytes());
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
            z.extend_from_slice(&[0u8; 12]);
            z.extend_from_slice(&0u32.to_le_bytes()); // local header offset
            z.extend_from_slice(name);
            let cdlen = z.len() - cd;
            z.extend_from_slice(b"PK\x05\x06\x00\x00\x00\x00\x01\x00\x01\x00");
            z.extend_from_slice(&(cdlen as u32).to_le_bytes());
            z.extend_from_slice(&(cd as u32).to_le_bytes());
            z.extend_from_slice(&0u16.to_le_bytes());
        }
        let text = payload_csv(&z).unwrap();
        let s = parse_csv(&text).unwrap();
        assert_eq!(s.len(), 2);
        let a = s.values().next().unwrap();
        assert!((a[0] - 5.0).abs() < 1e-4);
        assert!(
            (a[1] - 0.0).abs() < 1e-3,
            "northerly wind (v<0) blows from 0°: {}",
            a[1]
        );
        assert!((a[4] - 15.0).abs() < 1e-3 && (a[5] - 1013.25).abs() < 1e-2);
        assert!((a[7] - 0.2).abs() < 1e-4);
        assert!(a[2].is_nan(), "no 100 m wind in the time-series dataset");
        let b = s.values().nth(1).unwrap();
        assert!(
            (b[1] - 216.87).abs() < 0.01,
            "u=3,v=4 blows from the SW: {}",
            b[1]
        );
        assert!(payload_csv(b"\x89HDF....").is_err());
    }
}
