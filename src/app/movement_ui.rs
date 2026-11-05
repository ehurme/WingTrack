//! Side-panel UI for the on-demand movement analyses ([`crate::movement`]) and land cover /
//! habitat selection ([`crate::landcover`]). Nothing here runs until the user presses a button;
//! results are kept per track (invalidated when the track's fixes change) and written to
//! [`Fix::mv`] so they can colour the track, be charted and exported.

use super::*;
use crate::landcover as lc;
use crate::movement::{self as mv, CpVar, ErrorMode, Obs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LcTime {
    All,
    Night,
    Day,
}

impl LcTime {
    const ALL: [LcTime; 3] = [LcTime::All, LcTime::Night, LcTime::Day];
    fn label(self) -> &'static str {
        match self {
            LcTime::All => "all fixes",
            LcTime::Night => "night fixes (sun below horizon)",
            LcTime::Day => "day fixes (roosts)",
        }
    }
}

/// One home-range contour to draw: colour, width, dashed, (lat, lon) segments.
pub(super) type HrLine = (Color32, f32, bool, Vec<[(f64, f64); 2]>);

/// Home-range methods (bit flags in the share link).
pub(super) const HR_METHODS: [&str; 4] = ["AKDE-like", "KDE", "MCP", "Gaussian (model)"];

pub(super) struct MoveUi {
    /// Side-panel section open at start (UI tests).
    pub open: bool,
    /// Track analysed (track key); `None` = selected / first visible.
    pub track: Option<String>,
    /// Run on every visible track at once.
    pub all: bool,
    pub err_mode: ErrorMode,
    pub q: f64,
    pub hr_methods: [bool; 4],
    pub show_hr: bool,
    /// FPT radius in km; 0 = the peak of the scan.
    pub fpt_r_km: f64,
    pub hmm_k: usize,
    pub cp_var: CpVar,
    pub cp_mult: f64,
    pub cp_min: usize,
    pub lc_year: u16,
    pub lc_url: String,
    pub lc_layer: String,
    pub lc_radius_m: f64,
    pub lc_max: usize,
    pub lc_time: LcTime,
    pub results: HashMap<String, TrackMove>,
    pub lc_queue: VecDeque<LcJob>,
    pub lc_inflight: usize,
    pub lc_run: u64,
}

impl Default for MoveUi {
    fn default() -> Self {
        MoveUi {
            open: false,
            track: None,
            all: false,
            err_mode: ErrorMode::FromFixes,
            q: 0.95,
            hr_methods: [true, false, true, false],
            show_hr: true,
            fpt_r_km: 0.0,
            hmm_k: 2,
            cp_var: CpVar::Displacement,
            cp_mult: 1.0,
            cp_min: 5,
            lc_year: 2021,
            lc_url: lc::TERRASCOPE_WMS.into(),
            lc_layer: lc::worldcover_layer(2021).into(),
            lc_radius_m: 1000.0,
            lc_max: 200,
            lc_time: LcTime::All,
            results: HashMap::new(),
            lc_queue: VecDeque::new(),
            lc_inflight: 0,
            lc_run: 0,
        }
    }
}

