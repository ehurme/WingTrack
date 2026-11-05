//! Land cover at the fixes and habitat selection, run on demand.
//!
//! Land cover comes from **ESA WorldCover** (10 m, 2020 v100 / 2021 v200), read through the
//! Terrascope WMS (`WORLDCOVER_2021_MAP`, PNG in the official class colours) one small window
//! per fix, so the browser never downloads the 3×3° GeoTIFF tiles. Pixel colours are mapped
//! back to class codes with the WorldCover palette; any WMS serving a WorldCover-style
//! palette (or grey values equal to the class codes) works through the custom URL option.
//!
//! **Habitat selection** follows Manly et al. (2002) selection ratios with availability
//! defined per fix (design III style): around every sampled fix, *available* is the class
//! composition inside a circle of the chosen radius and *used* is the composition inside the
//! fix's location-error circle (the fix pixel when the error is unknown or tiny). The ratio
//! `wᵢ = mean usedᵢ / mean availableᵢ` is > 1 for selected and < 1 for avoided classes; 95 %
//! intervals come from bootstrapping the fixes.

/// ESA WorldCover classes: code, name, map colour.
pub const CLASSES: [(u8, &str, [u8; 3]); 11] = [
    (10, "Tree cover", [0, 100, 0]),
    (20, "Shrubland", [255, 187, 34]),
    (30, "Grassland", [255, 255, 76]),
    (40, "Cropland", [240, 150, 255]),
    (50, "Built-up", [250, 0, 0]),
    (60, "Bare / sparse vegetation", [180, 180, 180]),
    (70, "Snow and ice", [240, 240, 240]),
    (80, "Permanent water", [0, 100, 200]),
    (90, "Herbaceous wetland", [0, 150, 160]),
    (95, "Mangroves", [0, 207, 117]),
    (100, "Moss and lichen", [250, 230, 160]),
];

pub const NC: usize = CLASSES.len();

pub fn class_index(code: u8) -> Option<usize> {
    CLASSES.iter().position(|c| c.0 == code)
}

pub fn class_name(code: u8) -> &'static str {
    class_index(code).map(|i| CLASSES[i].1).unwrap_or("no data")
}

pub fn class_color(code: u8) -> [u8; 3] {
    class_index(code)
        .map(|i| CLASSES[i].2)
        .unwrap_or([120, 120, 120])
}

pub const TERRASCOPE_WMS: &str = "https://services.terrascope.be/wms/v2";

/// WorldCover year → Terrascope layer.
pub fn worldcover_layer(year: u16) -> &'static str {
    if year <= 2020 {
        "WORLDCOVER_2020_MAP"
    } else {
        "WORLDCOVER_2021_MAP"
    }
}

/// WMS 1.1.1 GetMap URL (EPSG:4326, lon/lat axis order) for a square window of half-width
/// `radius_m` around a point, `px` × `px` pixels.
pub fn getmap_url(base: &str, layer: &str, lat: f64, lon: f64, radius_m: f64, px: u32) -> String {
    let dlat = radius_m / 111_320.0;
    let dlon = radius_m / (111_320.0 * lat.to_radians().cos().max(0.01));
    let sep = if base.contains('?') { '&' } else { '?' };
    format!(
        "{base}{sep}SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS={layer}&STYLES=&SRS=EPSG:4326&BBOX={:.6},{:.6},{:.6},{:.6}&WIDTH={px}&HEIGHT={px}&FORMAT=image/png&TRANSPARENT=true",
        lon - dlon,
        lat - dlat,
        lon + dlon,
        lat + dlat
    )
}

/// A decoded window: class codes (0 = no data), row-major from the north-west corner.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub w: usize,
    pub h: usize,
    pub cls: Vec<u8>,
}

/// Nearest WorldCover class of an RGB colour (`0` when nothing is close).
pub fn classify_rgb(c: [u8; 3]) -> u8 {
    let mut best = (u32::MAX, 0u8);
    for (code, _, col) in CLASSES {
        let d: u32 = (0..3)
            .map(|k| (c[k] as i32 - col[k] as i32).pow(2) as u32)
            .sum();
        if d < best.0 {
            best = (d, code);
        }
    }
    if best.0 <= 45 * 45 { best.1 } else { 0 }
}

