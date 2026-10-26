//! The egui application: map + satellite layers, timeline, charts, weather annotation, export.

use crate::firmware;
use crate::geo;
use crate::model::{Fix, LocationSource, SensorKind, Track};
use crate::table::{Table, fmt_date, fmt_time};
use crate::tiles::{Basemap, Gibs, SatLayer};
use crate::video;
use crate::weather::{self, AnnotateOptions, Model, Request, WeatherStore, WindLevel};
use egui::{Align2, Color32, ColorImage, FontId, Pos2, Rect, Stroke, Ui, pos2, vec2};
use egui_plot::{GridMark, HoverPosition, Legend, Line, Plot, PlotPoints, Points, VLine};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use walkers::{HttpTiles, MapMemory, Plugin, Position, Projector, Tiles, lon_lat};

// ---------------------------------------------------------------------------
// Messages from background work (file pickers, HTTP)
// ---------------------------------------------------------------------------

enum Msg {
    File {
        name: String,
        bytes: Vec<u8>,
    },
    Weather {
        id: usize,
        result: Result<String, String>,
    },
    Status(String),
}

struct LoadedFile {
    name: String,
    bytes: Vec<u8>,
    firmware: String,
    ranking: Vec<(&'static str, f32)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ColorBy {
    Individual,
    WindSupport,
    SegmentWind,
    Altitude,
    GroundSpeed,
    Time,
}

impl ColorBy {
    const ALL: [ColorBy; 6] = [
        ColorBy::Individual,
        ColorBy::WindSupport,
        ColorBy::SegmentWind,
        ColorBy::Altitude,
        ColorBy::GroundSpeed,
        ColorBy::Time,
    ];
    fn label(self) -> &'static str {
        match self {
            ColorBy::Individual => "Individual",
            ColorBy::WindSupport => "Wind support at fix",
            ColorBy::SegmentWind => "Along-step wind support",
            ColorBy::Altitude => "Altitude",
            ColorBy::GroundSpeed => "Ground speed",
            ColorBy::Time => "Time",
        }
    }
}

/// Colour scale for the ERA5 wind-field arrows (wind speed 0–20 m/s).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WindPalette {
    Viridis,
    Warm,
    Cool,
    Turbo,
    White,
    Black,
}