/// Results for one track.
#[derive(Default)]
pub(super) struct TrackMove {
    /// Fixes used (count, first and last time) — results are dropped when this changes.
    pub fp: (usize, i64, i64),
    pub proj: Option<mv::Proj>,
    pub vario: Vec<mv::VarioPoint>,
    pub fits: Vec<mv::Fit>,
    pub mean_err: f64,
    pub thinned: bool,
    /// (method, contours, CI of the area for the Gaussian)
    pub hr: Vec<(&'static str, Vec<mv::Contour>)>,
    pub naive: Option<(f64, f64)>,
    pub fpt_scan: Vec<(f64, f64, usize)>,
    pub fpt_r: Option<f64>,
    pub hmm: Option<mv::Hmm>,
    pub cp: Option<mv::Segments>,
    pub lc_samples: Vec<lc::Sample>,
    pub lc_ratios: Vec<lc::Ratio>,
    pub lc_pending: usize,
    pub lc_failed: usize,
    pub lc_err: Option<String>,
    /// Land-cover run the pending requests belong to.
    pub lc_run: u64,
}

pub(super) struct LcJob {
    key: String,
    fix: usize,
    err: Option<f64>,
    url: String,
    run: u64,
}

pub(super) fn fmt_dur(s: f64) -> String {
    if !s.is_finite() {
        "∞".into()
    } else if s < 120.0 {
        format!("{s:.0} s")
    } else if s < 2.0 * 3600.0 {
        format!("{:.1} min", s / 60.0)
    } else if s < 2.0 * 86400.0 {
        format!("{:.1} h", s / 3600.0)
    } else {
        format!("{:.1} d", s / 86400.0)
    }
}

pub(super) fn fmt_area(m2: f64) -> String {
    if !m2.is_finite() {
        "–".into()
    } else if m2 < 1e5 {
        format!("{:.1} ha", m2 / 1e4)
    } else if m2 < 1e8 {
        format!("{:.2} km²", m2 / 1e6)
    } else {
        format!("{:.0} km²", m2 / 1e6)
    }
}

fn fmt_ci(e: &mv::Est, f: &dyn Fn(f64) -> String) -> String {
    if e.lo.is_finite() && e.hi.is_finite() {
        format!("{} ({} – {})", f(e.est), f(e.lo), f(e.hi))
    } else {
        f(e.est)
    }
}

/// Up to 20 000 fixes go into the model fits (evenly thinned beyond).
const FIT_MAX: usize = 20_000;

impl WingApp {
    /// Fixes used by the analyses: inside the date filter and the max-error filter.
    fn mv_fixes(&self, ti: usize) -> Vec<usize> {
        let max_e = self.max_error_km * 1000.0;
        self.tracks[ti]
            .fixes
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                f.lat.is_finite() && f.lon.is_finite() && !f.error_m.is_some_and(|e| e > max_e)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn mv_fp(&self, ti: usize) -> (usize, i64, i64) {
        let idx = self.mv_fixes(ti);
        let f = &self.tracks[ti].fixes;
        (
            idx.len(),
            idx.first().map(|&i| f[i].t).unwrap_or(0),
            idx.last().map(|&i| f[i].t).unwrap_or(0),
        )
    }

    /// (fix indices, projection, observations) of a track.
    fn mv_obs(&self, ti: usize) -> (Vec<usize>, mv::Proj, Vec<Obs>) {
        let idx = self.mv_fixes(ti);
        let f = &self.tracks[ti].fixes;
        let pts: Vec<(i64, f64, f64, Option<f32>)> = idx
            .iter()
            .map(|&i| (f[i].t, f[i].lat, f[i].lon, f[i].error_m))
            .collect();
        let (proj, obs) = mv::observations(&pts);
        (idx, proj, obs)
    }

    /// Result slot of a track, reset when its fixes changed.
    fn mv_slot(&mut self, ti: usize) -> &mut TrackMove {
        let key = self.track_key(ti);
        let fp = self.mv_fp(ti);
        let r = self.mv_ui.results.entry(key).or_default();
        if r.fp != fp {
            *r = TrackMove {
                fp,
                ..Default::default()
            };
        }
        r
    }

    /// Tracks the buttons act on.
    fn mv_targets(&self) -> Vec<usize> {
        if self.mv_ui.all {
            return (0..self.tracks.len())
                .filter(|&i| self.tracks[i].visible)
                .collect();
        }
        self.mv_current().into_iter().collect()
    }

    fn mv_current(&self) -> Option<usize> {
        if let Some(k) = &self.mv_ui.track
            && let Some(i) = (0..self.tracks.len()).find(|&i| &self.track_key(i) == k)
        {
            return Some(i);
        }
        self.selected_track
            .or_else(|| (0..self.tracks.len()).find(|&i| self.tracks[i].visible))
    }

    fn run_models(&mut self, ti: usize) {
        let (_, proj, obs) = self.mv_obs(ti);
        let id = self.tracks[ti].id.clone();
        if obs.len() < 10 {
            self.status(format!(
                "{id}: too few fixes for movement models ({})",
                obs.len()
            ));
            return;
        }
        let err = self.mv_ui.err_mode;
        let thinned = obs.len() > FIT_MAX;
        let fobs: Vec<Obs> = if thinned {
            (0..FIT_MAX)
                .map(|k| obs[k * (obs.len() - 1) / (FIT_MAX - 1)])
                .collect()
        } else {
            obs.clone()
        };
        let vario = mv::variogram(&obs);
        let fits = mv::select(&fobs, err);
        let mean_err = mv::mean_known_error(&obs, err);
        let naive = mv::naive_speed(&obs);
        let msg = match fits.first() {
            Some(b) => format!(
                "{id}: best model {} (σ = {:.0} m, τp = {}{}), 95 % area {}",
                b.model.short(),
                b.sigma.sqrt(),
                fmt_dur(b.tau_p),
                if b.model == mv::Model::Ouf {
                    format!(", τv = {}", fmt_dur(b.tau_v))
                } else {
                    String::new()
                },
                fmt_area(b.area(0.95).est)
            ),
            None => format!("{id}: model fit failed"),
        };
        let r = self.mv_slot(ti);
        r.proj = Some(proj);
        r.vario = vario;
        r.fits = fits;
        r.mean_err = mean_err;
        r.thinned = thinned;
        r.naive = naive;
        self.status(msg);
    }

    fn run_home_range(&mut self, ti: usize) {
        let (_, proj, obs) = self.mv_obs(ti);
        if obs.len() < 5 {
            return;
        }
        let q = self.mv_ui.q;
        let m = self.mv_ui.hr_methods;
        // the AKDE-like bandwidth and the Gaussian range need the model fit
        if (m[0] || m[3]) && self.mv_slot(ti).fits.is_empty() {
            self.run_models(ti);
        }
        let best = self.mv_slot(ti).fits.first().cloned();
        let mut hr: Vec<(&'static str, Vec<mv::Contour>)> = Vec::new();
        let sd = mv::sample_sd(&obs);
        if m[0]
            && let Some(b) = &best
        {
            let neff = b.dof_area();
            let n = if neff.is_finite() {
                neff.max(2.0)
            } else {
                obs.len() as f64
            };
            let h = mv::reference_bandwidth(b.sigma.sqrt(), n);
            hr.push((HR_METHODS[0], mv::kde(&obs, &proj, h, &[0.5, q])));
        }
        if m[1] {
            let h = mv::reference_bandwidth(sd, obs.len() as f64);
            hr.push((HR_METHODS[1], mv::kde(&obs, &proj, h, &[0.5, q])));
        }
        if m[2]
            && let Some(c) = mv::mcp(&obs, &proj, q)
        {
            hr.push((HR_METHODS[2], vec![c]));
        }
        if m[3]
            && let Some(b) = &best
        {
            hr.push((
                HR_METHODS[3],
                vec![mv::circle(&proj, b.mean, b.radius(q), q)],
            ));
        }
        let id = self.tracks[ti].id.clone();
        let txt: Vec<String> = hr
            .iter()
            .filter_map(|(n, c)| c.last().map(|c| format!("{n} {}", fmt_area(c.area_m2))))
            .collect();
        let r = self.mv_slot(ti);
        r.proj = Some(proj);
        r.hr = hr;
        self.status(format!(
            "{id}: {:.0} % home range — {}",
            q * 100.0,
            txt.join(", ")
        ));
    }

    fn run_fpt(&mut self, ti: usize) {
        let (idx, _, obs) = self.mv_obs(ti);
        if obs.len() < 5 {
            return;
        }
        let scan = mv::fpt_scan(&obs);
        let r_m = if self.mv_ui.fpt_r_km > 0.0 {
            self.mv_ui.fpt_r_km * 1000.0
        } else {
            scan.iter()
                .filter(|s| s.1.is_finite() && s.2 >= 10)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|s| s.0)
                .unwrap_or(1000.0)
        };
        let f = mv::fpt(&obs, r_m);
        let tr = &mut self.tracks[ti];
        for x in tr.fixes.iter_mut().filter_map(|x| x.mv.as_mut()) {
            x.fpt_h = None;
        }
        for (k, v) in f.iter().enumerate() {
            if let Some(v) = v {
                tr.fixes[idx[k]].mv_mut().fpt_h = Some((*v / 3600.0) as f32);
            }
        }
        let id = tr.id.clone();
        let n = f.iter().flatten().count();
        let r = self.mv_slot(ti);
        r.fpt_scan = scan;
        r.fpt_r = Some(r_m);
        self.status(format!(
            "{id}: first-passage time at r = {:.2} km for {n} fixes",
            r_m / 1000.0
        ));
    }

    fn run_hmm(&mut self, ti: usize) {
        let (idx, _, obs) = self.mv_obs(ti);
        let id = self.tracks[ti].id.clone();
        let Some(h) = mv::hmm(&obs, self.mv_ui.hmm_k) else {
            self.status(format!("{id}: HMM needs more steps with movement"));
            return;
        };
        let tr = &mut self.tracks[ti];
        for x in tr.fixes.iter_mut().filter_map(|x| x.mv.as_mut()) {
            x.hmm_state = None;
        }
        for (k, s) in h.path.iter().enumerate() {
            if let Some(s) = s {
                tr.fixes[idx[k]].mv_mut().hmm_state = Some(*s);
            }
        }
        self.status(format!(
            "{id}: {}-state HMM, log-likelihood {:.1}, state speeds {}",
            h.states.len(),
            h.loglik,
            h.states
                .iter()
                .map(|s| sig3(s.speed_mean))
                .collect::<Vec<_>>()
                .join(" / ")
                + " m/s"
        ));
        self.mv_slot(ti).hmm = Some(h);
    }

    fn run_cp(&mut self, ti: usize) {
        let (idx, _, obs) = self.mv_obs(ti);
        let id = self.tracks[ti].id.clone();
        let Some(s) = mv::change_points(
            &obs,
            self.mv_ui.cp_var,
            self.mv_ui.cp_mult,
            self.mv_ui.cp_min,
        ) else {
            self.status(format!("{id}: too few fixes for change points"));
            return;
        };
        let tr = &mut self.tracks[ti];
        for x in tr.fixes.iter_mut().filter_map(|x| x.mv.as_mut()) {
            x.segment = None;
        }
        for (k, g) in s.seg.iter().enumerate() {
            if let Some(g) = g {
                tr.fixes[idx[k]].mv_mut().segment = Some(*g);
            }
        }
        self.status(format!(
            "{id}: {} segments ({})",
            s.parts.len(),
            s.var.label()
        ));
        self.mv_slot(ti).cp = Some(s);
    }

    fn start_landcover(&mut self, ti: usize) {
        let idx = self.mv_fixes(ti);
        let key = self.track_key(ti);
        let tr = &self.tracks[ti];
        let time = self.mv_ui.lc_time;
        let cand: Vec<usize> = idx
            .into_iter()
            .filter(|&i| {
                let f = &tr.fixes[i];
                match time {
                    LcTime::All => true,
                    LcTime::Night => geo::is_night(f.t, f.lat, f.lon),
                    LcTime::Day => !geo::is_night(f.t, f.lat, f.lon),
                }
            })
            .collect();
        if cand.is_empty() {
            self.status(format!("{}: no fixes to sample", tr.id));
            return;
        }
        let max = self.mv_ui.lc_max.max(1);
        let pick: Vec<usize> = if cand.len() <= max {
            cand
        } else {
            (0..max)
                .map(|k| cand[k * (cand.len() - 1) / (max - 1).max(1)])
                .collect()
        };
        let r = self.mv_ui.lc_radius_m;
        // ~10 m pixels for small windows, at most 160 px
        let px = ((2.0 * r / 10.0).ceil() as u32).clamp(32, 160);
        self.mv_ui.lc_run += 1;
        let run = self.mv_ui.lc_run;
        let (url, layer) = (self.mv_ui.lc_url.clone(), self.mv_ui.lc_layer.clone());
        let jobs: Vec<LcJob> = pick
            .iter()
            .map(|&i| {
                let f = &tr.fixes[i];
                LcJob {
                    key: key.clone(),
                    fix: i,
                    err: f.error_m.map(|e| e as f64),
                    url: lc::getmap_url(&url, &layer, f.lat, f.lon, r, px),
                    run,
                }
            })
            .collect();
        let n = jobs.len();
        let id = tr.id.clone();
        for f in self.tracks[ti]
            .fixes
            .iter_mut()
            .filter_map(|x| x.mv.as_mut())
        {
            f.landcover = None;
        }
        self.mv_ui.lc_queue.retain(|j| j.key != key);
        self.mv_ui.lc_queue.extend(jobs);
        let slot = self.mv_slot(ti);
        slot.lc_samples.clear();
        slot.lc_ratios.clear();
        slot.lc_pending = n;
        slot.lc_failed = 0;
        slot.lc_err = None;
        slot.lc_run = run;
        self.status(format!("{id}: sampling land cover around {n} fixes"));
    }

    pub(super) fn pump_landcover(&mut self, ctx: &egui::Context) {
        while self.mv_ui.lc_inflight < 4 {
            let Some(job) = self.mv_ui.lc_queue.pop_front() else {
                return;
            };
            self.mv_ui.lc_inflight += 1;
            let (tx, ctx2) = (self.tx.clone(), ctx.clone());
            let req = ehttp::Request::get(&job.url);
            let LcJob {
                key, fix, err, run, ..
            } = job;
            ehttp::fetch(req, move |res| {
                let result = match res {
                    Ok(r) if r.ok => Ok(r.bytes),
                    Ok(r) => Err(format!(
                        "HTTP {} {}",
                        r.status,
                        r.text()
                            .map(|t| t.chars().take(200).collect::<String>())
                            .unwrap_or_default()
                    )),
                    Err(e) => Err(e),
                };
                let _ = tx.send(Msg::LandCover {
                    key,
                    fix,
                    err,
                    run,
                    result,
                });
                ctx2.request_repaint();
            });
        }
    }

    pub(super) fn on_landcover(
        &mut self,
        key: String,
        fix: usize,
        err: Option<f64>,
        run: u64,
        result: Result<Vec<u8>, String>,
    ) {
        self.mv_ui.lc_inflight = self.mv_ui.lc_inflight.saturating_sub(1);
        // superseded by a newer run for the same track, or cancelled
        if self.mv_ui.results.get(&key).is_none_or(|r| r.lc_run != run) {
            return;
        }
        let Some(ti) = (0..self.tracks.len()).find(|&i| self.track_key(i) == key) else {
            return;
        };
        let radius = self.mv_ui.lc_radius_m;
        let sample = result.and_then(|b| lc::decode(&b)).and_then(|w| {
            lc::sample(fix, &w, radius, err).ok_or_else(|| "no land-cover data".into())
        });
        if let Ok(s) = &sample
            && let Some(f) = self.tracks[ti].fixes.get_mut(fix)
        {
            f.mv_mut().landcover = (s.at != 0).then_some(s.at);
        }
        let id = self.tracks[ti].id.clone();
        let Some(slot) = self.mv_ui.results.get_mut(&key) else {
            return;
        };
        slot.lc_pending = slot.lc_pending.saturating_sub(1);
        match sample {
            Ok(s) => slot.lc_samples.push(s),
            Err(e) => {
                slot.lc_failed += 1;
                slot.lc_err.get_or_insert(e);
            }
        }
        if slot.lc_pending == 0 {
            slot.lc_ratios = lc::selection(&slot.lc_samples);
            let (n, failed, e) = (slot.lc_samples.len(), slot.lc_failed, slot.lc_err.clone());
            if n == 0 {
                self.status(format!(
                    "{id}: land cover failed for all fixes ({}). If the browser blocks the WMS \
                     (CORS), use the desktop app or another WMS.",
                    e.unwrap_or_default()
                ));
            } else {
                self.status(format!(
                    "{id}: land cover at {n} fixes{}",
                    if failed > 0 {
                        format!(" ({failed} failed)")
                    } else {
                        String::new()
                    }
                ));
            }
        }
    }

    /// Home-range contours to draw: (colour, width, dashed, segments).
    pub(super) fn home_range_draw(&self) -> Vec<HrLine> {
        if !self.mv_ui.show_hr {
            return Vec::new();
        }
        let mut out = Vec::new();
        for i in 0..self.tracks.len() {
            let tr = &self.tracks[i];
            if !tr.visible || self.selected_track.is_some_and(|s| s != i) {
                continue;
            }
            let Some(r) = self.mv_ui.results.get(&self.track_key(i)) else {
                continue;
            };
            if r.fp != self.mv_fp(i) {
                continue;
            }
            let c = Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]);
            for (m, cs) in &r.hr {
                for (k, ct) in cs.iter().enumerate() {
                    // the 50 % core thinner than the outer contour
                    let core = cs.len() > 1 && k == 0;
                    let w = match (*m, core) {
                        (_, true) => 1.2,
                        ("AKDE-like", _) => 2.8,
                        _ => 1.8,
                    };
                    out.push((c, w, *m == "MCP", ct.segs.clone()));
                }
            }
        }
        out
    }

