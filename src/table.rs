//! A small in-memory CSV table with delimiter sniffing and header normalisation,
//! so firmware parsers can look columns up by tolerant aliases.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

pub struct Table {
    pub name: String,
    /// Original header names.
    pub headers: Vec<String>,
    /// Normalised header names (lowercase, non-alphanumerics → `_`).
    pub norm: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// `"Radius (m) (Source/Status)"` → `"radius_m_source_status"`, `"location-lat"` → `"location_lat"`.
pub fn normalize_header(h: &str) -> String {
    let h = h
        .trim()
        .trim_start_matches('\u{feff}')
        .replace('\u{a0}', " ");
    let mut out = String::with_capacity(h.len());
    let mut last_us = true;
    for ch in h.chars().flat_map(|c| c.to_lowercase()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn sniff_delimiter(text: &str) -> u8 {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let count = |c: char| {
        // Count delimiters outside quotes.
        let mut n = 0;
        let mut q = false;
        for ch in first.chars() {
            if ch == '"' {
                q = !q;
            } else if ch == c && !q {
                n += 1;
            }
        }
        n
    };
    let cands = [(b',', count(',')), (b';', count(';')), (b'\t', count('\t'))];
    cands
        .iter()
        .max_by_key(|c| c.1)
        .map(|c| c.0)
        .unwrap_or(b',')
}

/// Re-join header names that were split on a delimiter inside parentheses — e.g. an unquoted
/// Wildcloud `Base Stations (ID, RSSI, Reps)` header becomes one column again.
fn merge_split_headers(raw: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut open = 0i32;
    for h in raw {
        if open > 0 {
            if let Some(last) = out.last_mut() {
                last.push_str(", ");
                last.push_str(h.trim());
            }
        } else {
            out.push(h.clone());
        }
        open += h.matches('(').count() as i32 - h.matches(')').count() as i32;
        open = open.max(0);
    }
    out
}

/// R's `write.csv` writes list/geometry columns unquoted, e.g. `c(8.99, 47.76)` or `c(NA, NA)`,
/// so the comma inside splits one value into two fields and shifts every later column.
/// Re-join such pieces (only called when a row has more fields than the header).
fn merge_r_vectors(raw: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut open = 0i32;
    for f in raw {
        if open > 0 {
            if let Some(last) = out.last_mut() {
                last.push_str(", ");
                last.push_str(&f);
            }
        } else {
            let t = f.trim_start();
            if !(t.starts_with("c(") || t.starts_with("list(")) {
                out.push(f);
                continue;
            }
            out.push(f.clone());
        }
        open += f.matches('(').count() as i32 - f.matches(')').count() as i32;
        open = open.max(0);
    }
    out
}

impl Table {
    pub fn from_bytes(name: &str, bytes: &[u8]) -> Result<Table, String> {
        let text = match std::str::from_utf8(bytes) {
            Ok(s) => s.to_owned(),
            // Wildcloud / Excel exports are sometimes Latin-1 (°C etc.)
            Err(_) => bytes.iter().map(|&b| b as char).collect(),
        };
        Self::from_str(name, &text)
    }

    pub fn from_str(name: &str, text: &str) -> Result<Table, String> {
        let delim = sniff_delimiter(text);
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delim)
            .flexible(true)
            .trim(csv::Trim::All)
            .from_reader(text.as_bytes());
        let raw: Vec<String> = rdr
            .headers()
            .map_err(|e| format!("{name}: cannot read header: {e}"))?
            .iter()
            .map(|h| h.trim_start_matches('\u{feff}').to_string())
            .collect();
        let headers = merge_split_headers(raw);
        if headers.len() < 2 {
            return Err(format!(
                "{name}: expected a delimited table with a header row"
            ));
        }
        let norm = headers.iter().map(|h| normalize_header(h)).collect();
        let mut rows = Vec::new();
        for rec in rdr.records() {
            let rec = rec.map_err(|e| format!("{name}: {e}"))?;
            if rec.iter().all(|f| f.is_empty()) {
                continue;
            }
            let mut fields: Vec<String> = rec.iter().map(|s| s.to_string()).collect();
            if fields.len() > headers.len() {
                fields = merge_r_vectors(fields);
            }
            rows.push(fields);
        }
        Ok(Table {
            name: name.to_string(),
            headers,
            norm,
            rows,
        })
    }

    /// First column whose normalised name equals one of `aliases` (already normalised).
    pub fn col(&self, aliases: &[&str]) -> Option<usize> {
        aliases
            .iter()
            .find_map(|a| self.norm.iter().position(|n| n == a))
    }

    /// First column whose normalised name starts with `prefix`.
    pub fn col_prefix(&self, prefix: &str) -> Option<usize> {
        self.norm.iter().position(|n| n.starts_with(prefix))
    }

    /// All columns matching a predicate on the normalised name.
    pub fn cols_where(&self, pred: impl Fn(&str) -> bool) -> Vec<usize> {
        self.norm
            .iter()
            .enumerate()
            .filter(|(_, n)| pred(n))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn has(&self, aliases: &[&str]) -> bool {
        self.col(aliases).is_some()
    }

    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.col_prefix(prefix).is_some()
    }

    pub fn get<'a>(&self, row: &'a [String], c: Option<usize>) -> &'a str {
        c.and_then(|i| row.get(i)).map(|s| s.as_str()).unwrap_or("")
    }

