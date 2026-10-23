//! Geodesy, solar position, barometric altitude and wind-vector helpers.

pub const EARTH_R: f64 = 6_371_008.8;

pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_R * a.sqrt().asin()
}

/// Initial great-circle bearing, degrees clockwise from north, 0..360.
pub fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

/// Great-circle interpolation between two points, `w` in 0..1.
pub fn interpolate(lat1: f64, lon1: f64, lat2: f64, lon2: f64, w: f64) -> (f64, f64) {
    let d = haversine_m(lat1, lon1, lat2, lon2) / EARTH_R;
    if d < 1e-9 {
        return (lat1, lon1);
    }
    let (p1, l1, p2, l2) = (
        lat1.to_radians(),
        lon1.to_radians(),
        lat2.to_radians(),
        lon2.to_radians(),
    );
    let a = ((1.0 - w) * d).sin() / d.sin();
    let b = (w * d).sin() / d.sin();
    let x = a * p1.cos() * l1.cos() + b * p2.cos() * l2.cos();
    let y = a * p1.cos() * l1.sin() + b * p2.cos() * l2.sin();
    let z = a * p1.sin() + b * p2.sin();
    (
        z.atan2((x * x + y * y).sqrt()).to_degrees(),
        y.atan2(x).to_degrees(),
    )
}

/// Point reached travelling `dist_m` from (lat, lon) on `bearing` degrees.
pub fn destination(lat: f64, lon: f64, bearing: f64, dist_m: f64) -> (f64, f64) {
    let d = dist_m / EARTH_R;
    let (p1, l1, th) = (lat.to_radians(), lon.to_radians(), bearing.to_radians());
    let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * th.cos()).asin();
    let l2 = l1 + (th.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
    (p2.to_degrees(), ((l2.to_degrees() + 540.0) % 360.0) - 180.0)
}

/// Solar elevation angle (degrees) — NOAA approximation, good to ~0.5°.
pub fn sun_elevation_deg(t_unix: i64, lat: f64, lon: f64) -> f64 {
    let jd = t_unix as f64 / 86400.0 + 2440587.5;
    let jc = (jd - 2451545.0) / 36525.0;
    let l0 = (280.46646 + jc * (36000.76983 + jc * 0.0003032)) % 360.0;
    let m = 357.52911 + jc * (35999.05029 - 0.0001537 * jc);
    let e = 0.016708634 - jc * (0.000042037 + 0.0000001267 * jc);
    let mr = m.to_radians();
    let c = mr.sin() * (1.914602 - jc * (0.004817 + 0.000014 * jc))
        + (2.0 * mr).sin() * (0.019993 - 0.000101 * jc)
        + (3.0 * mr).sin() * 0.000289;
    let true_long = l0 + c;
    let omega = 125.04 - 1934.136 * jc;
    let app_long = true_long - 0.00569 - 0.00478 * omega.to_radians().sin();
    let eps0 =
        23.0 + (26.0 + (21.448 - jc * (46.815 + jc * (0.00059 - jc * 0.001813))) / 60.0) / 60.0;
    let eps = eps0 + 0.00256 * omega.to_radians().cos();
    let decl = (eps.to_radians().sin() * app_long.to_radians().sin()).asin();
    let y = (eps.to_radians() / 2.0).tan().powi(2);
    let l0r = l0.to_radians();
    let eq_time = 4.0
        * (y * (2.0 * l0r).sin() - 2.0 * e * mr.sin() + 4.0 * e * y * mr.sin() * (2.0 * l0r).cos()
            - 0.5 * y * y * (4.0 * l0r).sin()
            - 1.25 * e * e * (2.0 * mr).sin())
        .to_degrees();
    let minutes = (t_unix.rem_euclid(86400)) as f64 / 60.0;
    let tst = (minutes + eq_time + 4.0 * lon).rem_euclid(1440.0);
    let ha = if tst / 4.0 < 0.0 {
        tst / 4.0 + 180.0
    } else {
        tst / 4.0 - 180.0
    };
    let latr = lat.to_radians();
    let cos_zen = latr.sin() * decl.sin() + latr.cos() * decl.cos() * ha.to_radians().cos();
    90.0 - cos_zen.clamp(-1.0, 1.0).acos().to_degrees()
}

