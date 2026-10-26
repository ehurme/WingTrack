//! Shareable links: the whole dashboard state encoded in the URL fragment, like a map link.
//!
//! The fragment is a readable `key=value&key=value` list (values percent-encoded), e.g.
//!
//! ```text
//! https://ehurme.github.io/WingTrack/#v=1&tab=3d&t=1757371200&map=46.2,7.9,5.5&cb=SegmentWind&demo=1
//! ```
//!
//! Unknown keys are ignored and missing keys keep their defaults, so old links keep working as
//! settings are added. Data travels in one of three ways:
//! * `demo=1` — the built-in simulated files,
//! * `data=<url>` (repeatable) — CSV files the recipient's browser downloads (needs CORS, e.g.
//!   GitHub raw links),
//! * `emb=<base64url>` — the CSV files themselves, gzip-compressed inside the link.
//!
//! Weather is not stored: the recipient's app downloads it again from Open-Meteo.
//! The fragment (`#…`) is never sent to the web server.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::io::{Read, Write};

/// Where the published web app lives (used for links made from the desktop app).
pub const WEB_APP_URL: &str = "https://ehurme.github.io/WingTrack/";
pub const VERSION: &str = "1";

/// Ordered key/value pairs of a share link.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShareMap {
    pub pairs: Vec<(String, String)>,
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b',' | b':' | b';' | b'/')
}

pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl ShareMap {
    pub fn set(&mut self, k: &str, v: impl ToString) {
        self.pairs.push((k.to_string(), v.to_string()));
    }

    /// Set only if `on`.
    pub fn set_if(&mut self, on: bool, k: &str, v: impl ToString) {
        if on {
            self.set(k, v);
        }
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.pairs.iter().find(|p| p.0 == k).map(|p| p.1.as_str())
    }

    pub fn get_all<'a>(&'a self, k: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs
            .iter()
            .filter(move |p| p.0 == k)
            .map(|p| p.1.as_str())
    }

    pub fn f64(&self, k: &str) -> Option<f64> {
        self.get(k)?.parse().ok().filter(|v: &f64| v.is_finite())
    }

    /// Comma-separated numbers.
    pub fn nums(&self, k: &str) -> Vec<f64> {
        self.get(k)
            .map(|s| {
                s.split(',')
                    .map(|x| x.parse::<f64>().unwrap_or(f64::NAN))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn flag(&self, k: &str) -> Option<bool> {
        self.get(k).map(|v| v == "1" || v == "true")
    }

    pub fn encode(&self) -> String {
        self.pairs
            .iter()
            .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Parse a fragment (with or without the leading `#`) or a whole URL.
    pub fn decode(s: &str) -> ShareMap {
        let frag = s.split_once('#').map(|(_, f)| f).unwrap_or(s);
        let pairs = frag
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) => (pct_decode(k), pct_decode(v)),
                None => (pct_decode(p), String::new()),
            })
            .collect();
        ShareMap { pairs }
    }
}

/// Encode an enum value by its `Debug` name.
pub fn name<T: std::fmt::Debug>(v: &T) -> String {
    format!("{v:?}")
}

/// Decode an enum value by matching `Debug` names against `all`.
pub fn from_name<T: std::fmt::Debug + Copy>(all: &[T], s: &str) -> Option<T> {
    all.iter().copied().find(|v| format!("{v:?}") == s)
}

/// Round for compact links.
pub fn r(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// A data file inside a link: name, firmware profile id ("" = auto-detect) and raw bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedFile {
    pub name: String,
    pub firmware: String,
    pub bytes: Vec<u8>,
}

/// gzip + base64url of all files (length-prefixed records).
pub fn pack_files(files: &[EmbeddedFile]) -> String {
    let mut raw = Vec::new();
    for f in files {
        for part in [f.name.as_bytes(), f.firmware.as_bytes(), f.bytes.as_slice()] {
            raw.extend_from_slice(&(part.len() as u32).to_le_bytes());
            raw.extend_from_slice(part);
        }
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    let _ = enc.write_all(&raw);
    URL_SAFE_NO_PAD.encode(enc.finish().unwrap_or_default())
}

pub fn unpack_files(s: &str) -> Result<Vec<EmbeddedFile>, String> {
    let gz = URL_SAFE_NO_PAD
        .decode(s.trim().trim_end_matches('='))
        .map_err(|e| format!("embedded data is not valid base64: {e}"))?;
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice())
        .read_to_end(&mut raw)
        .map_err(|e| format!("embedded data is not valid gzip: {e}"))?;
    let mut out = Vec::new();
    let mut i = 0;
    let take = |i: &mut usize| -> Result<Vec<u8>, String> {
        let n = raw
            .get(*i..*i + 4)
            .ok_or("embedded data truncated")?
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_| "embedded data truncated")? as usize;
        *i += 4;
        let v = raw
            .get(*i..*i + n)
            .ok_or("embedded data truncated")?
            .to_vec();
        *i += n;
        Ok(v)
    };
    while i < raw.len() {
        let name = String::from_utf8_lossy(&take(&mut i)?).into_owned();
        let firmware = String::from_utf8_lossy(&take(&mut i)?).into_owned();
        let bytes = take(&mut i)?;
        out.push(EmbeddedFile {
            name,
            firmware,
            bytes,
        });
    }
    Ok(out)
}

/// Name for a file fetched from a URL: the last path segment, without query.
pub fn file_name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("data.csv");
    pct_decode(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_pairs() {
        let mut m = ShareMap::default();
        m.set("v", VERSION);
        m.set("hide", "Bat A,Bat&B");
        m.set("data", "https://raw.githubusercontent.com/x/y/main/a b.csv");
        m.set("chart", name(&Some(3)));
        let s = m.encode();
        assert!(!s.contains(' ') && !s.contains("&B"));
        let back = ShareMap::decode(&format!("https://host/app/#{s}"));
        assert_eq!(back, m);
        assert_eq!(back.get("hide"), Some("Bat A,Bat&B"));
        assert_eq!(ShareMap::decode("#a=1&b").get("b"), Some(""));
        assert_eq!(r(47.123400, 4), "47.1234");
        assert_eq!(r(5.0, 2), "5");
    }

    #[test]
    fn roundtrip_files() {
        let files = vec![
            EmbeddedFile {
                name: "a.csv".into(),
                firmware: "".into(),
                bytes: b"x,y\n1,2\n".to_vec(),
            },
            EmbeddedFile {
                name: "b.csv".into(),
                firmware: "ctt-gps".into(),
                bytes: vec![0, 255, 10],
            },
        ];
        let s = pack_files(&files);
        assert!(
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(unpack_files(&s).unwrap(), files);
        assert!(unpack_files("not-base64!").is_err());
        assert_eq!(
            file_name_from_url("https://x.org/d/My%20bats.csv?raw=1"),
            "My bats.csv"
        );
    }

    #[test]
    fn enum_names() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum E {
            A,
            B(u8),
        }
        let all = [E::A, E::B(2)];
        assert_eq!(from_name(&all, &name(&E::B(2))), Some(E::B(2)));
        assert_eq!(from_name(&all, "C"), None);
    }
}