impl WindPalette {
    const ALL: [WindPalette; 6] = [
        WindPalette::Viridis,
        WindPalette::Warm,
        WindPalette::Cool,
        WindPalette::Turbo,
        WindPalette::White,
        WindPalette::Black,
    ];
    fn label(self) -> &'static str {
        match self {
            WindPalette::Viridis => "Viridis (light maps)",
            WindPalette::Warm => "Warm: white → yellow → magenta",
            WindPalette::Cool => "Cool: cyan → magenta",
            WindPalette::Turbo => "Turbo (rainbow)",
            WindPalette::White => "White",
            WindPalette::Black => "Black",
        }
    }
    /// Satellite imagery is dark and busy, so it gets bright colours; street/topo maps are light.
    fn default_for(b: Basemap) -> Self {
        match b {
            Basemap::EsriImagery => WindPalette::Warm,
            Basemap::CartoDark => WindPalette::Cool,
            Basemap::OpenStreetMap | Basemap::EsriTopo => WindPalette::Viridis,
        }
    }
    /// `x` = wind speed / 20 m/s.
    fn color(self, x: f32) -> Color32 {
        match self {
            WindPalette::Viridis => viridis(x),
            WindPalette::Warm => lerp_stops(
                &[
                    [255.0, 255.0, 255.0],
                    [255.0, 240.0, 80.0],
                    [255.0, 170.0, 0.0],
                    [255.0, 80.0, 40.0],
                    [255.0, 0.0, 150.0],
                ],
                x,
            ),
            WindPalette::Cool => lerp_stops(&[[0.0, 255.0, 255.0], [255.0, 0.0, 255.0]], x),
            WindPalette::Turbo => lerp_stops(
                &[
                    [48.0, 18.0, 59.0],
                    [70.0, 130.0, 248.0],
                    [27.0, 229.0, 181.0],
                    [164.0, 252.0, 59.0],
                    [251.0, 185.0, 56.0],
                    [226.0, 66.0, 9.0],
                    [122.0, 4.0, 3.0],
                ],
                x,
            ),
            WindPalette::White => Color32::WHITE,
            WindPalette::Black => Color32::BLACK,
        }
    }
    /// Contrasting outline drawn under each arrow.
    fn halo(self) -> Option<Color32> {
        match self {
            WindPalette::Viridis => None,
            WindPalette::Black => Some(Color32::from_white_alpha(170)),
            _ => Some(Color32::from_black_alpha(170)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChartVar {
    Altitude,
    GroundSpeed,
    WindSupport,
    SegmentWind,
    Crosswind,
    Airspeed,
    WindSpeed,
    Era5Temp,
    Precip,
    LocError,
    Sensor(SensorKind),
}

impl ChartVar {
    const ALL: [ChartVar; 14] = [
        ChartVar::Altitude,
        ChartVar::GroundSpeed,
        ChartVar::WindSupport,
        ChartVar::SegmentWind,
        ChartVar::Crosswind,
        ChartVar::Airspeed,
        ChartVar::WindSpeed,
        ChartVar::Era5Temp,
        ChartVar::Precip,
        ChartVar::LocError,
        ChartVar::Sensor(SensorKind::Vedba),
        ChartVar::Sensor(SensorKind::Temperature),
        ChartVar::Sensor(SensorKind::Pressure),
        ChartVar::Sensor(SensorKind::Activity),
    ];
    fn label(self) -> &'static str {
        match self {
            ChartVar::Altitude => "Altitude (m, ERA5-corrected if barometric)",
            ChartVar::GroundSpeed => "Ground speed (m/s)",
            ChartVar::WindSupport => "Wind support at fix (m/s)",
            ChartVar::SegmentWind => "Along-step wind support (m/s)",
            ChartVar::Crosswind => "Crosswind (m/s)",
            ChartVar::Airspeed => "Airspeed (m/s)",
            ChartVar::WindSpeed => "ERA5 wind speed (m/s)",
            ChartVar::Era5Temp => "ERA5 2 m temperature (°C)",
            ChartVar::Precip => "ERA5 precipitation (mm/h)",
            ChartVar::LocError => "Location error (km)",
            ChartVar::Sensor(k) => k.label(),
        }
    }
    fn of_fix(self, f: &Fix) -> Option<f32> {
        match self {
            ChartVar::Altitude => f.best_alt(),
            ChartVar::GroundSpeed => f.step_significant.then_some(f.ground_speed_ms).flatten(),
            ChartVar::WindSupport => f.weather.wind_support,
            ChartVar::SegmentWind => f.weather.seg_wind_support,
            ChartVar::Crosswind => f.weather.crosswind,
            ChartVar::Airspeed => f.weather.airspeed,
            ChartVar::WindSpeed => f.weather.wind_speed,
            ChartVar::Era5Temp => f.weather.temp_2m,
            ChartVar::Precip => f.weather.precipitation_mm,
            ChartVar::LocError => f.error_m.map(|e| e / 1000.0),
            ChartVar::Sensor(_) => None,
        }
    }
}

const PALETTE: [[u8; 3]; 10] = [
    [230, 159, 0],
    [86, 180, 233],
    [0, 158, 115],
    [240, 228, 66],
    [213, 94, 0],
    [204, 121, 167],
    [0, 114, 178],
    [255, 255, 255],
    [170, 120, 255],
    [120, 220, 120],
];

const SPEEDS: [(f64, &str); 6] = [
    (600.0, "10 min/s"),
    (3600.0, "1 h/s"),
    (6.0 * 3600.0, "6 h/s"),
    (86400.0, "1 day/s"),
    (3.0 * 86400.0, "3 days/s"),
    (7.0 * 86400.0, "1 week/s"),
];

pub struct WingApp {
    tracks: Vec<Track>,
    files: Vec<LoadedFile>,
    log: Vec<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    // time
    t_min: f64,
    t_max: f64,
    t_now: f64,
    playing: bool,
    speed_idx: usize,
    trail_to_cursor: bool,

    // map
    memory: MapMemory,
    basemap: Basemap,
    base_tiles: HashMap<Basemap, HttpTiles>,
    sat_on: bool,
    sat_layer: SatLayer,
    sat_opacity: f32,
    sat_tiles: HashMap<(SatLayer, String), HttpTiles>,
    fit_pending: bool,
    follow: bool,

    // display
    color_by: ColorBy,
    show_error: bool,
    show_wind_arrows: bool,
    show_wind_field: bool,
    wind_field_night_only: bool,
    /// User's wind palette per basemap; missing → [`WindPalette::default_for`].
    wind_palettes: HashMap<Basemap, WindPalette>,
    skip_daytime: bool,
    max_error_km: f32,
    hide_insignificant: bool,
    selected: Option<(usize, usize)>,
    selected_track: Option<usize>,
    chart_var: ChartVar,

    // weather
    store: WeatherStore,
    opts: AnnotateOptions,
    queue: VecDeque<Request>,
    in_flight: HashMap<usize, Request>,
    next_req_id: usize,
    req_total: usize,
    req_done: usize,
    req_failed: usize,
    since_annotate: usize,

    // video export
    video: video::Settings,
    video_dialog: bool,
    ffmpeg: Option<bool>,
    rec: Option<Recording>,
    map_rect: Rect,
    timeline_rect: Rect,
}

/// An in-progress video export (see [`crate::video`]).
struct Recording {
    sink: video::Sink,
    shot_tx: Sender<Arc<ColorImage>>,
    shot_rx: Receiver<Arc<ColorImage>>,
    /// Capture area in points, fixed when recording starts.
    rect: Rect,
    downscale: usize,
    t: f64,
    t_end: f64,
    dt: f64,
    waiting: bool,
    /// Frames to render before the first capture so map tiles can load.
    warmup: u32,
    frames: usize,
    total: usize,
    name: String,
}

impl WingApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (tx, rx) = channel();
        let mut app = WingApp {
            tracks: Vec::new(),
            files: Vec::new(),
            log: vec!["Drop CSV files here or use Open… (try Load demo to explore).".into()],
            tx,
            rx,
            t_min: 0.0,
            t_max: 1.0,
            t_now: 0.0,
            playing: false,
            speed_idx: 3,
            trail_to_cursor: true,
            memory: MapMemory::default(),
            basemap: Basemap::EsriImagery,
            base_tiles: HashMap::new(),
            sat_on: false,
            sat_layer: SatLayer::ModisTerra,
            sat_opacity: 0.75,
            sat_tiles: HashMap::new(),
            fit_pending: false,
            follow: false,
            color_by: ColorBy::Individual,
            show_error: true,
            show_wind_arrows: false,
            show_wind_field: true,
            wind_field_night_only: false,
            wind_palettes: HashMap::new(),
            skip_daytime: false,
            max_error_km: 25.0,
            hide_insignificant: false,
            selected: None,
            selected_track: None,
            chart_var: ChartVar::Altitude,
            store: WeatherStore::default(),
            opts: AnnotateOptions::default(),
            queue: VecDeque::new(),
            in_flight: HashMap::new(),
            next_req_id: 0,
            req_total: 0,
            req_done: 0,
            req_failed: 0,
            since_annotate: 0,
            video: video::Settings::default(),
            video_dialog: false,
            ffmpeg: None,
            rec: None,
            map_rect: Rect::NOTHING,
            timeline_rect: Rect::NOTHING,
        };
        let _ = app.memory.set_zoom(5.0);
        app.memory.center_at(lon_lat(9.17, 47.67));
        if std::env::var("WINGTRACK_DEMO").is_ok() {
            app.load_demo();
        }
        if std::env::var("WINGTRACK_SYNTHETIC_WIND").is_ok() {
            weather::synthetic_field(&mut app.store, &app.tracks);
            let n = weather::annotate(&mut app.tracks, &app.store, &app.opts);
            app.color_by = ColorBy::SegmentWind;
            app.status(format!(
                "SYNTHETIC test wind field loaded (not ERA5) - {n} fixes annotated"
            ));
        }
        app
    }

    fn status(&mut self, s: impl Into<String>) {
        let s = s.into();
        log::info!("{s}");
        self.log.push(s);
        if self.log.len() > 200 {
            self.log.drain(0..100);
        }
    }

    // ------------------------------------------------------------------ loading

    fn load_demo(&mut self) {
        for (name, csv) in crate::demo::all() {
            self.add_file(name.to_string(), csv.into_bytes(), None);
        }
    }

    fn add_file(&mut self, name: String, bytes: Vec<u8>, force: Option<&str>) {
        let table = match Table::from_bytes(&name, &bytes) {
            Ok(t) => t,
            Err(e) => return self.status(format!("! {e}")),
        };
        let ranking = firmware::rank(&table);
        match firmware::parse(&table, force) {
            Ok((out, fw)) => {
                self.tracks.retain(|t| t.file != name);
                self.files.retain(|f| f.name != name);
                let n_fix: usize = out.tracks.iter().map(|t| t.fixes.len()).sum();
                self.status(format!(
                    "Loaded {name}: {} track(s), {n_fix} fixes - {}",
                    out.tracks.len(),
                    firmware::label_for(fw)
                ));
                for w in out.warnings {
                    self.status(format!("  {w}"));
                }
                for mut tr in out.tracks {
                    tr.file = name.clone();
                    tr.color = PALETTE[self.tracks.len() % PALETTE.len()];
                    self.tracks.push(tr);
                }
                self.files.push(LoadedFile {
                    name,
                    bytes,
                    firmware: fw.to_string(),
                    ranking,
                });
                self.recompute_time_range();
                if !self.store.is_empty() {
                    weather::annotate(&mut self.tracks, &self.store, &self.opts);
                }
                self.selected = None;
                self.fit_pending = true;
            }
            Err(e) => self.status(format!("! {e}")),
        }
    }

    fn recompute_time_range(&mut self) {
        let (mut lo, mut hi) = (i64::MAX, i64::MIN);
        for tr in &self.tracks {
            if let Some((a, b)) = tr.time_range() {
                lo = lo.min(a);
                hi = hi.max(b);
            }
        }
        if lo <= hi {
            self.t_min = lo as f64;
            self.t_max = hi.max(lo + 1) as f64;
            self.t_now = self.t_max;
        }
    }

    fn open_dialog(&self, ctx: &egui::Context) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(move || {
            if let Some(paths) = rfd::FileDialog::new()
                .add_filter("Tracking data", &["csv", "txt", "tsv"])
                .pick_files()
            {
                for p in paths {
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    match std::fs::read(&p) {
                        Ok(bytes) => {
                            let _ = tx.send(Msg::File { name, bytes });
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Status(format!("! {}: {e}", p.display())));
                        }
                    }
                }
                ctx.request_repaint();
            }
        });
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(files) = rfd::AsyncFileDialog::new()
                .add_filter("Tracking data", &["csv", "txt", "tsv"])
                .pick_files()
                .await
            {
                for f in files {
                    let bytes = f.read().await;
                    let _ = tx.send(Msg::File {
                        name: f.file_name(),
                        bytes,
                    });
                }
                ctx.request_repaint();
            }
        });
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        for h in dropped {
            let name = h
                .path()
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "dropped.csv".into());
            #[cfg(not(target_arch = "wasm32"))]
            match h.bytes() {
                Ok(bytes) => self.add_file(name, bytes, None),
                Err(e) => self.status(format!("! {name}: {e}")),
            }
            #[cfg(target_arch = "wasm32")]
            {
                let tx = self.tx.clone();
                let ctx = ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match h.bytes_async().await {
                        Ok(bytes) => {
                            let _ = tx.send(Msg::File { name, bytes });
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Status(format!("! {name}: {e}")));
                        }
                    }
                    ctx.request_repaint();
                });
            }
        }
    }

    fn export(&mut self) {
        let csv = crate::export::to_csv(self.tracks.iter().filter(|t| t.visible));
        let name = "wingtrack_annotated.csv";
        #[cfg(not(target_arch = "wasm32"))]
        {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                if let Some(p) = rfd::FileDialog::new()
                    .set_file_name(name)
                    .add_filter("CSV", &["csv"])
                    .save_file()
                {
                    let msg = match std::fs::write(&p, csv) {
                        Ok(_) => format!("Exported {}", p.display()),
                        Err(e) => format!("! export failed: {e}"),
                    };
                    let _ = tx.send(Msg::Status(msg));
                }
            });
        }
        #[cfg(target_arch = "wasm32")]
        {
            let msg = match web_download(name, csv.as_bytes(), "text/csv") {
                Ok(()) => format!("Downloaded {name}"),
                Err(e) => format!("! download failed: {e}"),
            };
            self.status(msg);
        }
    }

    // ------------------------------------------------------------------ video

    fn capture_rect(&self) -> Rect {
        if self.video.include_timeline && self.timeline_rect.is_positive() {
            self.map_rect.union(self.timeline_rect)
        } else {
            self.map_rect
        }
    }

    /// (start, end, tracking seconds per frame, frame count)
    fn video_plan(&self) -> (f64, f64, f64, usize) {
        let dt = SPEEDS[self.video.speed_idx].0 / self.video.effective_fps() as f64;
        let t0 = if self.video.from_cursor {
            self.t_now
        } else {
            self.t_min
        };
        let total = ((self.t_max - t0).max(0.0) / dt).floor() as usize + 1;
        (t0, self.t_max, dt, total)
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        let rect = self.capture_rect();
        if !rect.is_positive() {
            self.status("! video: nothing to capture");
            return;
        }
        let (t0, t_end, dt, total) = self.video_plan();
        let (fmt, fps) = (self.video.format, self.video.effective_fps());
        #[cfg(not(target_arch = "wasm32"))]
        let (sink, name) = {
            let Some(path) = rfd::FileDialog::new()
                .set_file_name(format!("wingtrack.{}", fmt.ext()))
                .add_filter(fmt.label(), &[fmt.ext()])
                .save_file()
            else {
                return;
            };
            let name = path.display().to_string();
            (video::Sink::new(fmt, fps, path), name)
        };
        #[cfg(target_arch = "wasm32")]
        let (sink, name) = (
            video::Sink::new(fmt, fps),
            format!("wingtrack.{}", fmt.ext()),
        );
        let (shot_tx, shot_rx) = channel();
        self.playing = false;
        self.video_dialog = false;
        self.t_now = t0;
        self.status(format!("Recording {total} frames to {name}…"));
        self.rec = Some(Recording {
            sink,
            shot_tx,
            shot_rx,
            rect,
            downscale: self.video.downscale,
            t: t0,
            t_end,
            dt,
            waiting: false,
            warmup: 60,
            frames: 0,
            total,
            name,
        });
        ctx.request_repaint();
    }

    /// One step of the capture loop: collect a finished screenshot, advance the clock, and
    /// request the next one. Runs before the UI so the requested frame shows the new time.
    fn record_tick(&mut self, ctx: &egui::Context) {
        let Some(mut rec) = self.rec.take() else {
            return;
        };
        self.playing = false;
        let ppp = ctx.pixels_per_point();
        let px = [
            (rec.rect.min.x * ppp).round() as usize,
            (rec.rect.min.y * ppp).round() as usize,
            (rec.rect.width() * ppp).round() as usize,
            (rec.rect.height() * ppp).round() as usize,
        ];
        let mut done: Option<Result<(), String>> = None;
        while let Ok(img) = rec.shot_rx.try_recv() {
            match video::prepare(&img, px, rec.downscale).and_then(|f| rec.sink.push(f)) {
                Ok(()) => {
                    rec.frames += 1;
                    rec.waiting = false;
                    rec.t += rec.dt;
                }
                Err(e) => {
                    done = Some(Err(e));
                    break;
                }
            }
        }
        if done.is_none() && !rec.waiting {
            if rec.warmup > 0 {
                rec.warmup -= 1;
                self.t_now = rec.t;
                ctx.request_repaint();
            } else {
                self.t_now = rec.t;
                if self.skip_daytime {
                    self.skip_to_night();
                    rec.t = self.t_now;
                }
                if rec.t > rec.t_end {
                    done = Some(Ok(()));
                } else {
                    let tx = rec.shot_tx.clone();
                    let c = ctx.clone();
                    ctx.request_screenshot(move |img| {
                        let _ = tx.send(img);
                        c.request_repaint();
                    });
                    rec.waiting = true;
                }
            }
        }
        match done {
            None => self.rec = Some(rec),
            Some(r) => self.finish_recording(rec, r, ctx),
        }
    }

    fn finish_recording(&mut self, rec: Recording, r: Result<(), String>, ctx: &egui::Context) {
        let Recording {
            sink, name, frames, ..
        } = rec;
        if let Err(e) = r {
            self.status(format!("! video export failed: {e}"));
            #[cfg(not(target_arch = "wasm32"))]
            std::thread::spawn(move || drop(sink.finish()));
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.status(format!("Finishing video ({frames} frames)…"));
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let msg = match sink.finish() {
                    Ok(()) => format!("Saved video {name} ({frames} frames)"),
                    Err(e) => format!("! video export failed: {e}"),
                };
                let _ = tx.send(Msg::Status(msg));
                ctx.request_repaint();
            });
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            let msg = match sink
                .finish()
                .and_then(|b| web_download(&name, &b, "image/gif"))
            {
                Ok(()) => format!("Downloaded {name} ({frames} frames)"),
                Err(e) => format!("! video export failed: {e}"),
            };
            self.status(msg);
        }
    }

    fn open_video_dialog(&mut self) {
        self.video_dialog = true;
        self.video.speed_idx = self.speed_idx;
        if self.ffmpeg.is_none() {
            let ok = video::ffmpeg_available();
            self.ffmpeg = Some(ok);
            if ok {
                self.video.format = video::Format::Mp4;
            }
        }
    }

    fn video_window(&mut self, ctx: &egui::Context) {
        if !self.video_dialog {
            return;
        }
        let mut open = true;
        let mut start = false;
        egui::Window::new("Export video")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                let ffmpeg = self.ffmpeg.unwrap_or(false);
                egui::ComboBox::from_label("format")
                    .selected_text(self.video.format.label())
                    .show_ui(ui, |ui| {
                        ui.add_enabled_ui(ffmpeg, |ui| {
                            ui.selectable_value(&mut self.video.format, video::Format::Mp4, video::Format::Mp4.label())
                        })
                        .response
                        .on_disabled_hover_text(if cfg!(target_arch = "wasm32") {
                            "MP4 needs the desktop app"
                        } else {
                            "ffmpeg was not found on PATH"
                        });
                        ui.selectable_value(&mut self.video.format, video::Format::Gif, video::Format::Gif.label());
                    });
                ui.add(egui::Slider::new(&mut self.video.fps, 5..=60).text("frames per second"));
                if self.video.effective_fps() != self.video.fps {
                    ui.weak(format!("GIF is limited to {} fps", self.video.effective_fps()));
                }
                egui::ComboBox::from_label("tracking time per video second")
                    .selected_text(SPEEDS[self.video.speed_idx].1)
                    .show_ui(ui, |ui| {
                        for (i, s) in SPEEDS.iter().enumerate() {
                            ui.selectable_value(&mut self.video.speed_idx, i, s.1);
                        }
                    });
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.video.from_cursor, false, "whole timeline");
                    ui.radio_value(&mut self.video.from_cursor, true, "from cursor to end");
                });
                let res = |k: usize| match k {
                    1 => "full",
                    2 => "1/2",
                    _ => "1/3",
                };
                egui::ComboBox::from_label("resolution")
                    .selected_text(res(self.video.downscale))
                    .show_ui(ui, |ui| {
                        for k in 1..=3 {
                            ui.selectable_value(&mut self.video.downscale, k, res(k));
                        }
                    });
                ui.checkbox(&mut self.video.include_timeline, "include timeline & sensor chart");
                ui.separator();
                let r = self.capture_rect();
                let ppp = ctx.pixels_per_point();
                let k = self.video.downscale as f32;
                let (_, _, _, total) = self.video_plan();
                ui.label(format!(
                    "{total} frames · {:.1} s of video · {}×{} px",
                    total as f32 / self.video.effective_fps() as f32,
                    ((r.width() * ppp / k) as u32) & !1,
                    ((r.height() * ppp / k) as u32) & !1,
                ));
                if self.skip_daytime {
                    ui.weak("“skip daytime” is on: day hours are left out, so the video will be shorter.");
                }
                if total > 5000 {
                    ui.colored_label(Color32::from_rgb(255, 180, 80), "That's a lot of frames — pick a faster speed?");
                }
                ui.weak(
                    "Records the map (basemap, satellite layer, wind field, legend) as currently shown. \
                     Set zoom and layers first and keep the window size fixed while recording.",
                );
                if ui.add_enabled(total > 0, egui::Button::new("Start recording")).clicked() {
                    start = true;
                }
            });
        self.video_dialog = open && !start;
        if start {
            self.start_recording(ctx);
        }
    }

    // ------------------------------------------------------------------ weather

    fn start_weather(&mut self) {
        let mut pts = weather::sample_points(&self.tracks, &self.opts);
        pts.extend(weather::field_points(&self.tracks, &self.opts));
        let reqs = self
            .store
            .plan(&pts, self.opts.model, weather::latest_available_day());
        if reqs.is_empty() {
            let n = weather::annotate(&mut self.tracks, &self.store, &self.opts);
            self.status(format!("ERA5: everything cached - annotated {n} fixes"));
            return;
        }
        self.status(format!(
            "ERA5: {} request(s) for {} sample points ({} days lag; Open-Meteo, free for non-commercial use)",
            reqs.len(),
            pts.len(),
            6
        ));
        self.req_total = reqs.len() + self.queue.len() + self.in_flight.len();
        self.req_done = 0;
        self.req_failed = 0;
        self.queue.extend(reqs);
    }

    fn pump_weather(&mut self, ctx: &egui::Context) {
        while self.in_flight.len() < 3 {
            let Some(req) = self.queue.pop_front() else {
                break;
            };
            let id = self.next_req_id;
            self.next_req_id += 1;
            let tx = self.tx.clone();
            let ctx2 = ctx.clone();
            ehttp::fetch(ehttp::Request::get(&req.url), move |res| {
                let result = match res {
                    Ok(r) if r.ok => r
                        .text()
                        .map(|s| s.to_string())
                        .ok_or_else(|| "non-UTF8 body".to_string()),
                    Ok(r) => Err(format!(
                        "HTTP {} {}",
                        r.status,
                        r.text()
                            .map(|t| t.chars().take(200).collect::<String>())
                            .unwrap_or_default()
                    )),
                    Err(e) => Err(e),
                };
                let _ = tx.send(Msg::Weather { id, result });
                ctx2.request_repaint();
            });
            self.in_flight.insert(id, req);
        }
    }

    fn process_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::File { name, bytes } => self.add_file(name, bytes, None),
                Msg::Status(s) => self.status(s),
                Msg::Weather { id, result } => {
                    let Some(req) = self.in_flight.remove(&id) else {
                        continue;
                    };
                    self.req_done += 1;
                    match result.and_then(|body| self.store.ingest(&req, &body)) {
                        Ok(_) => {}
                        Err(e) => {
                            self.req_failed += 1;
                            self.store.mark_done(&req);
                            self.status(format!("! ERA5 request failed: {e}"));
                        }
                    }
                    self.since_annotate += 1;
                    let finished = self.queue.is_empty() && self.in_flight.is_empty();
                    if finished || self.since_annotate >= 4 {
                        let n = weather::annotate(&mut self.tracks, &self.store, &self.opts);
                        self.since_annotate = 0;
                        if finished {
                            self.status(format!(
                                "ERA5 done: {} cells, {} hours cached; {n} fixes annotated{}",
                                self.store.cells.len(),
                                self.store.hours(),
                                if self.req_failed > 0 {
                                    format!(" ({} requests failed)", self.req_failed)
                                } else {
                                    String::new()
                                }
                            ));
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ helpers

    fn fix_visible(&self, f: &Fix) -> bool {
        if f.error_m.is_some_and(|e| e > self.max_error_km * 1000.0) {
            return false;
        }
        if self.hide_insignificant && !f.step_significant {
            return false;
        }
        true
    }

    fn data_center(&self) -> Position {
        let mut n = 0.0;
        let (mut la, mut lo) = (0.0, 0.0);
        for tr in self.tracks.iter().filter(|t| t.visible) {
            for f in &tr.fixes {
                la += f.lat;
                lo += f.lon;
                n += 1.0;
            }
        }
        if n > 0.0 {
            lon_lat(lo / n, la / n)
        } else {
            lon_lat(9.17, 47.67)
        }
    }

    fn fit_to(&mut self, rect: Rect, only: Option<usize>) {
        let (mut a, mut b, mut c, mut d) = (90.0f64, -90.0f64, 180.0f64, -180.0f64);
        for (i, tr) in self.tracks.iter().enumerate() {
            if !tr.visible || only.is_some_and(|o| o != i) {
                continue;
            }
            for f in tr.fixes.iter().filter(|f| self.fix_visible(f)) {
                a = a.min(f.lat);
                b = b.max(f.lat);
                c = c.min(f.lon);
                d = d.max(f.lon);
            }
        }
        if a > b {
            return;
        }
        let merc = |lat: f64| {
            (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
                .tan()
                .ln()
                / std::f64::consts::TAU
        };
        let dx = ((d - c) / 360.0).max(1e-5);
        let dy = (merc(b) - merc(a)).max(1e-5);
        let zx = (rect.width() as f64 / (256.0 * dx)).log2();
        let zy = (rect.height() as f64 / (256.0 * dy)).log2();
        let z = (zx.min(zy) - 0.4).clamp(2.0, 15.0);
        let _ = self.memory.set_zoom(z);
        self.memory.center_at(lon_lat((c + d) / 2.0, (a + b) / 2.0));
    }

    fn wind_palette(&self) -> WindPalette {
        self.wind_palettes
            .get(&self.basemap)
            .copied()
            .unwrap_or(WindPalette::default_for(self.basemap))
    }

    /// Corners and centre of the area of interest: the downloaded ERA5 cells, or the
    /// visible fixes when no weather is loaded.
    fn night_probe_points(&self) -> Vec<(f64, f64)> {
        let pts: Vec<(f64, f64)> = if self.store.is_empty() {
            self.tracks
                .iter()
                .filter(|t| t.visible)
                .flat_map(|t| t.fixes.iter().map(|f| (f.lat, f.lon)))
                .collect()
        } else {
            self.store
                .cells
                .keys()
                .map(|&k| weather::cell_center(k))
                .collect()
        };
        let Some(&(la0, lo0)) = pts.first() else {
            return Vec::new();
        };
        let (mut s, mut n, mut w, mut e) = (la0, la0, lo0, lo0);
        for &(la, lo) in &pts {
            s = s.min(la);
            n = n.max(la);
            w = w.min(lo);
            e = e.max(lo);
        }
        vec![
            (s, w),
            (s, e),
            (n, w),
            (n, e),
            ((s + n) / 2.0, (w + e) / 2.0),
        ]
    }

    /// If it is day everywhere in the area at `t_now`, jump forward to the next night.
    fn skip_to_night(&mut self) {
        let probes = self.night_probe_points();
        if probes.is_empty() {
            return;
        }
        let night = |t: f64| {
            probes
                .iter()
                .any(|&(la, lo)| geo::is_night(t as i64, la, lo))
        };
        if night(self.t_now) {
            return;
        }
        // 10-min steps, at most two days ahead (polar summer has no night).
        let limit = (self.t_now + 2.0 * 86400.0).min(self.t_max);
        let mut t = self.t_now;
        while t < limit && !night(t) {
            t += 600.0;
        }
        if night(t) {
            self.t_now = t;
        }
    }

    fn step_selected(&mut self, dir: i32) {
        let ti = self
            .selected
            .map(|s| s.0)
            .or(self.selected_track)
            .or(if self.tracks.is_empty() {
                None
            } else {
                Some(0)
            });
        let Some(ti) = ti else { return };
        let tr = &self.tracks[ti];
        if tr.fixes.is_empty() {
            return;
        }
        let cur = tr.fixes.partition_point(|f| (f.t as f64) < self.t_now);
        let idx = if dir > 0 {
            (cur + usize::from(cur < tr.fixes.len() && tr.fixes[cur].t as f64 <= self.t_now))
                .min(tr.fixes.len() - 1)
        } else {
            cur.saturating_sub(1)
        };
        self.t_now = tr.fixes[idx].t as f64;
        self.selected = Some((ti, idx));
    }

    // ------------------------------------------------------------------ UI

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.strong("WingTrack");
            ui.separator();
            if ui.button("Open…").on_hover_text("CSV from Wildcloud, Movebank, CTT or R").clicked() {
                self.open_dialog(ui.ctx());
            }
            if ui.button("Load demo").on_hover_text("Simulated NanoFox, TinyFox and CTT GPS files").clicked() {
                self.load_demo();
            }
            if ui.add_enabled(!self.tracks.is_empty(), egui::Button::new("Export annotated CSV")).clicked() {
                self.export();
            }
            if let Some(rec) = &self.rec {
                let frac = rec.frames as f32 / rec.total.max(1) as f32;
                let text = format!("REC {}/{}", rec.frames, rec.total);
                ui.add(egui::ProgressBar::new(frac).desired_width(160.0).text(text));
                if ui.button("Stop & save").clicked()
                    && let Some(rec) = self.rec.take()
                {
                    let ctx = ui.ctx().clone();
                    self.finish_recording(rec, Ok(()), &ctx);
                }
            } else if ui
                .add_enabled(!self.tracks.is_empty(), egui::Button::new("Export video…"))
                .on_hover_text("MP4 or GIF of the animated map, legend and sensor chart")
                .clicked()
            {
                self.open_video_dialog();
            }
            if ui.add_enabled(!self.tracks.is_empty(), egui::Button::new("Clear")).clicked() {
                self.tracks.clear();
                self.files.clear();
                self.selected = None;
                self.selected_track = None;
            }
            ui.separator();
            egui::ComboBox::from_id_salt("basemap").selected_text(self.basemap.label()).show_ui(ui, |ui| {
                for b in Basemap::ALL {
                    ui.selectable_value(&mut self.basemap, b, b.label());
                }
            });
            ui.checkbox(&mut self.sat_on, "Daily satellite").on_hover_text(
                "NASA GIBS imagery for the date under the time cursor - see cloud fronts the animals flew through",
            );
            if self.sat_on {
                egui::ComboBox::from_id_salt("sat").selected_text(self.sat_layer.label()).show_ui(ui, |ui| {
                    for l in SatLayer::ALL {
                        ui.selectable_value(&mut self.sat_layer, l, l.label());
                    }
                });
                ui.add(egui::Slider::new(&mut self.sat_opacity, 0.1..=1.0).text("opacity"));
            }
        });
    }

    fn side_panel(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // ---- files & firmware
            egui::CollapsingHeader::new(format!("Files & firmware ({})", self.files.len())).default_open(true).show(ui, |ui| {
                if self.files.is_empty() {
                    ui.weak("No files yet. Supported: Wildcloud TinyFox / NanoFox 30Days / FineScalePressure, Movebank Sigfox & GPS, CTT GPS, any lat/lon/time CSV.");
                }
                let mut reparse: Option<(usize, String)> = None;
                for (i, f) in self.files.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&f.name).small());
                    });
                    let mut chosen = f.firmware.clone();
                    egui::ComboBox::from_id_salt(("fw", i))
                        .width(260.0)
                        .selected_text(firmware::label_for(&chosen))
                        .show_ui(ui, |ui| {
                            for fw in firmware::registry() {
                                let score = f.ranking.iter().find(|r| r.0 == fw.id()).map(|r| r.1).unwrap_or(0.0);
                                ui.selectable_value(&mut chosen, fw.id().to_string(), format!("{}  ({:.0}%)", fw.label(), score * 100.0))
                                    .on_hover_text(fw.description());
                            }
                        });
                    if chosen != f.firmware {
                        reparse = Some((i, chosen));
                    }
                }
                if let Some((i, fw)) = reparse {
                    let name = self.files[i].name.clone();
                    let bytes = self.files[i].bytes.clone();
                    self.add_file(name, bytes, Some(&fw));
                }
            });

            // ---- tracks
            egui::CollapsingHeader::new(format!("Tracks ({})", self.tracks.len())).default_open(true).show(ui, |ui| {
                if !self.tracks.is_empty() {
                    let n_vis = self.tracks.iter().filter(|t| t.visible).count();
                    let mut all = n_vis == self.tracks.len();
                    ui.horizontal(|ui| {
                        let partial = n_vis > 0 && !all;
                        if ui
                            .add(egui::Checkbox::new(&mut all, format!("all individuals ({n_vis}/{})", self.tracks.len())).indeterminate(partial))
                            .changed()
                        {
                            self.tracks.iter_mut().for_each(|t| t.visible = all);
                        }
                        if ui.small_button("none").clicked() {
                            self.tracks.iter_mut().for_each(|t| t.visible = false);
                        }
                        if ui.small_button("invert").clicked() {
                            self.tracks.iter_mut().for_each(|t| t.visible = !t.visible);
                        }
                    });
                    ui.separator();
                }
                let mut zoom_to = None;
                for (i, tr) in self.tracks.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut tr.visible, "");
                        ui.color_edit_button_srgb(&mut tr.color);
                        let sel = self.selected_track == Some(i);
                        if ui.add(egui::Button::new(egui::RichText::new(&tr.id).strong()).selected(sel)).clicked() {
                            self.selected_track = if sel { None } else { Some(i) };
                        }
                        if ui.small_button("zoom").on_hover_text("Zoom to track").clicked() {
                            zoom_to = Some(i);
                        }
                    });
                    let range = tr.time_range().map(|(a, b)| format!("{} to {}", fmt_date(a), fmt_date(b))).unwrap_or_default();
                    ui.weak(format!(
                        "   {} · {} fixes · {}{}",
                        tr.tag,
                        tr.fixes.len(),
                        range,
                        tr.species.as_ref().map(|s| format!(" · {s}")).unwrap_or_default()
                    ));
                }
                if let Some(i) = zoom_to {
                    self.selected_track = Some(i);
                    self.fit_pending = true;
                }
            });

            // ---- display
            egui::CollapsingHeader::new("Display").default_open(true).show(ui, |ui| {
                egui::ComboBox::from_label("colour by").selected_text(self.color_by.label()).show_ui(ui, |ui| {
                    for c in ColorBy::ALL {
                        ui.selectable_value(&mut self.color_by, c, c.label());
                    }
                });
                ui.add(egui::Slider::new(&mut self.max_error_km, 0.5..=50.0).logarithmic(true).text("max location error (km)"))
                    .on_hover_text("Hide Sigfox fixes whose Atlas radius exceeds this");
                ui.checkbox(&mut self.hide_insignificant, "hide steps smaller than location error");
                ui.checkbox(&mut self.show_error, "location-error circles");
                ui.checkbox(&mut self.show_wind_arrows, "wind arrows at fixes");
                ui.checkbox(&mut self.show_wind_field, "ERA5 wind field at cursor time");
                ui.indent("wind_field_opts", |ui| {
                    ui.add_enabled(
                        self.show_wind_field,
                        egui::Checkbox::new(&mut self.wind_field_night_only, "night cells only (sun < −6°)"),
                    )
                    .on_hover_text("Draw wind arrows only where the sun is more than 6° below the horizon at the cursor time");
                    ui.checkbox(&mut self.skip_daytime, "skip daytime during playback")
                        .on_hover_text("Playback jumps ahead whenever it is day across the whole wind-field / track area");
                    let mut pal = self.wind_palette();
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("wind_palette").selected_text(pal.label()).show_ui(ui, |ui| {
                            for p in WindPalette::ALL {
                                ui.selectable_value(&mut pal, p, p.label());
                            }
                        });
                        ui.label("palette");
                        if self.wind_palettes.contains_key(&self.basemap)
                            && ui.small_button("default").on_hover_text("Use this basemap's default palette").clicked()
                        {
                            self.wind_palettes.remove(&self.basemap);
                            pal = WindPalette::default_for(self.basemap);
                        }
                    })
                    .response
                    .on_hover_text("Remembered separately for each basemap");
                    if pal != self.wind_palette() {
                        self.wind_palettes.insert(self.basemap, pal);
                    }
                });
                ui.checkbox(&mut self.trail_to_cursor, "draw tracks only up to cursor");
                ui.checkbox(&mut self.follow, "follow selected animal");
            });

            // ---- weather
            egui::CollapsingHeader::new("Wind & weather (ERA5)").default_open(true).show(ui, |ui| {
                egui::ComboBox::from_label("model").selected_text(self.opts.model.label()).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.opts.model, Model::Era5, Model::Era5.label());
                    ui.selectable_value(&mut self.opts.model, Model::BestMatch, Model::BestMatch.label());
                });
                egui::ComboBox::from_label("wind level").selected_text(self.opts.level.label()).show_ui(ui, |ui| {
                    for l in [WindLevel::Auto, WindLevel::M10, WindLevel::M100] {
                        ui.selectable_value(&mut self.opts.level, l, l.label());
                    }
                });
                ui.checkbox(&mut self.opts.segments, "sample wind along each step (hourly)")
                    .on_hover_text("Essential for Sigfox: fixes are hours apart and usually at the roost");
                ui.add_enabled(self.opts.segments, egui::Checkbox::new(&mut self.opts.night_only, "night hours only (sun < −6°)"));
                ui.add_enabled(
                    self.opts.segments,
                    egui::Slider::new(&mut self.opts.max_segment_h, 6.0..=168.0).text("max step length (h)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.opts.field_margin_deg, 0.0..=5.0)
                        .step_by(0.25)
                        .suffix("°")
                        .text("wind-field margin"),
                )
                .on_hover_text("Also download a grid of cells this far around the tracks, so the wind field covers the whole region");
                let spacing = |s: i32| format!("{}°", s as f64 * weather::GRID);
                ui.add_enabled_ui(self.opts.field_margin_deg > 0.0, |ui| {
                    egui::ComboBox::from_label("wind-field spacing").selected_text(spacing(self.opts.field_stride)).show_ui(ui, |ui| {
                        for s in [1, 2, 4] {
                            ui.selectable_value(&mut self.opts.field_stride, s, spacing(s));
                        }
                    });
                });
                if self.opts.field_margin_deg > 0.0 && !self.tracks.is_empty() {
                    let n = weather::field_cells(&self.tracks, &self.opts).len();
                    let txt = format!("{n} wind-field cells (downloaded with the next Annotate)");
                    if n > 600 {
                        ui.colored_label(Color32::from_rgb(255, 180, 80), txt + " — many API calls; widen the spacing?");
                    } else {
                        ui.weak(txt);
                    }
                }
                let busy = !self.queue.is_empty() || !self.in_flight.is_empty();
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.tracks.is_empty() && !busy, egui::Button::new("Annotate with ERA5")).clicked() {
                        self.start_weather();
                    }
                    if ui.add_enabled(!self.store.is_empty(), egui::Button::new("Re-apply")).on_hover_text("Recompute with the current options from cached data").clicked() {
                        let n = weather::annotate(&mut self.tracks, &self.store, &self.opts);
                        self.status(format!("Re-annotated {n} fixes"));
                    }
                });
                if busy || self.req_total > 0 && self.req_done < self.req_total {
                    let frac = if self.req_total > 0 { self.req_done as f32 / self.req_total as f32 } else { 0.0 };
                    ui.add(egui::ProgressBar::new(frac).text(format!("{}/{} requests", self.req_done, self.req_total)));
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
                }
                if !self.store.is_empty() {
                    ui.weak(format!("cache: {} cells · {} hours", self.store.cells.len(), self.store.hours()));
                }
            });

            // ---- selected fix
            if let Some((ti, fi)) = self.selected
                && let Some(f) = self.tracks.get(ti).and_then(|t| t.fixes.get(fi)) {
                    let tr = &self.tracks[ti];
                    egui::CollapsingHeader::new(format!("Selected fix · {}", tr.id)).default_open(true).show(ui, |ui| {
                        fix_details(ui, tr, f);
                    });
                }

            egui::CollapsingHeader::new("Log").default_open(false).show(ui, |ui| {
                for l in self.log.iter().rev().take(40) {
                    ui.small(l);
                }
            });
        });
    }

    fn timeline(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui
                .button(if self.playing { "Pause" } else { "Play" })
                .clicked()
            {
                if !self.playing && self.t_now >= self.t_max {
                    self.t_now = self.t_min;
                }
                self.playing = !self.playing;
            }
            if ui
                .button("< fix")
                .on_hover_text("previous fix (left arrow)")
                .clicked()
            {
                self.step_selected(-1);
            }
            if ui
                .button("fix >")
                .on_hover_text("next fix (right arrow)")
                .clicked()
            {
                self.step_selected(1);
            }
            egui::ComboBox::from_id_salt("speed")
                .width(80.0)
                .selected_text(SPEEDS[self.speed_idx].1)
                .show_ui(ui, |ui| {
                    for (i, s) in SPEEDS.iter().enumerate() {
                        ui.selectable_value(&mut self.speed_idx, i, s.1);
                    }
                });
            let (lo, hi) = (self.t_min, self.t_max);
            ui.spacing_mut().slider_width = (ui.available_width() - 190.0).max(100.0);
            ui.add(
                egui::Slider::new(&mut self.t_now, lo..=hi)
                    .show_value(false)
                    .trailing_fill(true),
            );
            ui.monospace(fmt_time(self.t_now as i64) + " UTC");
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("chartvar")
                .selected_text(self.chart_var.label())
                .show_ui(ui, |ui| {
                    for v in ChartVar::ALL {
                        ui.selectable_value(&mut self.chart_var, v, v.label());
                    }
                });
            ui.weak(match self.selected_track {
                Some(i) => format!(
                    "showing {}",
                    self.tracks.get(i).map(|t| t.id.as_str()).unwrap_or("")
                ),
                None => "showing all visible tracks (click a track name to isolate)".into(),
            });
        });

        let var = self.chart_var;
        let mut series: Vec<(String, Color32, Vec<[f64; 2]>)> = Vec::new();
        for (i, tr) in self.tracks.iter().enumerate() {
            if !tr.visible || self.selected_track.is_some_and(|s| s != i) {
                continue;
            }
            let pts: Vec<[f64; 2]> = match var {
                ChartVar::Sensor(k) => tr
                    .sensors
                    .iter()
                    .filter(|s| s.kind == k)
                    .map(|s| [s.t as f64 / 86400.0, s.value as f64])
                    .collect(),
                _ => tr
                    .fixes
                    .iter()
                    .filter(|f| self.fix_visible(f))
                    .filter_map(|f| var.of_fix(f).map(|v| [f.t as f64 / 86400.0, v as f64]))
                    .collect(),
            };
            if !pts.is_empty() {
                series.push((
                    tr.id.clone(),
                    Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]),
                    pts,
                ));
            }
        }
        let t_now_d = self.t_now / 86400.0;
        let zero_line = matches!(
            var,
            ChartVar::WindSupport | ChartVar::SegmentWind | ChartVar::Crosswind
        );
        let resp = Plot::new("chart")
            .legend(Legend::default())
            .allow_scroll(false)
            .x_axis_formatter(|m: GridMark, _r| fmt_date((m.value * 86400.0) as i64))
            .label_formatter(|h: &HoverPosition<'_>| {
                Some(match h {
                    HoverPosition::NearDataPoint {
                        plot_name,
                        position: p,
                        ..
                    } => {
                        format!(
                            "{plot_name}\n{}\n{:.2}",
                            fmt_time((p.x * 86400.0) as i64),
                            p.y
                        )
                    }
                    HoverPosition::Elsewhere { position: p } => fmt_time((p.x * 86400.0) as i64),
                })
            })
            .show(ui, |pui| {
                for (name, c, pts) in &series {
                    pui.line(
                        Line::new(name.clone(), PlotPoints::from(pts.clone()))
                            .color(*c)
                            .width(1.5),
                    );
                    pui.points(
                        Points::new(name.clone(), PlotPoints::from(pts.clone()))
                            .color(*c)
                            .radius(2.0),
                    );
                }
                if zero_line {
                    pui.hline(egui_plot::HLine::new("0", 0.0).color(Color32::from_gray(90)));
                }
                pui.vline(VLine::new("now", t_now_d).color(Color32::from_rgb(255, 80, 80)));
                if pui.response().clicked() {
                    pui.pointer_coordinate()
                } else {
                    None
                }
            });
        if let Some(p) = resp.inner {
            self.t_now = (p.x * 86400.0).clamp(self.t_min, self.t_max);
            self.playing = false;
        }
    }

    fn map(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.available_rect_before_wrap();
        if self.fit_pending {
            self.fit_to(rect, self.selected_track);
            self.fit_pending = false;
        }
        // Follow mode
        let mut my_pos = self.data_center();
        if self.follow
            && let Some(ti) = self.selected.map(|s| s.0).or(self.selected_track)
            && let Some((la, lo, _)) = self.tracks.get(ti).and_then(|t| t.position_at(self.t_now))
        {
            my_pos = lon_lat(lo, la);
            self.memory.follow_my_position();
        }

        let basemap = self.basemap;
        let wind_pal = self.wind_palette();
        let base = self
            .base_tiles
            .entry(basemap)
            .or_insert_with(|| HttpTiles::new(basemap, ctx.clone()));
        let date = fmt_date(self.t_now as i64);
        let sat_key = (self.sat_layer, date.clone());
        if self.sat_on && !self.sat_tiles.contains_key(&sat_key) {
            if self.sat_tiles.len() > 4 {
                self.sat_tiles.clear();
            }
            self.sat_tiles.insert(
                sat_key.clone(),
                HttpTiles::new(
                    Gibs {
                        layer: self.sat_layer,
                        date: date.clone(),
                    },
                    ctx.clone(),
                ),
            );
        }

        let mut interaction = Interaction::default();
        let layer = TrackLayer {
            tracks: &self.tracks,
            t_now: self.t_now,
            trail_to_cursor: self.trail_to_cursor,
            show_error: self.show_error,
            show_wind_arrows: self.show_wind_arrows,
            show_wind_field: self.show_wind_field,
            wind_field_night_only: self.wind_field_night_only,
            wind_palette: wind_pal,
            store: &self.store,
            selected: self.selected,
            app: ViewCfg {
                max_error_m: self.max_error_km * 1000.0,
                hide_insignificant: self.hide_insignificant,
                color_by: self.color_by,
            },
            out: &mut interaction,
        };

        let attribution = base.attribution().text;
        let mut map = walkers::Map::new(Some(base as &mut dyn Tiles), &mut self.memory, my_pos);
        if self.sat_on
            && let Some(st) = self.sat_tiles.get_mut(&sat_key)
        {
            map = map.with_layer(st as &mut dyn Tiles, self.sat_opacity);
        }
        let color_by = self.color_by;
        let wind_legend = (self.show_wind_field && !self.store.is_empty()).then_some(wind_pal);
        let sat_on = self.sat_on;
        let sat_label = self.sat_layer.label();
        let t_now = self.t_now;
        let sun_at = self
            .selected
            .map(|s| s.0)
            .or(self.selected_track)
            .and_then(|ti| self.tracks.get(ti).and_then(|t| t.position_at(t_now)));
        map.with_plugin(layer).show(ui, |ui, _resp, _proj, _mem| {
            let r = ui.max_rect();
            let p = ui.painter();
            // attribution
            let attr = if sat_on {
                format!("{attribution} | NASA GIBS")
            } else {
                attribution.to_string()
            };
            let ar = p.text(
                r.right_bottom() - vec2(6.0, 4.0),
                Align2::RIGHT_BOTTOM,
                &attr,
                FontId::proportional(10.0),
                Color32::from_gray(230),
            );
            p.rect_filled(ar.expand(2.0), 2.0, Color32::from_black_alpha(120));
            p.text(
                r.right_bottom() - vec2(6.0, 4.0),
                Align2::RIGHT_BOTTOM,
                &attr,
                FontId::proportional(10.0),
                Color32::from_gray(230),
            );
            // clock
            let mut clock = format!("{} UTC", fmt_time(t_now as i64));
            if let Some((la, lo, _)) = sun_at {
                let e = geo::sun_elevation_deg(t_now as i64, la, lo);
                clock.push_str(if e < -6.0 {
                    "   night"
                } else if e < 0.0 {
                    "   twilight"
                } else {
                    "   day"
                });
                clock.push_str(&format!(" (sun {e:.0}°)"));
            }
            if sat_on {
                clock.push_str(&format!("\nsatellite: {sat_label}"));
            }
            let cr = p.text(
                r.center_top() + vec2(0.0, 10.0),
                Align2::CENTER_TOP,
                &clock,
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            p.rect_filled(cr.expand(5.0), 4.0, Color32::from_black_alpha(150));
            p.text(
                r.center_top() + vec2(0.0, 10.0),
                Align2::CENTER_TOP,
                &clock,
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            legend(p, r, color_by, wind_legend);
        });

        if let Some(sel) = interaction.clicked {
            self.selected = Some(sel);
            self.t_now = self.tracks[sel.0].fixes[sel.1].t as f64;
            self.playing = false;
        }
        if let Some((ti, fi, pos)) = interaction.hover {
            let tr = &self.tracks[ti];
            let f = &tr.fixes[fi];
            let mut s = format!("{} · {}\n{}", tr.id, tr.tag, fmt_time(f.t));
            if let Some(e) = f.error_m {
                s.push_str(&format!("\n± {:.1} km", e / 1000.0));
            }
            if let Some(a) = f.best_alt() {
                s.push_str(&format!("\nalt {a:.0} m"));
            }
            if let Some(w) = f.weather.wind_support {
                s.push_str(&format!("\nwind support {w:+.1} m/s"));
            }
            if let Some(w) = f.weather.seg_wind_support {
                s.push_str(&format!("\nstep wind {w:+.1} m/s"));
            }
            let painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::unique("maptip"),
            ));
            let galley_rect = painter.text(
                pos + vec2(14.0, 0.0),
                Align2::LEFT_TOP,
                &s,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
            painter.rect_filled(galley_rect.expand(5.0), 4.0, Color32::from_black_alpha(200));
            painter.text(
                pos + vec2(14.0, 0.0),
                Align2::LEFT_TOP,
                &s,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
    }
}

fn fix_details(ui: &mut Ui, tr: &Track, f: &Fix) {
    let w = &f.weather;
    let row = |ui: &mut Ui, k: &str, v: String| {
        ui.label(k);
        ui.monospace(v);
        ui.end_row();
    };
    let o = |v: Option<f32>, unit: &str, d: usize| {
        v.map(|x| format!("{x:.d$} {unit}"))
            .unwrap_or_else(|| "-".into())
    };
    egui::Grid::new("fixgrid")
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            row(ui, "time (UTC)", fmt_time(f.t));
            row(ui, "position", format!("{:.5}, {:.5}", f.lat, f.lon));
            row(ui, "source", f.source.label().into());
            row(
                ui,
                "error radius",
                o(f.error_m.map(|e| e / 1000.0), "km", 2),
            );
            row(
                ui,
                "altitude",
                match (f.alt_m, f.alt_source) {
                    (Some(a), Some(s)) => format!("{a:.0} m ({})", s.label()),
                    _ => "-".into(),
                },
            );
            row(ui, "alt (ERA5 MSLP)", o(w.alt_msl_corrected, "m", 0));
            row(ui, "pressure", o(f.pressure_hpa, "hPa", 1));
            row(ui, "step", o(f.step_m.map(|s| s / 1000.0), "km", 1));
            row(ui, "time since prev", o(f.dt_s.map(|s| s / 3600.0), "h", 1));
            row(ui, "ground speed", o(f.ground_speed_ms, "m/s", 1));
            row(ui, "heading", o(f.bearing_deg, "°", 0));
            row(
                ui,
                "real movement",
                if f.step_significant {
                    "yes".into()
                } else {
                    "no (< error)".into()
                },
            );
            row(ui, "tag temp", o(f.temp_c, "°C", 1));
            row(ui, "activity", o(f.activity, "", 0));
            row(ui, "wind level", w.level.unwrap_or("-").into());
            row(
                ui,
                "wind",
                match (w.wind_speed, w.wind_from_deg) {
                    (Some(s), Some(d)) => format!("{s:.1} m/s from {d:.0}°"),
                    _ => "-".into(),
                },
            );
            row(ui, "wind support", o(w.wind_support, "m/s", 1));
            row(ui, "crosswind", o(w.crosswind, "m/s", 1));
            row(ui, "airspeed", o(w.airspeed, "m/s", 1));
            row(
                ui,
                "step wind support",
                match (w.seg_wind_support, w.seg_hours) {
                    (Some(s), Some(h)) => format!("{s:+.1} m/s ({h} h)"),
                    _ => "-".into(),
                },
            );
            row(ui, "step crosswind", o(w.seg_crosswind, "m/s", 1));
            row(ui, "ERA5 T2m", o(w.temp_2m, "°C", 1));
            row(ui, "ERA5 MSLP", o(w.msl_hpa, "hPa", 1));
            row(ui, "precipitation", o(w.precipitation_mm, "mm", 1));
            row(ui, "cloud cover", o(w.cloud_cover, "%", 0));
            for (k, v) in &f.extra {
                row(ui, k, v.clone());
            }
        });
    ui.weak(format!(
        "firmware profile: {} · file: {}",
        firmware::label_for(&tr.firmware),
        tr.file
    ));
}

