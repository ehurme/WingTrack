//! 3D view: tilted basemap, wind-speed raster, stacked pressure-level planes with flowing
//! particles, and tracks drawn at flight altitude with drop lines.
//!
//! Everything is projected on the CPU and painted with egui (no extra GPU code), so it runs
//! unchanged on desktop and in the browser. Depth sorting is by layer: ground → raster →
//! planes/particles (low → high) → tracks → markers.

use crate::geo;
use crate::model::Track;
use crate::wind3d::FieldSlice;
use egui::{
    Align2, Color32, FontId, Mesh, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, pos2, vec2,
};
use walkers::{Tile, TileId, Tiles};

const EARTH_KM: f64 = 6371.0088;

#[derive(Clone, Debug)]
pub struct Camera {
    /// Heading the camera looks towards (degrees from north).
    pub yaw: f32,
    /// Angle above the horizon the camera looks down from (degrees).
    pub pitch: f32,
    /// Distance from the target (km).
    pub dist: f32,
    /// Look-at point, local ENU km.
    pub target: [f32; 3],
    pub fov_deg: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Camera {
            yaw: 0.0,
            pitch: 42.0,
            dist: 800.0,
            target: [0.0; 3],
            fov_deg: 45.0,
        }
    }
}

struct Particle {
    lat: f64,
    lon: f64,
    trail: Vec<(f64, f64)>,
    age: u32,
    max_age: u32,
}

pub struct View3D {
    pub cam: Camera,
    /// Vertical exaggeration.
    pub exag: f32,
    pub ground_dim: f32,
    pub show_raster: bool,
    /// Which displayed level (index into the slice levels) colours the ground raster.
    pub raster_level: usize,
    pub show_planes: bool,
    pub particles_on: bool,
    pub particles_per_level: usize,
    /// Simulated hours of wind transport per real second.
    pub flow_hours_per_s: f32,
    pub show_drop_lines: bool,
    pub show_shadows: bool,
    /// Altitude (m) used for fixes without any altitude.
    pub default_alt_m: f32,
    /// A step is "not detected" (dotted) when longer than this × the track's median interval.
    pub gap_factor: f32,
    pub follow: bool,
    origin: Option<(f64, f64)>,
    needs_fit: bool,
    particles: Vec<Vec<Particle>>,
    particle_levels: Vec<usize>,
    rng: u64,
}

