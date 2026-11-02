//! Acceleration (ACC) data: readers, per-burst features, flight detection, behavioural
//! clustering, and summaries per track fix.
//!
//! The feature set follows `burst_features()` / `wingbeat_freq()` in
//! [flea](https://github.com/ehurme/flea) (`R/flea_functions.R`):
//! dynamic SD, VeDBA, ODBA, peak |a|, clipping, jerk, posture (static acceleration, pitch,
//! roll), wingbeat frequency as the peak of the spectrum **summed over the three axes**
//! (avoids landing on the 2nd harmonic that dominates the fore–aft axis), the share of
//! power at that peak, spectral entropy, the 2nd-harmonic ratio and the share of variance on
//! the first principal component.
//!
//! Readers:
//! * **Movebank e-obs** bursts (`eobs:accelerations-raw`, XYZXYZ… counts):
//!   g = (raw − 2048) × slope, slope by tag generation as in Movebank's own example
//!   (`tag ≤ 2241`: 0.001, `≤ 4117`: 0.0022, newer: 1/512), overridable.
//! * **Per-sample x/y/z** tables: Movebank `acceleration-raw-x/y/z` or `acceleration-x/y/z`,
//!   Technosmart (AxyTrek) `Date`, `Time`, `X`, `Y`, `Z`, or any `timestamp` + `acc_x/y/z`
//!   table; consecutive samples are grouped into bursts at gaps.
//! * **FleaTag** `.txt` / `.csv` exports (`lineCnt, timeMilliseconds, burstCount,
//!   accX_mg…`). The tags have no clock: times are `tag start + t × (1 + clock rate)`,
//!   with the start set in the app (default: the matched track's first fix).
//!
//! Long continuous recordings are cut into windows (default 1 s) and treated as bursts.

use crate::model::Track;
use crate::table::Table;
use std::collections::HashMap;

// ---------------------------------------------------------------- data

/// One burst (or window) of 3-axis acceleration in g.
#[derive(Clone, Debug)]
pub struct Burst {
    /// Time of the first sample: unix ms (or ms since tag start for unanchored FleaTag data).
    pub t_ms: i64,
    /// Sampling rate per axis (Hz).
    pub sr: f32,
    pub xyz: Vec<[f32; 3]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    MovebankEobs,
    PerSample,
    FleaTag,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::MovebankEobs => "Movebank e-obs bursts",
            Format::PerSample => "per-sample x/y/z",
            Format::FleaTag => "FleaTag export",
        }
    }
}

/// All bursts of one animal / tag from one file.
#[derive(Clone, Debug)]
pub struct AccSet {
    /// Individual / tag id as written in the file.
    pub id: String,
    pub file: String,
    pub format: Format,
    pub bursts: Vec<Burst>,
    /// FleaTag: burst times are relative; `start_ms` + rel × (1 + `clock_rate`).
    pub relative: bool,
    pub start_ms: Option<i64>,
    pub clock_rate: f64,
    /// Track (by `track_key`) these data belong to.
    pub track: Option<String>,
    /// Header notes (FleaTag mode, AccHz, …).
    pub notes: Vec<(String, String)>,
}

impl AccSet {
    /// Absolute time (unix ms) of a burst; `None` for relative data without a start.
    pub fn abs_ms(&self, b: &Burst) -> Option<i64> {
        if self.relative {
            self.start_ms
                .map(|s| s + (b.t_ms as f64 * (1.0 + self.clock_rate)).round() as i64)
        } else {
            Some(b.t_ms)
        }
    }

    pub fn samples(&self) -> usize {
        self.bursts.iter().map(|b| b.xyz.len()).sum()
    }
}

// ---------------------------------------------------------------- readers

/// Decompress gzip (`.csv.gz` Movebank exports), at most `limit` bytes.
fn gunzip(bytes: &[u8], limit: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return None;
    }
    let mut out = Vec::new();
    let _ = flate2::read::MultiGzDecoder::new(bytes)
        .take(limit as u64)
        .read_to_end(&mut out);
    Some(out)
}

/// Does this file look like acceleration data (rather than locations)?
pub fn sniff(name: &str, bytes: &[u8]) -> bool {
    let unz = gunzip(bytes, 20_000);
    let bytes = unz.as_deref().unwrap_or(bytes);
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(20_000)]).to_lowercase();
    if head.contains("linecnt") && head.contains("accx_mg") {
        return true;
    }
    let first = head.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let cols: Vec<String> = first
        .split([',', ';', '\t'])
        .map(crate::table::normalize_header)
        .collect();
    let has = |c: &str| cols.iter().any(|x| x == c);
    if cols.iter().any(|c| c.ends_with("accelerations_raw")) {
        return true;
    }
    let xyz = |p: &str| has(&format!("{p}x")) && has(&format!("{p}y")) && has(&format!("{p}z"));
    let named = [
        "acceleration_raw_",
        "acceleration_",
        "acc_",
        "acc",
        "accel_",
        "a",
    ]
    .iter()
    .any(|p| xyz(p));
    // Technosmart: X, Y, Z next to Date + Time (and no plain lat/lon fixes)
    let axy = xyz("") && has("date") && has("time");
    let _ = name;
    named || axy
}

/// Read an acceleration file into one set per individual.
pub fn read(name: &str, bytes: &[u8], opt: &ReadOptions) -> Result<Vec<AccSet>, String> {
    let unz = gunzip(bytes, usize::MAX);
    let bytes = unz.as_deref().unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    let lower_head = text[..text.len().min(20_000)].to_lowercase();
    if lower_head.contains("linecnt") && lower_head.contains("accx_mg") {
        return read_fleatag(name, &text, opt).map(|s| vec![s]);
    }
    let t = Table::from_bytes(name, bytes)?;
    if let Some(c) = t.norm.iter().position(|h| h.ends_with("accelerations_raw")) {
        return read_eobs(&t, c, opt);
    }
    read_per_sample(&t, opt)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReadOptions {
    /// Override for e-obs counts → g (slope); `None` = by tag generation.
    pub eobs_slope: Option<f64>,
    /// Cut bursts longer than this into windows (s).
    pub window_s: f64,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions {
            eobs_slope: None,
            window_s: 1.0,
        }
    }
}

/// e-obs slope (g per count) by tag number, as in Movebank's example code.
pub fn eobs_slope(tag: &str) -> f64 {
    match tag.trim().parse::<u32>() {
        Ok(n) if n <= 2241 => 0.001,
        Ok(n) if n <= 4117 => 0.0022,
        _ => 1.0 / 512.0,
    }
}