impl eframe::App for WingApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_messages();
        self.pump_weather(ctx);
        self.record_tick(ctx);
        if self.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1) as f64;
            self.t_now += dt * SPEEDS[self.speed_idx].0;
            if self.skip_daytime {
                self.skip_to_night();
            }
            if self.t_now >= self.t_max {
                self.t_now = self.t_max;
                self.playing = false;
            }
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_drops(&ctx);
        let typing = ctx.egui_wants_keyboard_input();
        if !typing {
            if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
                self.playing = !self.playing;
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                self.step_selected(1);
            }
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                self.step_selected(-1);
            }
        }
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("side")
            .resizable(true)
            .default_size(330.0)
            .show(ui, |ui| self.side_panel(ui));
        self.timeline_rect = egui::Panel::bottom("timeline")
            .resizable(true)
            .default_size(250.0)
            .show(ui, |ui| self.timeline(ui))
            .response
            .rect;
        self.map_rect = egui::CentralPanel::no_frame()
            .show(ui, |ui| self.map(ui))
            .response
            .rect;
        self.video_window(&ctx);
    }
}

// ---------------------------------------------------------------------------
// Map drawing plugin
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Interaction {
    hover: Option<(usize, usize, Pos2)>,
    clicked: Option<(usize, usize)>,
}