impl Default for View3D {
    fn default() -> Self {
        View3D {
            cam: Camera::default(),
            exag: 120.0,
            ground_dim: 0.45,
            show_raster: true,
            raster_level: 0,
            show_planes: true,
            particles_on: true,
            particles_per_level: 700,
            flow_hours_per_s: 3.0,
            show_drop_lines: true,
            show_shadows: true,
            default_alt_m: 0.0,
            gap_factor: 2.5,
            follow: false,
            origin: None,
            needs_fit: true,
            particles: Vec::new(),
            particle_levels: Vec::new(),
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// Everything the 3D view needs from the app for one frame.
pub struct Scene<'a> {
    pub tracks: &'a [Track],
    pub t_now: f64,
    pub trail_to_cursor: bool,
    pub selected: Option<(usize, usize)>,
    pub focus_track: Option<usize>,
    pub slice: Option<&'a FieldSlice>,
    /// Pressure (hPa) of each slice level, for labels.
    pub slice_level_hpa: Vec<f32>,
    pub base: Option<&'a mut dyn Tiles>,
    pub sat: Option<(&'a mut dyn Tiles, f32)>,
    pub source_label: String,
    pub basemap_attribution: String,
    pub max_error_m: f32,
}

#[derive(Default)]
pub struct Output {
    pub hover: Option<(usize, usize, Pos2)>,
    pub clicked: Option<(usize, usize)>,
}

struct Proj {
    lat0: f64,
    lon0: f64,
    coslat: f64,
    exag: f32,
    eye: [f32; 3],
    f: [f32; 3],
    r: [f32; 3],
    u: [f32; 3],
    center: Pos2,
    scale: f32,
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn norm(a: [f32; 3]) -> [f32; 3] {
    let l = dot(a, a).sqrt().max(1e-9);
    [a[0] / l, a[1] / l, a[2] / l]
}

impl Proj {
    fn new(cam: &Camera, origin: (f64, f64), exag: f32, rect: Rect) -> Proj {
        let (yaw, pitch) = (
            cam.yaw.to_radians(),
            cam.pitch.clamp(3.0, 89.9).to_radians(),
        );
        let f = [
            yaw.sin() * pitch.cos(),
            yaw.cos() * pitch.cos(),
            -pitch.sin(),
        ];
        let eye = [
            cam.target[0] - f[0] * cam.dist,
            cam.target[1] - f[1] * cam.dist,
            cam.target[2] - f[2] * cam.dist,
        ];
        let r = norm(cross(f, [0.0, 0.0, 1.0]));
        let u = cross(r, f);
        let scale = (rect.height() / 2.0) / (cam.fov_deg.to_radians() / 2.0).tan();
        Proj {
            lat0: origin.0,
            lon0: origin.1,
            coslat: origin.0.to_radians().cos(),
            exag,
            eye,
            f,
            r,
            u,
            center: rect.center(),
            scale,
        }
    }
    fn enu(&self, lat: f64, lon: f64, alt_m: f32) -> [f32; 3] {
        let x = (lon - self.lon0).to_radians() * EARTH_KM * self.coslat;
        let y = (lat - self.lat0).to_radians() * EARTH_KM;
        [x as f32, y as f32, alt_m / 1000.0 * self.exag]
    }
    fn latlon(&self, x: f32, y: f32) -> (f64, f64) {
        (
            self.lat0 + (y as f64 / EARTH_KM).to_degrees(),
            self.lon0 + (x as f64 / (EARTH_KM * self.coslat)).to_degrees(),
        )
    }
    fn project(&self, p: [f32; 3]) -> Option<Pos2> {
        let d = sub(p, self.eye);
        let zc = dot(d, self.f);
        if zc < 1.0 {
            return None;
        }
        Some(pos2(
            self.center.x + dot(d, self.r) / zc * self.scale,
            self.center.y - dot(d, self.u) / zc * self.scale,
        ))
    }
    fn geo(&self, lat: f64, lon: f64, alt_m: f32) -> Option<Pos2> {
        self.project(self.enu(lat, lon, alt_m))
    }
}

/// Wind-speed ramp (m/s) similar to common wind maps: teal → green → yellow → red → purple.
pub fn wind_color(speed: f32, alpha: u8) -> Color32 {
    const S: [(f32, [u8; 3]); 6] = [
        (0.0, [30, 60, 90]),
        (5.0, [40, 140, 120]),
        (10.0, [150, 190, 60]),
        (15.0, [240, 200, 40]),
        (20.0, [230, 80, 40]),
        (28.0, [150, 30, 120]),
    ];
    let s = speed.clamp(0.0, 28.0);
    let mut c = S[5].1;
    for w in S.windows(2) {
        if s <= w[1].0 {
            let f = (s - w[0].0) / (w[1].0 - w[0].0);
            let l = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f) as u8;
            c = [
                l(w[0].1[0], w[1].1[0]),
                l(w[0].1[1], w[1].1[1]),
                l(w[0].1[2], w[1].1[2]),
            ];
            break;
        }
    }
    Color32::from_rgba_unmultiplied(c[0], c[1], c[2], alpha)
}

fn tile_lat(y: f64, z: u8) -> f64 {
    let n = std::f64::consts::PI * (1.0 - 2.0 * y / (1u64 << z) as f64);
    n.sinh().atan().to_degrees()
}
fn tile_lon(x: f64, z: u8) -> f64 {
    x / (1u64 << z) as f64 * 360.0 - 180.0
}
fn lon_to_tile(lon: f64, z: u8) -> f64 {
    (lon + 180.0) / 360.0 * (1u64 << z) as f64
}
fn lat_to_tile(lat: f64, z: u8) -> f64 {
    let r = lat.clamp(-85.0, 85.0).to_radians();
    (1.0 - (r.tan() + 1.0 / r.cos()).ln() / std::f64::consts::PI) / 2.0 * (1u64 << z) as f64
}

impl View3D {
    pub fn request_fit(&mut self) {
        self.needs_fit = true;
    }

    fn rand(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    fn fit(&mut self, scene: &Scene) {
        let (mut s, mut n, mut w, mut e) = (90.0f64, -90.0f64, 180.0f64, -180.0f64);
        for (i, tr) in scene.tracks.iter().enumerate() {
            if !tr.visible || scene.focus_track.is_some_and(|f| f != i) {
                continue;
            }
            for f in &tr.fixes {
                s = s.min(f.lat);
                n = n.max(f.lat);
                w = w.min(f.lon);
                e = e.max(f.lon);
            }
        }
        if s > n {
            return;
        }
        let lat0 = (s + n) / 2.0;
        self.origin = Some((lat0, (w + e) / 2.0));
        let ext = ((n - s).to_radians() * EARTH_KM)
            .max((e - w).to_radians() * EARTH_KM * lat0.to_radians().cos())
            .max(50.0);
        self.cam.target = [0.0, 0.0, 0.0];
        self.cam.dist =
            (ext as f32) * 1.15 / (self.cam.fov_deg.to_radians() / 2.0).tan() / 2.0 + 50.0;
        self.cam.yaw = 0.0;
        self.cam.pitch = 42.0;
        self.needs_fit = false;
    }

    fn controls(&mut self, ui: &Ui, resp: &Response) {
        if resp.dragged_by(egui::PointerButton::Primary) && !ui.input(|i| i.modifiers.shift) {
            let d = resp.drag_delta();
            self.cam.yaw = (self.cam.yaw - d.x * 0.3).rem_euclid(360.0);
            self.cam.pitch = (self.cam.pitch + d.y * 0.25).clamp(5.0, 89.9);
        } else if resp.dragged_by(egui::PointerButton::Secondary)
            || resp.dragged_by(egui::PointerButton::Middle)
            || (resp.dragged_by(egui::PointerButton::Primary) && ui.input(|i| i.modifiers.shift))
        {
            let d = resp.drag_delta();
            let k = self.cam.dist / 800.0;
            let (sy, cy) = self.cam.yaw.to_radians().sin_cos();
            // screen right = (cos yaw, -sin yaw), screen up = (sin yaw, cos yaw) on the ground
            self.cam.target[0] -= (d.x * cy - d.y * sy) * k;
            self.cam.target[1] -= (-d.x * sy - d.y * cy) * k;
            self.follow = false;
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta().y);
            let zoom = ui.input(|i| i.zoom_delta());
            if scroll != 0.0 {
                self.cam.dist = (self.cam.dist * (-scroll * 0.0015).exp()).clamp(5.0, 20000.0);
            }
            if zoom != 1.0 {
                self.cam.dist = (self.cam.dist / zoom).clamp(5.0, 20000.0);
            }
        }
    }

    fn update_particles(&mut self, slice: &FieldSlice, dt: f32) {
        if self.particle_levels != slice.levels || self.particles.len() != slice.levels.len() {
            self.particle_levels = slice.levels.clone();
            self.particles = (0..slice.levels.len()).map(|_| Vec::new()).collect();
        }
        let (s, n, w, e) = slice.bbox();
        let sim_s = (self.flow_hours_per_s * 3600.0 * dt.min(0.1)) as f64;
        let want = self.particles_per_level;
        for k in 0..self.particles.len() {
            let mut ps = std::mem::take(&mut self.particles[k]);
            ps.truncate(want);
            while ps.len() < want {
                let (la, lo) = (s + self.rand() * (n - s), w + self.rand() * (e - w));
                let max_age = 50 + (self.rand() * 90.0) as u32;
                let age = (self.rand() * max_age as f64) as u32;
                ps.push(Particle {
                    lat: la,
                    lon: lo,
                    trail: Vec::new(),
                    age,
                    max_age,
                });
            }
            for p in &mut ps {
                p.age += 1;
                let wind = slice.wind(k, p.lat, p.lon);
                let out = p.lat < s || p.lat > n || p.lon < w || p.lon > e;
                if p.age > p.max_age || wind.is_none() || out {
                    p.lat = s + self.rand() * (n - s);
                    p.lon = w + self.rand() * (e - w);
                    p.trail.clear();
                    p.age = 0;
                    continue;
                }
                let (u, v) = wind.unwrap_or((0.0, 0.0));
                p.trail.push((p.lat, p.lon));
                if p.trail.len() > 14 {
                    p.trail.remove(0);
                }
                p.lat += (v as f64 * sim_s / 1000.0 / EARTH_KM).to_degrees();
                p.lon += (u as f64 * sim_s / 1000.0 / (EARTH_KM * p.lat.to_radians().cos()))
                    .to_degrees();
            }
            self.particles[k] = ps;
        }
    }

    /// Draw the 3D view into `ui`'s available rect.
    pub fn show(&mut self, ui: &mut Ui, mut scene: Scene) -> Output {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let mut out = Output::default();
        if self.needs_fit || self.origin.is_none() {
            self.fit(&scene);
        }
        let Some(origin) = self.origin else {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Load tracks to see them in 3D",
                FontId::proportional(16.0),
                Color32::GRAY,
            );
            return out;
        };
        self.controls(ui, &resp);
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(14, 18, 24));

        // Follow the selected animal.
        if self.follow
            && let Some(ti) = scene.selected.map(|s| s.0).or(scene.focus_track)
            && let Some((la, lo, _)) = scene
                .tracks
                .get(ti)
                .and_then(|t| t.position_at(scene.t_now))
        {
            let p0 = Proj::new(&self.cam, origin, self.exag, rect);
            let tgt = p0.enu(la, lo, 0.0);
            self.cam.target[0] += (tgt[0] - self.cam.target[0]) * 0.15;
            self.cam.target[1] += (tgt[1] - self.cam.target[1]) * 0.15;
        }
        let pr = Proj::new(&self.cam, origin, self.exag, rect);

        // ---------------------------------------------------------------- ground
        let view_bbox = {
            let r = (self.cam.dist * 1.6).max(100.0);
            let (s, w) = pr.latlon(self.cam.target[0] - r, self.cam.target[1] - r);
            let (n, e) = pr.latlon(self.cam.target[0] + r, self.cam.target[1] + r);
            (s.max(-84.0), n.min(84.0), w.max(-179.9), e.min(179.9))
        };
        let tint = Color32::from_gray(((1.0 - self.ground_dim) * 255.0) as u8);
        if let Some(base) = scene.base.as_deref_mut() {
            draw_tiles(&painter, &pr, base, view_bbox, tint);
        }
        if let Some((sat, op)) = scene.sat.as_mut() {
            let a = (*op * (1.0 - self.ground_dim * 0.5) * 255.0) as u8;
            draw_tiles(
                &painter,
                &pr,
                &mut **sat,
                view_bbox,
                Color32::from_rgba_premultiplied(a, a, a, a),
            );
        }

        // ---------------------------------------------------------------- wind field
        if let Some(slice) = scene.slice {
            let dt = ui.input(|i| i.stable_dt);
            if self.particles_on {
                self.update_particles(slice, dt);
                ui.ctx().request_repaint();
            }
            let rl = self.raster_level.min(slice.levels.len().saturating_sub(1));
            if self.show_raster && !slice.levels.is_empty() {
                let mut mesh = Mesh::default();
                let nl = slice.nlon;
                let mut idx = vec![u32::MAX; slice.nlat * nl];
                for i in 0..slice.nlat {
                    for j in 0..nl {
                        let (u, v) = slice.uv[rl][i * nl + j];
                        if !u.is_finite() {
                            continue;
                        }
                        let (la, lo) = (
                            slice.lat0 + i as f64 * slice.step_deg,
                            slice.lon0 + j as f64 * slice.step_deg,
                        );
                        if let Some(p) = pr.geo(la, lo, 0.0) {
                            idx[i * nl + j] = mesh.vertices.len() as u32;
                            mesh.colored_vertex(p, wind_color((u * u + v * v).sqrt(), 120));
                        }
                    }
                }
                for i in 0..slice.nlat.saturating_sub(1) {
                    for j in 0..nl.saturating_sub(1) {
                        let (a, b, c, d) = (
                            idx[i * nl + j],
                            idx[i * nl + j + 1],
                            idx[(i + 1) * nl + j],
                            idx[(i + 1) * nl + j + 1],
                        );
                        if a != u32::MAX && b != u32::MAX && c != u32::MAX && d != u32::MAX {
                            mesh.add_triangle(a, b, d);
                            mesh.add_triangle(a, d, c);
                        }
                    }
                }
                painter.add(Shape::mesh(mesh));
            }
            let (s, n, w, e) = slice.bbox();
            for (k, &h) in slice.mean_z.iter().enumerate() {
                if self.show_planes {
                    let corners = [(s, w), (s, e), (n, e), (n, w), (s, w)];
                    let pts: Vec<Pos2> = corners
                        .iter()
                        .filter_map(|&(la, lo)| pr.geo(la, lo, h))
                        .collect();
                    if pts.len() == 5 {
                        painter.add(Shape::line(
                            pts.clone(),
                            Stroke::new(1.0, Color32::from_white_alpha(70)),
                        ));
                        let anchor = pts[..4]
                            .iter()
                            .copied()
                            .min_by(|a, b| a.x.total_cmp(&b.x))
                            .unwrap_or(pts[0]);
                        let label = match scene.slice_level_hpa.get(k) {
                            Some(p) => format!("{h:.0} m  ({p:.0} hPa)"),
                            None => format!("{h:.0} m"),
                        };
                        painter.text(
                            anchor + vec2(-6.0, 0.0),
                            Align2::RIGHT_CENTER,
                            label,
                            FontId::monospace(13.0),
                            Color32::from_gray(220),
                        );
                    }
                }
                if self.particles_on && k < self.particles.len() {
                    for p in &self.particles[k] {
                        if p.trail.len() < 2 {
                            continue;
                        }
                        let sp = slice
                            .wind(k, p.lat, p.lon)
                            .map(|(u, v)| (u * u + v * v).sqrt())
                            .unwrap_or(0.0);
                        let base_a = (90.0 + sp * 9.0).min(230.0);
                        let fade_in = (p.age as f32 / 10.0).min(1.0)
                            * (1.0 - (p.age as f32 / p.max_age as f32).powi(4));
                        let pts: Vec<Pos2> = p
                            .trail
                            .iter()
                            .filter_map(|&(la, lo)| pr.geo(la, lo, h))
                            .collect();
                        let m = pts.len();
                        for (q, seg) in pts.windows(2).enumerate() {
                            let a = (base_a * fade_in * (q + 1) as f32 / m as f32) as u8;
                            painter.line_segment(
                                [seg[0], seg[1]],
                                Stroke::new(1.1, Color32::from_white_alpha(a)),
                            );
                        }
                    }
                }
            }
        }

        // ---------------------------------------------------------------- tracks
        let mut hits: Vec<(Pos2, usize, usize)> = Vec::new();
        let mut labels: Vec<(Pos2, Color32, String)> = Vec::new();
        for (ti, tr) in scene.tracks.iter().enumerate() {
            if !tr.visible || tr.fixes.is_empty() {
                continue;
            }
            let col = Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]);
            let alt = |i: usize| {
                tr.fixes[i]
                    .best_alt()
                    .map(|a| a.max(0.0))
                    .unwrap_or(self.default_alt_m)
            };
            let gap = gap_threshold(tr) * self.gap_factor as f64;
            let keep: Vec<usize> = (0..tr.fixes.len())
                .filter(|&i| tr.fixes[i].error_m.is_none_or(|e| e <= scene.max_error_m))
                .collect();
            let mut prev: Option<(usize, Pos2, Pos2)> = None;
            for &i in &keep {
                let f = &tr.fixes[i];
                let future = scene.trail_to_cursor && f.t as f64 > scene.t_now;
                let a = alt(i);
                let (Some(p), Some(g)) = (pr.geo(f.lat, f.lon, a), pr.geo(f.lat, f.lon, 0.0))
                else {
                    prev = None;
                    continue;
                };
                let c = if future {
                    col.gamma_multiply(0.18)
                } else {
                    col
                };
                if let Some((pi, pp, pg)) = prev {
                    let dt = (f.t - tr.fixes[pi].t) as f64;
                    if self.show_shadows && !future {
                        painter.line_segment(
                            [pg, g],
                            Stroke::new(1.0, Color32::from_black_alpha(110)),
                        );
                    }
                    if dt > gap {
                        painter.extend(Shape::dashed_line(&[pp, p], Stroke::new(1.6, c), 2.0, 4.0));
                    } else {
                        painter.line_segment([pp, p], Stroke::new(2.2, c));
                    }
                }
                if self.show_drop_lines && !future && tr.fixes[i].best_alt().is_some() {
                    painter.line_segment([p, g], Stroke::new(0.7, col.gamma_multiply(0.45)));
                }
                if !future {
                    let sel = scene.selected == Some((ti, i));
                    painter.circle(
                        p,
                        if sel { 5.0 } else { 2.3 },
                        c,
                        Stroke::new(if sel { 2.0 } else { 0.5 }, Color32::from_black_alpha(160)),
                    );
                    hits.push((p, ti, i));
                }
                prev = Some((i, p, g));
            }
            // current position
            if let Some((la, lo, idx)) = tr.position_at(scene.t_now) {
                let a0 = alt(idx);
                let a1 = if idx + 1 < tr.fixes.len() {
                    alt(idx + 1)
                } else {
                    a0
                };
                let span = tr
                    .fixes
                    .get(idx + 1)
                    .map(|b| (b.t - tr.fixes[idx].t) as f64)
                    .unwrap_or(1.0)
                    .max(1.0);
                let w = ((scene.t_now - tr.fixes[idx].t as f64) / span).clamp(0.0, 1.0) as f32;
                if let Some(p) = pr.geo(la, lo, a0 + (a1 - a0) * w) {
                    painter.circle(p, 6.5, col, Stroke::new(2.0, Color32::from_gray(20)));
                    labels.push((p, col, tr.id.clone()));
                }
            }
        }
        for (p, c, s) in labels {
            let fid = FontId::proportional(15.0);
            painter.text(
                p + vec2(1.0, -9.0),
                Align2::CENTER_BOTTOM,
                &s,
                fid.clone(),
                Color32::from_black_alpha(200),
            );
            painter.text(p + vec2(0.0, -10.0), Align2::CENTER_BOTTOM, &s, fid, c);
        }

