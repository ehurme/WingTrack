//! Movement analyses, run only when the user asks for them (side panel → *Movement analysis*).
//!
//! * **Autocorrelated movement models** in the spirit of the R package `ctmm`
//!   (Calabrese, Fleming & Gurarie 2016; Fleming et al. 2014): the empirical semi-variogram
//!   for irregular sampling, and maximum-likelihood fits (exact Kalman filter, irregular time
//!   steps, location error) of three continuous-time models — IID (no autocorrelation),
//!   OU (autocorrelated positions, home-range crossing time τp) and OUF (also autocorrelated
//!   velocities, τv) — compared by AICc. As in ctmm, σ is the per-axis positional variance, so
//!   the Gaussian home range of coverage q is `−2 ln(1−q) π σ`, and the OUF mean speed is
//!   `√(π/2) · √(σ / (τp τv))`. Confidence intervals come from the observed information
//!   (log-parameters); the effective sample size of the area is `DOF ≈ 1 / Var(ln σ̂)`.
//!   Like ctmm, these models assume *range residency*: restrict migratory tracks to a resident
//!   period with the per-tag date filter first.
//! * **Home ranges:** MCP, KDE (Gaussian reference bandwidth with all N fixes) and an
//!   AKDE-like KDE whose bandwidth uses the effective sample size of the fitted model
//!   (so autocorrelated data are not over-smoothed into a too-small range), plus the
//!   Gaussian range of the fitted model.
//! * **Segmentation:** first-passage time (Fauchald & Tveraa 2003) with a radius scan,
//!   a 2–3 state hidden Markov model on speed (zero-inflated gamma) and turning angle
//!   (von Mises) fitted by EM and decoded with Viterbi, and change points in log speed or net
//!   displacement (penalised least squares, PELT; Killick et al. 2012 — the idea behind
//!   Lavielle's segmentation).
//!
//! Positions are projected to metres with a local equirectangular projection about the
//! track's mean position ([`Proj`]).

// Numerical code: index loops over small matrices read better than iterator chains, and
// `!(x > 0.0)` deliberately also rejects NaN.
#![allow(clippy::needless_range_loop, clippy::neg_cmp_op_on_partial_ord)]

use std::f64::consts::PI;

const R_EARTH: f64 = 6_371_008.8;

// ---------------------------------------------------------------------------------------------
// projection + observations

/// Local equirectangular projection (metres east / north of a reference point).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Proj {
    pub lat0: f64,
    pub lon0: f64,
    coslat: f64,
}

impl Proj {
    pub fn new(lat0: f64, lon0: f64) -> Self {
        Proj {
            lat0,
            lon0,
            coslat: lat0.to_radians().cos().max(0.01),
        }
    }
    pub fn fwd(&self, lat: f64, lon: f64) -> (f64, f64) {
        let dlon = (lon - self.lon0 + 540.0).rem_euclid(360.0) - 180.0;
        (
            R_EARTH * dlon.to_radians() * self.coslat,
            R_EARTH * (lat - self.lat0).to_radians(),
        )
    }
    /// (lat, lon) of a projected point.
    pub fn inv(&self, x: f64, y: f64) -> (f64, f64) {
        let lat = self.lat0 + (y / R_EARTH).to_degrees();
        let lon = self.lon0 + (x / (R_EARTH * self.coslat)).to_degrees();
        (lat, (lon + 540.0).rem_euclid(360.0) - 180.0)
    }
}

/// One location in projected metres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Obs {
    /// Seconds.
    pub t: f64,
    pub x: f64,
    pub y: f64,
    /// Horizontal error radius (m), read as the RMS (1-σ radial) error.
    pub err_m: Option<f64>,
}

/// Project (unix s, lat, lon, error radius) points about their mean position, sorted by time.
pub fn observations(pts: &[(i64, f64, f64, Option<f32>)]) -> (Proj, Vec<Obs>) {
    let n = pts.len().max(1) as f64;
    let lat0 = pts.iter().map(|p| p.1).sum::<f64>() / n;
    // circular mean of longitude (dateline-safe)
    let (s, c) = pts.iter().fold((0.0, 0.0), |a, p| {
        (a.0 + p.2.to_radians().sin(), a.1 + p.2.to_radians().cos())
    });
    let lon0 = if pts.is_empty() {
        0.0
    } else {
        s.atan2(c).to_degrees()
    };
    let proj = Proj::new(lat0, lon0);
    let mut obs: Vec<Obs> = pts
        .iter()
        .filter(|p| p.1.is_finite() && p.2.is_finite())
        .map(|p| {
            let (x, y) = proj.fwd(p.1, p.2);
            Obs {
                t: p.0 as f64,
                x,
                y,
                err_m: p.3.map(|e| e as f64).filter(|e| e.is_finite() && *e > 0.0),
            }
        })
        .collect();
    obs.sort_by(|a, b| a.t.total_cmp(&b.t));
    (proj, obs)
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let m = v.len() / 2;
    Some(if v.len() % 2 == 1 {
        v[m]
    } else {
        0.5 * (v[m - 1] + v[m])
    })
}

/// Median time between consecutive fixes (s).
pub fn median_dt(obs: &[Obs]) -> Option<f64> {
    median(
        obs.windows(2)
            .map(|w| w[1].t - w[0].t)
            .filter(|d| *d > 0.0)
            .collect(),
    )
}

/// Evenly thinned copy (keeps first and last) for O(N²) methods.
fn thin(obs: &[Obs], max: usize) -> Vec<Obs> {
    if obs.len() <= max || max < 2 {
        return obs.to_vec();
    }
    (0..max)
        .map(|k| obs[k * (obs.len() - 1) / (max - 1)])
        .collect()
}

// ---------------------------------------------------------------------------------------------
// variogram

/// One lag bin of the empirical semi-variogram (per axis, m²).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VarioPoint {
    pub lag_s: f64,
    pub gamma: f64,
    pub n: usize,
}

/// Empirical semi-variogram `γ(τ) = ½ ⟨(x(t+τ) − x(t))²⟩` averaged over both axes, with
/// log-spaced lag bins (irregular sampling), up to half the tracking duration.
pub fn variogram(obs: &[Obs]) -> Vec<VarioPoint> {
    let o = thin(obs, 2500);
    if o.len() < 3 {
        return Vec::new();
    }
    let dur = o.last().unwrap().t - o[0].t;
    let d0 = (median_dt(&o).unwrap_or(dur / o.len() as f64) * 0.5).max(1.0);
    let max_lag = dur * 0.5;
    if max_lag <= d0 {
        return Vec::new();
    }
    let nb = 36usize;
    let ratio = (max_lag / d0).powf(1.0 / nb as f64);
    let lr = ratio.ln();
    let mut sum = vec![0.0; nb + 1];
    let mut lag = vec![0.0; nb + 1];
    let mut cnt = vec![0usize; nb + 1];
    for i in 0..o.len() {
        for j in i + 1..o.len() {
            let dt = o[j].t - o[i].t;
            if dt <= 0.0 {
                continue;
            }
            if dt > max_lag {
                break;
            }
            let b = if dt <= d0 {
                0
            } else {
                ((dt / d0).ln() / lr).ceil() as usize
            }
            .min(nb);
            let dx = o[j].x - o[i].x;
            let dy = o[j].y - o[i].y;
            sum[b] += (dx * dx + dy * dy) / 4.0;
            lag[b] += dt;
            cnt[b] += 1;
        }
    }
    (0..=nb)
        .filter(|&b| cnt[b] > 0)
        .map(|b| VarioPoint {
            lag_s: lag[b] / cnt[b] as f64,
            gamma: sum[b] / cnt[b] as f64,
            n: cnt[b],
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// continuous-time models

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Model {
    Iid,
    Ou,
    Ouf,
}

impl Model {
    pub const ALL: [Model; 3] = [Model::Iid, Model::Ou, Model::Ouf];
    pub fn label(self) -> &'static str {
        match self {
            Model::Iid => "IID (no autocorrelation)",
            Model::Ou => "OU (range-resident, positions autocorrelated)",
            Model::Ouf => "OUF (positions + velocities autocorrelated)",
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            Model::Iid => "IID",
            Model::Ou => "OU",
            Model::Ouf => "OUF",
        }
    }
}

/// How location error enters the likelihood.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorMode {
    /// Positions are exact.
    Ignore,
    /// Per-fix error radius from the data (Sigfox radius, GPS accuracy) as the RMS error.
    FromFixes,
    /// One error variance estimated with the movement parameters (the variogram "nugget").
    Estimate,
}

impl ErrorMode {
    pub const ALL: [ErrorMode; 3] = [ErrorMode::FromFixes, ErrorMode::Estimate, ErrorMode::Ignore];
    pub fn label(self) -> &'static str {
        match self {
            ErrorMode::Ignore => "ignore (exact positions)",
            ErrorMode::FromFixes => "from fixes (error radius = RMS error)",
            ErrorMode::Estimate => "estimate (one error variance)",
        }
    }
}

