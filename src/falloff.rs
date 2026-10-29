//! Detect tags that fell off the animal: a port of `detect_tag_fell_off()` from
//! [SigfoxTagPrep](https://github.com/ehurme/SigfoxTagPrep).
//!
//! After the last *active* message, a tag that has fallen off keeps transmitting with a flat
//! activity signal. If that inactive run reaches the end of the track and is at least
//! `min_inactive_run` messages long, everything from the second inactive message on is
//! flagged (the first is kept: the tag may still have reached that position on the animal).
//!
//! Signal by tag type, as in the R defaults:
//! * **TinyFox** — 24 h activity % (`tinyfox_activity_percent_last_24h`) > 1; without it,
//!   the VeDBA counter rate (reset-safe diffs of the cumulative total per hour) > 2440 × 3.9.
//! * **NanoFox** (and anything else with VeDBA) — VeDBA per message > 0; a NanoFox without
//!   VeDBA is skipped, as in R.
//! * **uWasp / GPS / other** — distance from the previous fix > 0.5 km (conservative:
//!   Sigfox jitter looks like movement).
//!
//! A `tag_fell_off` column already computed in R (kept as a fix extra) takes precedence.

use crate::model::{SensorKind, Track};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub min_inactive_run: usize,
    pub keep_first_inactive: bool,
    /// NanoFox VeDBA per message strictly above this is active.
    pub vedba_threshold: f32,
    /// TinyFox 24 h activity % strictly above this is active.
    pub activity_threshold: f32,
    /// TinyFox VeDBA counter increase per hour strictly above this is active.
    pub tinyfox_rate_threshold: f32,
    /// Counter rates above this are artefacts and ignored.
    pub tinyfox_max_rate: f32,
    /// Step distance (km) strictly above this is active (tags without activity data).
    pub dist_threshold_km: f32,
    /// Trust a `tag_fell_off` column from the file (SigfoxTagPrep output) when present.
    pub use_file_flag: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            min_inactive_run: 3,
            keep_first_inactive: true,
            vedba_threshold: 0.0,
            activity_threshold: 1.0,
            tinyfox_rate_threshold: 2440.0 * 3.9,
            tinyfox_max_rate: 50_000.0,
            dist_threshold_km: 0.5,
            use_file_flag: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FallOff {
    /// Data at or after this time (unix s) is after the tag fell off.
    pub cut_t: i64,
    /// Time of the last active message.
    pub last_active_t: Option<i64>,
    /// Inactive messages from the last active one to the end.
    pub inactive_run: usize,
    pub signal: &'static str,
}

/// One message: its time and whether the animal was active.
type Row = (i64, bool);

fn is_tinyfox(tr: &Track) -> bool {
    tr.tag.to_lowercase().contains("tinyfox")
}

/// The activity series the R function would use for this track (sorted by time).
fn signal(tr: &Track, p: &Params) -> Option<(Vec<Row>, &'static str)> {
    let samples = |k: SensorKind| -> Vec<(i64, f32)> {
        let mut v: Vec<(i64, f32)> = tr
            .sensors
            .iter()
            .filter(|s| s.kind == k && s.value.is_finite())
            .map(|s| (s.t, s.value))
            .collect();
        v.sort_by_key(|x| x.0);
        v.dedup_by_key(|x| x.0);
        v
    };
    if is_tinyfox(tr) {
        let act = samples(SensorKind::Activity);
        if !act.is_empty() {
            let rows = act
                .iter()
                .map(|&(t, a)| (t, a > p.activity_threshold))
                .collect();
            return Some((rows, "24 h activity %"));
        }
        // Cumulative VeDBA counter → reset-safe rate per hour between messages.
        let tot: Vec<(i64, f64)> = tr
            .fixes
            .iter()
            .filter_map(|f| {
                let v = f.extra.iter().find(|e| e.0 == "tinyfox_total_vedba")?;
                Some((f.t, v.1.parse::<f64>().ok()?))
            })
            .collect();
        if tot.len() >= 2 {
            let mut rows = vec![(tot[0].0, false)];
            for w in tot.windows(2) {
                let (dt_h, d) = ((w[1].0 - w[0].0) as f64 / 3600.0, w[1].1 - w[0].1);
                let rate = (dt_h > 0.0 && d >= 0.0).then(|| d / dt_h);
                let active = rate.is_some_and(|r| {
                    r <= p.tinyfox_max_rate as f64 && r > p.tinyfox_rate_threshold as f64
                });
                rows.push((w[1].0, active));
            }
            return Some((rows, "VeDBA counter rate"));
        }
    } else {
        // NanoFox: VeDBA summed per message when the fixes carry it (Wildcloud),
        // else the separate VeDBA rows (Movebank: one per message window).
        let with_act = tr.fixes.iter().filter(|f| f.activity.is_some()).count();
        if with_act > 0 && with_act * 2 >= tr.fixes.len() {
            let rows = tr
                .fixes
                .iter()
                .filter_map(|f| Some((f.t, f.activity? > p.vedba_threshold)))
                .collect();
            return Some((rows, "VeDBA per message"));
        }
        let v = samples(SensorKind::Vedba);
        if !v.is_empty() {
            // Separate VeDBA rows are 36-min bins (several per message): sum them per
            // message, i.e. per fix over (previous fix, this fix], so the run length
            // counts messages as in R. Bins after the last fix form one more message.
            let mut rows = Vec::new();
            let mut k = 0;
            let mut prev_t = i64::MIN;
            for f in &tr.fixes {
                let (mut sum, mut any) = (0.0, false);
                while k < v.len() && v[k].0 <= f.t {
                    if v[k].0 > prev_t {
                        sum += v[k].1;
                        any = true;
                    }
                    k += 1;
                }
                if any {
                    rows.push((f.t, sum > p.vedba_threshold));
                }
                prev_t = f.t;
            }
            if k < v.len() {
                let sum: f32 = v[k..].iter().map(|x| x.1).sum();
                rows.push((v[v.len() - 1].0, sum > p.vedba_threshold));
            }
            return Some((rows, "VeDBA per message"));
        }
        // NanoFox without any VeDBA: no usable signal (R skips these too).
        if tr.tag.to_lowercase().contains("nanofox") {
            return None;
        }
    }
    if tr.fixes.len() < 2 {
        return None;
    }
    let rows = tr
        .fixes
        .iter()
        .map(|f| {
            (
                f.t,
                f.step_m.is_some_and(|d| d / 1000.0 > p.dist_threshold_km),
            )
        })
        .collect();
    Some((rows, "distance between fixes"))
}