fn read_eobs(t: &Table, c_raw: usize, opt: &ReadOptions) -> Result<Vec<AccSet>, String> {
    let c_id = t.col(&[
        "individual_local_identifier",
        "individual_id",
        "tag_local_identifier",
    ]);
    let c_tag = t.col(&["tag_local_identifier", "tag_id"]);
    let c_sr = t.col(&[
        "eobs_acceleration_sampling_frequency_per_axis",
        "acceleration_sampling_frequency_per_axis",
    ]);
    let c_axes = t.col(&["eobs_acceleration_axes", "acceleration_axes"]);
    let c_t = t.col(&["eobs_start_timestamp", "start_timestamp", "timestamp"]);
    let mut sets: HashMap<String, AccSet> = HashMap::new();
    for row in &t.rows {
        let Some(ts) = crate::table::parse_time_ms(t.get(row, c_t)) else {
            continue;
        };
        let sr = t.num(row, c_sr).unwrap_or(0.0) as f32;
        if sr <= 0.0 {
            continue;
        }
        let axes: Vec<usize> = {
            let a = t.get(row, c_axes).to_uppercase();
            let v: Vec<usize> = a
                .chars()
                .filter_map(|ch| match ch {
                    'X' => Some(0),
                    'Y' => Some(1),
                    'Z' => Some(2),
                    _ => None,
                })
                .collect();
            if v.is_empty() { vec![0, 1, 2] } else { v }
        };
        let tag = t.get(row, c_tag).to_string();
        let slope = opt.eobs_slope.unwrap_or_else(|| eobs_slope(&tag));
        let raw: Vec<f64> = row[c_raw]
            .split_whitespace()
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();
        let xyz: Vec<[f32; 3]> = raw
            .chunks_exact(axes.len())
            .map(|c| {
                let mut v = [0.0f32; 3];
                for (k, &a) in axes.iter().enumerate() {
                    v[a] = ((c[k] - 2048.0) * slope) as f32;
                }
                v
            })
            .collect();
        if xyz.len() < 4 {
            continue;
        }
        let id = Some(t.get(row, c_id).to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| tag.clone());
        let set = sets.entry(id.clone()).or_insert_with(|| AccSet {
            id,
            file: t.name.clone(),
            format: Format::MovebankEobs,
            bursts: Vec::new(),
            relative: false,
            start_ms: None,
            clock_rate: 0.0,
            track: None,
            notes: vec![("g per count".into(), format!("{slope}"))],
        });
        push_windows(&mut set.bursts, ts, sr, xyz, opt.window_s);
    }
    finish(sets)
}

fn read_per_sample(t: &Table, opt: &ReadOptions) -> Result<Vec<AccSet>, String> {
    let pick = |c: &str| -> Option<usize> {
        let names: Vec<String> = [
            "acceleration_raw_",
            "acceleration_",
            "acc_",
            "acc",
            "accel_",
            "a",
            "",
        ]
        .iter()
        .map(|p| format!("{p}{c}"))
        .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        t.col(&refs)
    };
    let (Some(cx), Some(cy), Some(cz)) = (pick("x"), pick("y"), pick("z")) else {
        return Err("no x / y / z acceleration columns".into());
    };
    let raw_counts = t.norm[cx].starts_with("acceleration_raw");
    let c_id = t.col(&[
        "individual_local_identifier",
        "tag_local_identifier",
        "tagid",
        "tag_id",
        "id",
    ]);
    let c_t = t.col(&["timestamp", "datetime", "date_time", "time_utc"]);
    let (c_date, c_time) = (t.col(&["date"]), t.col(&["time"]));
    let time_of = |row: &[String]| -> Option<i64> {
        if c_t.is_some() {
            return crate::table::parse_time_ms(t.get(row, c_t));
        }
        let s = format!(
            "{} {}",
            t.get(row, c_date).trim(),
            t.get(row, c_time).trim()
        );
        crate::table::parse_dmy_time_ms(&s)
    };
    // per individual: (t_ms, xyz)
    let mut by: HashMap<String, Vec<(i64, [f64; 3])>> = HashMap::new();
    for row in &t.rows {
        let (Some(x), Some(y), Some(z)) = (
            t.num(row, Some(cx)),
            t.num(row, Some(cy)),
            t.num(row, Some(cz)),
        ) else {
            continue;
        };
        let Some(ts) = time_of(row) else { continue };
        let id = Some(t.get(row, c_id).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| t.name.clone());
        by.entry(id).or_default().push((ts, [x, y, z]));
    }
    if by.is_empty() {
        return Err("no rows with time and x / y / z".into());
    }
    let mut sets = HashMap::new();
    for (id, mut v) in by {
        v.sort_by_key(|x| x.0);
        // units: e-obs-like counts (centred near 2048), mg, m/s², or g
        let mags: Vec<f64> = v
            .iter()
            .map(|(_, a)| (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt())
            .collect();
        let med = median(&mut mags.clone());
        let mean0 = v.iter().map(|x| x.1[0]).sum::<f64>() / v.len() as f64;
        let (off, scale, unit) = if raw_counts || (mean0 > 1500.0 && mean0 < 2600.0) {
            (2048.0, opt.eobs_slope.unwrap_or(1.0 / 512.0), "counts → g")
        } else if med > 200.0 {
            (0.0, 0.001, "mg → g")
        } else if med > 4.0 && med < 30.0 {
            (0.0, 1.0 / 9.80665, "m/s² → g")
        } else {
            (0.0, 1.0, "g")
        };
        let dts: Vec<f64> = v
            .windows(2)
            .map(|w| (w[1].0 - w[0].0) as f64)
            .filter(|d| *d > 0.0)
            .collect();
        let dt = median(&mut dts.clone()).max(1.0);
        let sr = (1000.0 / dt) as f32;
        let mut bursts = Vec::new();
        let mut cur: Vec<[f32; 3]> = Vec::new();
        let mut t0 = v[0].0;
        let mut prev = v[0].0;
        for &(ts, a) in &v {
            if ts - prev > (2.5 * dt) as i64 && !cur.is_empty() {
                push_windows(&mut bursts, t0, sr, std::mem::take(&mut cur), opt.window_s);
                t0 = ts;
            }
            if cur.is_empty() {
                t0 = ts;
            }
            cur.push([
                ((a[0] - off) * scale) as f32,
                ((a[1] - off) * scale) as f32,
                ((a[2] - off) * scale) as f32,
            ]);
            prev = ts;
        }
        push_windows(&mut bursts, t0, sr, cur, opt.window_s);
        sets.insert(
            id.clone(),
            AccSet {
                id,
                file: t.name.clone(),
                format: Format::PerSample,
                bursts,
                relative: false,
                start_ms: None,
                clock_rate: 0.0,
                track: None,
                notes: vec![
                    ("units".into(), unit.into()),
                    ("rate".into(), format!("{sr:.1} Hz")),
                ],
            },
        );
    }
    finish(sets)
}

fn read_fleatag(name: &str, text: &str, opt: &ReadOptions) -> Result<AccSet, String> {
    let lines: Vec<&str> = text.lines().collect();
    let h = lines
        .iter()
        .position(|l| l.trim_start().starts_with("lineCnt"))
        .ok_or("FleaTag export without a lineCnt header")?;
    let mut notes = Vec::new();
    let mut id = String::new();
    let mut hz = None;
    for l in &lines[..h] {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("ID") {
                id = v.to_uppercase();
            }
            if k == "AccHz" {
                hz = v.parse::<f32>().ok();
            }
            if !k.is_empty() && !v.is_empty() && notes.len() < 12 {
                notes.push((k.to_string(), v.to_string()));
            }
        }
    }
    let header: Vec<String> = lines[h]
        .split([',', ';', '\t'])
        .map(|s| s.trim().to_string())
        .collect();
    let col = |n: &str| header.iter().position(|c| c.eq_ignore_ascii_case(n));
    let (Some(ct), Some(cb), Some(cx), Some(cy), Some(cz)) = (
        col("timeMilliseconds"),
        col("burstCount"),
        col("accX_mg"),
        col("accY_mg"),
        col("accZ_mg"),
    ) else {
        return Err("FleaTag header lacks timeMilliseconds / burstCount / accX_mg…".into());
    };
    let mut rows: Vec<(i64, i64, [f32; 3])> = Vec::new();
    for l in &lines[h + 1..] {
        if l.starts_with("Delete memory") {
            break;
        }
        let f: Vec<&str> = l.split([',', ';', '\t']).map(str::trim).collect();
        let num = |c: usize| f.get(c).and_then(|s| s.parse::<f64>().ok());
        let (Some(t), Some(b), Some(x), Some(y), Some(z)) =
            (num(ct), num(cb), num(cx), num(cy), num(cz))
        else {
            continue; // malformed rows are dropped, as in colombia25.R
        };
        rows.push((
            b as i64,
            t as i64,
            [x as f32 / 1000.0, y as f32 / 1000.0, z as f32 / 1000.0],
        ));
    }
    if rows.len() < 4 {
        return Err("FleaTag export without acceleration rows".into());
    }
    let dts: Vec<f64> = rows
        .windows(2)
        .map(|w| (w[1].1 - w[0].1) as f64)
        .filter(|d| *d > 0.0)
        .collect();
    let sr = hz.unwrap_or_else(|| (1000.0 / median(&mut dts.clone()).max(1.0)) as f32);
    let mut bursts = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let b = rows[i].0;
        let j = rows[i..]
            .iter()
            .position(|r| r.0 != b)
            .map_or(rows.len(), |k| i + k);
        let xyz: Vec<[f32; 3]> = rows[i..j].iter().map(|r| r.2).collect();
        push_windows(&mut bursts, rows[i].1, sr, xyz, opt.window_s);
        i = j;
    }
    if id.is_empty() {
        id = name.split(['.', '_']).next().unwrap_or(name).to_string();
    }
    Ok(AccSet {
        id,
        file: name.to_string(),
        format: Format::FleaTag,
        bursts,
        relative: true,
        start_ms: start_from_name(name),
        clock_rate: 0.0,
        track: None,
        notes,
    })
}