#[derive(Clone, Copy)]
struct ViewCfg {
    max_error_m: f32,
    hide_insignificant: bool,
    color_by: ColorBy,
}

struct TrackLayer<'a> {
    tracks: &'a [Track],
    t_now: f64,
    trail_to_cursor: bool,
    show_error: bool,
    show_wind_arrows: bool,
    show_wind_field: bool,
    wind_field_night_only: bool,
    wind_palette: WindPalette,
    store: &'a WeatherStore,
    selected: Option<(usize, usize)>,
    app: ViewCfg,
    out: &'a mut Interaction,
}

impl ViewCfg {
    fn visible(&self, f: &Fix) -> bool {
        !(f.error_m.is_some_and(|e| e > self.max_error_m)
            || self.hide_insignificant && !f.step_significant)
    }
    fn color(&self, tr: &Track, f: &Fix) -> Color32 {
        let base = Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]);
        let none = Color32::from_gray(140);
        match self.color_by {
            ColorBy::Individual => base,
            ColorBy::WindSupport => f
                .weather
                .wind_support
                .map(|v| diverging(v / 10.0))
                .unwrap_or(none),
            ColorBy::SegmentWind => f
                .weather
                .seg_wind_support
                .map(|v| diverging(v / 10.0))
                .unwrap_or(none),
            ColorBy::Altitude => f.best_alt().map(|a| viridis(a / 1500.0)).unwrap_or(none),
            ColorBy::GroundSpeed => f
                .ground_speed_ms
                .filter(|_| f.step_significant)
                .map(|s| viridis(s / 15.0))
                .unwrap_or(none),
            ColorBy::Time => {
                let (a, b) = tr.time_range().unwrap_or((0, 1));
                viridis(((f.t - a) as f32) / ((b - a).max(1) as f32))
            }
        }
    }
}