    pub(super) fn movement_panel(&mut self, ui: &mut egui::Ui) {
        if self.tracks.is_empty() {
            ui.weak("Load tracks first.");
            return;
        }
        ui.weak(
            "Nothing here runs by itself: pick a track and press the buttons. Analyses use the \
             fixes inside the date filter and the max-error filter.",
        );
        // ---- track
        let names: Vec<(String, String)> = (0..self.tracks.len())
            .map(|i| (self.track_key(i), self.tracks[i].id.clone()))
            .collect();
        let cur = self.mv_current();
        ui.horizontal(|ui| {
            ui.label("track");
            let txt = cur.map(|i| names[i].1.clone()).unwrap_or_default();
            egui::ComboBox::from_id_salt("mv_track")
                .selected_text(txt)
                .show_ui(ui, |ui| {
                    for (k, n) in &names {
                        if ui
                            .selectable_label(cur.is_some_and(|c| &names[c].0 == k), n)
                            .clicked()
                        {
                            self.mv_ui.track = Some(k.clone());
                        }
                    }
                });
            ui.checkbox(&mut self.mv_ui.all, "all visible")
                .on_hover_text("Run each analysis on every visible track");
        });
        let Some(ti) = cur else {
            return;
        };
        let n_fix = self.mv_fixes(ti).len();
        let fp = self.mv_fp(ti);
        let key = self.track_key(ti);
        let stale = self.mv_ui.results.get(&key).is_some_and(|r| r.fp != fp);
        if stale {
            ui.colored_label(
                Color32::from_rgb(255, 180, 80),
                "The track's fixes changed since the last run – results cleared.",
            );
            self.mv_ui.results.remove(&key);
        }
        ui.weak(format!("{n_fix} fixes"));

        // ---- models
        ui.separator();
        ui.strong("Autocorrelated movement models (ctmm-style)");
        ui.horizontal(|ui| {
            ui.label("location error");
            egui::ComboBox::from_id_salt("mv_err")
                .selected_text(self.mv_ui.err_mode.label())
                .show_ui(ui, |ui| {
                    for m in ErrorMode::ALL {
                        ui.selectable_value(&mut self.mv_ui.err_mode, m, m.label());
                    }
                });
        });
        if ui
            .button("Fit variogram + IID / OU / OUF")
            .on_hover_text(
                "Empirical variogram, then maximum-likelihood fits of three continuous-time \
                 models (Kalman filter, irregular sampling, location error) compared by AICc. \
                 Gives the home-range crossing time, autocorrelation-corrected home-range area \
                 and (OUF) mean speed with 95 % CIs.",
            )
            .clicked()
        {
            for t in self.mv_targets() {
                self.run_models(t);
            }
        }
        self.models_ui(ui, &key);

        // ---- home range
        ui.separator();
        ui.strong("Home range");
        ui.horizontal(|ui| {
            ui.label("coverage");
            let mut pct = self.mv_ui.q * 100.0;
            if ui
                .add(
                    egui::DragValue::new(&mut pct)
                        .range(50.0..=99.9)
                        .speed(0.5)
                        .suffix(" %"),
                )
                .changed()
            {
                self.mv_ui.q = pct / 100.0;
            }
            ui.checkbox(&mut self.mv_ui.show_hr, "show on map");
        });
        ui.horizontal_wrapped(|ui| {
            for (k, m) in HR_METHODS.iter().enumerate() {
                ui.checkbox(&mut self.mv_ui.hr_methods[k], *m).on_hover_text(match k {
                    0 => "KDE with the Gaussian reference bandwidth for the model's effective sample size (DOF) instead of N: the autocorrelation correction of AKDE. Fits the models first if needed.",
                    1 => "Classic KDE, Gaussian reference bandwidth with all N fixes (too small a range for autocorrelated data)",
                    2 => "Minimum convex polygon of the fixes closest to the centroid",
                    _ => "Circle of the fitted Gaussian range (with CI in the model table)",
                });
            }
        });
        if ui.button("Compute home range").clicked() {
            for t in self.mv_targets() {
                self.run_home_range(t);
            }
        }
        if let Some(r) = self.mv_ui.results.get(&key)
            && !r.hr.is_empty()
        {
            egui::Grid::new("mv_hr").striped(true).show(ui, |ui| {
                ui.strong("method");
                ui.strong("50 %");
                ui.strong(format!("{:.0} %", self.mv_ui.q * 100.0));
                ui.end_row();
                for (m, cs) in &r.hr {
                    ui.label(*m);
                    if cs.len() > 1 {
                        ui.label(fmt_area(cs[0].area_m2));
                    } else {
                        ui.label("");
                    }
                    ui.label(cs.last().map(|c| fmt_area(c.area_m2)).unwrap_or_default());
                    ui.end_row();
                }
            });
        }

        // ---- segmentation
        ui.separator();
        ui.strong("Segmentation");
        ui.horizontal(|ui| {
            ui.label("FPT radius");
            ui.add(
                egui::DragValue::new(&mut self.mv_ui.fpt_r_km)
                    .range(0.0..=1000.0)
                    .speed(0.05)
                    .suffix(" km"),
            )
            .on_hover_text("0 = the radius where the variance of log FPT peaks (scale of area-restricted search)");
            if ui
                .button("First-passage time")
                .on_hover_text("Fauchald & Tveraa (2003): time to leave a circle around each fix. Colour by / chart 'FPT'.")
                .clicked()
            {
                for t in self.mv_targets() {
                    self.run_fpt(t);
                }
            }
        });
        if let Some(r) = self.mv_ui.results.get(&key)
            && !r.fpt_scan.is_empty()
        {
            let pts: Vec<[f64; 2]> = r
                .fpt_scan
                .iter()
                .filter(|s| s.1.is_finite())
                .map(|s| [s.0 / 1000.0, s.1])
                .collect();
            let rr = r.fpt_r.unwrap_or(0.0) / 1000.0;
            Plot::new("mv_fpt_scan")
                .height(110.0)
                .allow_scroll(false)
                .allow_drag(false)
                .allow_zoom(false)
                .x_axis_label("radius (km)")
                .y_axis_label("Var ln FPT")
                .show(ui, |pui| {
                    pui.line(Line::new("var ln FPT", PlotPoints::from(pts)).width(1.5));
                    pui.vline(VLine::new("radius used", rr).color(Color32::from_rgb(255, 80, 80)));
                });
        }
        ui.horizontal(|ui| {
            ui.label("HMM states");
            ui.add(egui::DragValue::new(&mut self.mv_ui.hmm_k).range(2..=3));
            if ui
                .button("Fit HMM")
                .on_hover_text("Hidden Markov model on step speed (zero-inflated gamma) and turning angle (von Mises), EM + Viterbi. States sorted by speed: 1 = slowest.")
                .clicked()
            {
                for t in self.mv_targets() {
                    self.run_hmm(t);
                }
            }
        });
        if let Some(h) = self.mv_ui.results.get(&key).and_then(|r| r.hmm.as_ref()) {
            egui::Grid::new("mv_hmm").striped(true).show(ui, |ui| {
                for t in ["state", "speed m/s", "zero", "turn κ", "stay"] {
                    ui.strong(t);
                }
                ui.end_row();
                for (k, s) in h.states.iter().enumerate() {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().rect_filled(r, 2.0, hmm_color(k as u8));
                        ui.label(format!("{}", k + 1));
                    });
                    ui.label(format!("{} ± {}", sig3(s.speed_mean), sig3(s.speed_sd)));
                    ui.label(format!("{:.0} %", s.zero_p * 100.0));
                    ui.label(format!("{:.2}", s.kappa));
                    ui.label(format!("{:.0} %", h.trans[k][k] * 100.0));
                    ui.end_row();
                }
            });
            ui.weak(format!(
                "log-lik {:.1} · AIC {:.1} · BIC {:.1} · {} EM iterations",
                h.loglik, h.aic, h.bic, h.iterations
            ));
            if h.dt_cv > 0.5 {
                ui.colored_label(
                    Color32::from_rgb(255, 180, 80),
                    format!(
                        "Irregular sampling (CV of time steps {:.1}): HMMs assume regular steps; speeds are used instead of step lengths, but states of long gaps are uncertain.",
                        h.dt_cv
                    ),
                );
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("change points in");
            egui::ComboBox::from_id_salt("mv_cp")
                .selected_text(self.mv_ui.cp_var.label())
                .show_ui(ui, |ui| {
                    for v in CpVar::ALL {
                        ui.selectable_value(&mut self.mv_ui.cp_var, v, v.label());
                    }
                });
            ui.label("penalty ×");
            ui.add(egui::DragValue::new(&mut self.mv_ui.cp_mult).range(0.1..=50.0).speed(0.05))
                .on_hover_text("Multiplier of the BIC-type penalty 2σ²ln n: larger = fewer segments");
            ui.label("min length");
            ui.add(egui::DragValue::new(&mut self.mv_ui.cp_min).range(2..=500));
            if ui
                .button("Find change points")
                .on_hover_text("Penalised least-squares change points in the mean (PELT, exact). Net displacement finds departures / arrivals of migration; log speed finds behavioural modes.")
                .clicked()
            {
                for t in self.mv_targets() {
                    self.run_cp(t);
                }
            }
        });
        if let Some(s) = self.mv_ui.results.get(&key).and_then(|r| r.cp.as_ref()) {
            let fixes = &self.tracks[ti].fixes;
            let idx = self.mv_fixes(ti);
            egui::ScrollArea::vertical()
                .id_salt("mv_cp_list")
                .max_height(120.0)
                .show(ui, |ui| {
                    for (k, (a, b, m)) in s.parts.iter().enumerate() {
                        let (ta, tb) = (
                            idx.get(*a).map(|&i| fixes[i].t).unwrap_or(0),
                            idx.get(*b).map(|&i| fixes[i].t).unwrap_or(0),
                        );
                        let val = match s.var {
                            CpVar::LogSpeed => format!("{:.2} m/s", m.exp()),
                            CpVar::Displacement => format!("{m:.1} km"),
                        };
                        ui.label(format!(
                            "{}  {} – {}  {val}",
                            k + 1,
                            fmt_time(ta),
                            fmt_time(tb)
                        ));
                    }
                });
        }

        // ---- land cover
        ui.separator();
        ui.strong("Land cover & habitat selection");
        ui.horizontal_wrapped(|ui| {
            ui.label("ESA WorldCover");
            for y in [2020u16, 2021] {
                if ui
                    .selectable_label(
                        self.mv_ui.lc_year == y && self.mv_ui.lc_url == lc::TERRASCOPE_WMS,
                        y.to_string(),
                    )
                    .clicked()
                {
                    self.mv_ui.lc_year = y;
                    self.mv_ui.lc_url = lc::TERRASCOPE_WMS.into();
                    self.mv_ui.lc_layer = lc::worldcover_layer(y).into();
                }
            }
        });
        egui::CollapsingHeader::new("WMS source")
            .default_open(false)
            .show(ui, |ui| {
                ui.weak("Any WMS 1.1.1 serving the WorldCover palette (or grey = class code).");
                ui.horizontal(|ui| {
                    ui.label("URL");
                    ui.text_edit_singleline(&mut self.mv_ui.lc_url);
                });
                ui.horizontal(|ui| {
                    ui.label("layer");
                    ui.text_edit_singleline(&mut self.mv_ui.lc_layer);
                });
            });
        ui.horizontal_wrapped(|ui| {
            ui.label("availability radius");
            ui.add(
                egui::DragValue::new(&mut self.mv_ui.lc_radius_m)
                    .range(50.0..=20_000.0)
                    .speed(10.0)
                    .suffix(" m"),
            )
            .on_hover_text("Available = land cover within this distance of each fix (e.g. a nightly foraging radius)");
            ui.label("max fixes");
            ui.add(egui::DragValue::new(&mut self.mv_ui.lc_max).range(10..=2000));
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("mv_lct")
                .selected_text(self.mv_ui.lc_time.label())
                .show_ui(ui, |ui| {
                    for v in LcTime::ALL {
                        ui.selectable_value(&mut self.mv_ui.lc_time, v, v.label());
                    }
                });
            let busy = !self.mv_ui.lc_queue.is_empty() || self.mv_ui.lc_inflight > 0;
            if ui
                .add_enabled(!busy, egui::Button::new("Sample land cover"))
                .on_hover_text("One small WMS image per fix (evenly thinned to 'max fixes'): class at the fix, used = classes in the error circle, available = classes within the radius. Selection ratios (Manly) with bootstrap CIs.")
                .clicked()
            {
                for t in self.mv_targets() {
                    self.start_landcover(t);
                }
            }
            if busy {
                ui.spinner();
                ui.weak(format!(
                    "{} to go",
                    self.mv_ui.lc_queue.len() + self.mv_ui.lc_inflight
                ));
                if ui.small_button("Cancel").clicked() {
                    self.mv_ui.lc_queue.clear();
                    for r in self.mv_ui.results.values_mut() {
                        if r.lc_pending > 0 {
                            r.lc_pending = 0;
                            r.lc_run = 0;
                            r.lc_ratios = lc::selection(&r.lc_samples);
                        }
                    }
                }
            }
        });
        if let Some(r) = self.mv_ui.results.get(&key)
            && !r.lc_ratios.is_empty()
        {
            ui.weak(format!(
                "{} fixes sampled{}",
                r.lc_samples.len(),
                if r.lc_failed > 0 {
                    format!(", {} failed", r.lc_failed)
                } else {
                    String::new()
                }
            ));
            egui::Grid::new("mv_lc").striped(true).show(ui, |ui| {
                for t in ["class", "used", "avail.", "w (95 % CI)"] {
                    ui.strong(t);
                }
                ui.end_row();
                for x in &r.lc_ratios {
                    ui.horizontal(|ui| {
                        let (rr, _) =
                            ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                        let c = lc::class_color(x.code);
                        ui.painter()
                            .rect_filled(rr, 2.0, Color32::from_rgb(c[0], c[1], c[2]));
                        ui.label(lc::class_name(x.code));
                    });
                    ui.label(format!("{:.0} %", x.used * 100.0));
                    ui.label(format!("{:.0} %", x.avail * 100.0));
                    let txt = format!("{:.2} ({:.2}–{:.2})", x.w, x.lo, x.hi);
                    if x.lo > 1.0 {
                        ui.colored_label(Color32::from_rgb(120, 220, 120), txt + " selected");
                    } else if x.hi < 1.0 {
                        ui.colored_label(Color32::from_rgb(255, 140, 120), txt + " avoided");
                    } else {
                        ui.label(txt);
                    }
                    ui.end_row();
                }
            });
        } else if let Some(e) = self.mv_ui.results.get(&key).and_then(|r| r.lc_err.as_ref())
            && self
                .mv_ui
                .results
                .get(&key)
                .is_some_and(|r| r.lc_samples.is_empty() && r.lc_pending == 0)
        {
            ui.colored_label(Color32::from_rgb(255, 180, 80), format!("failed: {e}"));
        }

        // ---- export
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Export movement summary (CSV)")
                .on_hover_text(
                    "One row per analysed track: models, speed, home ranges, HMM states, segments",
                )
                .clicked()
            {
                let csv = self.movement_summary_csv();
                self.save_csv("wingtrack_movement_summary.csv", csv);
            }
            let any_lc = self.mv_ui.results.values().any(|r| !r.lc_ratios.is_empty());
            if any_lc && ui.button("Export selection ratios (CSV)").clicked() {
                let mut csv = String::new();
                for i in 0..self.tracks.len() {
                    if let Some(r) = self.mv_ui.results.get(&self.track_key(i))
                        && !r.lc_ratios.is_empty()
                    {
                        let part = lc::selection_csv(
                            &self.tracks[i].id,
                            self.mv_ui.lc_radius_m,
                            r.lc_samples.len(),
                            &r.lc_ratios,
                        );
                        if csv.is_empty() {
                            csv = part;
                        } else {
                            csv.extend(part.lines().skip(1).map(|l| format!("{l}\n")));
                        }
                    }
                }
                self.save_csv("wingtrack_habitat_selection.csv", csv);
            }
        });
        ui.weak("Per-fix HMM state, segment, FPT and land cover are in the annotated CSV export, 'Colour by' and the chart list.");
    }

    /// Model table + variogram plot.
    fn models_ui(&mut self, ui: &mut egui::Ui, key: &str) {
        let Some(r) = self.mv_ui.results.get(key) else {
            return;
        };
        if r.fits.is_empty() {
            return;
        }
        let best = &r.fits[0];
        egui::Grid::new("mv_models").striped(true).show(ui, |ui| {
            ui.strong("model");
            ui.strong("ΔAICc");
            ui.strong("τp");
            ui.strong("τv");
            ui.end_row();
            for f in &r.fits {
                ui.label(f.model.short()).on_hover_text(f.model.label());
                ui.label(format!("{:.1}", f.aicc - best.aicc));
                ui.label(if f.model == mv::Model::Iid {
                    "–".into()
                } else {
                    fmt_dur(f.tau_p)
                });
                ui.label(if f.model == mv::Model::Ouf {
                    fmt_dur(f.tau_v)
                } else {
                    "–".into()
                });
                ui.end_row();
            }
        });
        let q = self.mv_ui.q;
        ui.label(format!(
            "Best: {} · N = {} · effective N (DOF area) ≈ {:.1}",
            best.model.short(),
            best.n,
            best.dof_area()
        ));
        ui.label(format!(
            "Gaussian {:.0} % area: {}",
            q * 100.0,
            fmt_ci(&best.area(q), &fmt_area)
        ));
        if let Some(t) = best.tau_p_est().filter(|_| best.model != mv::Model::Iid) {
            ui.label(format!(
                "τ position (range crossing): {}",
                fmt_ci(&t, &fmt_dur)
            ));
        }
        if let Some(t) = best.tau_v_est().filter(|_| best.model == mv::Model::Ouf) {
            ui.label(format!("τ velocity: {}", fmt_ci(&t, &fmt_dur)));
        }
        match best.speed() {
            Some(s) => {
                ui.label(format!(
                    "Mean speed (OUF): {} · {}",
                    fmt_ci(&s, &|v| format!("{v:.2} m/s")),
                    fmt_ci(&s.scaled(86.4), &|v| format!("{v:.1} km/day"))
                ));
            }
            None => {
                ui.weak("Speed: not identifiable (velocity autocorrelation not resolved by the sampling – OUF not selected).");
            }
        }
        if let Some((v, d)) = r.naive {
            ui.weak(format!(
                "Straight-line speed between fixes: {v:.2} m/s · {:.1} km/day (biased by sampling interval and error)",
                d / 1000.0
            ));
        }
        if let Some(e) = best.error_rms() {
            ui.label(format!(
                "Estimated location error (RMS): {}",
                fmt_ci(&e, &|v| format!("{v:.0} m"))
            ));
        }
        if !best.range_resident() {
            ui.colored_label(
                Color32::from_rgb(255, 180, 80),
                "Not range resident: τp is long compared with the tracking period, so the home range is not meaningful. Restrict the date filter to a resident period (e.g. before / after migration).",
            );
        }
        if r.thinned {
            ui.weak(format!("Model fit on {FIT_MAX} evenly thinned fixes."));
        }
        // variogram
        if r.vario.is_empty() {
            return;
        }
        let max_lag = r.vario.last().map(|p| p.lag_s).unwrap_or(1.0);
        let (unit, div) = if max_lag > 4.0 * 86400.0 {
            ("days", 86400.0)
        } else {
            ("hours", 3600.0)
        };
        let gmax = r.vario.iter().map(|p| p.gamma).fold(0.0, f64::max);
        let (yu, ydiv) = if gmax > 1e5 {
            ("km²", 1e6)
        } else {
            ("m²", 1.0)
        };
        let emp: Vec<[f64; 2]> = r
            .vario
            .iter()
            .map(|p| [p.lag_s / div, p.gamma / ydiv])
            .collect();
        let curves: Vec<(String, Vec<[f64; 2]>)> = r
            .fits
            .iter()
            .map(|f| {
                (
                    f.model.short().to_string(),
                    (0..=120)
                        .map(|k| {
                            let lag = max_lag * k as f64 / 120.0;
                            [lag / div, f.gamma(lag, r.mean_err) / ydiv]
                        })
                        .collect(),
                )
            })
            .collect();
        Plot::new("mv_vario")
            .height(150.0)
            .legend(Legend::default())
            .allow_scroll(false)
            .x_axis_label(format!("lag ({unit})"))
            .y_axis_label(format!("semi-variance ({yu})"))
            .show(ui, |pui| {
                pui.points(
                    Points::new("empirical", PlotPoints::from(emp))
                        .radius(2.5)
                        .color(Color32::from_gray(220)),
                );
                let cols = [
                    Color32::from_rgb(150, 150, 150),
                    Color32::from_rgb(86, 180, 233),
                    Color32::from_rgb(230, 159, 0),
                ];
                for (k, (n, c)) in curves.into_iter().enumerate() {
                    let col = match n.as_str() {
                        "IID" => cols[0],
                        "OU" => cols[1],
                        _ => cols[2],
                    };
                    pui.line(
                        Line::new(n, PlotPoints::from(c))
                            .color(col)
                            .width(if k == 0 { 2.2 } else { 1.0 }),
                    );
                }
            });
        ui.weak("The variogram should level off (range residency); its early rise shows velocity autocorrelation (OUF) and the intercept the location error.");
    }

    /// `WINGTRACK_MOVE=1`: run every analysis on the longest track and show it.
    pub(super) fn run_movement_test_env(&mut self) {
        let ti = (0..self.tracks.len())
            .max_by_key(|&i| self.tracks[i].fixes.len())
            .unwrap_or(0);
        self.mv_ui.track = Some(self.track_key(ti));
        self.mv_ui.open = true;
        self.run_models(ti);
        self.run_home_range(ti);
        self.run_fpt(ti);
        self.run_hmm(ti);
        self.run_cp(ti);
        self.selected_track = Some(ti);
        self.color_by = ColorBy::HmmState;
        self.chart_var = ChartVar::Move(MoveMetric::Hmm);
        self.chart_subplots = vec![
            ChartVar::Move(MoveMetric::Displacement),
            ChartVar::GroundSpeed,
        ];
        self.fit_pending = true;
    }

    pub(super) fn movement_summary_csv(&self) -> String {
        let mut s = String::from(
            "individual_local_identifier,n_fixes,start,end,best_model,delta_aicc_next,sigma_m,tau_position_h,tau_position_low_h,tau_position_high_h,tau_velocity_min,tau_velocity_low_min,tau_velocity_high_min,dof_area,gaussian_area_q,gaussian_area_km2,gaussian_area_low_km2,gaussian_area_high_km2,speed_ms,speed_low_ms,speed_high_ms,speed_km_day,straight_line_speed_ms,range_resident,location_error_mode,error_rms_m,akde_area_km2,kde_area_km2,mcp_area_km2,hmm_states,hmm_state_speeds_ms,hmm_loglik,n_segments,segment_variable,fpt_radius_km,landcover_fixes\n",
        );
        let o = |v: Option<f64>, k: f64| -> String {
            v.filter(|x| x.is_finite())
                .map(|x| format!("{:.4}", x * k))
                .unwrap_or_default()
        };
        for i in 0..self.tracks.len() {
            let Some(r) = self.mv_ui.results.get(&self.track_key(i)) else {
                continue;
            };
            let tr = &self.tracks[i];
            let b = r.fits.first();
            let q = self.mv_ui.q;
            let area = b.map(|b| b.area(q));
            let tp = b
                .and_then(|b| b.tau_p_est())
                .filter(|_| b.is_some_and(|b| b.model != mv::Model::Iid));
            let tv = b
                .and_then(|b| b.tau_v_est())
                .filter(|_| b.is_some_and(|b| b.model == mv::Model::Ouf));
            let sp = b.and_then(|b| b.speed());
            let hr = |m: &str| {
                r.hr.iter()
                    .find(|h| h.0 == m)
                    .and_then(|h| h.1.last())
                    .map(|c| c.area_m2)
            };
            let cells: Vec<String> = vec![
                csv_cell(&tr.id),
                r.fp.0.to_string(),
                fmt_time(r.fp.1),
                fmt_time(r.fp.2),
                b.map(|b| b.model.short().to_string()).unwrap_or_default(),
                o(r.fits.get(1).zip(b).map(|(n, b)| n.aicc - b.aicc), 1.0),
                o(b.map(|b| b.sigma.sqrt()), 1.0),
                o(tp.map(|e| e.est), 1.0 / 3600.0),
                o(tp.map(|e| e.lo), 1.0 / 3600.0),
                o(tp.map(|e| e.hi), 1.0 / 3600.0),
                o(tv.map(|e| e.est), 1.0 / 60.0),
                o(tv.map(|e| e.lo), 1.0 / 60.0),
                o(tv.map(|e| e.hi), 1.0 / 60.0),
                o(b.map(|b| b.dof_area()), 1.0),
                if b.is_some() {
                    format!("{q}")
                } else {
                    String::new()
                },
                o(area.map(|a| a.est), 1e-6),
                o(area.map(|a| a.lo), 1e-6),
                o(area.map(|a| a.hi), 1e-6),
                o(sp.map(|e| e.est), 1.0),
                o(sp.map(|e| e.lo), 1.0),
                o(sp.map(|e| e.hi), 1.0),
                o(sp.map(|e| e.est), 86.4),
                o(r.naive.map(|n| n.0), 1.0),
                b.map(|b| b.range_resident().to_string())
                    .unwrap_or_default(),
                b.map(|b| format!("{:?}", b.err_mode)).unwrap_or_default(),
                o(b.and_then(|b| b.error_rms()).map(|e| e.est), 1.0),
                o(hr(HR_METHODS[0]), 1e-6),
                o(hr(HR_METHODS[1]), 1e-6),
                o(hr(HR_METHODS[2]), 1e-6),
                r.hmm
                    .as_ref()
                    .map(|h| h.states.len().to_string())
                    .unwrap_or_default(),
                r.hmm
                    .as_ref()
                    .map(|h| {
                        h.states
                            .iter()
                            .map(|s| format!("{:.3}", s.speed_mean))
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .unwrap_or_default(),
                o(r.hmm.as_ref().map(|h| h.loglik), 1.0),
                r.cp.as_ref()
                    .map(|c| c.parts.len().to_string())
                    .unwrap_or_default(),
                r.cp.as_ref()
                    .map(|c| format!("{:?}", c.var))
                    .unwrap_or_default(),
                o(r.fpt_r, 1e-3),
                if r.lc_samples.is_empty() {
                    String::new()
                } else {
                    r.lc_samples.len().to_string()
                },
            ];
            s.push_str(&cells.join(","));
            s.push('\n');
        }
        s
    }

    pub(super) fn mv_default_share() -> String {
        let u = MoveUi::default();
        mv_share_of(&u)
    }

    /// Share-link value of the analysis settings (the analyses themselves are not re-run).
    pub(super) fn mv_share(&self) -> String {
        mv_share_of(&self.mv_ui)
    }
    pub(super) fn mv_apply(&mut self, v: &[f64]) {
        if let [e, q, m, show, fr, k, cv, cm, cmin, year, rad, max, time, ..] = v[..] {
            let u = &mut self.mv_ui;
            u.err_mode = ErrorMode::ALL[(e as usize).min(2)];
            u.q = q.clamp(0.5, 0.999);
            for (b, on) in u.hr_methods.iter_mut().enumerate() {
                *on = (m as u32 >> b) & 1 == 1;
            }
            u.show_hr = show != 0.0;
            u.fpt_r_km = fr.max(0.0);
            u.hmm_k = (k as usize).clamp(2, 3);
            u.cp_var = CpVar::ALL[(cv as usize).min(1)];
            u.cp_mult = cm.clamp(0.1, 50.0);
            u.cp_min = (cmin as usize).clamp(2, 500);
            u.lc_year = if year <= 2020.0 { 2020 } else { 2021 };
            u.lc_layer = lc::worldcover_layer(u.lc_year).into();
            u.lc_radius_m = rad.clamp(50.0, 20_000.0);
            u.lc_max = (max as usize).clamp(10, 2000);
            u.lc_time = LcTime::ALL[(time as usize).min(2)];
        }
    }
}

fn mv_share_of(u: &MoveUi) -> String {
    let methods = u
        .hr_methods
        .iter()
        .enumerate()
        .map(|(k, on)| (*on as u32) << k)
        .sum::<u32>();
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{}",
        ErrorMode::ALL
            .iter()
            .position(|m| *m == u.err_mode)
            .unwrap_or(0),
        u.q,
        methods,
        u.show_hr as u8,
        u.fpt_r_km,
        u.hmm_k,
        CpVar::ALL.iter().position(|v| *v == u.cp_var).unwrap_or(0),
        u.cp_mult,
        u.cp_min,
        u.lc_year,
        u.lc_radius_m,
        u.lc_max,
        LcTime::ALL
            .iter()
            .position(|v| *v == u.lc_time)
            .unwrap_or(0),
    )
}