/// `20250312_183000_…`, `2025-03-12_18-30-00…` style start times in file names (UTC).
pub fn start_from_name(name: &str) -> Option<i64> {
    let digits: String = name
        .chars()
        .map(|c| if c.is_ascii_digit() { c } else { ' ' })
        .collect();
    let parts: Vec<&str> = digits.split_whitespace().collect();
    for w in parts.windows(2) {
        if w[0].len() == 8 && (w[1].len() == 6 || w[1].len() == 4) {
            let s = format!(
                "{} {}",
                w[0],
                if w[1].len() == 4 {
                    format!("{}00", w[1])
                } else {
                    w[1].to_string()
                }
            );
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&s, "%Y%m%d %H%M%S") {
                return Some(dt.and_utc().timestamp_millis());
            }
        }
    }
    for w in parts.windows(6) {
        if w[0].len() == 4 && w[1..].iter().all(|p| p.len() <= 2) {
            let s = w.join(" ");
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&s, "%Y %m %d %H %M %S") {
                return Some(dt.and_utc().timestamp_millis());
            }
        }
    }
    None
}

/// Append a burst, cut into windows of `window_s` when it is much longer (continuous data).
fn push_windows(out: &mut Vec<Burst>, t_ms: i64, sr: f32, xyz: Vec<[f32; 3]>, window_s: f64) {
    if xyz.len() < 4 {
        return;
    }
    // at least 8 samples per window (0.45 Hz FleaTag mode → ~18 s windows)
    let w = ((window_s * sr as f64).round() as usize).max(8);
    if xyz.len() <= 2 * w {
        out.push(Burst { t_ms, sr, xyz });
        return;
    }
    for (k, c) in xyz.chunks(w).enumerate() {
        if c.len() * 2 >= w {
            out.push(Burst {
                t_ms: t_ms + (k as f64 * w as f64 * 1000.0 / sr as f64).round() as i64,
                sr,
                xyz: c.to_vec(),
            });
        }
    }
}

fn finish(sets: HashMap<String, AccSet>) -> Result<Vec<AccSet>, String> {
    let mut v: Vec<AccSet> = sets
        .into_values()
        .filter(|s| !s.bursts.is_empty())
        .collect();
    if v.is_empty() {
        return Err("no usable acceleration bursts".into());
    }
    for s in &mut v {
        s.bursts.sort_by_key(|b| b.t_ms);
    }
    v.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(v)
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

// ---------------------------------------------------------------- spectrum

/// In-place radix-2 FFT (len must be a power of two).
fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f64::consts::PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let (xr, xi) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
                let t = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = t;
            }
        }
        len <<= 1;
    }
}