impl Plugin for TrackLayer<'_> {
    fn run(
        self: Box<Self>,
        ui: &mut Ui,
        response: &egui::Response,
        projector: &Projector,
        _memory: &MapMemory,
    ) {
        let painter = ui.painter().clone();
        let clip = response.rect;
        let to_screen =
            |lat: f64, lon: f64| -> Pos2 { projector.project(lon_lat(lon, lat)).to_pos2() };
        let hover_pos = response.hover_pos();
        let mut best: Option<(f32, usize, usize, Pos2)> = None;

        // ERA5 wind field at the cursor time.
        if self.show_wind_field && !self.store.is_empty() {
            let t = self.t_now as i64;
            let ppm = projector.scale_pixel_per_meter(projector.unproject(clip.center().to_vec2()));
            let cell_px = (weather::GRID * 111_000.0) as f32 * ppm;
            let stride = ((22.0 / cell_px.max(0.1)).ceil() as i32).max(1);
            // One arrow per stride×stride block of cells: the cell with data closest to the
            // block's corner, so the pattern is stable while panning and works for cells on
            // any download spacing (fix cells at 0.25°, wind-field grid at 0.5° / 1°).
            let mut blocks: HashMap<(i32, i32), (i32, Pos2, f32, f32)> = HashMap::new();
            for &k in self.store.cells.keys() {
                let (la, lo) = weather::cell_center(k);
                let p = to_screen(la, lo);
                if !clip.expand(20.0).contains(p) {
                    continue;
                }
                if self.wind_field_night_only && !geo::is_night(t, la, lo) {
                    continue;
                }
                let Some(v) = self.store.sample_cell(k, t) else {
                    continue;
                };
                let (ws, wd) = (v[2], v[3]);
                if ws.is_nan() || wd.is_nan() {
                    continue;
                }
                let score = k.0.rem_euclid(stride) + k.1.rem_euclid(stride);
                let b = (k.0.div_euclid(stride), k.1.div_euclid(stride));
                if blocks.get(&b).is_none_or(|e| score < e.0) {
                    blocks.insert(b, (score, p, ws, wd));
                }
            }
            let pal = self.wind_palette;
            for (_, p, ws, wd) in blocks.into_values() {
                let (u, vv) = geo::wind_uv(ws as f64, wd as f64);
                let len = (ws * 2.2).clamp(4.0, 34.0);
                let dir = vec2(u as f32, -(vv as f32)).normalized() * len;
                if let Some(h) = pal.halo() {
                    painter.arrow(p - dir * 0.5, dir, Stroke::new(3.8, h));
                }
                let c = pal.color(ws / 20.0).gamma_multiply(0.9);
                painter.arrow(p - dir * 0.5, dir, Stroke::new(1.6, c));
            }
        }

        for (ti, tr) in self.tracks.iter().enumerate() {
            if !tr.visible || tr.fixes.is_empty() {
                continue;
            }
            let base = Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]);
            let idx: Vec<usize> = (0..tr.fixes.len())
                .filter(|&i| self.app.visible(&tr.fixes[i]))
                .filter(|&i| !self.trail_to_cursor || tr.fixes[i].t as f64 <= self.t_now)
                .collect();
            // future part (faint) when drawing only to cursor
            if self.trail_to_cursor {
                let fut: Vec<Pos2> = tr
                    .fixes
                    .iter()
                    .filter(|f| self.app.visible(f) && f.t as f64 > self.t_now)
                    .map(|f| to_screen(f.lat, f.lon))
                    .collect();
                if fut.len() > 1 {
                    painter.add(egui::Shape::line(
                        fut,
                        Stroke::new(1.0, base.gamma_multiply(0.25)),
                    ));
                }
            }
            let mut prev: Option<Pos2> = None;
            for &i in &idx {
                let f = &tr.fixes[i];
                let p = to_screen(f.lat, f.lon);
                let c = self.app.color(tr, f);
                if let Some(q) = prev {
                    let dashed = !f.step_significant;
                    let st = Stroke::new(
                        if dashed { 1.2 } else { 2.4 },
                        if dashed { c.gamma_multiply(0.5) } else { c },
                    );
                    painter.line_segment([q, p], st);
                }
                prev = Some(p);
            }
            for &i in &idx {
                let f = &tr.fixes[i];
                let p = to_screen(f.lat, f.lon);
                if !clip.expand(50.0).contains(p) {
                    continue;
                }
                let c = self.app.color(tr, f);
                if self.show_error
                    && f.source == LocationSource::SigfoxAtlas
                    && let Some(e) = f.error_m
                {
                    let r = e * projector.scale_pixel_per_meter(lon_lat(f.lon, f.lat));
                    if r > 3.0 && r < 2000.0 {
                        painter.circle(
                            p,
                            r,
                            base.gamma_multiply(0.06),
                            Stroke::new(0.7, base.gamma_multiply(0.35)),
                        );
                    }
                }
                if self.show_wind_arrows
                    && let (Some(u), Some(v)) = (f.weather.u, f.weather.v)
                {
                    let s = (u * u + v * v).sqrt();
                    let len = (s * 2.5).clamp(5.0, 40.0);
                    let dir = vec2(u, -v).normalized() * len;
                    let col = f
                        .weather
                        .seg_wind_support
                        .or(f.weather.wind_support)
                        .map(|w| diverging(w / 10.0))
                        .unwrap_or(Color32::from_gray(200));
                    painter.arrow(p, dir, Stroke::new(1.8, col));
                }
                let sel = self.selected == Some((ti, i));
                painter.circle_filled(p, if sel { 6.0 } else { 3.2 }, c);
                if sel {
                    painter.circle_stroke(p, 9.0, Stroke::new(2.0, Color32::WHITE));
                }
                if let Some(h) = hover_pos {
                    let d = h.distance(p);
                    if d < 9.0 && best.is_none_or(|b| d < b.0) {
                        best = Some((d, ti, i, p));
                    }
                }
            }
            // current position marker
            if let Some((la, lo, _)) = tr.position_at(self.t_now) {
                let p = to_screen(la, lo);
                painter.circle(p, 7.5, base, Stroke::new(2.0, Color32::BLACK));
                let label_rect = painter.text(
                    p + vec2(10.0, -10.0),
                    Align2::LEFT_BOTTOM,
                    &tr.id,
                    FontId::proportional(13.0),
                    Color32::WHITE,
                );
                painter.rect_filled(label_rect.expand(2.0), 3.0, Color32::from_black_alpha(140));
                painter.text(
                    p + vec2(10.0, -10.0),
                    Align2::LEFT_BOTTOM,
                    &tr.id,
                    FontId::proportional(13.0),
                    Color32::WHITE,
                );
            }
        }

        if let Some((_, ti, fi, p)) = best {
            self.out.hover = Some((ti, fi, p));
            if response.clicked() {
                self.out.clicked = Some((ti, fi));
            }
        }
    }
}