/// A fitted model. Parameters are estimated on the log scale; `cov` is their covariance.
#[derive(Clone, Debug)]
pub struct Fit {
    pub model: Model,
    pub err_mode: ErrorMode,
    /// Per-axis positional variance (m²).
    pub sigma: f64,
    /// Position (home-range crossing) timescale, s (OU, OUF).
    pub tau_p: f64,
    /// Velocity timescale, s (OUF).
    pub tau_v: f64,
    /// Estimated per-axis error variance (m², `ErrorMode::Estimate`).
    pub err_var: f64,
    /// Range centre, projected metres.
    pub mean: (f64, f64),
    pub loglik: f64,
    pub aicc: f64,
    /// Free parameters including the 2 mean coordinates.
    pub k: usize,
    pub names: Vec<&'static str>,
    pub theta: Vec<f64>,
    /// Covariance of `theta` (NaN when the information matrix was singular).
    pub cov: Vec<Vec<f64>>,
    pub n: usize,
    pub duration_s: f64,
}

struct Spec<'a> {
    model: Model,
    err: ErrorMode,
    obs: &'a [Obs],
    /// Known per-axis error variance per fix (`FromFixes`), else zeros.
    known: Vec<f64>,
    centre: (f64, f64),
}

type M2 = [[f64; 2]; 2];

fn mm(a: &M2, b: &M2) -> M2 {
    [
        [
            a[0][0] * b[0][0] + a[0][1] * b[1][0],
            a[0][0] * b[0][1] + a[0][1] * b[1][1],
        ],
        [
            a[1][0] * b[0][0] + a[1][1] * b[1][0],
            a[1][0] * b[0][1] + a[1][1] * b[1][1],
        ],
    ]
}

fn tr(a: &M2) -> M2 {
    [[a[0][0], a[1][0]], [a[0][1], a[1][1]]]
}

/// Transition and process-noise matrices of the state (position, velocity) over `dt`.
fn transition(model: Model, sigma: f64, tau_p: f64, tau_v: f64, dt: f64) -> (M2, M2) {
    match model {
        Model::Iid => ([[0.0, 0.0], [0.0, 0.0]], [[sigma, 0.0], [0.0, 0.0]]),
        Model::Ou => {
            let f = (-dt / tau_p).exp();
            (
                [[f, 0.0], [0.0, 0.0]],
                [[sigma * (1.0 - f * f), 0.0], [0.0, 0.0]],
            )
        }
        Model::Ouf => {
            let tv = if (tau_p - tau_v).abs() < 1e-4 * tau_p {
                tau_v * (1.0 - 1e-4)
            } else {
                tau_v
            };
            let (l1, l2) = (-1.0 / tau_p, -1.0 / tv);
            let (e1, e2) = ((l1 * dt).exp(), (l2 * dt).exp());
            let c1 = (e1 - e2) / (l1 - l2);
            let c0 = (l1 * e2 - l2 * e1) / (l1 - l2);
            // A = [[0, 1], [−l1 l2, l1 + l2]];  expm(A dt) = c0 I + c1 A
            let f = [[c0, c1], [-l1 * l2 * c1, c0 + c1 * (l1 + l2)]];
            let s = [[sigma, 0.0], [0.0, sigma * l1 * l2]];
            // stationary process: Q = Σ − F Σ Fᵀ
            let fsf = mm(&mm(&f, &s), &tr(&f));
            let mut q = [
                [s[0][0] - fsf[0][0], -fsf[0][1]],
                [-fsf[1][0], s[1][1] - fsf[1][1]],
            ];
            q[0][0] = q[0][0].max(0.0);
            q[1][1] = q[1][1].max(0.0);
            let off = 0.5 * (q[0][1] + q[1][0]);
            let lim = (q[0][0] * q[1][1]).sqrt();
            q[0][1] = off.clamp(-lim, lim);
            q[1][0] = q[0][1];
            (f, q)
        }
    }
}

impl Spec<'_> {
    fn names(&self) -> Vec<&'static str> {
        let mut v = vec!["ln σ"];
        if self.model != Model::Iid {
            v.push("ln τp");
        }
        if self.model == Model::Ouf {
            v.push("ln τv");
        }
        if self.err == ErrorMode::Estimate {
            v.push("ln error variance");
        }
        v
    }
    fn unpack(&self, th: &[f64]) -> (f64, f64, f64, f64) {
        let sigma = th[0].exp();
        let mut i = 1;
        let mut tau_p = f64::INFINITY;
        let mut tau_v = 0.0;
        if self.model != Model::Iid {
            tau_p = th[i].exp();
            i += 1;
        }
        if self.model == Model::Ouf {
            tau_v = th[i].exp();
            i += 1;
        }
        let err = if self.err == ErrorMode::Estimate {
            th[i].exp()
        } else {
            0.0
        };
        // OUF is symmetric in the two timescales: τp is the longer one
        if self.model == Model::Ouf && tau_v > tau_p {
            std::mem::swap(&mut tau_p, &mut tau_v);
        }
        (sigma, tau_p, tau_v, err)
    }

    /// Exact Gaussian log-likelihood (Kalman filter), with the mean profiled out.
    fn loglik(&self, th: &[f64]) -> (f64, (f64, f64)) {
        let (sigma, tau_p, tau_v, err) = self.unpack(th);
        if !sigma.is_finite() || sigma <= 0.0 {
            return (f64::NEG_INFINITY, (0.0, 0.0));
        }
        let p0: M2 = match self.model {
            Model::Ouf => [[sigma, 0.0], [0.0, sigma / (tau_p * tau_v)]],
            _ => [[sigma, 0.0], [0.0, 0.0]],
        };
        let mut p = p0;
        // three filtered "columns": x, y and a column of ones (for the profiled mean)
        let mut s = [[0.0f64; 2]; 3];
        let (mut ldet, mut axx, mut ax1, mut ayy, mut ay1, mut a11) =
            (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let mut last_t = f64::NAN;
        let mut last_dt = f64::NAN;
        let (mut f, mut q) = ([[0.0; 2]; 2], [[0.0; 2]; 2]);
        for (i, o) in self.obs.iter().enumerate() {
            if i > 0 {
                let dt = (o.t - last_t).max(0.0);
                if dt != last_dt {
                    (f, q) = transition(self.model, sigma, tau_p, tau_v, dt);
                    last_dt = dt;
                }
                for c in s.iter_mut() {
                    *c = [
                        f[0][0] * c[0] + f[0][1] * c[1],
                        f[1][0] * c[0] + f[1][1] * c[1],
                    ];
                }
                let fp = mm(&mm(&f, &p), &tr(&f));
                p = [
                    [fp[0][0] + q[0][0], fp[0][1] + q[0][1]],
                    [fp[1][0] + q[1][0], fp[1][1] + q[1][1]],
                ];
            }
            last_t = o.t;
            let r = self.known[i] + err;
            let sv = p[0][0] + r;
            if !(sv > 0.0) || !sv.is_finite() {
                return (f64::NEG_INFINITY, (0.0, 0.0));
            }
            let z = [o.x - self.centre.0, o.y - self.centre.1, 1.0];
            let k = [p[0][0] / sv, p[1][0] / sv];
            let mut inn = [0.0; 3];
            for c in 0..3 {
                inn[c] = z[c] - s[c][0];
                s[c][0] += k[0] * inn[c];
                s[c][1] += k[1] * inn[c];
            }
            let row = [p[0][0], p[0][1]];
            for a in 0..2 {
                for b in 0..2 {
                    p[a][b] -= k[a] * row[b];
                }
            }
            let sym = 0.5 * (p[0][1] + p[1][0]);
            p[0][1] = sym;
            p[1][0] = sym;
            ldet += sv.ln();
            axx += inn[0] * inn[0] / sv;
            ax1 += inn[0] * inn[2] / sv;
            ayy += inn[1] * inn[1] / sv;
            ay1 += inn[1] * inn[2] / sv;
            a11 += inn[2] * inn[2] / sv;
        }
        if a11 <= 0.0 {
            return (f64::NEG_INFINITY, (0.0, 0.0));
        }
        let (mx, my) = (ax1 / a11, ay1 / a11);
        let quad = (axx - ax1 * mx) + (ayy - ay1 * my);
        let n = self.obs.len() as f64;
        let ll = -0.5 * (2.0 * ldet + quad + 2.0 * n * (2.0 * PI).ln());
        (ll, (self.centre.0 + mx, self.centre.1 + my))
    }
}