/// Civil-twilight night test (sun more than 6° below the horizon).
pub fn is_night(t_unix: i64, lat: f64, lon: f64) -> bool {
    sun_elevation_deg(t_unix, lat, lon) < -6.0
}

/// ISA hypsometric altitude (m) from pressure (hPa), with reference sea-level pressure p0.
/// Same formula as `SigfoxTagPrep::pressure_to_altitude_m()`.
pub fn pressure_to_altitude_m(p_hpa: f64, p0_hpa: f64) -> f64 {
    44330.0 * (1.0 - (p_hpa / p0_hpa).powf(1.0 / 5.255))
}

/// Meteorological (from-direction, degrees) speed/direction → (u, v) towards east / north.
pub fn wind_uv(speed: f64, from_deg: f64) -> (f64, f64) {
    let r = from_deg.to_radians();
    (-speed * r.sin(), -speed * r.cos())
}

/// (wind support, crosswind) for heading `heading_deg` (degrees the animal travels *to*).
/// Crosswind is positive when the wind blows from the animal's left.
pub fn wind_components(u: f64, v: f64, heading_deg: f64) -> (f64, f64) {
    let h = heading_deg.to_radians();
    let support = u * h.sin() + v * h.cos();
    let cross = u * h.cos() - v * h.sin();
    (support, cross)
}

/// Airspeed = |ground velocity − wind velocity|.
pub fn airspeed(ground_speed: f64, heading_deg: f64, u: f64, v: f64) -> f64 {
    let h = heading_deg.to_radians();
    let (gx, gy) = (ground_speed * h.sin(), ground_speed * h.cos());
    ((gx - u).powi(2) + (gy - v).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances_and_bearings() {
        // Konstanz -> Radolfzell ~ 19 km, roughly west-north-west
        let d = haversine_m(47.6779, 9.1732, 47.7369, 8.9697);
        assert!((d - 16_700.0).abs() < 1_500.0, "{d}");
        let b = bearing_deg(47.6779, 9.1732, 47.7369, 8.9697);
        assert!(b > 270.0 && b < 310.0, "{b}");
        let (la, lo) = destination(47.0, 9.0, 90.0, 10_000.0);
        assert!((la - 47.0).abs() < 0.01 && lo > 9.1);
        let (mla, mlo) = interpolate(47.0, 9.0, 47.0, 10.0, 0.5);
        assert!((mlo - 9.5).abs() < 0.01 && mla > 47.0);
    }

    #[test]
    fn wind() {
        // Wind from the north (0°) at 10 m/s blows towards the south: v = -10.
        let (u, v) = wind_uv(10.0, 0.0);
        assert!(u.abs() < 1e-9 && (v + 10.0).abs() < 1e-9);
        // Animal heading south gets a full tailwind.
        let (s, c) = wind_components(u, v, 180.0);
        assert!((s - 10.0).abs() < 1e-9 && c.abs() < 1e-9);
        // Animal heading east: wind from its left (north) → positive crosswind.
        let (s, c) = wind_components(u, v, 90.0);
        assert!(s.abs() < 1e-9 && (c - 10.0).abs() < 1e-9);
        // Ground speed 15 m/s south with 10 m/s tailwind → airspeed 5 m/s.
        assert!((airspeed(15.0, 180.0, u, v) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn sun() {
        // Konstanz, 2024-06-21 11:20 UTC ≈ solar noon: elevation ≈ 65.8°
        let noon = 1718968800; // 2024-06-21T11:20:00Z
        let e = sun_elevation_deg(noon, 47.66, 9.17);
        assert!((e - 65.8).abs() < 1.0, "{e}");
        // Midnight is night.
        assert!(is_night(1718928000 - 3600, 47.66, 9.17));
    }

    #[test]
    fn baro() {
        assert!(pressure_to_altitude_m(1013.25, 1013.25).abs() < 1e-6);
        let h = pressure_to_altitude_m(900.0, 1013.25);
        assert!((h - 988.0).abs() < 10.0, "{h}");
    }
}