/// Periodogram of each (demeaned, detrended, 10 % cosine-tapered, ×4 zero-padded) axis.
/// Returns (frequencies, power per axis).
pub fn spectrum(dyn_: &[[f64; 3]], sr: f64) -> (Vec<f64>, Vec<[f64; 3]>) {
    let n = dyn_.len();
    let nfft = (n * 4).next_power_of_two();
    let taper = |i: usize| {
        // split cosine bell over 10 % at each end
        let m = ((n as f64) * 0.1).floor().max(1.0);
        let i = i as f64;
        let e = (n - 1) as f64 - i;
        let edge = i.min(e);
        if edge < m {
            0.5 * (1.0 - (std::f64::consts::PI * (edge + 0.5) / m).cos())
        } else {
            1.0
        }
    };
    let half = nfft / 2;
    let freqs: Vec<f64> = (0..=half).map(|k| k as f64 * sr / nfft as f64).collect();
    let mut pow = vec![[0.0; 3]; half + 1];
    for ax in 0..3 {
        // linear detrend
        let xs: Vec<f64> = dyn_.iter().map(|v| v[ax]).collect();
        let tm = (n as f64 - 1.0) / 2.0;
        let xm = xs.iter().sum::<f64>() / n as f64;
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for (i, x) in xs.iter().enumerate() {
            sxy += (i as f64 - tm) * (x - xm);
            sxx += (i as f64 - tm).powi(2);
        }
        let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
        let mut re = vec![0.0; nfft];
        let mut im = vec![0.0; nfft];
        for (i, x) in xs.iter().enumerate() {
            re[i] = (x - xm - slope * (i as f64 - tm)) * taper(i);
        }
        fft(&mut re, &mut im);
        for k in 0..=half {
            pow[k][ax] = (re[k] * re[k] + im[k] * im[k]) / (sr * n as f64);
        }
    }
    (freqs, pow)
}

// ---------------------------------------------------------------- features

/// Per-burst features (units: g, Hz, degrees).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Features {
    pub t_ms: i64,
    pub n: usize,
    pub sr: f32,
    pub dur_s: f32,
    pub dyn_sd: f32,
    pub vedba: f32,
    pub odba: f32,
    pub peak_g: f32,
    pub clip_frac: f32,
    pub jerk: f32,
    pub mean: [f32; 3],
    pub static_g: f32,
    pub pitch_deg: f32,
    pub roll_deg: f32,
    pub share: [f32; 3],
    /// Wingbeat frequency: peak of the 3-axis summed spectrum within the band.
    pub wbf_hz: Option<f32>,
    /// Share of in-band power within ±max(1 Hz, 2 bins) of the peak.
    pub peak_frac: f32,
    pub spec_entropy: f32,
    /// log10(power at 2·f / power at f), when 2·f is below Nyquist.
    pub harm_ratio: Option<f32>,
    pub pc1_frac: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Wingbeat band (Hz).
    pub band: (f32, f32),
    /// Flight when dynamic SD (g) exceeds this …
    pub flight_g: f32,
    /// … and, if > 0, the spectral peak holds at least this share of in-band power.
    pub min_peak_frac: f32,
    pub clip_g: f32,
    /// Behavioural clusters (0 = off).
    pub k: usize,
    pub window_s: f32,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            band: (4.0, 20.0),
            flight_g: 0.5,
            min_peak_frac: 0.0,
            clip_g: 7.9,
            k: 4,
            window_s: 1.0,
        }
    }
}

/// Wingbeat bands used in the flea scripts and typical for e-obs bats.
pub const BAND_PRESETS: [(&str, (f32, f32)); 5] = [
    ("bats (4–20 Hz)", (4.0, 20.0)),
    ("birds (2–15 Hz)", (2.0, 15.0)),
    ("butterflies (4–14 Hz)", (4.0, 14.0)),
    ("hawkmoths (18–35 Hz)", (18.0, 35.0)),
    ("hummingbirds (15–50 Hz)", (15.0, 50.0)),
];