        // ---------------------------------------------------------------- hover / click
        if let Some(h) = resp.hover_pos()
            && let Some(&(p, ti, fi)) = hits
                .iter()
                .filter(|x| x.0.distance(h) < 9.0)
                .min_by(|a, b| a.0.distance(h).total_cmp(&b.0.distance(h)))
        {
            out.hover = Some((ti, fi, p));
            if resp.clicked() {
                out.clicked = Some((ti, fi));
            }
        }

        // ---------------------------------------------------------------- HUD
        self.hud(ui, &painter, rect, &scene);
        out
    }

    fn hud(&mut self, ui: &mut Ui, painter: &egui::Painter, rect: Rect, scene: &Scene) {
        let amber = Color32::from_rgb(245, 190, 90);
        // big date, like a story caption
        let t = scene.t_now as i64;
        let date = chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0)
            .map(|d| {
                (
                    d.format("%b %d").to_string().to_uppercase(),
                    d.format("%H:%M UTC").to_string(),
                )
            })
            .unwrap_or_default();
        let r1 = painter.text(
            rect.left_top() + vec2(18.0, 14.0),
            Align2::LEFT_TOP,
            &date.0,
            FontId::monospace(30.0),
            amber,
        );
        painter.text(
            r1.right_bottom() + vec2(10.0, -4.0),
            Align2::LEFT_BOTTOM,
            &date.1,
            FontId::monospace(16.0),
            Color32::from_gray(210),
        );
        if let Some(slice) = scene.slice {
            let hs: Vec<String> = slice.mean_z.iter().map(|h| format!("{h:.0}")).collect();
            painter.text(
                r1.left_bottom() + vec2(0.0, 6.0),
                Align2::LEFT_TOP,
                format!(
                    "Winds shown at {} m. Dotted line: not detected.",
                    hs.join(", ")
                ),
                FontId::proportional(14.0),
                Color32::from_gray(200),
            );
        } else {
            painter.text(
                r1.left_bottom() + vec2(0.0, 6.0),
                Align2::LEFT_TOP,
                "No pressure-level wind for this time — download or import it in the side panel.",
                FontId::proportional(14.0),
                Color32::from_gray(170),
            );
        }

        // km from where each was tagged
        let mut x = rect.left() + 18.0;
        let y = rect.bottom() - 46.0;
        painter.text(
            pos2(x, y - 6.0),
            Align2::LEFT_BOTTOM,
            "km from where each was tagged",
            FontId::proportional(12.0),
            Color32::from_gray(200),
        );
        for tr in scene.tracks.iter().filter(|t| t.visible).take(8) {
            let Some(first) = tr.fixes.first() else {
                continue;
            };
            let Some((la, lo, _)) = tr.position_at(scene.t_now).or_else(|| {
                if scene.t_now > tr.fixes.last()?.t as f64 {
                    let l = tr.fixes.last()?;
                    Some((l.lat, l.lon, 0))
                } else {
                    None
                }
            }) else {
                continue;
            };
            let km = geo::haversine_m(first.lat, first.lon, la, lo) / 1000.0;
            let col = Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]);
            let r = painter.text(
                pos2(x, y),
                Align2::LEFT_TOP,
                &tr.id,
                FontId::proportional(12.0),
                col,
            );
            let r2 = painter.text(
                pos2(x, y + 15.0),
                Align2::LEFT_TOP,
                format!("{km:.0}"),
                FontId::proportional(20.0),
                Color32::WHITE,
            );
            x += r.width().max(r2.width()) + 16.0;
        }

        // legend
        if self.show_raster
            && let Some(slice) = scene.slice
            && let Some(h) = slice
                .mean_z
                .get(self.raster_level.min(slice.mean_z.len().saturating_sub(1)))
        {
            let w = 150.0;
            let o = pos2(rect.right() - w - 40.0, rect.bottom() - 62.0);
            painter.text(
                o - vec2(8.0, -5.0),
                Align2::RIGHT_CENTER,
                format!("wind at {h:.0} m"),
                FontId::proportional(12.0),
                Color32::from_gray(210),
            );
            for i in 0..30 {
                let s = i as f32 / 29.0 * 28.0;
                painter.rect_filled(
                    Rect::from_min_size(
                        o + vec2(i as f32 * w / 30.0, 0.0),
                        vec2(w / 30.0 + 0.5, 10.0),
                    ),
                    0.0,
                    wind_color(s, 255),
                );
            }
            painter.text(
                o + vec2(w + 4.0, 5.0),
                Align2::LEFT_CENTER,
                "m/s",
                FontId::proportional(11.0),
                Color32::from_gray(200),
            );
            painter.text(
                o + vec2(0.0, 13.0),
                Align2::LEFT_TOP,
                "0",
                FontId::proportional(10.0),
                Color32::from_gray(200),
            );
            painter.text(
                o + vec2(w, 13.0),
                Align2::RIGHT_TOP,
                "28",
                FontId::proportional(10.0),
                Color32::from_gray(200),
            );
        }
        painter.text(
            rect.right_bottom() - vec2(8.0, 6.0),
            Align2::RIGHT_BOTTOM,
            format!(
                "3D winds: {} · {}",
                scene.source_label, scene.basemap_attribution
            ),
            FontId::proportional(10.0),
            Color32::from_gray(170),
        );

        // camera buttons
        let mut y = rect.top() + 12.0;
        for (label, act) in [("Fit", 0), ("Top", 1), ("Tilt", 2), ("Follow", 3)] {
            let r = Rect::from_min_size(pos2(rect.right() - 74.0, y), vec2(62.0, 22.0));
            let selected = act == 3 && self.follow;
            if ui
                .put(r, egui::Button::new(label).selected(selected))
                .clicked()
            {
                match act {
                    0 => self.needs_fit = true,
                    1 => self.cam.pitch = 89.9,
                    2 => self.cam.pitch = 35.0,
                    _ => self.follow = !self.follow,
                }
            }
            y += 26.0;
        }
        painter.text(
            pos2(rect.right() - 12.0, y + 4.0),
            Align2::RIGHT_TOP,
            "drag: orbit\nshift/right-drag: pan\nscroll: zoom",
            FontId::proportional(10.0),
            Color32::from_gray(140),
        );
    }
}