/// Track colour legend, plus the wind-field speed scale stacked above it when shown.
fn legend(p: &egui::Painter, r: Rect, by: ColorBy, wind: Option<WindPalette>) {
    let mut origin = pos2(r.left() + 12.0, r.bottom() - 40.0);
    if track_legend(p, origin, by) {
        origin.y -= 60.0;
    }
    if let Some(pal) = wind {
        colorbar(p, origin, "ERA5 wind speed (m/s)", "0", "20+", &|x| {
            pal.color(x)
        });
    }
}

fn track_legend(p: &egui::Painter, origin: Pos2, by: ColorBy) -> bool {
    let (title, lo, hi, div): (&str, &str, &str, bool) = match by {
        ColorBy::Individual => return false,
        ColorBy::WindSupport => ("wind support at fix (m/s)", "-10 head", "+10 tail", true),
        ColorBy::SegmentWind => (
            "along-step wind support (m/s)",
            "-10 head",
            "+10 tail",
            true,
        ),
        ColorBy::Altitude => ("altitude (m)", "0", "1500", false),
        ColorBy::GroundSpeed => ("ground speed (m/s)", "0", "15", false),
        ColorBy::Time => ("time within track", "start", "end", false),
    };
    let f = |x: f32| {
        if div {
            diverging(x * 2.0 - 1.0)
        } else {
            viridis(x)
        }
    };
    colorbar(p, origin, title, lo, hi, &f);
    true
}