pub fn features(b: &Burst, p: &Params) -> Features {
    let n = b.xyz.len();
    let sr = b.sr as f64;
    let a: Vec<[f64; 3]> = b
        .xyz
        .iter()
        .map(|v| [v[0] as f64, v[1] as f64, v[2] as f64])
        .collect();
    let mut m = [0.0; 3];
    for v in &a {
        for k in 0..3 {
            m[k] += v[k] / n as f64;
        }
    }
    let dyn_: Vec<[f64; 3]> = a
        .iter()
        .map(|v| [v[0] - m[0], v[1] - m[1], v[2] - m[2]])
        .collect();
    let var: [f64; 3] = std::array::from_fn(|k| {
        dyn_.iter().map(|d| d[k] * d[k]).sum::<f64>() / (n as f64 - 1.0).max(1.0)
    });
    let tv: f64 = var.iter().sum();
    let vedba: Vec<f64> = dyn_
        .iter()
        .map(|d| (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt())
        .collect();
    let mut f = Features {
        t_ms: b.t_ms,
        n,
        sr: b.sr,
        dur_s: (n as f64 / sr) as f32,
        dyn_sd: tv.sqrt() as f32,
        vedba: (vedba.iter().sum::<f64>() / n as f64) as f32,
        odba: (dyn_
            .iter()
            .map(|d| d[0].abs() + d[1].abs() + d[2].abs())
            .sum::<f64>()
            / n as f64) as f32,
        peak_g: a
            .iter()
            .map(|v| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt())
            .fold(0.0, f64::max) as f32,
        clip_frac: (a
            .iter()
            .flat_map(|v| v.iter())
            .filter(|x| x.abs() >= p.clip_g as f64)
            .count() as f64
            / (3 * n) as f64) as f32,
        jerk: (a
            .windows(2)
            .map(|w| {
                ((w[1][0] - w[0][0]).powi(2)
                    + (w[1][1] - w[0][1]).powi(2)
                    + (w[1][2] - w[0][2]).powi(2))
                .sqrt()
            })
            .sum::<f64>()
            / (n as f64 - 1.0).max(1.0)
            * sr) as f32,
        mean: [m[0] as f32, m[1] as f32, m[2] as f32],
        static_g: (m[0] * m[0] + m[1] * m[1] + m[2] * m[2]).sqrt() as f32,
        pitch_deg: m[0].atan2((m[1] * m[1] + m[2] * m[2]).sqrt()).to_degrees() as f32,
        roll_deg: m[1].atan2(m[2]).to_degrees() as f32,
        share: std::array::from_fn(|k| if tv > 0.0 { (var[k] / tv) as f32 } else { 0.0 }),
        ..Default::default()
    };
    // principal component share (power iteration on the 3×3 covariance)
    let mut c = [[0.0f64; 3]; 3];
    for d in &dyn_ {
        for i in 0..3 {
            for j in 0..3 {
                c[i][j] += d[i] * d[j];
            }
        }
    }
    let tr = c[0][0] + c[1][1] + c[2][2];
    if tr > 0.0 {
        let mut v = [1.0, 0.7, 0.3];
        let mut lam = 0.0;
        for _ in 0..60 {
            let w: [f64; 3] =
                std::array::from_fn(|i| c[i][0] * v[0] + c[i][1] * v[1] + c[i][2] * v[2]);
            let nrm = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
            if nrm == 0.0 {
                break;
            }
            lam = nrm;
            v = [w[0] / nrm, w[1] / nrm, w[2] / nrm];
        }
        f.pc1_frac = (lam / tr) as f32;
    }
    // spectrum
    let nyq = sr / 2.0;
    let (lo, hi) = (p.band.0 as f64, (p.band.1 as f64).min(nyq * 0.98));
    if n >= 8 && hi > lo {
        let (fr, pw) = spectrum(&dyn_, sr);
        let tot: Vec<f64> = pw.iter().map(|x| x[0] + x[1] + x[2]).collect();
        let in_band: Vec<usize> = (0..fr.len())
            .filter(|&k| fr[k] >= lo && fr[k] <= hi)
            .collect();
        if let Some(&i0) = in_band.iter().max_by(|&&a, &&b| tot[a].total_cmp(&tot[b])) {
            // parabolic refinement of the peak
            let mut f0 = fr[i0];
            if i0 > 0 && i0 + 1 < tot.len() {
                let (y0, y1, y2) = (tot[i0 - 1], tot[i0], tot[i0 + 1]);
                let den = y0 - 2.0 * y1 + y2;
                if den.abs() > 0.0 {
                    let dx = (0.5 * (y0 - y2) / den).clamp(-0.5, 0.5);
                    f0 += dx * (fr[1] - fr[0]);
                }
            }
            let bin = fr[1] - fr[0];
            let near_w = (1.0f64).max(2.0 * bin);
            let pow_near = |fc: f64| -> f64 {
                (0..fr.len())
                    .filter(|&k| (fr[k] - fc).abs() <= near_w)
                    .map(|k| tot[k])
                    .sum()
            };
            let band_pow: f64 = in_band.iter().map(|&k| tot[k]).sum();
            let p0 = pow_near(fr[i0]);
            f.wbf_hz = Some(f0 as f32);
            f.peak_frac = if band_pow > 0.0 {
                (p0 / band_pow).min(1.0) as f32
            } else {
                0.0
            };
            if 2.0 * fr[i0] + near_w < nyq {
                let p2 = pow_near(2.0 * fr[i0]);
                if p0 > 0.0 && p2 > 0.0 {
                    f.harm_ratio = Some((p2 / p0).log10() as f32);
                }
            }
            let wide: Vec<f64> = (0..fr.len())
                .filter(|&k| fr[k] >= lo.min(1.0).max(fr[1]) && fr[k] <= nyq)
                .map(|k| tot[k])
                .collect();
            let s: f64 = wide.iter().sum();
            if s > 0.0 && wide.len() > 1 {
                let h: f64 = wide
                    .iter()
                    .filter(|&&x| x > 0.0)
                    .map(|&x| -(x / s) * (x / s).ln())
                    .sum();
                f.spec_entropy = (h / (wide.len() as f64).ln()) as f32;
            }
        }
    }
    f
}

impl Features {
    pub fn is_flight(&self, p: &Params) -> bool {
        self.dyn_sd > p.flight_g && (p.min_peak_frac <= 0.0 || self.peak_frac >= p.min_peak_frac)
    }

    /// Standardisable vector for clustering (activity, posture, rhythm, axis structure).
    fn vector(&self) -> [f64; 8] {
        let r = (self.roll_deg as f64).to_radians();
        [
            (self.dyn_sd as f64 + 0.01).log10(),
            (self.jerk as f64 + 0.01).log10(),
            self.pitch_deg as f64 / 45.0,
            r.sin(),
            r.cos(),
            self.spec_entropy as f64,
            self.peak_frac as f64,
            self.pc1_frac as f64,
        ]
    }
}

// ---------------------------------------------------------------- clustering

/// k-means (k-means++ seeding, a few restarts, fixed seed) on standardised features.
/// Returns one label per row, clusters numbered by increasing mean dynamic SD.
pub fn cluster(feats: &[&Features], k: usize) -> Vec<usize> {
    let n = feats.len();
    if k < 2 || n < k * 3 {
        return vec![0; n];
    }
    let raw: Vec<[f64; 8]> = feats.iter().map(|f| f.vector()).collect();
    let mut mu = [0.0; 8];
    let mut sd = [0.0; 8];
    for v in &raw {
        for j in 0..8 {
            mu[j] += v[j] / n as f64;
        }
    }
    for v in &raw {
        for j in 0..8 {
            sd[j] += (v[j] - mu[j]).powi(2) / n as f64;
        }
    }
    let x: Vec<[f64; 8]> = raw
        .iter()
        .map(|v| {
            std::array::from_fn(|j| {
                if sd[j] > 1e-12 {
                    (v[j] - mu[j]) / sd[j].sqrt()
                } else {
                    0.0
                }
            })
        })
        .collect();
    let d2 =
        |a: &[f64; 8], b: &[f64; 8]| a.iter().zip(b).map(|(p, q)| (p - q).powi(2)).sum::<f64>();
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };
    // fit on at most 20 000 bursts (evenly spaced), then label all
    let stride = n.div_ceil(20_000).max(1);
    let fit: Vec<[f64; 8]> = x.iter().step_by(stride).copied().collect();
    let all = x;
    let x = fit;
    let n = x.len();
    let mut best: (f64, Vec<[f64; 8]>) = (f64::INFINITY, Vec::new());
    for _ in 0..6 {
        // k-means++
        let mut cent: Vec<[f64; 8]> = vec![x[(rnd() * n as f64) as usize % n]];
        while cent.len() < k {
            let dist: Vec<f64> = x
                .iter()
                .map(|p| cent.iter().map(|c| d2(p, c)).fold(f64::INFINITY, f64::min))
                .collect();
            let s: f64 = dist.iter().sum();
            let mut r = rnd() * s;
            let mut pick = n - 1;
            for (i, d) in dist.iter().enumerate() {
                r -= d;
                if r <= 0.0 {
                    pick = i;
                    break;
                }
            }
            cent.push(x[pick]);
        }
        let mut lab = vec![0usize; n];
        for _ in 0..100 {
            let mut changed = false;
            for (i, p) in x.iter().enumerate() {
                let c = (0..k)
                    .min_by(|&a, &b| d2(p, &cent[a]).total_cmp(&d2(p, &cent[b])))
                    .unwrap_or(0);
                if c != lab[i] {
                    lab[i] = c;
                    changed = true;
                }
            }
            let mut sum = vec![[0.0; 8]; k];
            let mut cnt = vec![0usize; k];
            for (i, p) in x.iter().enumerate() {
                cnt[lab[i]] += 1;
                for j in 0..8 {
                    sum[lab[i]][j] += p[j];
                }
            }
            for c in 0..k {
                if cnt[c] > 0 {
                    cent[c] = std::array::from_fn(|j| sum[c][j] / cnt[c] as f64);
                }
            }
            if !changed {
                break;
            }
        }
        let inertia: f64 = x.iter().zip(&lab).map(|(p, &c)| d2(p, &cent[c])).sum();
        if inertia < best.0 {
            best = (inertia, cent);
        }
    }
    let cent = best.1;
    let labels: Vec<usize> = all
        .iter()
        .map(|p| {
            (0..k)
                .min_by(|&a, &b| d2(p, &cent[a]).total_cmp(&d2(p, &cent[b])))
                .unwrap_or(0)
        })
        .collect();
    // order clusters by mean dynamic SD (C1 = calmest)
    let mut mean_dyn = vec![(0.0f64, 0usize); k];
    for (f, &c) in feats.iter().zip(&labels) {
        mean_dyn[c].0 += f.dyn_sd as f64;
        mean_dyn[c].1 += 1;
    }
    let mut order: Vec<usize> = (0..k).collect();
    order.sort_by(|&a, &b| {
        let ma = mean_dyn[a].0 / mean_dyn[a].1.max(1) as f64;
        let mb = mean_dyn[b].0 / mean_dyn[b].1.max(1) as f64;
        ma.total_cmp(&mb)
    });
    let mut rank = vec![0; k];
    for (r, &c) in order.iter().enumerate() {
        rank[c] = r;
    }
    labels.iter().map(|&c| rank[c]).collect()
}