/// Median fix interval of a track (s), at least 1 h.
fn gap_threshold(tr: &Track) -> f64 {
    let mut d: Vec<i64> = tr
        .fixes
        .windows(2)
        .map(|w| w[1].t - w[0].t)
        .filter(|d| *d > 0)
        .collect();
    if d.is_empty() {
        return 3600.0;
    }
    d.sort_unstable();
    (d[d.len() / 2] as f64).max(3600.0)
}

fn draw_tiles(
    painter: &egui::Painter,
    pr: &Proj,
    tiles: &mut dyn Tiles,
    bbox: (f64, f64, f64, f64),
    tint: Color32,
) {
    let (s, n, w, e) = bbox;
    let span = (e - w).max(1e-3);
    let mut z = ((360.0 / (span / 6.0)).log2().floor() as i32).clamp(2, 12) as u8;
    // keep the tile count reasonable
    loop {
        let nx = lon_to_tile(e, z).floor() - lon_to_tile(w, z).floor() + 1.0;
        let ny = lat_to_tile(s, z).floor() - lat_to_tile(n, z).floor() + 1.0;
        if nx * ny <= 144.0 || z <= 2 {
            break;
        }
        z -= 1;
    }
    let max = (1u32 << z) - 1;
    let (x0, x1) = (
        lon_to_tile(w, z).floor() as u32,
        (lon_to_tile(e, z).floor() as u32).min(max),
    );
    let (y0, y1) = (
        lat_to_tile(n, z).floor() as u32,
        (lat_to_tile(s, z).floor() as u32).min(max),
    );
    const SUB: usize = 6;
    for ty in y0..=y1 {
        for tx in x0..=x1 {
            let Some(piece) = tiles.at(TileId {
                x: tx,
                y: ty,
                zoom: z,
            }) else {
                continue;
            };
            #[allow(irrefutable_let_patterns)]
            let Tile::Raster(tex) = &piece.tile else {
                continue;
            };
            let mut mesh = Mesh::with_texture(tex.id());
            let mut ok = [true; (SUB + 1) * (SUB + 1)];
            for a in 0..=SUB {
                for b in 0..=SUB {
                    let (fx, fy) = (b as f64 / SUB as f64, a as f64 / SUB as f64);
                    let la = tile_lat(ty as f64 + fy, z);
                    let lo = tile_lon(tx as f64 + fx, z);
                    let p = pr.geo(la, lo, 0.0);
                    ok[a * (SUB + 1) + b] = p.is_some();
                    let uv = piece.uv.min
                        + vec2(piece.uv.width() * fx as f32, piece.uv.height() * fy as f32);
                    mesh.vertices.push(egui::epaint::Vertex {
                        pos: p.unwrap_or(Pos2::ZERO),
                        uv,
                        color: tint,
                    });
                }
            }
            for a in 0..SUB {
                for b in 0..SUB {
                    let i = (a * (SUB + 1) + b) as u32;
                    let (i1, i2, i3) = (i + 1, i + (SUB + 1) as u32, i + (SUB + 2) as u32);
                    if ok[i as usize] && ok[i1 as usize] && ok[i2 as usize] && ok[i3 as usize] {
                        mesh.add_triangle(i, i1, i3);
                        mesh.add_triangle(i, i3, i2);
                    }
                }
            }
            painter.add(Shape::mesh(mesh));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_basics() {
        let cam = Camera {
            yaw: 0.0,
            pitch: 89.9,
            dist: 100.0,
            target: [0.0; 3],
            fov_deg: 45.0,
        };
        let rect = Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0));
        let pr = Proj::new(&cam, (47.0, 9.0), 100.0, rect);
        let c = pr.geo(47.0, 9.0, 0.0).unwrap();
        assert!((c.x - 400.0).abs() < 1.0 && (c.y - 300.0).abs() < 1.0);
        // north is up, east is right when looking straight down with yaw 0
        let n = pr.geo(47.1, 9.0, 0.0).unwrap();
        let e = pr.geo(47.0, 9.1, 0.0).unwrap();
        assert!(n.y < c.y && e.x > c.x);
        // higher points come closer to the camera → further from centre
        let up = pr.geo(47.1, 9.0, 500.0).unwrap();
        assert!(up.y < n.y);
        let (la, lo) = pr.latlon(10.0, -5.0);
        let back = pr.enu(la, lo, 0.0);
        assert!((back[0] - 10.0).abs() < 1e-3 && (back[1] + 5.0).abs() < 1e-3);
    }

    #[test]
    fn tiles_math() {
        assert!((tile_lat(lat_to_tile(47.5, 6), 6) - 47.5).abs() < 1e-9);
        assert!((tile_lon(lon_to_tile(9.25, 6), 6) - 9.25).abs() < 1e-9);
        assert_eq!(wind_color(0.0, 255), Color32::from_rgb(30, 60, 90));
    }
}