    pub fn num(&self, row: &[String], c: Option<usize>) -> Option<f64> {
        parse_num(self.get(row, c))
    }
}

pub fn parse_num(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty()
        || s.eq_ignore_ascii_case("na")
        || s.eq_ignore_ascii_case("nan")
        || s.eq_ignore_ascii_case("null")
    {
        return None;
    }
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Parse the timestamp formats seen in Movebank, Wildcloud, CTT and R exports. Returns unix seconds (UTC).
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim().trim_matches('"');
    if s.is_empty() {
        return None;
    }
    // Pure numbers: unix seconds or milliseconds.
    if let Ok(v) = s.parse::<f64>() {
        if v > 1e11 {
            return Some((v / 1000.0) as i64);
        }
        if v > 1e8 {
            return Some(v as i64);
        }
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).timestamp());
    }
    // Strip a trailing " UTC" / "Z" / " GMT".
    let cleaned = s
        .trim_end_matches(" UTC")
        .trim_end_matches(" GMT")
        .trim_end_matches('Z')
        .replace('T', " ");
    const FORMATS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%d.%m.%Y, %H:%M:%S", // Wildcloud: "14.06.2025, 17:56:24"
        "%d.%m.%Y %H:%M:%S",
        "%d.%m.%Y, %H:%M",
        "%d.%m.%Y %H:%M",
        "%Y/%m/%d %H:%M:%S%.f",
        "%Y/%m/%d %H:%M",
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y %H:%M",
        "%d/%m/%Y %H:%M:%S",
        "%Y-%m-%d_%H:%M:%S", // CTT "GPS_YYYY-MM-DD_HH:MM:SS"
        "%Y%m%d %H%M%S",
    ];
    for f in FORMATS {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&cleaned, f) {
            return Some(dt.and_utc().timestamp());
        }
    }
    for f in ["%Y-%m-%d", "%d.%m.%Y", "%m/%d/%Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(&cleaned, f) {
            return d.and_hms_opt(12, 0, 0).map(|dt| dt.and_utc().timestamp());
        }
    }
    None
}

pub fn fmt_time(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

pub fn fmt_date(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        assert_eq!(
            normalize_header("Radius (m) (Source/Status)"),
            "radius_m_source_status"
        );
        assert_eq!(normalize_header("location-lat"), "location_lat");
        assert_eq!(normalize_header("\u{feff}Time (UTC)"), "time_utc");
        assert_eq!(normalize_header("latitude [°]"), "latitude");
        assert_eq!(
            normalize_header("24h Min. Pressure (mbar)"),
            "24h_min_pressure_mbar"
        );
    }

    #[test]
    fn times() {
        let t = parse_time("14.06.2025, 17:56:24").unwrap();
        assert_eq!(fmt_time(t), "2025-06-14 17:56:24");
        assert_eq!(parse_time("2025-06-14 17:56:24.000"), Some(t));
        assert_eq!(parse_time("2025-06-14T17:56:24Z"), Some(t));
        assert_eq!(parse_time("1749923784"), Some(t));
        assert_eq!(parse_time("2025-06-14_17:56:24"), Some(t));
    }

    #[test]
    fn delimiters() {
        let t = Table::from_str("x", "a;b;c\n1;2;3\n").unwrap();
        assert_eq!(t.headers.len(), 3);
        let t = Table::from_str(
            "x",
            "Device,Time (UTC),Position\nAB,\"14.06.2025, 17:56:24\",\"47.1, 9.2\"\n",
        )
        .unwrap();
        assert_eq!(t.rows[0][2], "47.1, 9.2");
        let t = Table::from_str(
            "x",
            "Device,Base Stations (ID, RSSI, Reps),LQI\nA,\"1,-100,2\",GOOD\n",
        )
        .unwrap();
        assert_eq!(
            t.headers,
            vec!["Device", "Base Stations (ID, RSSI, Reps)", "LQI"]
        );
        assert_eq!(t.rows[0][2], "GOOD");
        // R list column written unquoted: c(lon, lat) / c(NA, NA)
        let t = Table::from_str(
            "x",
            "a,geometry,alt,z\n1,c(8.99, 47.76),389.3,ok\n2,c(NA, NA),NA,ok\n",
        )
        .unwrap();
        assert_eq!(t.rows[0], vec!["1", "c(8.99, 47.76)", "389.3", "ok"]);
        assert_eq!(t.rows[1][2], "NA");
        assert_eq!(t.rows[1][3], "ok");
    }
}