// ---------------------------------------------------------------- analysis

/// Features of every burst of every set, with absolute times, flight flags and clusters.
#[derive(Default)]
pub struct Analysis {
    /// Per set: (features with absolute t_ms, flight, cluster).
    pub per_set: Vec<Vec<(Features, bool, usize)>>,
    pub params: Params,
    /// Per cluster: (n, mean dyn SD, median wingbeat Hz, mean pitch, flight share).
    pub clusters: Vec<ClusterInfo>,
}

#[derive(Clone, Debug, Default)]
pub struct ClusterInfo {
    pub n: usize,
    pub dyn_sd: f32,
    pub wbf_hz: Option<f32>,
    pub pitch_deg: f32,
    pub flight_frac: f32,
    pub peak_frac: f32,
}

pub fn analyse(sets: &[AccSet], p: &Params) -> Analysis {
    let mut per_set: Vec<Vec<(Features, bool, usize)>> = sets
        .iter()
        .map(|s| {
            s.bursts
                .iter()
                .filter_map(|b| {
                    let t = s.abs_ms(b)?;
                    let mut f = features(b, p);
                    f.t_ms = t;
                    let fl = f.is_flight(p);
                    Some((f, fl, 0))
                })
                .collect()
        })
        .collect();
    // one clustering over all animals, so cluster numbers mean the same thing everywhere
    let all: Vec<&Features> = per_set.iter().flatten().map(|x| &x.0).collect();
    let labels = cluster(&all, p.k);
    let mut it = labels.into_iter();
    for v in &mut per_set {
        for x in v.iter_mut() {
            x.2 = it.next().unwrap_or(0);
        }
    }
    let k = p.k.max(1);
    let mut clusters = vec![ClusterInfo::default(); k];
    let mut wbfs: Vec<Vec<f64>> = vec![Vec::new(); k];
    for (f, fl, c) in per_set.iter().flatten() {
        let ci = &mut clusters[(*c).min(k - 1)];
        ci.n += 1;
        ci.dyn_sd += f.dyn_sd;
        ci.pitch_deg += f.pitch_deg;
        ci.peak_frac += f.peak_frac;
        ci.flight_frac += *fl as u8 as f32;
        if let Some(w) = f.wbf_hz.filter(|_| f.peak_frac > 0.2) {
            wbfs[(*c).min(k - 1)].push(w as f64);
        }
    }
    for (ci, w) in clusters.iter_mut().zip(wbfs.iter_mut()) {
        let n = ci.n.max(1) as f32;
        ci.dyn_sd /= n;
        ci.pitch_deg /= n;
        ci.peak_frac /= n;
        ci.flight_frac /= n;
        ci.wbf_hz = (!w.is_empty()).then(|| median(w) as f32);
    }
    Analysis {
        per_set,
        params: *p,
        clusters,
    }
}

/// Summary of the bursts during the step that arrived at a fix.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FixAcc {
    pub n: usize,
    pub flight_frac: f32,
    pub dyn_sd: f32,
    pub vedba: f32,
    /// Median wingbeat frequency of the flight bursts.
    pub wbf_hz: Option<f32>,
    pub pitch_deg: f32,
    /// Most frequent cluster, and the share of bursts in each cluster.
    pub cluster: Option<usize>,
    pub cluster_frac: Vec<f32>,
}

/// Per fix of `tr` (index → summary) from the sets assigned to it. The window of fix i is
/// (t[i−1], t[i]]; the first fix takes the hour before it.
pub fn summarise_track(tr: &Track, sets: &[usize], a: &Analysis) -> HashMap<usize, FixAcc> {
    let mut out = HashMap::new();
    let k = a.params.k.max(1);
    let mut rows: Vec<&(Features, bool, usize)> =
        sets.iter().flat_map(|&s| a.per_set[s].iter()).collect();
    rows.sort_by_key(|r| r.0.t_ms);
    let mut j = 0;
    for (i, f) in tr.fixes.iter().enumerate() {
        let lo = if i == 0 {
            f.t * 1000 - 3_600_000
        } else {
            tr.fixes[i - 1].t * 1000
        };
        let hi = f.t * 1000;
        while j < rows.len() && rows[j].0.t_ms <= lo {
            j += 1;
        }
        let mut e = j;
        while e < rows.len() && rows[e].0.t_ms <= hi {
            e += 1;
        }
        let w = &rows[j..e];
        if w.is_empty() {
            continue;
        }
        let n = w.len() as f32;
        let mut cf = vec![0.0f32; k];
        for r in w {
            cf[r.2.min(k - 1)] += 1.0 / n;
        }
        let mut wbf: Vec<f64> = w
            .iter()
            .filter(|r| r.1)
            .filter_map(|r| r.0.wbf_hz.map(|x| x as f64))
            .collect();
        out.insert(
            i,
            FixAcc {
                n: w.len(),
                flight_frac: w.iter().filter(|r| r.1).count() as f32 / n,
                dyn_sd: w.iter().map(|r| r.0.dyn_sd).sum::<f32>() / n,
                vedba: w.iter().map(|r| r.0.vedba).sum::<f32>() / n,
                wbf_hz: (!wbf.is_empty()).then(|| median(&mut wbf) as f32),
                pitch_deg: w.iter().map(|r| r.0.pitch_deg).sum::<f32>() / n,
                cluster: (a.params.k >= 2)
                    .then(|| (0..k).max_by(|&x, &y| cf[x].total_cmp(&cf[y])))
                    .flatten(),
                cluster_frac: if a.params.k >= 2 { cf } else { Vec::new() },
            },
        );
        j = e;
    }
    out
}