/// Where (if anywhere) this track's tag fell off.
pub fn detect(tr: &Track, p: &Params) -> Option<FallOff> {
    // A fall-off flag computed upstream (SigfoxTagPrep → Movebank) decides, including
    // "never fell off"; set-aside fixes count so a second run doesn't cut further.
    let flag = |f: &crate::model::Fix| {
        f.extra
            .iter()
            .find(|e| e.0 == "tag_fell_off")
            .map(|e| e.1.eq_ignore_ascii_case("true"))
    };
    if p.use_file_flag
        && tr
            .fixes
            .iter()
            .chain(&tr.excluded_fixes)
            .any(|f| flag(f).is_some())
    {
        let f = tr.fixes.iter().find(|f| flag(f) == Some(true))?;
        return Some(FallOff {
            cut_t: f.t,
            last_active_t: None,
            inactive_run: tr.fixes.iter().filter(|g| g.t >= f.t).count(),
            signal: "tag_fell_off column",
        });
    }
    let (rows, signal) = signal(tr, p)?;
    let n = rows.len();
    if n <= 1 {
        return None;
    }
    let first_inactive = rows.iter().rposition(|r| r.1).map_or(0, |i| i + 1);
    let run = n - first_inactive;
    if run == 0 || run < p.min_inactive_run.max(1) {
        return None;
    }
    let start = first_inactive + p.keep_first_inactive as usize;
    let cut_t = rows.get(start)?.0;
    Some(FallOff {
        cut_t,
        last_active_t: first_inactive.checked_sub(1).map(|i| rows[i].0),
        inactive_run: run,
        signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fix, LocationSource, SensorSample};

    fn track(tag: &str, n: usize) -> Track {
        let mut tr = Track::new("a", tag, "test");
        for i in 0..n {
            tr.fixes.push(Fix::new(
                i as i64 * 3600,
                47.0 + i as f64 * 0.1,
                9.0,
                LocationSource::SigfoxAtlas,
            ));
        }
        tr.finalize();
        tr
    }

    #[test]
    fn nanofox_flat_vedba_at_the_end() {
        let mut tr = track("NanoFox", 8);
        for (f, v) in tr
            .fixes
            .iter_mut()
            .zip([5.0, 3.0, 0.0, 4.0, 0.0, 0.0, 0.0, 0.0])
        {
            f.activity = Some(v);
        }
        let r = detect(&tr, &Params::default()).unwrap();
        // last active at row 3; rows 4.. inactive (4 rows); row 4 kept, cut at row 5
        assert_eq!(r.last_active_t, Some(3 * 3600));
        assert_eq!(r.inactive_run, 4);
        assert_eq!(r.cut_t, 5 * 3600);
        // a short flat tail is not enough
        tr.fixes[6].activity = Some(9.0);
        assert!(detect(&tr, &Params::default()).is_none());
        // active to the end: nothing to cut
        tr.fixes[7].activity = Some(1.0);
        assert!(detect(&tr, &Params::default()).is_none());
    }

    #[test]
    fn tinyfox_activity_first_then_counter() {
        let mut tr = track("TinyFox", 6);
        for (i, a) in [20.0, 15.0, 0.0, 0.5, 0.0, 0.0].into_iter().enumerate() {
            tr.sensors.push(SensorSample {
                t: i as i64 * 3600,
                kind: SensorKind::Activity,
                value: a,
            });
        }
        let r = detect(&tr, &Params::default()).unwrap();
        assert_eq!((r.cut_t, r.signal), (3 * 3600, "24 h activity %"));

        // no activity %: cumulative counter with a reset — reset is not "activity"
        tr.sensors.clear();
        for (f, tot) in tr.fixes.iter_mut().zip([0.0, 20_000.0, 2.0, 2.0, 3.0, 3.0]) {
            f.extra
                .push(("tinyfox_total_vedba".into(), format!("{tot}")));
        }
        let r = detect(&tr, &Params::default()).unwrap();
        assert_eq!(r.signal, "VeDBA counter rate");
        assert_eq!(r.last_active_t, Some(3600));
        assert_eq!(r.cut_t, 3 * 3600);
    }

    #[test]
    fn distance_fallback_and_upstream_flag() {
        let mut tr = track("uWasp", 6);
        for i in 3..6 {
            tr.fixes[i].lat = tr.fixes[2].lat;
        }
        tr.compute_steps();
        let r = detect(&tr, &Params::default()).unwrap();
        assert_eq!((r.cut_t, r.signal), (4 * 3600, "distance between fixes"));
        tr.fixes[1]
            .extra
            .push(("tag_fell_off".into(), "TRUE".into()));
        assert_eq!(detect(&tr, &Params::default()).unwrap().cut_t, 3600);
    }
}