/// Decode a WMS PNG into class codes.
pub fn decode(bytes: &[u8]) -> Result<Window, String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(160)]).to_string();
        format!("not a PNG ({e}): {head}")
    })?;
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("PNG too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    let mut cls = Vec::with_capacity(w * h);
    for y in 0..h {
        let row = &buf[y * info.line_size..];
        for x in 0..w {
            let code = match info.color_type {
                png::ColorType::Rgb => {
                    let p = &row[x * 3..x * 3 + 3];
                    classify_rgb([p[0], p[1], p[2]])
                }
                png::ColorType::Rgba => {
                    let p = &row[x * 4..x * 4 + 4];
                    if p[3] < 128 {
                        0
                    } else {
                        classify_rgb([p[0], p[1], p[2]])
                    }
                }
                // grey values = class codes (raw-value WMS)
                png::ColorType::Grayscale => row[x],
                png::ColorType::GrayscaleAlpha => {
                    if row[x * 2 + 1] < 128 {
                        0
                    } else {
                        row[x * 2]
                    }
                }
                png::ColorType::Indexed => return Err("unexpanded palette PNG".into()),
            };
            cls.push(if class_index(code).is_some() { code } else { 0 });
        }
    }
    Ok(Window { w, h, cls })
}

/// Land cover around one fix.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// Index of the fix in its track.
    pub fix: usize,
    /// Class at the fix (centre pixel; 0 = no data).
    pub at: u8,
    /// Class proportions inside the error circle and inside the availability circle.
    pub used: [f64; NC],
    pub avail: [f64; NC],
}

/// Proportions of each class inside the circle of radius `r_m` about the window centre
/// (`radius_m` = window half-width). Returns `None` when the circle has no data.
fn composition(win: &Window, radius_m: f64, r_m: f64) -> Option<[f64; NC]> {
    let mut c = [0.0; NC];
    let mut n = 0.0;
    let px = 2.0 * radius_m / win.w.max(1) as f64;
    let r = r_m.max(0.5 * px);
    for y in 0..win.h {
        for x in 0..win.w {
            let dx = ((x as f64 + 0.5) / win.w as f64 * 2.0 - 1.0) * radius_m;
            let dy = ((y as f64 + 0.5) / win.h as f64 * 2.0 - 1.0) * radius_m;
            if dx * dx + dy * dy > r * r {
                continue;
            }
            if let Some(i) = class_index(win.cls[y * win.w + x]) {
                c[i] += 1.0;
                n += 1.0;
            }
        }
    }
    if n == 0.0 {
        // tiny circle between pixel centres: nearest pixel
        if r_m < px * 2.0 {
            let (x, y) = (win.w / 2, win.h / 2);
            let i = class_index(*win.cls.get(y * win.w + x)?)?;
            c[i] = 1.0;
            return Some(c);
        }
        return None;
    }
    for v in c.iter_mut() {
        *v /= n;
    }
    Some(c)
}

/// Used (error circle) and available (radius circle) composition of one window.
pub fn sample(fix: usize, win: &Window, radius_m: f64, err_m: Option<f64>) -> Option<Sample> {
    let at = *win.cls.get((win.h / 2) * win.w + win.w / 2)?;
    let avail = composition(win, radius_m, radius_m)?;
    let used = composition(win, radius_m, err_m.unwrap_or(0.0).min(radius_m))?;
    Some(Sample {
        fix,
        at,
        used,
        avail,
    })
}

/// Selection ratio of one class.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ratio {
    pub code: u8,
    pub used: f64,
    pub avail: f64,
    pub w: f64,
    pub lo: f64,
    pub hi: f64,
}

/// Manly selection ratios over the samples, with bootstrap 95 % intervals.
pub fn selection(samples: &[Sample]) -> Vec<Ratio> {
    let n = samples.len();
    if n == 0 {
        return Vec::new();
    }
    let mean = |pick: &dyn Fn(usize) -> usize| -> ([f64; NC], [f64; NC]) {
        let (mut u, mut a) = ([0.0; NC], [0.0; NC]);
        for k in 0..n {
            let s = &samples[pick(k)];
            for c in 0..NC {
                u[c] += s.used[c] / n as f64;
                a[c] += s.avail[c] / n as f64;
            }
        }
        (u, a)
    };
    let (u, a) = mean(&|k| k);
    // bootstrap (deterministic xorshift)
    let mut st = 0x9E37_79B9_7F4A_7C15u64 ^ n as u64;
    let mut rnd = move || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st
    };
    let b = 400;
    let mut boot: Vec<Vec<f64>> = (0..NC).map(|_| Vec::with_capacity(b)).collect();
    for _ in 0..b {
        let idx: Vec<usize> = (0..n).map(|_| (rnd() % n as u64) as usize).collect();
        let (bu, ba) = mean(&|k| idx[k]);
        for c in 0..NC {
            if ba[c] > 0.0 {
                boot[c].push(bu[c] / ba[c]);
            }
        }
    }
    (0..NC)
        .filter(|&c| a[c] > 0.0 || u[c] > 0.0)
        .map(|c| {
            let w = if a[c] > 0.0 {
                u[c] / a[c]
            } else {
                f64::INFINITY
            };
            let v = &mut boot[c];
            v.sort_by(|x, y| x.total_cmp(y));
            let q = |p: f64| {
                if v.is_empty() {
                    f64::NAN
                } else {
                    v[((p * (v.len() - 1) as f64).round() as usize).min(v.len() - 1)]
                }
            };
            Ratio {
                code: CLASSES[c].0,
                used: u[c],
                avail: a[c],
                w,
                lo: q(0.025),
                hi: q(0.975),
            }
        })
        .collect()
}