/// One row per burst with all features, flight flag and cluster.
pub fn bursts_csv(sets: &[AccSet], a: &Analysis, track_of: impl Fn(usize) -> String) -> String {
    let mut w = csv::Writer::from_writer(Vec::new());
    let _ = w.write_record([
        "individual_local_identifier",
        "acc_id",
        "acc_file",
        "timestamp",
        "n_samples",
        "sampling_hz",
        "duration_s",
        "dyn_sd_g",
        "vedba_g",
        "odba_g",
        "peak_g",
        "clip_frac",
        "jerk_g_s",
        "mean_x_g",
        "mean_y_g",
        "mean_z_g",
        "static_g",
        "pitch_deg",
        "roll_deg",
        "share_x",
        "share_y",
        "share_z",
        "wbf_hz",
        "peak_frac",
        "spec_entropy",
        "harm_ratio",
        "pc1_frac",
        "flight",
        "cluster",
    ]);
    for (si, set) in sets.iter().enumerate() {
        let trk = track_of(si);
        for (f, fl, c) in &a.per_set[si] {
            let o = |v: Option<f32>| v.map(|x| format!("{x:.3}")).unwrap_or_default();
            let ts = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(f.t_ms)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                .unwrap_or_default();
            let _ = w.write_record([
                trk.clone(),
                set.id.clone(),
                set.file.clone(),
                ts,
                f.n.to_string(),
                format!("{:.2}", f.sr),
                format!("{:.3}", f.dur_s),
                format!("{:.4}", f.dyn_sd),
                format!("{:.4}", f.vedba),
                format!("{:.4}", f.odba),
                format!("{:.3}", f.peak_g),
                format!("{:.3}", f.clip_frac),
                format!("{:.3}", f.jerk),
                format!("{:.4}", f.mean[0]),
                format!("{:.4}", f.mean[1]),
                format!("{:.4}", f.mean[2]),
                format!("{:.4}", f.static_g),
                format!("{:.2}", f.pitch_deg),
                format!("{:.2}", f.roll_deg),
                format!("{:.3}", f.share[0]),
                format!("{:.3}", f.share[1]),
                format!("{:.3}", f.share[2]),
                o(f.wbf_hz),
                format!("{:.3}", f.peak_frac),
                format!("{:.3}", f.spec_entropy),
                o(f.harm_ratio),
                format!("{:.3}", f.pc1_frac),
                fl.to_string(),
                if a.params.k >= 2 {
                    format!("C{}", c + 1)
                } else {
                    String::new()
                },
            ]);
        }
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flapping at `f` Hz with amplitude `amp` g on z (plus a 2nd harmonic on x), or rest.
    fn burst(t_ms: i64, sr: f32, n: usize, f: f32, amp: f32, pitch: f32) -> Burst {
        let g = [pitch.to_radians().sin(), 0.0, pitch.to_radians().cos()];
        let mut s = 7u32;
        let xyz = (0..n)
            .map(|i| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
                let noise = ((s >> 16) as f32 / 65_536.0 - 0.5) * 0.02;
                let ph = std::f32::consts::TAU * f * i as f32 / sr;
                [
                    g[0] + 0.4 * amp * (2.0 * ph).sin() + noise,
                    g[1] + noise,
                    g[2] + amp * ph.sin() + noise,
                ]
            })
            .collect();
        Burst { t_ms, sr, xyz }
    }

    #[test]
    fn wingbeat_and_flight_features() {
        let p = Params::default();
        let f = features(&burst(0, 40.0, 80, 9.0, 1.5, 10.0), &p);
        let w = f.wbf_hz.unwrap();
        assert!(
            (w - 9.0).abs() < 0.3,
            "fundamental, not the 18 Hz harmonic on x: {w}"
        );
        assert!(f.is_flight(&p) && f.peak_frac > 0.5 && f.dyn_sd > 0.9);
        assert!(f.share[2] > 0.8 && f.pc1_frac > 0.8);
        let r = features(&burst(0, 40.0, 80, 9.0, 0.0, 60.0), &p);
        assert!(!r.is_flight(&p) && r.dyn_sd < 0.05);
        assert!((r.pitch_deg - 60.0).abs() < 1.0, "{}", r.pitch_deg);
        assert!(
            r.spec_entropy > f.spec_entropy,
            "noise is flatter than flapping"
        );
        // hawkmoth at 210 Hz
        let hp = Params {
            band: (18.0, 35.0),
            ..p
        };
        let h = features(&burst(0, 210.0, 210, 28.0, 3.0, 0.0), &hp);
        assert!((h.wbf_hz.unwrap() - 28.0).abs() < 0.5);
        assert!(h.harm_ratio.is_some());
    }

    #[test]
    fn clusters_separate_flight_postures() {
        let p = Params {
            k: 3,
            ..Default::default()
        };
        let mut bursts = Vec::new();
        for i in 0..30 {
            bursts.push(burst(i * 1000, 40.0, 80, 9.0, 1.5, 10.0)); // flight
            bursts.push(burst(i * 1000 + 300, 40.0, 80, 9.0, 0.0, 80.0)); // hanging
            bursts.push(burst(i * 1000 + 600, 40.0, 80, 9.0, 0.0, -10.0)); // lying
        }
        let set = AccSet {
            id: "bat".into(),
            file: "f".into(),
            format: Format::PerSample,
            bursts,
            relative: false,
            start_ms: None,
            clock_rate: 0.0,
            track: None,
            notes: vec![],
        };
        let a = analyse(&[set], &p);
        let lab: Vec<usize> = a.per_set[0].iter().map(|x| x.2).collect();
        // every flight burst in the most dynamic cluster (C3), postures split in C1/C2
        for i in 0..30 {
            assert_eq!(lab[3 * i], 2);
            assert_ne!(lab[3 * i + 1], lab[3 * i + 2]);
            assert_eq!(lab[3 * i + 1], lab[1]);
        }
        assert!(a.clusters[2].flight_frac > 0.99 && a.clusters[0].flight_frac < 0.01);
        assert!((a.clusters[2].wbf_hz.unwrap() - 9.0).abs() < 0.3);
    }

    #[test]
    fn readers() {
        // e-obs: 2 bursts, XYZ, 20 Hz, counts around 2048 (tag 5000 → 1/512 g per count)
        let mut raw1 = Vec::new();
        for i in 0..40 {
            let z = 2048.0 + 512.0 + 300.0 * (std::f64::consts::TAU * 6.0 * i as f64 / 20.0).sin();
            raw1.push(format!("2048 2048 {}", z.round()));
        }
        let csv = format!(
            "event-id,timestamp,eobs:accelerations-raw,eobs:acceleration-axes,eobs:acceleration-sampling-frequency-per-axis,individual-local-identifier,tag-local-identifier\n\
             1,2025-08-01 22:00:00.000,{},XYZ,20,Nyctalus 1,5000\n\
             2,2025-08-01 22:05:00.000,{},XYZ,20,Nyctalus 1,5000\n",
            raw1.join(" "),
            raw1.join(" ")
        );
        assert!(sniff("acc.csv", csv.as_bytes()));
        let gz = {
            use std::io::Write;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(csv.as_bytes()).unwrap();
            e.finish().unwrap()
        };
        assert!(sniff("acc.csv.gz", &gz));
        assert_eq!(
            read("acc.csv.gz", &gz, &ReadOptions::default()).unwrap()[0]
                .bursts
                .len(),
            2
        );
        let v = read("acc.csv", csv.as_bytes(), &ReadOptions::default()).unwrap();
        assert_eq!(
            (v.len(), v[0].bursts.len(), v[0].id.as_str()),
            (1, 2, "Nyctalus 1")
        );
        let b = &v[0].bursts[0];
        assert_eq!(b.sr, 20.0);
        assert!(
            (b.xyz[0][2] - 1.0).abs() < 0.6,
            "≈1 g on z: {}",
            b.xyz[0][2]
        );
        let f = features(b, &Params::default());
        assert!((f.wbf_hz.unwrap() - 6.0).abs() < 0.3);

        // FleaTag: header, 2 bursts at 100 Hz, relative clock, start from the file name
        let mut t = String::from(
            "ID: ad011\nAccHz: 100\nFLEATAG_SC V14: COLIBRIS\nlineCnt,timeMilliseconds,burstCount,accX_mg,accY_mg,accZ_mg,ColorSensRed_cnt,ColorSensGreen_cnt,ColorSensBlue_cnt,ColorSensIR_cnt\n",
        );
        let mut line = 0;
        for burst in 0..2 {
            for i in 0..30 {
                let ms = burst * 120_000 + i * 10;
                t.push_str(&format!("{line},{ms},{burst},0,0,1000,1,1,1,1\n"));
                line += 1;
            }
        }
        t.push_str("Delete memory by pressing button for 8 seconds\n");
        let v = read(
            "20250312_183000_AD011_Bat.txt",
            t.as_bytes(),
            &ReadOptions::default(),
        )
        .unwrap();
        let s = &v[0];
        assert_eq!(
            (s.id.as_str(), s.bursts.len(), s.format),
            ("AD011", 2, Format::FleaTag)
        );
        assert!(s.relative);
        let start = s.start_ms.unwrap();
        assert_eq!(crate::table::fmt_time(start / 1000), "2025-03-12 18:30:00");
        let mut s2 = s.clone();
        s2.clock_rate = 0.01;
        assert_eq!(s2.abs_ms(&s2.bursts[1]).unwrap() - start, 121_200);

        // Technosmart-style per-sample, mg, continuous 25 Hz → 1 s windows
        let mut t = String::from("TagID,Date,Time,X,Y,Z\n");
        for i in 0..100 {
            t.push_str(&format!(
                "bat7,01/08/2025,22:00:{:06.3},0,0,1000\n",
                i as f64 * 0.04
            ));
        }
        assert!(sniff("axy.csv", t.as_bytes()));
        let v = read("axy.csv", t.as_bytes(), &ReadOptions::default()).unwrap();
        assert_eq!(v[0].bursts.len(), 4);
        assert!((v[0].bursts[0].xyz[0][2] - 1.0).abs() < 1e-3);
        assert!((v[0].bursts[0].sr - 25.0).abs() < 0.5);
        // a GPS file is not ACC
        assert!(!sniff("gps.csv", b"timestamp,location-lat,location-long\n"));
    }

    #[test]
    fn fix_summaries() {
        let p = Params {
            k: 2,
            ..Default::default()
        };
        let mut bursts = Vec::new();
        for i in 0..20 {
            // first hour flying, second hour resting
            let fly = i < 10;
            bursts.push(burst(
                i * 360_000,
                40.0,
                80,
                9.0,
                if fly { 1.5 } else { 0.0 },
                10.0,
            ));
        }
        let set = AccSet {
            id: "b".into(),
            file: "f".into(),
            format: Format::PerSample,
            bursts,
            relative: false,
            start_ms: None,
            clock_rate: 0.0,
            track: None,
            notes: vec![],
        };
        let a = analyse(std::slice::from_ref(&set), &p);
        let mut tr = Track::new("b", "GPS", "t");
        for h in [0, 1, 2] {
            tr.fixes.push(crate::model::Fix::new(
                h * 3600,
                47.0,
                9.0,
                crate::model::LocationSource::Gps,
            ));
        }
        let s = summarise_track(&tr, &[0], &a);
        // fix 0 gets the burst at t=0 only (window (−1 h, 0])
        assert_eq!(s[&0].n, 1);
        let f1 = &s[&1];
        assert_eq!(f1.n, 10);
        assert!((f1.flight_frac - 0.9).abs() < 1e-6);
        assert!((f1.wbf_hz.unwrap() - 9.0).abs() < 0.3);
        assert_eq!(f1.cluster, Some(1));
        assert_eq!(s[&2].flight_frac, 0.0);
        assert_eq!(s[&2].cluster, Some(0));
        let csv = bursts_csv(&[set], &a, |_| "b".into());
        assert_eq!(csv.lines().count(), 21);
    }
}