/// Nelder–Mead minimisation.
fn nelder_mead(f: &dyn Fn(&[f64]) -> f64, x0: &[f64], step: f64, iters: usize) -> (Vec<f64>, f64) {
    let n = x0.len();
    let mut simplex: Vec<(Vec<f64>, f64)> = Vec::with_capacity(n + 1);
    simplex.push((x0.to_vec(), f(x0)));
    for i in 0..n {
        let mut x = x0.to_vec();
        x[i] += step;
        let v = f(&x);
        simplex.push((x, v));
    }
    let nan_big = |v: f64| if v.is_finite() { v } else { f64::MAX };
    for _ in 0..iters {
        simplex.sort_by(|a, b| nan_big(a.1).total_cmp(&nan_big(b.1)));
        let (best, worst) = (nan_big(simplex[0].1), nan_big(simplex[n].1));
        if (worst - best).abs() < 1e-9 * (1.0 + best.abs()) {
            let spread = (0..n)
                .map(|d| {
                    simplex
                        .iter()
                        .map(|s| (s.0[d] - simplex[0].0[d]).abs())
                        .fold(0.0, f64::max)
                })
                .fold(0.0, f64::max);
            if spread < 1e-6 {
                break;
            }
        }
        let mut c = vec![0.0; n];
        for s in &simplex[..n] {
            for d in 0..n {
                c[d] += s.0[d] / n as f64;
            }
        }
        let at = |t: f64| -> Vec<f64> {
            (0..n)
                .map(|d| c[d] + t * (simplex[n].0[d] - c[d]))
                .collect()
        };
        let xr = at(-1.0);
        let fr = nan_big(f(&xr));
        if fr < nan_big(simplex[0].1) {
            let xe = at(-2.0);
            let fe = nan_big(f(&xe));
            simplex[n] = if fe < fr { (xe, fe) } else { (xr, fr) };
        } else if fr < nan_big(simplex[n - 1].1) {
            simplex[n] = (xr, fr);
        } else {
            let (xc, fc) = if fr < worst {
                let x = at(-0.5);
                let v = nan_big(f(&x));
                (x, v)
            } else {
                let x = at(0.5);
                let v = nan_big(f(&x));
                (x, v)
            };
            if fc < worst.min(fr) {
                simplex[n] = (xc, fc);
            } else {
                // shrink towards the best
                let b = simplex[0].0.clone();
                for s in simplex.iter_mut().skip(1) {
                    for d in 0..n {
                        s.0[d] = b[d] + 0.5 * (s.0[d] - b[d]);
                    }
                    s.1 = f(&s.0);
                }
            }
        }
    }
    simplex.sort_by(|a, b| nan_big(a.1).total_cmp(&nan_big(b.1)));
    let b = simplex.swap_remove(0);
    (b.0, b.1)
}

/// Inverse of a small symmetric matrix (Gauss–Jordan); `None` if singular / not positive.
fn invert(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n = a.len();
    let mut m: Vec<Vec<f64>> = a
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut row = r.clone();
            row.extend((0..n).map(|j| if i == j { 1.0 } else { 0.0 }));
            row
        })
        .collect();
    for c in 0..n {
        let piv = (c..n).max_by(|&x, &y| m[x][c].abs().total_cmp(&m[y][c].abs()))?;
        if m[piv][c].abs() < 1e-12 {
            return None;
        }
        m.swap(c, piv);
        let d = m[c][c];
        for v in m[c].iter_mut() {
            *v /= d;
        }
        for r in 0..n {
            if r != c {
                let fct = m[r][c];
                if fct != 0.0 {
                    for k in 0..2 * n {
                        m[r][k] -= fct * m[c][k];
                    }
                }
            }
        }
    }
    let inv: Vec<Vec<f64>> = m.into_iter().map(|r| r[n..].to_vec()).collect();
    (0..n).all(|i| inv[i][i] > 0.0).then_some(inv)
}

/// Per-axis error variance per fix for `FromFixes` (missing radii → median radius).
fn known_errors(obs: &[Obs], mode: ErrorMode) -> Vec<f64> {
    if mode != ErrorMode::FromFixes {
        return vec![0.0; obs.len()];
    }
    let med = median(obs.iter().filter_map(|o| o.err_m).collect()).unwrap_or(0.0);
    obs.iter()
        .map(|o| {
            let e = o.err_m.unwrap_or(med);
            e * e / 2.0
        })
        .collect()
}

/// Fit one model by maximum likelihood.
pub fn fit(obs: &[Obs], model: Model, err: ErrorMode) -> Option<Fit> {
    if obs.len() < 5 {
        return None;
    }
    let n = obs.len();
    let cx = obs.iter().map(|o| o.x).sum::<f64>() / n as f64;
    let cy = obs.iter().map(|o| o.y).sum::<f64>() / n as f64;
    let var = obs
        .iter()
        .map(|o| ((o.x - cx).powi(2) + (o.y - cy).powi(2)) / 2.0)
        .sum::<f64>()
        / (n - 1) as f64;
    if !(var > 0.0) {
        return None;
    }
    let spec = Spec {
        model,
        err,
        obs,
        known: known_errors(obs, err),
        centre: (cx, cy),
    };
    let dur = (obs[n - 1].t - obs[0].t).max(1.0);
    let mdt = median_dt(obs).unwrap_or(dur / n as f64).max(1.0);
    // starting values from the variogram
    let vg = variogram(obs);
    let tau0 = vg
        .iter()
        .find(|p| p.gamma >= 0.63 * var)
        .map(|p| p.lag_s)
        .unwrap_or(dur / 3.0)
        .clamp(mdt * 0.5, dur * 10.0);
    let nug0 = vg
        .first()
        .map(|p| p.gamma * 0.5)
        .unwrap_or(var * 0.01)
        .max(var * 1e-6);
    let lo_t = 1.0f64.ln();
    let hi_t = (dur * 1e3).ln();
    let obj = |th: &[f64]| -> f64 {
        // keep timescales in a sane range
        let mut pen = 0.0;
        for (i, name) in spec.names().iter().enumerate() {
            if name.starts_with("ln τ") {
                if th[i] < lo_t {
                    pen += (lo_t - th[i]).powi(2) * 1e3;
                }
                if th[i] > hi_t {
                    pen += (th[i] - hi_t).powi(2) * 1e3;
                }
            }
        }
        let (ll, _) = spec.loglik(th);
        if ll.is_finite() { -ll + pen } else { f64::MAX }
    };
    let mut starts: Vec<Vec<f64>> = Vec::new();
    let tps: Vec<f64> = match model {
        Model::Iid => vec![0.0],
        _ => vec![tau0, tau0 * 0.1, tau0 * 10.0],
    };
    let tvs: Vec<f64> = if model == Model::Ouf {
        vec![mdt, (tau0 * 0.01).max(1.0)]
    } else {
        vec![0.0]
    };
    for &tp in &tps {
        for &tv in &tvs {
            let mut th = vec![var.ln()];
            if model != Model::Iid {
                th.push(tp.max(1.0).ln());
            }
            if model == Model::Ouf {
                th.push(tv.max(1.0).min(tp * 0.5).max(1.0).ln());
            }
            if err == ErrorMode::Estimate {
                th.push(nug0.ln());
            }
            starts.push(th);
        }
    }
    let mut best: Option<(Vec<f64>, f64)> = None;
    for s in starts {
        let (mut x, mut v) = nelder_mead(&obj, &s, 1.0, 400);
        // restart once from the optimum (Nelder–Mead can stall)
        let (x2, v2) = nelder_mead(&obj, &x, 0.3, 300);
        if v2 < v {
            x = x2;
            v = v2;
        }
        if best.as_ref().is_none_or(|b| v < b.1) {
            best = Some((x, v));
        }
    }
    let (theta, nll) = best?;
    if nll >= f64::MAX * 0.5 {
        return None;
    }
    let (ll, mean) = spec.loglik(&theta);
    // observed information (numerical Hessian of −ll)
    let kp = theta.len();
    let h = 1e-3;
    let fll = |t: &[f64]| -spec.loglik(t).0;
    let mut hess = vec![vec![0.0; kp]; kp];
    for i in 0..kp {
        for j in i..kp {
            let mut v = 0.0;
            for (si, sj, w) in [
                (1.0, 1.0, 1.0),
                (1.0, -1.0, -1.0),
                (-1.0, 1.0, -1.0),
                (-1.0, -1.0, 1.0),
            ] {
                let mut t = theta.clone();
                t[i] += si * h;
                t[j] += sj * h;
                v += w * fll(&t);
            }
            hess[i][j] = v / (4.0 * h * h);
            hess[j][i] = hess[i][j];
        }
    }
    let cov = invert(&hess).unwrap_or_else(|| vec![vec![f64::NAN; kp]; kp]);
    let (sigma, tau_p, tau_v, err_var) = spec.unpack(&theta);
    let k = kp + 2;
    let nn = (2 * n) as f64;
    let kf = k as f64;
    let aicc = 2.0 * kf - 2.0 * ll + 2.0 * kf * (kf + 1.0) / (nn - kf - 1.0).max(1.0);
    Some(Fit {
        model,
        err_mode: err,
        sigma,
        tau_p,
        tau_v,
        err_var,
        mean,
        loglik: ll,
        aicc,
        k,
        names: spec.names(),
        theta,
        cov,
        n,
        duration_s: dur,
    })
}

/// Fit all three models (best AICc first).
pub fn select(obs: &[Obs], err: ErrorMode) -> Vec<Fit> {
    let mut v: Vec<Fit> = Model::ALL
        .iter()
        .filter_map(|m| fit(obs, *m, err))
        .collect();
    v.sort_by(|a, b| a.aicc.total_cmp(&b.aicc));
    v
}

/// An estimate with a 95 % confidence interval.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Est {
    pub est: f64,
    pub lo: f64,
    pub hi: f64,
}