/// Three significant digits.
fn sig3(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        format!("{v}")
    } else {
        let d = (2 - v.abs().log10().floor() as i32).max(0) as usize;
        format!("{v:.d$}")
    }
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// HMM state colours, slow → fast.
pub(super) fn hmm_color(s: u8) -> Color32 {
    [
        Color32::from_rgb(0, 158, 115),
        Color32::from_rgb(230, 159, 0),
        Color32::from_rgb(213, 94, 0),
        Color32::from_rgb(204, 121, 167),
    ][(s as usize).min(3)]
}

/// Alternating colours for change-point segments.
pub(super) fn segment_color(s: u16) -> Color32 {
    [
        Color32::from_rgb(86, 180, 233),
        Color32::from_rgb(230, 159, 0),
        Color32::from_rgb(0, 158, 115),
        Color32::from_rgb(204, 121, 167),
        Color32::from_rgb(240, 228, 66),
        Color32::from_rgb(213, 94, 0),
    ][s as usize % 6]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_rgb(n: u32, f: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, n, n);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            let mut d = Vec::new();
            for y in 0..n {
                for x in 0..n {
                    d.extend_from_slice(&f(x, y));
                }
            }
            w.write_image_data(&d).unwrap();
        }
        out
    }

    #[test]
    fn movement_end_to_end() {
        let mut a = WingApp::blank();
        a.load_demo();
        let ti = (0..a.tracks.len())
            .max_by_key(|&i| a.tracks[i].fixes.len())
            .unwrap();
        a.mv_ui.track = Some(a.track_key(ti));
        a.mv_ui.hr_methods = [true, true, true, true];
        a.run_models(ti);
        a.run_home_range(ti);
        a.run_fpt(ti);
        a.run_hmm(ti);
        a.run_cp(ti);
        let key = a.track_key(ti);
        let r = &a.mv_ui.results[&key];
        assert_eq!(r.fits.len(), 3, "{:?}", a.log);
        assert_eq!(r.hr.len(), 4);
        assert!(
            r.hr.iter()
                .all(|h| h.1.last().is_some_and(|c| c.area_m2 > 0.0))
        );
        assert!(r.hmm.is_some() && r.cp.is_some() && r.fpt_r.is_some());
        let fx = &a.tracks[ti].fixes;
        assert!(
            fx.iter()
                .any(|f| f.mv.as_ref().is_some_and(|m| m.hmm_state.is_some()))
        );
        assert!(
            fx.iter()
                .any(|f| f.mv.as_ref().is_some_and(|m| m.segment.is_some()))
        );
        assert!(!a.home_range_draw().is_empty());
        // summary CSV: header + one row with the same number of cells
        let csv = a.movement_summary_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 2, "{csv}");
        assert_eq!(lines[0].split(',').count(), lines[1].split(',').count());
        // annotated export carries the columns
        let ex = crate::export::to_csv([&a.tracks[ti]]);
        assert!(
            ex.lines()
                .next()
                .unwrap()
                .contains("hmm_state,cp_segment,fpt_h")
        );

        // land cover: fake WMS answers (west half trees, east half cropland)
        a.mv_ui.lc_max = 12;
        a.start_landcover(ti);
        let jobs: Vec<LcJob> = a.mv_ui.lc_queue.drain(..).collect();
        assert_eq!(jobs.len(), 12);
        assert!(jobs[0].url.contains("WORLDCOVER_2021_MAP"));
        let img = png_rgb(
            64,
            |x, _| if x < 32 { [0, 100, 0] } else { [240, 150, 255] },
        );
        for (k, j) in jobs.into_iter().enumerate() {
            let res = if k == 0 {
                Err("HTTP 500".to_string())
            } else {
                Ok(img.clone())
            };
            a.mv_ui.lc_inflight += 1;
            a.on_landcover(j.key, j.fix, j.err, j.run, res);
        }
        let r = &a.mv_ui.results[&key];
        assert_eq!((r.lc_samples.len(), r.lc_failed, r.lc_pending), (11, 1, 0));
        assert!(!r.lc_ratios.is_empty());
        assert!(
            a.tracks[ti]
                .fixes
                .iter()
                .any(|f| f.mv.as_ref().is_some_and(|m| m.landcover == Some(40)))
        );

        // settings survive a share link; changing the date filter clears the results
        a.mv_ui.q = 0.9;
        a.mv_ui.hmm_k = 3;
        a.mv_ui.cp_var = CpVar::LogSpeed;
        a.mv_ui.lc_radius_m = 2500.0;
        let m = a.share_map(true);
        let mut b = WingApp::blank();
        b.apply_share_settings(&m);
        assert_eq!(b.mv_share(), a.mv_share());
        let (t0, t1) = a.tracks[ti].time_range().unwrap();
        a.set_date_filter(&[ti], Some((t0, (t0 + t1) / 2)));
        assert!(a.tracks[ti].fixes.iter().all(|f| f.mv.is_none()));
        assert!(a.home_range_draw().is_empty());
    }
}
