//! Synthetic demo datasets, written in the *raw export formats* so they exercise the same
//! firmware parsers as real data: a Wildcloud NanoFox 30Days file (two Leisler's bats),
//! a Wildcloud TinyFox file (one common noctule) and a CTT GPS export (one bird).
//! Positions are simulated (nightly migration flights SW from Lake Constance) — not real data.

use crate::geo::{destination, is_night};
use chrono::{DateTime, Utc};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.next().max(1e-12), self.next());
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Hourly "true" path: (t, lat, lon, flying, altitude m).
fn simulate(
    seed: u64,
    start: (f64, f64),
    t0: i64,
    days: i64,
    heading: f64,
    mig_prob: f64,
) -> Vec<(i64, f64, f64, bool, f64)> {
    let mut rng = Rng(seed);
    let (mut lat, mut lon) = start;
    let mut out = Vec::new();
    let mut migrating_tonight = false;
    for h in 0..days * 24 {
        let t = t0 + h * 3600;
        let hour = (t.rem_euclid(86400)) / 3600;
        if hour == 18 {
            migrating_tonight = rng.next() < mig_prob;
        }
        let night = is_night(t, lat, lon);
        let flying = migrating_tonight && night && rng.next() < 0.85;
        let mut alt = 0.0;
        if flying {
            let speed_kmh = 30.0 + 12.0 * rng.normal().abs();
            let hd = heading + 20.0 * rng.normal();
            let (la, lo) = destination(lat, lon, hd, speed_kmh * 1000.0);
            lat = la;
            lon = lo;
            alt = 400.0 + 500.0 * rng.next() + 300.0 * rng.normal().abs();
        }
        out.push((t, lat, lon, flying, alt));
    }
    out
}