impl Est {
    fn lognormal(est: f64, var_log: f64) -> Est {
        if var_log.is_finite() && var_log >= 0.0 {
            let s = var_log.sqrt() * 1.96;
            Est {
                est,
                lo: est * (-s).exp(),
                hi: est * s.exp(),
            }
        } else {
            Est {
                est,
                lo: f64::NAN,
                hi: f64::NAN,
            }
        }
    }
    pub fn scaled(self, k: f64) -> Est {
        Est {
            est: self.est * k,
            lo: self.lo * k,
            hi: self.hi * k,
        }
    }
}

impl Fit {
    fn var_of(&self, grad: &[f64]) -> f64 {
        let mut v = 0.0;
        for i in 0..grad.len() {
            for j in 0..grad.len() {
                v += grad[i] * grad[j] * self.cov[i][j];
            }
        }
        v
    }
    fn index(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| *n == name)
    }
    /// Effective sample size of the area estimate (≈ number of independent range crossings).
    pub fn dof_area(&self) -> f64 {
        let v = self.cov[0][0];
        if v.is_finite() && v > 0.0 {
            (1.0 / v).min(self.n as f64)
        } else {
            f64::NAN
        }
    }
    /// Gaussian home-range area (m²) of coverage `q`, with CI.
    pub fn area(&self, q: f64) -> Est {
        let a = -2.0 * (1.0 - q).ln() * PI * self.sigma;
        Est::lognormal(a, self.cov[0][0])
    }
    /// Radius (m) of the Gaussian home-range circle of coverage `q`.
    pub fn radius(&self, q: f64) -> f64 {
        (-2.0 * (1.0 - q).ln() * self.sigma).sqrt()
    }
    pub fn tau_p_est(&self) -> Option<Est> {
        // after unpack τp is the longer timescale; its variance is that of either log-τ
        let (a, b) = (self.index("ln τp")?, self.index("ln τv"));
        let i = match b {
            Some(b) if self.theta[b] > self.theta[a] => b,
            _ => a,
        };
        Some(Est::lognormal(self.tau_p, self.cov[i][i]))
    }
    pub fn tau_v_est(&self) -> Option<Est> {
        let (a, b) = (self.index("ln τp")?, self.index("ln τv")?);
        let i = if self.theta[b] > self.theta[a] { a } else { b };
        Some(Est::lognormal(self.tau_v, self.cov[i][i]))
    }
    /// Mean speed (m/s) of the OUF model, `√(π/2) √(σ/(τp τv))`.
    pub fn speed(&self) -> Option<Est> {
        if self.model != Model::Ouf {
            return None;
        }
        let s = (PI / 2.0).sqrt() * (self.sigma / (self.tau_p * self.tau_v)).sqrt();
        let mut g = vec![0.0; self.theta.len()];
        g[0] = 0.5;
        g[self.index("ln τp")?] = -0.5;
        g[self.index("ln τv")?] = -0.5;
        Some(Est::lognormal(s, self.var_of(&g)))
    }
    /// The RMS location error (m) the model estimated (`ErrorMode::Estimate`).
    pub fn error_rms(&self) -> Option<Est> {
        let i = self.index("ln error variance")?;
        let e = (2.0 * self.err_var).sqrt();
        Some(Est::lognormal(e, self.cov[i][i] / 4.0))
    }
    /// Model semi-variogram at lag `tau` (s), including the error nugget.
    pub fn gamma(&self, tau: f64, mean_err: f64) -> f64 {
        let nug = if tau > 0.0 {
            self.err_var + mean_err
        } else {
            0.0
        };
        nug + match self.model {
            Model::Iid => {
                if tau > 0.0 {
                    self.sigma
                } else {
                    0.0
                }
            }
            Model::Ou => self.sigma * (1.0 - (-tau / self.tau_p).exp()),
            Model::Ouf => {
                let (tp, tv) = (self.tau_p, self.tau_v.min(self.tau_p * (1.0 - 1e-6)));
                self.sigma * (1.0 - (tp * (-tau / tp).exp() - tv * (-tau / tv).exp()) / (tp - tv))
            }
        }
    }
    /// Range residency: enough home-range crossings for a meaningful area estimate.
    pub fn range_resident(&self) -> bool {
        self.model == Model::Iid || (self.tau_p.is_finite() && self.tau_p * 3.0 < self.duration_s)
    }
}

/// Mean per-axis error variance from the fixes (for the variogram plot of `FromFixes` fits).
pub fn mean_known_error(obs: &[Obs], mode: ErrorMode) -> f64 {
    let k = known_errors(obs, mode);
    if k.is_empty() {
        0.0
    } else {
        k.iter().sum::<f64>() / k.len() as f64
    }
}

/// Straight-line speed between consecutive fixes (m/s): mean, and total distance per day.
pub fn naive_speed(obs: &[Obs]) -> Option<(f64, f64)> {
    let (mut d, mut t) = (0.0, 0.0);
    for w in obs.windows(2) {
        let dt = w[1].t - w[0].t;
        if dt > 0.0 {
            d += ((w[1].x - w[0].x).powi(2) + (w[1].y - w[0].y).powi(2)).sqrt();
            t += dt;
        }
    }
    (t > 0.0).then_some((d / t, d / t * 86400.0))
}

// ---------------------------------------------------------------------------------------------
// home ranges

/// Contour of a home-range estimate, as (lat, lon) segments.
#[derive(Clone, Debug, PartialEq)]
pub struct Contour {
    pub q: f64,
    pub area_m2: f64,
    pub segs: Vec<[(f64, f64); 2]>,
}