/// CSV of the selection ratios (one row per class).
pub fn selection_csv(track: &str, radius_m: f64, n: usize, r: &[Ratio]) -> String {
    let mut s = String::from(
        "individual_local_identifier,class_code,class,used_prop,available_prop,selection_ratio,ci_low,ci_high,n_fixes,availability_radius_m\n",
    );
    for x in r {
        s.push_str(&format!(
            "{track},{},{},{:.4},{:.4},{:.3},{:.3},{:.3},{n},{radius_m:.0}\n",
            x.code,
            class_name(x.code),
            x.used,
            x.avail,
            x.w,
            x.lo,
            x.hi
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_rgba(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            let mut d = Vec::new();
            for y in 0..h {
                for x in 0..w {
                    d.extend_from_slice(&f(x, y));
                }
            }
            wr.write_image_data(&d).unwrap();
        }
        out
    }

    #[test]
    fn url_and_palette() {
        let u = getmap_url(
            TERRASCOPE_WMS,
            worldcover_layer(2021),
            48.0,
            9.0,
            1000.0,
            64,
        );
        assert!(u.starts_with("https://services.terrascope.be/wms/v2?SERVICE=WMS"));
        assert!(u.contains("LAYERS=WORLDCOVER_2021_MAP") && u.contains("SRS=EPSG:4326"));
        assert!(u.contains("BBOX=8.98"), "{u}");
        assert_eq!(worldcover_layer(2020), "WORLDCOVER_2020_MAP");
        for (code, _, c) in CLASSES {
            assert_eq!(classify_rgb(c), code);
        }
        assert_eq!(classify_rgb([10, 10, 10]), 0);
        assert_eq!(classify_rgb([2, 98, 3]), 10);
    }

    #[test]
    fn decode_and_sample() {
        // west half tree cover, east half cropland, transparent corner
        let bytes = png_rgba(40, 40, |x, y| {
            if x < 2 && y < 2 {
                [0, 0, 0, 0]
            } else if x < 20 {
                [0, 100, 0, 255]
            } else {
                [240, 150, 255, 255]
            }
        });
        let w = decode(&bytes).unwrap();
        assert_eq!((w.w, w.h), (40, 40));
        assert_eq!(w.cls[0], 0);
        assert_eq!(w.cls[5 * 40 + 5], 10);
        assert_eq!(w.cls[5 * 40 + 30], 40);
        let s = sample(3, &w, 1000.0, Some(10.0)).unwrap();
        assert_eq!(s.fix, 3);
        assert_eq!(s.at, 40); // centre pixel (20, 20) is in the east half
        let (tree, crop) = (class_index(10).unwrap(), class_index(40).unwrap());
        assert!((s.avail[tree] - 0.5).abs() < 0.05 && (s.avail[crop] - 0.5).abs() < 0.05);
        assert!((s.used[crop] - 1.0).abs() < 1e-9);
        assert!(decode(b"<ServiceException>nope</ServiceException>").is_err());
    }

    #[test]
    fn selection_ratios() {
        let (tree, crop) = (class_index(10).unwrap(), class_index(40).unwrap());
        let mk = |i: usize, used_tree: bool| {
            let mut u = [0.0; NC];
            let mut a = [0.0; NC];
            u[if used_tree { tree } else { crop }] = 1.0;
            a[tree] = 0.2;
            a[crop] = 0.8;
            Sample {
                fix: i,
                at: if used_tree { 10 } else { 40 },
                used: u,
                avail: a,
            }
        };
        // 60 % of fixes in trees although trees are 20 % of what's available
        let s: Vec<Sample> = (0..100).map(|i| mk(i, i % 5 < 3)).collect();
        let r = selection(&s);
        let t = r.iter().find(|x| x.code == 10).unwrap();
        let c = r.iter().find(|x| x.code == 40).unwrap();
        assert!((t.w - 3.0).abs() < 1e-9 && (c.w - 0.5).abs() < 1e-9);
        assert!(t.lo > 1.0 && c.hi < 1.0, "{t:?} {c:?}");
        let csv = selection_csv("bat1", 1000.0, 100, &r);
        assert!(
            csv.lines()
                .nth(1)
                .unwrap()
                .starts_with("bat1,10,Tree cover")
        );
    }
}