fn colorbar(
    p: &egui::Painter,
    origin: Pos2,
    title: &str,
    lo: &str,
    hi: &str,
    color: &dyn Fn(f32) -> Color32,
) {
    let w = 180.0;
    p.rect_filled(
        Rect::from_min_size(origin - vec2(6.0, 20.0), vec2(w + 12.0, 52.0)),
        4.0,
        Color32::from_black_alpha(160),
    );
    p.text(
        origin - vec2(0.0, 6.0),
        Align2::LEFT_BOTTOM,
        title,
        FontId::proportional(11.0),
        Color32::WHITE,
    );
    let n = 40;
    for i in 0..n {
        let x = i as f32 / (n - 1) as f32;
        let c = color(x);
        let rr = Rect::from_min_size(
            origin + vec2(x * (w - w / n as f32), 0.0),
            vec2(w / n as f32 + 1.0, 10.0),
        );
        p.rect_filled(rr, 0.0, c);
    }
    p.text(
        origin + vec2(0.0, 13.0),
        Align2::LEFT_TOP,
        lo,
        FontId::proportional(10.0),
        Color32::WHITE,
    );
    p.text(
        origin + vec2(w, 13.0),
        Align2::RIGHT_TOP,
        hi,
        FontId::proportional(10.0),
        Color32::WHITE,
    );
}