/// Minimum convex polygon of the `q` fraction of fixes closest to the centroid.
pub fn mcp(obs: &[Obs], proj: &Proj, q: f64) -> Option<Contour> {
    if obs.len() < 3 {
        return None;
    }
    let n = obs.len() as f64;
    let cx = obs.iter().map(|o| o.x).sum::<f64>() / n;
    let cy = obs.iter().map(|o| o.y).sum::<f64>() / n;
    let mut pts: Vec<(f64, f64, f64)> = obs
        .iter()
        .map(|o| (o.x, o.y, (o.x - cx).powi(2) + (o.y - cy).powi(2)))
        .collect();
    pts.sort_by(|a, b| a.2.total_cmp(&b.2));
    let keep = ((q * n).ceil() as usize).clamp(3, pts.len());
    let mut p: Vec<(f64, f64)> = pts[..keep].iter().map(|p| (p.0, p.1)).collect();
    // monotone chain convex hull
    p.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    p.dedup();
    if p.len() < 3 {
        return None;
    }
    let cross = |o: (f64, f64), a: (f64, f64), b: (f64, f64)| {
        (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
    };
    let mut hull: Vec<(f64, f64)> = Vec::new();
    for pass in 0..2 {
        let start = hull.len();
        let it: Box<dyn Iterator<Item = &(f64, f64)>> = if pass == 0 {
            Box::new(p.iter())
        } else {
            Box::new(p.iter().rev())
        };
        for &pt in it {
            while hull.len() >= start + 2
                && cross(hull[hull.len() - 2], hull[hull.len() - 1], pt) <= 0.0
            {
                hull.pop();
            }
            hull.push(pt);
        }
        hull.pop();
    }
    if hull.len() < 3 {
        return None;
    }
    let area = 0.5
        * (0..hull.len())
            .map(|i| {
                let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
                a.0 * b.1 - b.0 * a.1
            })
            .sum::<f64>()
            .abs();
    let segs = (0..hull.len())
        .map(|i| {
            let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
            [proj.inv(a.0, a.1), proj.inv(b.0, b.1)]
        })
        .collect();
    Some(Contour {
        q,
        area_m2: area,
        segs,
    })
}

/// Circle of radius `r` (m) about `c` (projected), as (lat, lon) segments.
pub fn circle(proj: &Proj, c: (f64, f64), r: f64, q: f64) -> Contour {
    let n = 96;
    let pt = |k: usize| {
        let a = k as f64 / n as f64 * 2.0 * PI;
        proj.inv(c.0 + r * a.cos(), c.1 + r * a.sin())
    };
    Contour {
        q,
        area_m2: PI * r * r,
        segs: (0..n).map(|k| [pt(k), pt(k + 1)]).collect(),
    }
}

/// Kernel density estimate with an isotropic Gaussian kernel of bandwidth `h` (m): the
/// smallest region holding fraction `q` of the density, for each `q`.
pub fn kde(obs: &[Obs], proj: &Proj, h: f64, qs: &[f64]) -> Vec<Contour> {
    if obs.len() < 3 || !(h > 0.0) {
        return Vec::new();
    }
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for o in obs {
        x0 = x0.min(o.x);
        x1 = x1.max(o.x);
        y0 = y0.min(o.y);
        y1 = y1.max(o.y);
    }
    let pad = 4.0 * h;
    let (x0, x1, y0, y1) = (x0 - pad, x1 + pad, y0 - pad, y1 + pad);
    let ext = (x1 - x0).max(y1 - y0);
    let cell = (h / 3.0).max(ext / 320.0);
    let nx = ((x1 - x0) / cell).ceil() as usize + 1;
    let ny = ((y1 - y0) / cell).ceil() as usize + 1;
    let mut g = vec![0.0f64; nx * ny];
    for o in obs {
        // bilinear binning
        let fx = (o.x - x0) / cell;
        let fy = (o.y - y0) / cell;
        let (i, j) = (fx.floor() as usize, fy.floor() as usize);
        let (ax, ay) = (fx - i as f64, fy - j as f64);
        for (di, dj, w) in [
            (0, 0, (1.0 - ax) * (1.0 - ay)),
            (1, 0, ax * (1.0 - ay)),
            (0, 1, (1.0 - ax) * ay),
            (1, 1, ax * ay),
        ] {
            let (ii, jj) = (i + di, j + dj);
            if ii < nx && jj < ny {
                g[jj * nx + ii] += w;
            }
        }
    }
    // separable Gaussian convolution
    let kr = ((4.0 * h / cell).ceil() as isize).max(1);
    let ker: Vec<f64> = (-kr..=kr)
        .map(|k| (-0.5 * (k as f64 * cell / h).powi(2)).exp())
        .collect();
    let mut tmp = vec![0.0f64; nx * ny];
    for j in 0..ny {
        for i in 0..nx {
            let v = g[j * nx + i];
            if v == 0.0 {
                continue;
            }
            for (k, w) in ker.iter().enumerate() {
                let ii = i as isize + k as isize - kr;
                if ii >= 0 && (ii as usize) < nx {
                    tmp[j * nx + ii as usize] += v * w;
                }
            }
        }
    }
    let mut d = vec![0.0f64; nx * ny];
    for j in 0..ny {
        for i in 0..nx {
            let v = tmp[j * nx + i];
            if v == 0.0 {
                continue;
            }
            for (k, w) in ker.iter().enumerate() {
                let jj = j as isize + k as isize - kr;
                if jj >= 0 && (jj as usize) < ny {
                    d[jj as usize * nx + i] += v * w;
                }
            }
        }
    }
    let total: f64 = d.iter().sum();
    if !(total > 0.0) {
        return Vec::new();
    }
    let mut sorted = d.clone();
    sorted.sort_by(|a, b| b.total_cmp(a));
    qs.iter()
        .map(|&q| {
            let mut acc = 0.0;
            let mut thr = sorted[0];
            let mut cells = 0usize;
            for &v in &sorted {
                acc += v;
                cells += 1;
                thr = v;
                if acc >= q * total {
                    break;
                }
            }
            let segs = march(&d, nx, ny, thr)
                .into_iter()
                .map(|[a, b]| {
                    [
                        proj.inv(x0 + a.0 * cell, y0 + a.1 * cell),
                        proj.inv(x0 + b.0 * cell, y0 + b.1 * cell),
                    ]
                })
                .collect();
            Contour {
                q,
                area_m2: cells as f64 * cell * cell,
                segs,
            }
        })
        .collect()
}

/// Marching squares on a grid `d[j * nx + i]`: segments in grid units (i, j).
fn march(d: &[f64], nx: usize, ny: usize, level: f64) -> Vec<[(f64, f64); 2]> {
    let mut out = Vec::new();
    let at = |i: usize, j: usize| d[j * nx + i];
    for j in 0..ny.saturating_sub(1) {
        for i in 0..nx.saturating_sub(1) {
            let c = [at(i, j), at(i + 1, j), at(i + 1, j + 1), at(i, j + 1)];
            let pos = [
                (i as f64, j as f64),
                (i as f64 + 1.0, j as f64),
                (i as f64 + 1.0, j as f64 + 1.0),
                (i as f64, j as f64 + 1.0),
            ];
            let mut pts = Vec::with_capacity(4);
            for e in 0..4 {
                let (p, q) = (e, (e + 1) % 4);
                if (c[p] < level) != (c[q] < level) {
                    let f = (level - c[p]) / (c[q] - c[p]);
                    pts.push((
                        pos[p].0 + (pos[q].0 - pos[p].0) * f,
                        pos[p].1 + (pos[q].1 - pos[p].1) * f,
                    ));
                }
            }
            match pts.len() {
                2 => out.push([pts[0], pts[1]]),
                4 => {
                    let mean = (c[0] + c[1] + c[2] + c[3]) / 4.0;
                    if (mean < level) == (c[0] < level) {
                        out.push([pts[0], pts[3]]);
                        out.push([pts[1], pts[2]]);
                    } else {
                        out.push([pts[0], pts[1]]);
                        out.push([pts[2], pts[3]]);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Gaussian reference bandwidth `h = σ n^(−1/6)` (per-axis SD `σ`, `n` independent samples).
pub fn reference_bandwidth(sd: f64, n: f64) -> f64 {
    sd * n.max(2.0).powf(-1.0 / 6.0)
}

/// Pooled per-axis standard deviation of the positions.
pub fn sample_sd(obs: &[Obs]) -> f64 {
    let n = obs.len().max(2) as f64;
    let cx = obs.iter().map(|o| o.x).sum::<f64>() / n;
    let cy = obs.iter().map(|o| o.y).sum::<f64>() / n;
    (obs.iter()
        .map(|o| ((o.x - cx).powi(2) + (o.y - cy).powi(2)) / 2.0)
        .sum::<f64>()
        / (n - 1.0))
        .sqrt()
}

// ---------------------------------------------------------------------------------------------
// first-passage time

/// Time (s) the animal needs to leave a circle of radius `r` around each fix, backwards plus
/// forwards (linear interpolation between fixes); `None` near the ends of the track.
pub fn fpt(obs: &[Obs], r: f64) -> Vec<Option<f64>> {
    let exit = |i: usize, fwd: bool| -> Option<f64> {
        let c = obs[i];
        let mut prev = i;
        loop {
            let j = if fwd { prev + 1 } else { prev.checked_sub(1)? };
            let p = obs.get(j)?;
            let d2 = (p.x - c.x).powi(2) + (p.y - c.y).powi(2);
            if d2 > r * r {
                let a0 = obs[prev];
                let (dx, dy) = (p.x - a0.x, p.y - a0.y);
                let (ox, oy) = (a0.x - c.x, a0.y - c.y);
                let qa = dx * dx + dy * dy;
                let qb = 2.0 * (ox * dx + oy * dy);
                let qc = ox * ox + oy * oy - r * r;
                let s = if qa > 0.0 {
                    ((-qb + (qb * qb - 4.0 * qa * qc).max(0.0).sqrt()) / (2.0 * qa)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                return Some(a0.t + s * (p.t - a0.t));
            }
            prev = j;
        }
    };
    (0..obs.len())
        .map(|i| {
            let f = exit(i, true)?;
            let b = exit(i, false)?;
            Some(f - b)
        })
        .collect()
}

/// Variance of ln(FPT) for a range of radii: its peak marks the scale of area-restricted
/// search. Returns (radius m, variance, number of fixes with a defined FPT).
pub fn fpt_scan(obs: &[Obs]) -> Vec<(f64, f64, usize)> {
    let o = thin(obs, 3000);
    if o.len() < 5 {
        return Vec::new();
    }
    let steps: Vec<f64> = o
        .windows(2)
        .map(|w| ((w[1].x - w[0].x).powi(2) + (w[1].y - w[0].y).powi(2)).sqrt())
        .filter(|d| *d > 0.0)
        .collect();
    // below the typical step length FPT only measures the sampling interval
    let r0 = median(steps).unwrap_or(10.0).max(1.0);
    let ext = {
        let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in &o {
            x0 = x0.min(p.x);
            x1 = x1.max(p.x);
            y0 = y0.min(p.y);
            y1 = y1.max(p.y);
        }
        ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt()
    };
    let r1 = (ext * 0.5).max(r0 * 2.0);
    let k = 24;
    (0..k)
        .map(|i| r0 * (r1 / r0).powf(i as f64 / (k - 1) as f64))
        .map(|r| {
            let l: Vec<f64> = fpt(&o, r)
                .into_iter()
                .flatten()
                .filter(|v| *v > 0.0)
                .map(f64::ln)
                .collect();
            let n = l.len();
            let var = if n > 2 {
                let m = l.iter().sum::<f64>() / n as f64;
                l.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (n - 1) as f64
            } else {
                f64::NAN
            };
            (r, var, n)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// hidden Markov model

/// Per-state parameters of the HMM.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HmmState {
    /// Gamma speed distribution (m/s): mean and SD.
    pub speed_mean: f64,
    pub speed_sd: f64,
    /// Probability of a zero-length step.
    pub zero_p: f64,
    /// von Mises turning angle: mean (rad) and concentration.
    pub angle_mean: f64,
    pub kappa: f64,
}

#[derive(Clone, Debug)]
pub struct Hmm {
    pub states: Vec<HmmState>,
    pub trans: Vec<Vec<f64>>,
    pub loglik: f64,
    pub aic: f64,
    pub bic: f64,
    /// Viterbi state per observation (`None` for the first fix / zero time steps).
    pub path: Vec<Option<u8>>,
    /// Coefficient of variation of the time steps (HMMs assume regular sampling).
    pub dt_cv: f64,
    pub iterations: usize,
}

fn ln_gamma(x: f64) -> f64 {
    // Lanczos approximation (g = 7)
    const C: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        return (PI / (PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = C[0];
    let t = x + 7.5;
    for (i, c) in C.iter().enumerate().skip(1) {
        a += c / (x + i as f64);
    }
    0.5 * (2.0 * PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// ln I0(κ) (modified Bessel function of the first kind, order 0).
fn ln_i0(k: f64) -> f64 {
    let ax = k.abs();
    if ax < 3.75 {
        let y = (k / 3.75).powi(2);
        (1.0 + y
            * (3.515_622_9
                + y * (3.089_942_4
                    + y * (1.206_749_2 + y * (0.265_973_2 + y * (0.036_076_8 + y * 0.004_581_3))))))
            .ln()
    } else {
        let y = 3.75 / ax;
        ax - 0.5 * ax.ln()
            + (0.398_942_28
                + y * (0.013_285_92
                    + y * (0.002_253_19
                        + y * (-0.001_575_65
                            + y * (0.009_162_81
                                + y * (-0.020_577_06
                                    + y * (0.026_355_37
                                        + y * (-0.016_476_33 + y * 0.003_923_77))))))))
                .ln()
    }
}

/// Inverse of A1(κ) = I1/I0 (Best & Fisher 1981).
fn a1inv(r: f64) -> f64 {
    let r = r.clamp(0.0, 0.9999);
    let k = if r < 0.53 {
        2.0 * r + r.powi(3) + 5.0 * r.powi(5) / 6.0
    } else if r < 0.85 {
        -0.4 + 1.39 * r + 0.43 / (1.0 - r)
    } else {
        1.0 / (r.powi(3) - 4.0 * r * r + 3.0 * r)
    };
    k.clamp(0.0, 500.0)
}

/// Step speeds (m/s, index = arriving fix) and turning angles (rad) from the fixes.
pub fn steps(obs: &[Obs]) -> (Vec<Option<f64>>, Vec<Option<f64>>) {
    let n = obs.len();
    let mut spd = vec![None; n];
    let mut head = vec![None; n];
    let mut turn = vec![None; n];
    for i in 1..n {
        let dt = obs[i].t - obs[i - 1].t;
        if dt <= 0.0 {
            continue;
        }
        let (dx, dy) = (obs[i].x - obs[i - 1].x, obs[i].y - obs[i - 1].y);
        let d = (dx * dx + dy * dy).sqrt();
        spd[i] = Some(d / dt);
        if d > 0.0 {
            head[i] = Some(dy.atan2(dx));
        }
    }
    for i in 2..n {
        if let (Some(a), Some(b)) = (head[i - 1], head[i]) {
            let mut d = b - a;
            while d > PI {
                d -= 2.0 * PI;
            }
            while d < -PI {
                d += 2.0 * PI;
            }
            turn[i] = Some(d);
        }
    }
    (spd, turn)
}

/// Fit a `k`-state HMM on speed and turning angle (EM), then decode the states (Viterbi).
/// States are ordered by mean speed (0 = slowest).
pub fn hmm(obs: &[Obs], k: usize) -> Option<Hmm> {
    let (spd, turn) = steps(obs);
    let idx: Vec<usize> = (0..obs.len()).filter(|&i| spd[i].is_some()).collect();
    let t_n = idx.len();
    if t_n < 10 * k || !(1..=4).contains(&k) {
        return None;
    }
    let v: Vec<f64> = idx.iter().map(|&i| spd[i].unwrap()).collect();
    let a: Vec<Option<f64>> = idx.iter().map(|&i| turn[i]).collect();
    let any_zero = v.contains(&0.0);
    let mut nz: Vec<f64> = v.iter().copied().filter(|x| *x > 0.0).collect();
    if nz.len() < 2 * k {
        return None;
    }
    nz.sort_by(|a, b| a.total_cmp(b));
    let mut st: Vec<HmmState> = (0..k)
        .map(|s| {
            let q = nz[((s as f64 + 0.5) / k as f64 * nz.len() as f64) as usize % nz.len()];
            HmmState {
                speed_mean: q.max(1e-6),
                speed_sd: q.max(1e-6),
                zero_p: if any_zero { 0.1 } else { 0.0 },
                angle_mean: 0.0,
                kappa: 0.2 + 2.0 * s as f64 / (k.max(2) - 1) as f64,
            }
        })
        .collect();
    let mut tm = vec![vec![0.1 / (k - 1).max(1) as f64; k]; k];
    for (i, row) in tm.iter_mut().enumerate() {
        row[i] = if k == 1 { 1.0 } else { 0.9 };
    }
    let mut delta = vec![1.0 / k as f64; k];
    let lemit = |st: &[HmmState], t: usize| -> Vec<f64> {
        st.iter()
            .map(|s| {
                let ls = if v[t] == 0.0 {
                    s.zero_p.max(1e-300).ln()
                } else {
                    let shape = (s.speed_mean / s.speed_sd).powi(2);
                    let rate = shape / s.speed_mean;
                    let lg = shape * rate.ln() - ln_gamma(shape) + (shape - 1.0) * v[t].ln()
                        - rate * v[t];
                    (1.0 - s.zero_p).max(1e-300).ln() + lg
                };
                let la = match a[t] {
                    Some(th) => {
                        s.kappa * (th - s.angle_mean).cos() - (2.0 * PI).ln() - ln_i0(s.kappa)
                    }
                    None => 0.0,
                };
                ls + la
            })
            .collect()
    };
    let mut ll_old = f64::NEG_INFINITY;
    let mut ll = f64::NEG_INFINITY;
    let mut iters = 0;
    let mut gam = vec![vec![0.0; k]; t_n];
    for it in 0..300 {
        iters = it + 1;
        // E-step: scaled forward–backward
        let le: Vec<Vec<f64>> = (0..t_n).map(|t| lemit(&st, t)).collect();
        let mut b = vec![vec![0.0; k]; t_n];
        let mut off = vec![0.0; t_n];
        for t in 0..t_n {
            let m = le[t].iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            off[t] = m;
            for s in 0..k {
                b[t][s] = (le[t][s] - m).exp();
            }
        }
        let mut alpha = vec![vec![0.0; k]; t_n];
        let mut c = vec![0.0; t_n];
        for t in 0..t_n {
            for s in 0..k {
                let prior = if t == 0 {
                    delta[s]
                } else {
                    (0..k).map(|r| alpha[t - 1][r] * tm[r][s]).sum()
                };
                alpha[t][s] = prior * b[t][s];
            }
            c[t] = alpha[t].iter().sum::<f64>().max(1e-300);
            for s in 0..k {
                alpha[t][s] /= c[t];
            }
        }
        ll = (0..t_n).map(|t| c[t].ln() + off[t]).sum();
        let mut beta = vec![vec![1.0; k]; t_n];
        for t in (0..t_n - 1).rev() {
            for r in 0..k {
                beta[t][r] = (0..k)
                    .map(|s| tm[r][s] * b[t + 1][s] * beta[t + 1][s])
                    .sum::<f64>()
                    / c[t + 1];
            }
        }
        let mut xi = vec![vec![0.0; k]; k];
        for t in 0..t_n {
            let z: f64 = (0..k)
                .map(|s| alpha[t][s] * beta[t][s])
                .sum::<f64>()
                .max(1e-300);
            for s in 0..k {
                gam[t][s] = alpha[t][s] * beta[t][s] / z;
            }
            if t + 1 < t_n {
                for r in 0..k {
                    for s in 0..k {
                        xi[r][s] +=
                            alpha[t][r] * tm[r][s] * b[t + 1][s] * beta[t + 1][s] / c[t + 1];
                    }
                }
            }
        }
        if (ll - ll_old).abs() < 1e-7 * (1.0 + ll.abs()) {
            break;
        }
        ll_old = ll;
        // M-step
        delta = gam[0].clone();
        for r in 0..k {
            let s: f64 = xi[r].iter().sum::<f64>().max(1e-300);
            for c2 in 0..k {
                tm[r][c2] = (xi[r][c2] / s).max(1e-8);
            }
            let s2: f64 = tm[r].iter().sum();
            for c2 in 0..k {
                tm[r][c2] /= s2;
            }
        }
        for (s, state) in st.iter_mut().enumerate() {
            let (mut w0, mut wn, mut sv, mut slv) = (0.0, 0.0, 0.0, 0.0);
            let (mut wc, mut ws, mut wa) = (0.0, 0.0, 0.0);
            for t in 0..t_n {
                let w = gam[t][s];
                if v[t] == 0.0 {
                    w0 += w;
                } else {
                    wn += w;
                    sv += w * v[t];
                    slv += w * v[t].ln();
                }
                if let Some(th) = a[t] {
                    wc += w * th.cos();
                    ws += w * th.sin();
                    wa += w;
                }
            }
            if any_zero {
                state.zero_p = (w0 / (w0 + wn).max(1e-300)).clamp(1e-6, 1.0 - 1e-6);
            }
            if wn > 1e-9 {
                let m = (sv / wn).max(1e-9);
                let s_ = (m.ln() - slv / wn).max(1e-6);
                let shape = ((3.0 - s_ + ((s_ - 3.0).powi(2) + 24.0 * s_).sqrt()) / (12.0 * s_))
                    .clamp(0.05, 1e4);
                state.speed_mean = m;
                state.speed_sd = m / shape.sqrt();
            }
            if wa > 1e-9 {
                let (cm, sm) = (wc / wa, ws / wa);
                state.angle_mean = sm.atan2(cm);
                state.kappa = a1inv((cm * cm + sm * sm).sqrt());
            }
        }
    }
    // order by speed
    let mut order: Vec<usize> = (0..k).collect();
    order.sort_by(|&x, &y| st[x].speed_mean.total_cmp(&st[y].speed_mean));
    let st2: Vec<HmmState> = order.iter().map(|&o| st[o]).collect();
    let tm2: Vec<Vec<f64>> = order
        .iter()
        .map(|&r| order.iter().map(|&c2| tm[r][c2]).collect())
        .collect();
    let delta2: Vec<f64> = order.iter().map(|&o| delta[o]).collect();
    // Viterbi
    let le: Vec<Vec<f64>> = (0..t_n).map(|t| lemit(&st2, t)).collect();
    let mut dp = vec![vec![f64::NEG_INFINITY; k]; t_n];
    let mut bp = vec![vec![0usize; k]; t_n];
    for s in 0..k {
        dp[0][s] = delta2[s].max(1e-300).ln() + le[0][s];
    }
    for t in 1..t_n {
        for s in 0..k {
            let (bi, bv) = (0..k)
                .map(|r| (r, dp[t - 1][r] + tm2[r][s].ln()))
                .max_by(|x, y| x.1.total_cmp(&y.1))
                .unwrap();
            dp[t][s] = bv + le[t][s];
            bp[t][s] = bi;
        }
    }
    let mut cur = (0..k)
        .max_by(|&x, &y| dp[t_n - 1][x].total_cmp(&dp[t_n - 1][y]))
        .unwrap();
    let mut seq = vec![0usize; t_n];
    for t in (0..t_n).rev() {
        seq[t] = cur;
        if t > 0 {
            cur = bp[t][cur];
        }
    }
    let mut path = vec![None; obs.len()];
    for (t, &i) in idx.iter().enumerate() {
        path[i] = Some(seq[t] as u8);
    }
    let per_state = 4 + usize::from(any_zero);
    let np = (k * per_state + k * (k - 1) + (k - 1)) as f64;
    let dts: Vec<f64> = obs
        .windows(2)
        .map(|w| w[1].t - w[0].t)
        .filter(|d| *d > 0.0)
        .collect();
    let md = dts.iter().sum::<f64>() / dts.len().max(1) as f64;
    let sd = (dts.iter().map(|d| (d - md).powi(2)).sum::<f64>() / dts.len().max(1) as f64).sqrt();
    Some(Hmm {
        states: st2,
        trans: tm2,
        loglik: ll,
        aic: -2.0 * ll + 2.0 * np,
        bic: -2.0 * ll + np * (t_n as f64).ln(),
        path,
        dt_cv: if md > 0.0 { sd / md } else { f64::NAN },
        iterations: iters,
    })
}

// ---------------------------------------------------------------------------------------------
// change points

/// The series segmented by [`change_points`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CpVar {
    /// ln of the step speed — switches between movement modes.
    LogSpeed,
    /// Net displacement from the first fix (km) — migration departures / arrivals.
    Displacement,
}

impl CpVar {
    pub const ALL: [CpVar; 2] = [CpVar::Displacement, CpVar::LogSpeed];
    pub fn label(self) -> &'static str {
        match self {
            CpVar::LogSpeed => "log step speed",
            CpVar::Displacement => "net displacement from the first fix (km)",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Segments {
    pub var: CpVar,
    /// Segment per observation (`None` where the series is undefined).
    pub seg: Vec<Option<u16>>,
    /// (first obs, last obs, mean of the series) per segment.
    pub parts: Vec<(usize, usize, f64)>,
    pub penalty: f64,
}

/// Change points in the mean of the chosen series: penalised least squares, solved exactly
/// with PELT. Penalty = `mult · 2 σ̂² ln n` (σ̂ from the MAD of first differences), minimum
/// segment length `min_len` observations.
pub fn change_points(obs: &[Obs], var: CpVar, mult: f64, min_len: usize) -> Option<Segments> {
    let (spd, _) = steps(obs);
    let (xs, at): (Vec<f64>, Vec<usize>) = match var {
        CpVar::LogSpeed => (0..obs.len())
            .filter_map(|i| spd[i].map(|s| ((s.max(1e-3)).ln(), i)))
            .unzip(),
        CpVar::Displacement => {
            let o0 = obs.first()?;
            obs.iter()
                .enumerate()
                .map(|(i, o)| {
                    (
                        ((o.x - o0.x).powi(2) + (o.y - o0.y).powi(2)).sqrt() / 1000.0,
                        i,
                    )
                })
                .unzip()
        }
    };
    let n = xs.len();
    let min_len = min_len.max(1);
    if n < 2 * min_len {
        return None;
    }
    let diffs: Vec<f64> = xs.windows(2).map(|w| w[1] - w[0]).collect();
    let md = median(diffs.clone()).unwrap_or(0.0);
    let mad = median(diffs.iter().map(|d| (d - md).abs()).collect()).unwrap_or(0.0);
    let mut s2 = (1.4826 * mad).powi(2) / 2.0;
    if !(s2 > 0.0) {
        let m = xs.iter().sum::<f64>() / n as f64;
        s2 = (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n as f64).max(1e-12) * 0.01;
    }
    let beta = mult.max(1e-6) * 2.0 * s2 * (n as f64).ln();
    let mut cs = vec![0.0; n + 1];
    let mut cs2 = vec![0.0; n + 1];
    for i in 0..n {
        cs[i + 1] = cs[i] + xs[i];
        cs2[i + 1] = cs2[i] + xs[i] * xs[i];
    }
    let cost = |s: usize, t: usize| {
        let l = (t - s) as f64;
        let sm = cs[t] - cs[s];
        (cs2[t] - cs2[s]) - sm * sm / l
    };
    let mut f = vec![f64::INFINITY; n + 1];
    let mut last = vec![0usize; n + 1];
    f[0] = -beta;
    let mut cand: Vec<usize> = vec![0];
    for t in 1..=n {
        let mut best = (f64::INFINITY, 0usize);
        for &s in &cand {
            if t - s >= min_len && f[s].is_finite() {
                let v = f[s] + cost(s, t) + beta;
                if v < best.0 {
                    best = (v, s);
                }
            }
        }
        f[t] = best.0;
        last[t] = best.1;
        if f[t].is_finite() {
            let ft = f[t];
            cand.retain(|&s| t - s < min_len || !f[s].is_finite() || f[s] + cost(s, t) <= ft);
            cand.push(t);
        }
    }
    if !f[n].is_finite() {
        return None;
    }
    let mut bounds = Vec::new();
    let mut t = n;
    while t > 0 {
        let s = last[t];
        bounds.push((s, t));
        t = s;
    }
    bounds.reverse();
    let mut seg = vec![None; obs.len()];
    let mut parts = Vec::new();
    for (k, &(s, e)) in bounds.iter().enumerate() {
        for &i in &at[s..e] {
            seg[i] = Some(k as u16);
        }
        parts.push((at[s], at[e - 1], (cs[e] - cs[s]) / (e - s) as f64));
    }
    Some(Segments {
        var,
        seg,
        parts,
        penalty: beta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple deterministic RNG (xorshift) + Box–Muller.
    struct Rng(u64);
    impl Rng {
        fn u(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn n(&mut self) -> f64 {
            let (a, b) = (self.u().max(1e-300), self.u());
            (-2.0 * a.ln()).sqrt() * (2.0 * PI * b).cos()
        }
    }

    /// Exact OU simulation (per axis variance σ, timescale τ).
    fn sim_ou(n: usize, dt: f64, sigma: f64, tau: f64, seed: u64) -> Vec<Obs> {
        let mut r = Rng(seed);
        let (mut x, mut y) = (r.n() * sigma.sqrt(), r.n() * sigma.sqrt());
        let f = (-dt / tau).exp();
        let q = (sigma * (1.0 - f * f)).sqrt();
        (0..n)
            .map(|i| {
                if i > 0 {
                    x = f * x + q * r.n();
                    y = f * y + q * r.n();
                }
                Obs {
                    t: i as f64 * dt,
                    x,
                    y,
                    err_m: None,
                }
            })
            .collect()
    }

    #[test]
    fn projection_round_trip() {
        let p = Proj::new(48.0, 9.0);
        let (x, y) = p.fwd(48.1, 9.2);
        let (la, lo) = p.inv(x, y);
        assert!((la - 48.1).abs() < 1e-9 && (lo - 9.2).abs() < 1e-9);
        assert!((y - 11_119.5).abs() < 5.0, "{y}");
        // dateline
        let p = Proj::new(0.0, 179.9);
        let (x, _) = p.fwd(0.0, -179.9);
        assert!((x - 22_239.0).abs() < 10.0, "{x}");
    }

    #[test]
    fn ou_fit_recovers_parameters() {
        let sigma = 1000.0f64.powi(2);
        let tau = 6.0 * 3600.0;
        let obs = sim_ou(1500, 1800.0, sigma, tau, 42);
        let fits = select(&obs, ErrorMode::Ignore);
        assert!(!fits.is_empty());
        let ou = fits.iter().find(|f| f.model == Model::Ou).unwrap();
        assert!((ou.sigma / sigma - 1.0).abs() < 0.35, "σ {}", ou.sigma);
        assert!((ou.tau_p / tau - 1.0).abs() < 0.4, "τ {}", ou.tau_p);
        // IID must lose clearly on autocorrelated data
        let iid = fits.iter().find(|f| f.model == Model::Iid).unwrap();
        assert!(iid.aicc > ou.aicc + 100.0);
        assert!(ou.range_resident());
        let a = ou.area(0.95);
        assert!(a.lo < a.est && a.est < a.hi);
        // DOF ≈ duration / τ order of magnitude, far below N
        let dof = ou.dof_area();
        assert!(dof > 20.0 && dof < 400.0, "dof {dof}");
    }

    #[test]
    fn ouf_speed_is_recovered() {
        // OUF by exact discretisation of the same transition the filter uses
        let (sigma, tp, tv, dt) = (500.0f64.powi(2), 3.0 * 3600.0, 600.0, 120.0);
        let (f, q) = transition(Model::Ouf, sigma, tp, tv, dt);
        // Cholesky of q
        let l00 = q[0][0].sqrt();
        let l10 = q[1][0] / l00;
        let l11 = (q[1][1] - l10 * l10).max(0.0).sqrt();
        let mut r = Rng(7);
        let mut sx = [r.n() * sigma.sqrt(), r.n() * (sigma / (tp * tv)).sqrt()];
        let mut sy = [r.n() * sigma.sqrt(), r.n() * (sigma / (tp * tv)).sqrt()];
        let obs: Vec<Obs> = (0..2000)
            .map(|i| {
                if i > 0 {
                    for s in [&mut sx, &mut sy] {
                        let (a, b) = (r.n(), r.n());
                        let p = [
                            f[0][0] * s[0] + f[0][1] * s[1],
                            f[1][0] * s[0] + f[1][1] * s[1],
                        ];
                        *s = [p[0] + l00 * a, p[1] + l10 * a + l11 * b];
                    }
                }
                Obs {
                    t: i as f64 * dt,
                    x: sx[0],
                    y: sy[0],
                    err_m: None,
                }
            })
            .collect();
        let fits = select(&obs, ErrorMode::Ignore);
        assert_eq!(
            fits[0].model,
            Model::Ouf,
            "{:?}",
            fits.iter().map(|f| (f.model, f.aicc)).collect::<Vec<_>>()
        );
        let s = fits[0].speed().unwrap();
        let truth = (PI / 2.0).sqrt() * (sigma / (tp * tv)).sqrt();
        assert!(
            (s.est / truth - 1.0).abs() < 0.25,
            "speed {} vs {truth}",
            s.est
        );
        assert!(s.lo < truth * 1.2 && s.hi > truth * 0.8);
    }

    #[test]
    fn variogram_rises_to_sigma() {
        let sigma = 200.0f64.powi(2);
        let obs = sim_ou(2000, 600.0, sigma, 3600.0, 3);
        let v = variogram(&obs);
        assert!(v.len() > 10);
        assert!(v[0].gamma < 0.5 * sigma);
        let tail = v.iter().rev().take(5).map(|p| p.gamma).sum::<f64>() / 5.0;
        assert!((tail / sigma - 1.0).abs() < 0.4, "{tail}");
    }

    #[test]
    fn home_ranges_have_sensible_areas() {
        let sigma = 1000.0f64.powi(2);
        let obs = sim_ou(3000, 3600.0, sigma, 7200.0, 11);
        let p = Proj::new(50.0, 10.0);
        let gauss = -2.0 * (0.05f64).ln() * PI * sigma; // ≈ 18.8 km²
        let m = mcp(&obs, &p, 0.95).unwrap();
        assert!(
            m.area_m2 > 0.4 * gauss && m.area_m2 < 2.0 * gauss,
            "mcp {}",
            m.area_m2
        );
        let h = reference_bandwidth(sample_sd(&obs), obs.len() as f64);
        let k = kde(&obs, &p, h, &[0.5, 0.95]);
        assert_eq!(k.len(), 2);
        assert!(k[0].area_m2 < k[1].area_m2);
        assert!(
            k[1].area_m2 > 0.7 * gauss && k[1].area_m2 < 1.6 * gauss,
            "kde {}",
            k[1].area_m2
        );
        assert!(!k[1].segs.is_empty());
        // fewer effective samples → wider kernel → bigger range
        let h2 = reference_bandwidth(sample_sd(&obs), 30.0);
        let k2 = kde(&obs, &p, h2, &[0.95]);
        assert!(k2[0].area_m2 > k[1].area_m2);
    }

    /// Two behaviours: slow tortuous foraging bouts and fast directed travel.
    fn two_mode_track(seed: u64) -> (Vec<Obs>, Vec<u8>) {
        let mut r = Rng(seed);
        let (mut x, mut y, mut h) = (0.0f64, 0.0f64, 0.0f64);
        let mut obs = Vec::new();
        let mut truth = Vec::new();
        for i in 0..1200 {
            let fast = (i / 100) % 2 == 1;
            let (sp, turn) = if fast { (8.0, 0.1) } else { (0.5, 2.0) };
            h += turn * r.n();
            let v = sp * (1.0 + 0.3 * r.n()).abs();
            x += v * 60.0 * h.cos();
            y += v * 60.0 * h.sin();
            obs.push(Obs {
                t: i as f64 * 60.0,
                x,
                y,
                err_m: None,
            });
            truth.push(u8::from(fast));
        }
        (obs, truth)
    }

    #[test]
    fn hmm_finds_two_modes() {
        let (obs, truth) = two_mode_track(5);
        let h = hmm(&obs, 2).unwrap();
        assert!(
            h.states[0].speed_mean < 1.5 && h.states[1].speed_mean > 5.0,
            "{:?}",
            h.states
        );
        assert!(h.states[1].kappa > h.states[0].kappa);
        let agree = h
            .path
            .iter()
            .zip(&truth)
            .filter(|(p, t)| p.is_some_and(|p| p == **t))
            .count();
        assert!(agree as f64 > 0.9 * obs.len() as f64, "agree {agree}");
        assert!(h.dt_cv < 1e-9);
    }

    #[test]
    fn change_points_split_modes() {
        let (obs, _) = two_mode_track(9);
        let s = change_points(&obs, CpVar::LogSpeed, 1.0, 5).unwrap();
        // 12 bouts of 100 fixes
        assert!((10..=16).contains(&s.parts.len()), "{}", s.parts.len());
        // a migration: resident, travel, resident → displacement finds the plateaus
        let mut obs = Vec::new();
        let mut r = Rng(1);
        for i in 0..300 {
            let base = if i < 100 {
                0.0
            } else if i < 120 {
                (i - 100) as f64 * 10_000.0
            } else {
                200_000.0
            };
            obs.push(Obs {
                t: i as f64 * 3600.0,
                x: base + 500.0 * r.n(),
                y: 500.0 * r.n(),
                err_m: None,
            });
        }
        let s = change_points(&obs, CpVar::Displacement, 1.0, 5).unwrap();
        assert!(s.parts.len() >= 3);
        let first = s.parts[0];
        assert!((95..=105).contains(&first.1), "{:?}", s.parts);
        let lastp = s.parts.last().unwrap();
        assert!((lastp.2 - 200.0).abs() < 5.0);
    }

    #[test]
    fn fpt_and_scan() {
        // straight line at 1 m/s: FPT(r) = 2r
        let obs: Vec<Obs> = (0..200)
            .map(|i| Obs {
                t: i as f64 * 10.0,
                x: i as f64 * 10.0,
                y: 0.0,
                err_m: None,
            })
            .collect();
        let f = fpt(&obs, 55.0);
        assert!(f[0].is_none());
        assert!((f[100].unwrap() - 110.0).abs() < 1e-6);
        let (obs2, _) = two_mode_track(3);
        let scan = fpt_scan(&obs2);
        assert_eq!(scan.len(), 24);
        assert!(scan.iter().any(|s| s.1.is_finite()));
    }

    #[test]
    fn known_errors_enter_the_likelihood() {
        let sigma = 1000.0f64.powi(2);
        let mut obs = sim_ou(800, 3600.0, sigma, 4.0 * 3600.0, 21);
        let mut r = Rng(99);
        for o in obs.iter_mut() {
            o.x += 300.0 * r.n();
            o.y += 300.0 * r.n();
            o.err_m = Some(300.0 * 2f64.sqrt());
        }
        let with = fit(&obs, Model::Ou, ErrorMode::FromFixes).unwrap();
        let est = fit(&obs, Model::Ou, ErrorMode::Estimate).unwrap();
        let e = est.error_rms().unwrap();
        assert!(
            (e.est / (300.0 * 2f64.sqrt()) - 1.0).abs() < 0.5,
            "err {}",
            e.est
        );
        assert!((with.sigma / sigma - 1.0).abs() < 0.4);
    }
}