fn wc_time(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .map(|d| d.format("%d.%m.%Y, %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn iso(t: i64) -> String {
    DateTime::<Utc>::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn ground_pressure(rng: &mut Rng) -> f64 {
    965.0 + 6.0 * rng.normal()
}

fn alt_to_p(alt: f64, p_ground: f64) -> f64 {
    p_ground * (1.0 - alt / 44330.0).powf(5.255)
}

const T0: i64 = 1_756_684_800; // 2025-09-01 00:00 UTC

pub fn nanofox_wildcloud_csv() -> String {
    let mut s = String::from(
        "Device,Time (UTC),Raw Data,Position,Radius (m) (Source/Status),Sequence Number,LQI,Link Quality,Operator Name,Country Code,\"Base Stations (ID, RSSI, Reps)\",\
VeDBA sum 0 min ago,VeDBA sum 36 min ago,VeDBA sum 72 min ago,VeDBA sum 108 min ago,VeDBA sum 144 min ago,\
Average temperature 0 min ago,Average temperature 36 min ago,Average temperature 72 min ago,Average temperature 108 min ago,Average temperature 144 min ago,\
Min pressure of last 3 hrs (mbar),Min temperature of last 3 hrs (°C)\n",
    );
    for (dev, seed, start, heading) in [
        ("1B2C3D4", 7u64, (47.73, 8.97), 222.0),
        ("1B2C3E9", 11u64, (47.69, 9.18), 232.0),
    ] {
        let path = simulate(seed, start, T0, 21, heading, 0.45);
        let mut rng = Rng(seed * 31);
        let mut seq = 0;
        for (i, &(t, lat, lon, _, _)) in path.iter().enumerate() {
            let hour = (t.rem_euclid(86400)) / 3600;
            if hour % 3 != 0 || rng.next() < 0.18 {
                continue; // NanoFox 3-h schedule, ~18 % messages lost
            }
            seq += 1;
            let radius =
                [800.0, 1500.0, 3000.0, 6000.0, 12000.0, 20000.0][(rng.next() * 6.0) as usize];
            let (nla, nlo) = destination(
                lat,
                lon,
                rng.next() * 360.0,
                radius * 0.5 * rng.normal().abs(),
            );
            let window: Vec<_> = path[i.saturating_sub(3)..=i].to_vec();
            let pg = ground_pressure(&mut rng);
            let max_alt = window.iter().map(|w| w.4).fold(0.0, f64::max);
            let pmin = alt_to_p(max_alt, pg);
            let mut vedba = Vec::new();
            let mut temps = Vec::new();
            for k in 0..5 {
                let back = (k * 36) as usize / 60;
                let fl = path[i.saturating_sub(back)].3;
                vedba.push(if fl {
                    900.0 + 300.0 * rng.normal().abs()
                } else {
                    60.0 + 40.0 * rng.next()
                });
                temps.push(if fl {
                    31.0 + 2.0 * rng.normal()
                } else {
                    14.0 + 4.0 * rng.normal()
                });
            }
            let bs = format!(
                "4A1F,{:.0},2;3B22,{:.0},1",
                -100.0 - 30.0 * rng.next(),
                -110.0 - 25.0 * rng.next()
            );
            s.push_str(&format!(
                "{dev},\"{}\",0a1b2c,\"{:.5}, {:.5}\",{} (2/1),{seq},GOOD,{},SIGFOX_France_SAS,250,\"{}\",{},{},{:.1}, {}\n",
                wc_time(t),
                nla,
                nlo,
                radius as i64,
                1 + (rng.next() * 3.0) as i32,
                bs,
                vedba.iter().map(|v| format!("{v:.0}")).collect::<Vec<_>>().join(","),
                temps.iter().map(|v| format!("{v:.1}")).collect::<Vec<_>>().join(","),
                pmin,
                if temps.iter().cloned().fold(f64::MAX, f64::min) > 10.0 { ">10" } else { ">5 / <=10" }
            ));
        }
    }
    s
}

pub fn tinyfox_wildcloud_csv() -> String {
    let mut s = String::from(
        "Device,Time (UTC),Raw Data,Position,Radius (m) (Source/Status),Sequence Number,LQI,Total VeDBA,24h Active (%),24h Min. Temperature (°C),24h Max. Temperature (°C),24h Min. Pressure (mbar)\n",
    );
    let path = simulate(23, (47.80, 9.05), T0 + 2 * 86400, 24, 215.0, 0.4);
    let mut rng = Rng(99);
    let mut total = 125_000.0;
    let mut seq = 0;
    for day in 0..24 {
        let idx = (day * 24 + 12) as usize; // ~ noon transmission
        if idx >= path.len() || rng.next() < 0.1 {
            continue;
        }
        let (t, lat, lon, _, _) = path[idx];
        let lo_i = idx.saturating_sub(24);
        let flew = path[lo_i..=idx].iter().filter(|p| p.3).count();
        let max_alt = path[lo_i..=idx].iter().map(|p| p.4).fold(0.0, f64::max);
        total += 9_000.0 + 2_500.0 * flew as f64 + 1000.0 * rng.next();
        seq += 1;
        let radius = [1200.0, 2500.0, 5000.0, 9000.0][(rng.next() * 4.0) as usize];
        let (nla, nlo) = destination(
            lat,
            lon,
            rng.next() * 360.0,
            radius * 0.5 * rng.normal().abs(),
        );
        s.push_str(&format!(
            "B7F01A,\"{}\",ff00,\"{:.5}, {:.5}\",{} (1/1),{seq},AVERAGE,{:.0},{:.0},{:.1},{:.1},{:.1}\n",
            wc_time(t + (rng.next() * 1800.0) as i64),
            nla,
            nlo,
            radius as i64,
            total,
            3.0 + flew as f64 * 2.5,
            8.0 + 3.0 * rng.normal(),
            30.0 + 3.0 * rng.normal().abs(),
            alt_to_p(max_alt, ground_pressure(&mut rng)),
        ));
    }
    s
}

pub fn ctt_gps_csv() -> String {
    let mut s = String::from(
        "id (imei),alias,Time of Fix,Lat,Lng,Altitude,Speed,COG,HDOP,VDOP,Satellite Count,Fix Time,Error,Nav Type,Activity Index,Battery,Temp\n",
    );
    // A bird migrating by day: flip the night test by shifting time by 12 h.
    let raw = simulate(5, (47.95, 8.70), T0 + 86400, 14, 200.0, 0.6);
    let mut rng = Rng(5);
    let mut prev: Option<(f64, f64)> = None;
    for (k, &(t, lat, lon, flying, alt)) in raw.iter().enumerate() {
        if k % 2 != 0 {
            continue; // 2-h GPS schedule
        }
        let t = t + 12 * 3600;
        let (nla, nlo) = destination(lat, lon, rng.next() * 360.0, 6.0 * rng.normal().abs());
        let (spd, cog) = match prev {
            Some((pla, plo)) if flying => (
                crate::geo::haversine_m(pla, plo, nla, nlo) / 7200.0 / 0.514_444,
                crate::geo::bearing_deg(pla, plo, nla, nlo),
            ),
            _ => (0.0, 0.0),
        };
        prev = Some((nla, nlo));
        s.push_str(&format!(
            "86104305123456,Stork-07,{},{:.6},{:.6},{:.0},{:.1},{:.0},{:.1},{:.1},{},{},0,3,{},{},{:.1}\n",
            iso(t),
            nla,
            nlo,
            if flying { alt + 250.0 } else { 420.0 + 30.0 * rng.normal() },
            spd,
            cog,
            0.8 + rng.next(),
            1.2 + rng.next(),
            6 + (rng.next() * 6.0) as i32,
            10 + (rng.next() * 40.0) as i32,
            if flying { 80 } else { 5 },
            3900 + (rng.next() * 200.0) as i32,
            18.0 + 4.0 * rng.normal(),
        ));
    }
    s
}

pub fn all() -> Vec<(&'static str, String)> {
    vec![
        ("demo_wildcloud_nanofox_30days.csv", nanofox_wildcloud_csv()),
        ("demo_wildcloud_tinyfox.csv", tinyfox_wildcloud_csv()),
        ("demo_ctt_gps.csv", ctt_gps_csv()),
    ]
}