/// Viridis-like sequential scale, x in 0..1.
pub fn viridis(x: f32) -> Color32 {
    const S: [[f32; 3]; 5] = [
        [68.0, 1.0, 84.0],
        [59.0, 82.0, 139.0],
        [33.0, 145.0, 140.0],
        [94.0, 201.0, 98.0],
        [253.0, 231.0, 37.0],
    ];
    lerp_stops(&S, x)
}

/// Diverging scale for wind support, x in −1 (headwind, red-orange) .. +1 (tailwind, blue).
pub fn diverging(x: f32) -> Color32 {
    const S: [[f32; 3]; 5] = [
        [202.0, 0.0, 32.0],
        [244.0, 165.0, 130.0],
        [240.0, 240.0, 240.0],
        [146.0, 197.0, 222.0],
        [5.0, 113.0, 176.0],
    ];
    lerp_stops(&S, (x + 1.0) / 2.0)
}

fn lerp_stops(s: &[[f32; 3]], x: f32) -> Color32 {
    let x = if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) };
    let f = x * (s.len() - 1) as f32;
    let i = (f.floor() as usize).min(s.len() - 2);
    let w = f - i as f32;
    let c = |k: usize| (s[i][k] + (s[i + 1][k] - s[i][k]) * w) as u8;
    Color32::from_rgb(c(0), c(1), c(2))
}

#[cfg(target_arch = "wasm32")]
fn web_download(name: &str, bytes: &[u8], mime: &str) -> Result<(), String> {
    use wasm_bindgen::JsCast;
    let win = web_sys::window().ok_or("no window")?;
    let doc = win.document().ok_or("no document")?;
    let parts = js_sys::Array::new();
    parts.push(&js_sys::Uint8Array::from(bytes));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts)
        .map_err(|e| format!("{e:?}"))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(|e| format!("{e:?}"))?;
    let a: web_sys::HtmlAnchorElement = doc
        .create_element("a")
        .map_err(|e| format!("{e:?}"))?
        .dyn_into()
        .map_err(|_| "anchor")?;
    a.set_href(&url);
    a.set_download(name);
    a.click();
    let _ = web_sys::Url::revoke_object_url(&url);
    Ok(())
}
