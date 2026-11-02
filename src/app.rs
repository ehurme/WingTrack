//! The egui application: map + satellite layers, timeline, charts, weather annotation, export.

use crate::firmware;
use crate::geo;
use crate::model::{Fix, LocationSource, SensorKind, Track};
use crate::share::{self, ShareMap};
use crate::table::{Table, fmt_date, fmt_time};
use crate::tiles::{Basemap, Gibs, SatLayer};
use crate::video;
use crate::view3d::{Scene, View3D};
use crate::weather::{self, AnnotateOptions, Model, Request, WeatherStore, WindLevel};
use crate::wind3d::{
    self, Curtain, CurtainMetric, FieldSlice, OmModel, ProfileRequest, ProfileStore,
};
use egui::{Align2, Color32, ColorImage, FontId, Pos2, Rect, Stroke, Ui, pos2, vec2};
use egui_plot::{GridMark, HoverPosition, Legend, Line, Plot, PlotPoints, Points, VLine};
use std::collections::{HashMap, HashSet, VecDeque};
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
    /// A CSV downloaded from a URL (Open URL… or a shared link).
    RemoteFile {
        url: String,
        bytes: Vec<u8>,
        force: Option<String>,
        from_share: bool,
    },
    FetchFailed {
        url: String,
        err: String,
        from_share: bool,
    },
    /// A step of a Copernicus CDS job: HTTP status and body, or a network error.
    Cds {
        run: u64,
        result: Result<(u16, Vec<u8>), String>,
    },
    /// An Overpass (OpenStreetMap) answer for a batch of structure tiles.
    Overpass {
        tiles: Vec<crate::structures::Tile>,
        result: Result<String, String>,
    },
    /// A wind-turbine / structure register CSV picked by the user.
    StructFile {
        name: String,
        bytes: Vec<u8>,
    },
    /// An ERA5 wind-cube file picked by the user.
    Cube {
        name: String,
        bytes: Vec<u8>,
    },
}

/// One queued download.
enum Job {
    Surface(Request),
    Profile(ProfileRequest),
}

impl Job {
    /// Estimated Open-Meteo call count (see [`weather::call_weight`]).
    fn weight(&self) -> f32 {
        match self {
            Job::Surface(r) => r.weight(),
            Job::Profile(r) => r.weight(),
        }
    }

    fn url(&self) -> &str {
        match self {
            Job::Surface(r) => &r.url,
            Job::Profile(r) => &r.url,
        }
    }
}

/// Where point annotation gets ERA5 surface data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Era5Source {
    /// Open-Meteo archive API: free, no key, limits per IP address (shared by everyone on
    /// the same network).
    OpenMeteo,
    /// Copernicus Climate Data Store with the user's own key and quota.
    Cds,
}

impl Era5Source {
    fn label(self) -> &'static str {
        match self {
            Era5Source::OpenMeteo => "Open-Meteo (free, no key)",
            Era5Source::Cds => "Copernicus CDS (your API key)",
        }
    }
}

/// Stage of one CDS cell job.
#[derive(Clone, Debug)]
enum CdsStage {
    Submit,
    Poll(String),
    Results(String),
    Download(String),
}

#[derive(Clone, Debug)]
struct CdsRun {
    id: u64,
    job: crate::cds::CellJob,
    stage: CdsStage,
    /// A request for this run is in flight.
    busy: bool,
    /// Don't poll again before this time (app clock, s).
    next_at: f64,
    polls: u32,
}

/// Structures to draw on the map, and the steps to highlight (if on).
type StructLayer<'a> = (
    crate::structures::Draw<'a>,
    Option<&'a HashSet<(usize, usize)>>,
);

/// Structures analysis for the current tracks, store and parameters.
struct StCache {
    key: u64,
    a: crate::structures::Analysis,
    /// Structures within the radius of any visible track.
    hits: HashSet<usize>,
    /// (track, fix) whose arriving step passed within the radius of a turbine.
    near: HashSet<(usize, usize)>,
}

/// Parallel CDS jobs (the CDS also queues per user, so more doesn't help).
const CDS_PARALLEL: usize = 4;
/// Give up on a hanging Open-Meteo request after this many seconds.
const OM_TIMEOUT_S: f64 = 150.0;

/// Point an Open-Meteo URL at the customer servers when the user has an API key.
fn om_url(url: &str, key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        return url.to_string();
    }
    format!(
        "{}&apikey={key}",
        url.replacen("https://", "https://customer-", 1)
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ViewTab {
    Map,
    ThreeD,
}

struct LoadedFile {
    name: String,
    bytes: Vec<u8>,
    firmware: String,
    /// Firmware chosen by the user (None = auto-detected).
    forced: Option<String>,
    origin: FileOrigin,
    ranking: Vec<(&'static str, f32)>,
}

/// Where a file came from — decides how a share link carries it.
#[derive(Clone, Debug, PartialEq)]
enum FileOrigin {
    Local,
    Demo,
    Url(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ColorBy {
    Individual,
    WindSupport,
    SegmentWind,
    Altitude,
    GroundSpeed,
    Time,
    AccFlight,
    AccWbf,
    AccCluster,
}

impl ColorBy {
    const ALL: [ColorBy; 9] = [
        ColorBy::Individual,
        ColorBy::WindSupport,
        ColorBy::SegmentWind,
        ColorBy::Altitude,
        ColorBy::GroundSpeed,
        ColorBy::Time,
        ColorBy::AccFlight,
        ColorBy::AccWbf,
        ColorBy::AccCluster,
    ];
    fn label(self) -> &'static str {
        match self {
            ColorBy::Individual => "Individual",
            ColorBy::WindSupport => "Wind support at fix",
            ColorBy::SegmentWind => "Along-step wind support",
            ColorBy::Altitude => "Altitude",
            ColorBy::GroundSpeed => "Ground speed",
            ColorBy::Time => "Time",
            ColorBy::AccFlight => "ACC: flight share of the step",
            ColorBy::AccWbf => "ACC: wingbeat frequency",
            ColorBy::AccCluster => "ACC: behaviour cluster",
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
    FlightSupport,
    BestSupport,
    FlightHeight,
    /// Time × height wind along one track (pressure levels).
    Curtain,
    /// Wind profile at the selected fix / cursor.
    Profile,
    /// A per-burst acceleration metric over time.
    Acc(AccMetric),
    /// Waveform and spectrum of the burst nearest the cursor.
    AccBurst,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AccMetric {
    DynSd,
    Vedba,
    Wbf,
    Pitch,
    Cluster,
}

impl AccMetric {
    fn label(self) -> &'static str {
        match self {
            AccMetric::DynSd => "ACC: dynamic SD per burst (g)",
            AccMetric::Vedba => "ACC: VeDBA per burst (g)",
            AccMetric::Wbf => "ACC: wingbeat frequency of flight bursts (Hz)",
            AccMetric::Pitch => "ACC: pitch (posture, °)",
            AccMetric::Cluster => "ACC: behaviour cluster per burst",
        }
    }
    fn of(self, f: &crate::acc::Features, flight: bool, cluster: usize) -> Option<f64> {
        match self {
            AccMetric::DynSd => Some(f.dyn_sd as f64),
            AccMetric::Vedba => Some(f.vedba as f64),
            AccMetric::Wbf => f.wbf_hz.filter(|_| flight).map(|x| x as f64),
            AccMetric::Pitch => Some(f.pitch_deg as f64),
            AccMetric::Cluster => Some(cluster as f64 + 1.0),
        }
    }
}

/// Behaviour-cluster colours, calm (C1) → dynamic.
const CLUSTER_COLORS: [Color32; 8] = [
    Color32::from_rgb(69, 117, 180),
    Color32::from_rgb(145, 191, 219),
    Color32::from_rgb(171, 221, 164),
    Color32::from_rgb(254, 224, 144),
    Color32::from_rgb(252, 141, 89),
    Color32::from_rgb(215, 48, 39),
    Color32::from_rgb(118, 42, 131),
    Color32::from_rgb(165, 0, 38),
];

/// An acceleration file as loaded (kept to re-read with other settings / share by URL).
struct AccFile {
    name: String,
    bytes: Vec<u8>,
    origin: FileOrigin,
}

/// Burst features + per-fix summaries for the current sets, parameters and tracks.
struct AccCache {
    /// Sets + parameters (features and clusters).
    key: u64,
    a: crate::acc::Analysis,
    /// … + tracks and assignments (per-fix summaries).
    anno_key: u64,
}

impl ChartVar {
    const ALL: [ChartVar; 25] = [
        ChartVar::Curtain,
        ChartVar::Profile,
        ChartVar::FlightSupport,
        ChartVar::BestSupport,
        ChartVar::FlightHeight,
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
        ChartVar::Acc(AccMetric::DynSd),
        ChartVar::Acc(AccMetric::Vedba),
        ChartVar::Acc(AccMetric::Wbf),
        ChartVar::Acc(AccMetric::Pitch),
        ChartVar::Acc(AccMetric::Cluster),
        ChartVar::AccBurst,
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
            ChartVar::FlightSupport => "3D: wind support at flight altitude (m/s)",
            ChartVar::BestSupport => "3D: wind support at best level (m/s)",
            ChartVar::FlightHeight => "3D: flight height from profile (m ASL)",
            ChartVar::Curtain => "3D: time × height wind curtain along track",
            ChartVar::Profile => "3D: wind profile at selected fix / cursor",
            ChartVar::Acc(m) => m.label(),
            ChartVar::AccBurst => "ACC: burst at the cursor (waveform + spectrum)",
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
            ChartVar::FlightSupport => f.weather.flight.as_ref().and_then(|w| w.wind_support),
            ChartVar::BestSupport => f.weather.flight.as_ref().and_then(|w| w.best_wind_support),
            ChartVar::FlightHeight => f.weather.flight.as_ref().and_then(|w| w.flight_height_m),
            ChartVar::Sensor(_)
            | ChartVar::Curtain
            | ChartVar::Profile
            | ChartVar::Acc(_)
            | ChartVar::AccBurst => None,
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
    /// Track whose date-range editor is open, with the From / To text.
    date_edit: Option<usize>,
    falloff: crate::falloff::Params,
    // acceleration
    acc_files: Vec<AccFile>,
    acc_sets: Vec<crate::acc::AccSet>,
    acc_params: crate::acc::Params,
    /// Parameters being edited (applied with a button: analysis can take a moment).
    acc_edit: crate::acc::Params,
    acc_slope: Option<f64>,
    acc_cache: Option<AccCache>,
    /// FleaTag sets whose start was taken from the track's first fix.
    acc_start_guessed: HashSet<String>,
    acc_start_text: HashMap<String, String>,
    // wind turbines & tall structures
    st_on: bool,
    st_params: crate::structures::Params,
    st_highlight: bool,
    st_store: crate::structures::StructureStore,
    st_queue: VecDeque<Vec<crate::structures::Tile>>,
    st_busy: bool,
    st_server: usize,
    st_retry: HashMap<Vec<crate::structures::Tile>, u8>,
    st_next_at: f64,
    st_planned: Option<u64>,
    st_failed: usize,
    st_cache: Option<StCache>,
    date_from: String,
    date_to: String,
    chart_var: ChartVar,

    // weather
    store: WeatherStore,
    opts: AnnotateOptions,
    queue: VecDeque<Job>,
    in_flight: HashMap<usize, Job>,
    next_req_id: usize,
    req_total: usize,
    era5_source: Era5Source,
    /// Copernicus CDS personal access token (kept in prefs, never in share links).
    cds_key: String,
    /// Optional Open-Meteo API key (customer servers, higher limits).
    om_key: String,
    show_keys: bool,
    cds_pending: VecDeque<crate::cds::CellJob>,
    cds_running: Vec<CdsRun>,
    cds_next_id: u64,
    /// Send time of each in-flight Open-Meteo request (for the timeout watchdog).
    in_flight_t: HashMap<usize, f64>,
    /// Why queued downloads are waiting, shown under the progress bar.
    wait_note: Option<String>,
    /// Open-Meteo refused with its daily limit: no more requests before this time.
    om_blocked_until: f64,
    req_done: usize,
    req_failed: usize,
    since_annotate: usize,

    // 3D wind
    view_tab: ViewTab,
    // layout (phones / small windows)
    narrow: bool,
    side_open: bool,
    chart_open: bool,
    layout_init: bool,
    profiles: ProfileStore,
    om_model: OmModel,
    field_auto: bool,
    field_step_cells: i32,
    /// (day, visible-tracks key) for which the 3D field around the animals was requested.
    field_days_requested: HashSet<(i64, u64)>,
    /// Radius (degrees) of the 3D wind field around each animal.
    field_radius_deg: f32,
    /// Same bookkeeping for the 2D (surface) wind field; active after the first Annotate.
    surface_field_on: bool,
    surface_field_days: HashSet<(i64, u64)>,
    // Open-Meteo rate limiting: (time sent, weight) of recent requests, pause after HTTP 429.
    sent_log: VecDeque<(f64, f32)>,
    pause_until: f64,
    now: f64,
    retries: HashMap<String, u8>,
    display_levels: Vec<usize>,
    view3d: View3D,
    slice_cache: Option<(SliceKey, Option<FieldSlice>)>,
    curtain_metric: CurtainMetric,
    curtain_cache: Option<(CurtainKey, Option<Curtain>)>,

    // share links
    share_dialog: bool,
    share_embed: bool,
    url_dialog: bool,
    url_input: String,
    /// A share link being restored: (state, files still downloading).
    pending_share: Option<(ShareMap, usize)>,
    /// Centre of the 2D map (lat, lon) as last drawn.
    map_center: Option<(f64, f64)>,
    /// Packed data files for links: (files key, base64url).
    packed_cache: Option<(u64, String)>,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    last_hash: String,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    hash_synced_at: f64,

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

type CurtainKey = (usize, u64, CurtainMetric, usize);
type SliceKey = (i64, u64, Vec<usize>, (i32, i32, i32, i32));

impl WingApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self::blank();
        if let Some(link) = startup_link() {
            app.start_from_share(&ShareMap::decode(&link));
        } else if std::env::var("WINGTRACK_DEMO").is_ok() {
            app.load_demo();
        }
        app.apply_test_env();
        app
    }

    /// App with default state and no data (also used by tests).
    fn blank() -> Self {
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
            date_edit: None,
            falloff: crate::falloff::Params::default(),
            acc_files: Vec::new(),
            acc_sets: Vec::new(),
            acc_params: crate::acc::Params::default(),
            acc_edit: crate::acc::Params::default(),
            acc_slope: None,
            acc_cache: None,
            acc_start_guessed: HashSet::new(),
            acc_start_text: HashMap::new(),
            st_on: false,
            st_params: crate::structures::Params::default(),
            st_highlight: true,
            st_store: Default::default(),
            st_queue: VecDeque::new(),
            st_busy: false,
            st_server: 0,
            st_retry: HashMap::new(),
            st_next_at: 0.0,
            st_planned: None,
            st_failed: 0,
            st_cache: None,
            date_from: String::new(),
            date_to: String::new(),
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
            era5_source: match crate::prefs::load("era5_source").as_deref() {
                Some("cds") => Era5Source::Cds,
                _ => Era5Source::OpenMeteo,
            },
            cds_key: crate::prefs::load("cds_key").unwrap_or_default(),
            om_key: crate::prefs::load("open_meteo_key").unwrap_or_default(),
            show_keys: false,
            cds_pending: VecDeque::new(),
            cds_running: Vec::new(),
            cds_next_id: 0,
            in_flight_t: HashMap::new(),
            wait_note: None,
            om_blocked_until: 0.0,
            share_dialog: false,
            share_embed: true,
            url_dialog: false,
            url_input: String::new(),
            pending_share: None,
            map_center: None,
            packed_cache: None,
            last_hash: String::new(),
            hash_synced_at: 0.0,
            video: video::Settings::default(),
            video_dialog: false,
            ffmpeg: None,
            rec: None,
            map_rect: Rect::NOTHING,
            timeline_rect: Rect::NOTHING,
            view_tab: ViewTab::Map,
            narrow: false,
            side_open: true,
            chart_open: true,
            layout_init: false,
            profiles: ProfileStore::default(),
            om_model: OmModel::Gfs,
            field_auto: true,
            field_step_cells: 4,
            field_days_requested: HashSet::new(),
            field_radius_deg: 3.0,
            surface_field_on: false,
            surface_field_days: HashSet::new(),
            sent_log: VecDeque::new(),
            pause_until: 0.0,
            now: 0.0,
            retries: HashMap::new(),
            display_levels: Vec::new(),
            view3d: View3D::default(),
            slice_cache: None,
            curtain_metric: CurtainMetric::WindSupport,
            curtain_cache: None,
        };
        let _ = app.memory.set_zoom(5.0);
        app.memory.center_at(lon_lat(9.17, 47.67));
        app
    }

    /// Offline testing aids (synthetic winds, 3D tab) from environment variables.
    fn apply_test_env(&mut self) {
        let app = self;
        if std::env::var("WINGTRACK_SYNTHETIC_WIND").is_ok() {
            weather::synthetic_field(&mut app.store, &app.tracks);
            let n = weather::annotate(&mut app.tracks, &app.store, &app.opts);
            app.color_by = ColorBy::SegmentWind;
            app.status(format!(
                "SYNTHETIC test wind field loaded (not ERA5) - {n} fixes annotated"
            ));
            wind3d::synthetic_profiles(&mut app.profiles, &app.tracks);
            app.reset_display_levels();
            let n3 = wind3d::annotate(&mut app.tracks, &app.profiles);
            app.status(format!(
                "SYNTHETIC pressure-level winds (not real data) - {n3} fixes annotated"
            ));
            if let Ok(v) = std::env::var("WINGTRACK_3D") {
                app.view_tab = ViewTab::ThreeD;
                app.chart_var = if v == "profile" {
                    ChartVar::Profile
                } else {
                    ChartVar::Curtain
                };
                app.selected_track = Some(0);
                if v == "profile" {
                    app.t_now = app.t_min + 0.3 * (app.t_max - app.t_min);
                }
            }
        }
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
            self.add_file_from(
                name.to_string(),
                csv.into_bytes(),
                None,
                Some(FileOrigin::Demo),
            );
        }
    }

    fn add_file(&mut self, name: String, bytes: Vec<u8>, force: Option<&str>) {
        self.add_file_from(name, bytes, force, None);
    }

    /// Load a file; `origin` None keeps the origin of a same-named file (re-parse) or Local.
    fn add_file_from(
        &mut self,
        name: String,
        bytes: Vec<u8>,
        force: Option<&str>,
        origin: Option<FileOrigin>,
    ) {
        let origin = origin
            .or_else(|| {
                self.files
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| f.origin.clone())
            })
            .unwrap_or(FileOrigin::Local);
        if crate::acc::sniff(&name, &bytes) {
            return self.add_acc_file(name, bytes, origin);
        }
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
                    forced: force.map(|f| f.to_string()),
                    origin,
                    ranking,
                });
                self.packed_cache = None;
                self.recompute_time_range();
                if !self.store.is_empty() {
                    weather::annotate(&mut self.tracks, &self.store, &self.opts);
                }
                if !self.profiles.is_empty() {
                    wind3d::annotate(&mut self.tracks, &self.profiles);
                }
                self.selected = None;
                self.fit_pending = true;
                self.view3d.request_fit();
                self.curtain_cache = None;
            }
            Err(e) => self.status(format!("! {e}")),
        }
    }

    // ------------------------------------------------------------------ share links

    /// Changes whenever the set of loaded files changes (for the packed-data cache).
    fn files_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for f in &self.files {
            (
                &f.name,
                f.bytes.len(),
                &f.forced,
                f.origin == FileOrigin::Local,
            )
                .hash(&mut h);
        }
        h.finish()
    }

    /// Local files packed for embedding in a link (cached).
    fn packed_data(&mut self) -> String {
        let key = self.files_key();
        if let Some((k, s)) = &self.packed_cache
            && *k == key
        {
            return s.clone();
        }
        let files: Vec<share::EmbeddedFile> = self
            .files
            .iter()
            .filter(|f| f.origin == FileOrigin::Local)
            .map(|f| share::EmbeddedFile {
                name: f.name.clone(),
                firmware: f.forced.clone().unwrap_or_default(),
                bytes: f.bytes.clone(),
            })
            .collect();
        let s = if files.is_empty() {
            String::new()
        } else {
            share::pack_files(&files)
        };
        self.packed_cache = Some((key, s.clone()));
        s
    }

    /// The whole dashboard as link parameters. `embed`: include local data files.
    fn share_map(&mut self, embed: bool) -> ShareMap {
        use share::{name, r};
        let mut m = ShareMap::default();
        m.set("v", share::VERSION);
        // ---- data
        if self.files.iter().any(|f| f.origin == FileOrigin::Demo) {
            m.set("demo", "1");
        }
        let mut forced: Vec<String> = Vec::new();
        for f in &self.files {
            if let FileOrigin::Url(u) = &f.origin {
                m.set("data", u);
            }
            if f.origin != FileOrigin::Local
                && let Some(fw) = &f.forced
            {
                forced.push(format!("{}:{fw}", f.name));
            }
        }
        for fw in forced {
            m.set("fw", fw);
        }
        // acceleration files are re-fetched from their URL (local ones are not embedded:
        // they can be hundreds of MB)
        for f in &self.acc_files {
            if let FileOrigin::Url(u) = &f.origin {
                m.set("data", u);
            }
        }
        if embed {
            let packed = self.packed_data();
            m.set_if(!packed.is_empty(), "emb", packed);
        }
        // ---- view
        m.set(
            "tab",
            if self.view_tab == ViewTab::ThreeD {
                "3d"
            } else {
                "map"
            },
        );
        m.set("t", self.t_now.round() as i64);
        m.set("spd", self.speed_idx);
        if let Some((la, lo)) = self.map_center {
            m.set(
                "map",
                format!("{},{},{}", r(la, 4), r(lo, 4), r(self.memory.zoom(), 2)),
            );
        }
        m.set("base", name(&self.basemap));
        m.set_if(
            self.sat_on,
            "sat",
            format!(
                "{},{}",
                name(&self.sat_layer),
                r(self.sat_opacity as f64, 2)
            ),
        );
        if !self.wind_palettes.is_empty() {
            let mut p: Vec<String> = self
                .wind_palettes
                .iter()
                .map(|(b, w)| format!("{}:{}", name(b), name(w)))
                .collect();
            p.sort();
            m.set("pal", p.join(";"));
        }
        m.set("cb", name(&self.color_by));
        let flags = [
            (self.show_error, 'e'),
            (self.show_wind_arrows, 'a'),
            (self.show_wind_field, 'w'),
            (self.wind_field_night_only, 'n'),
            (self.skip_daytime, 'd'),
            (self.hide_insignificant, 'i'),
            (self.trail_to_cursor, 'c'),
            (self.follow, 'f'),
            (self.view3d.follow, 'F'),
        ];
        m.set(
            "disp",
            flags
                .iter()
                .filter(|x| x.0)
                .map(|x| x.1)
                .collect::<String>(),
        );
        m.set("err", r(self.max_error_km as f64, 2));
        m.set("chart", name(&self.chart_var));
        m.set("cm", name(&self.curtain_metric));
        m.set("co", self.chart_open as u8);
        // ---- weather
        let o = &self.opts;
        m.set(
            "wx",
            format!(
                "{},{},{},{},{},{},{}",
                name(&o.model),
                name(&o.level),
                o.segments as u8,
                o.night_only as u8,
                r(o.max_segment_h as f64, 1),
                r(o.field_margin_deg as f64, 2),
                o.field_stride
            ),
        );
        m.set_if(self.surface_field_on || !self.store.is_empty(), "era5", "1");
        // ---- 3D
        if let wind3d::Source::OpenMeteo(om) = self.profiles.source {
            m.set("w3d", name(&om));
            let annotated = self
                .tracks
                .iter()
                .any(|t| t.fixes.iter().any(|f| f.weather.flight.is_some()));
            m.set_if(annotated, "ann3d", "1");
        }
        m.set(
            "f3d",
            format!(
                "{},{},{}",
                self.field_auto as u8,
                self.field_step_cells,
                r(self.field_radius_deg as f64, 2)
            ),
        );
        if !self.display_levels.is_empty() {
            let lv: Vec<String> = self
                .display_levels
                .iter()
                .filter_map(|&i| self.profiles.levels.get(i))
                .map(|p| r(*p as f64, 0))
                .collect();
            m.set("lv", lv.join(","));
        }
        let v = &self.view3d;
        m.set("rl", v.raster_level);
        let c = &v.cam;
        m.set(
            "cam",
            format!(
                "{},{},{},{},{},{},{},{}{}",
                r(c.yaw as f64, 1),
                r(c.pitch as f64, 1),
                r(c.dist as f64, 1),
                r(c.target[0] as f64, 1),
                r(c.target[1] as f64, 1),
                r(c.target[2] as f64, 1),
                r(v.exag as f64, 1),
                v.auto_exag as u8,
                v.origin()
                    .map(|(la, lo)| format!(",{},{}", r(la, 5), r(lo, 5)))
                    .unwrap_or_default()
            ),
        );
        m.set(
            "v3",
            format!(
                "{},{},{},{},{},{},{},{},{},{},{}",
                v.particles_on as u8,
                v.particles_per_level,
                r(v.flow_hours_per_s as f64, 2),
                v.show_planes as u8,
                v.show_raster as u8,
                v.show_drop_lines as u8,
                v.show_shadows as u8,
                r(v.ground_dim as f64, 2),
                r(v.default_alt_m as f64, 0),
                r(v.gap_factor as f64, 2),
                v.terrain as u8
            ),
        );
        // ---- tracks
        let hidden: Vec<String> = (0..self.tracks.len())
            .filter(|&i| !self.tracks[i].visible)
            .map(|i| self.track_key(i))
            .collect();
        m.set_if(!hidden.is_empty(), "hide", hidden.join(","));
        let ranges: Vec<String> = (0..self.tracks.len())
            .filter_map(|i| {
                let (a, b) = self.tracks[i].date_filter?;
                Some(format!("{}:{a}-{b}", self.track_key(i)))
            })
            .collect();
        m.set_if(!ranges.is_empty(), "dr", ranges.join(";"));
        let ap = &self.acc_params;
        if !self.acc_sets.is_empty() || *ap != crate::acc::Params::default() {
            m.set(
                "acc",
                format!(
                    "{},{},{},{},{},{},{}",
                    r(ap.band.0 as f64, 2),
                    r(ap.band.1 as f64, 2),
                    r(ap.flight_g as f64, 3),
                    r(ap.min_peak_frac as f64, 2),
                    ap.k,
                    r(ap.window_s as f64, 2),
                    self.acc_slope
                        .map(|x| format!("{x}"))
                        .unwrap_or_else(|| "0".into())
                ),
            );
        }
        let accs: Vec<String> = self
            .acc_sets
            .iter()
            .map(|s| {
                format!(
                    "{}|{}|{}|{}",
                    Self::acc_key(s),
                    s.start_ms.map(|t| t.to_string()).unwrap_or_default(),
                    r(s.clock_rate * 1e6, 0),
                    s.track.clone().unwrap_or_default()
                )
            })
            .collect();
        m.set_if(!accs.is_empty(), "accs", accs.join(";"));
        let sp = &self.st_params;
        m.set_if(
            self.st_on,
            "st",
            format!(
                "{},{},{},{}",
                r(sp.radius_m as f64, 0),
                sp.include_other as u8,
                r(sp.min_other_height_m as f64, 0),
                self.st_highlight as u8
            ),
        );
        let cols: Vec<String> = (0..self.tracks.len())
            .map(|i| {
                let c = self.tracks[i].color;
                format!("{}:{:02x}{:02x}{:02x}", self.track_key(i), c[0], c[1], c[2])
            })
            .collect();
        m.set_if(!cols.is_empty(), "col", cols.join(";"));
        if let Some(i) = self.selected_track.filter(|&i| i < self.tracks.len()) {
            m.set("iso", self.track_key(i));
        }
        if let Some((ti, fi)) = self.selected
            && let Some(f) = self.tracks.get(ti).and_then(|t| t.fixes.get(fi))
        {
            m.set("sel", format!("{}:{}", self.track_key(ti), f.t));
        }
        m
    }

    /// Stable key of a track in links: its id, or `id@file` when the id occurs in several files.
    fn track_key(&self, i: usize) -> String {
        let t = &self.tracks[i];
        if self.tracks.iter().filter(|o| o.id == t.id).count() > 1 {
            format!("{}@{}", t.id, t.file)
        } else {
            t.id.clone()
        }
    }

    fn track_by_key(&self, key: &str) -> Option<usize> {
        (0..self.tracks.len()).find(|&i| self.track_key(i) == key)
    }

    /// Full link for the current view.
    fn share_url(&mut self, embed: bool) -> String {
        format!("{}#{}", page_base_url(), self.share_map(embed).encode())
    }

    /// Settings that don't depend on the data.
    fn apply_share_settings(&mut self, m: &ShareMap) {
        if let [lo, hi, fl, pk, k, win, slope, ..] = m.nums("acc")[..] {
            self.acc_params = crate::acc::Params {
                band: (lo as f32, hi as f32),
                flight_g: fl as f32,
                min_peak_frac: pk as f32,
                k: (k as usize).min(8),
                window_s: win as f32,
                ..Default::default()
            };
            self.acc_edit = self.acc_params;
            self.acc_slope = (slope > 0.0).then_some(slope);
        }
        let stv = m.nums("st");
        self.st_on = m.get("st").is_some();
        if let [radius, other, minh, hl, ..] = stv[..] {
            self.st_params.radius_m = (radius as f32).clamp(10.0, 50_000.0);
            self.st_params.include_other = other != 0.0;
            self.st_params.min_other_height_m = minh as f32;
            self.st_highlight = hl != 0.0;
        }
        use share::from_name;
        if let Some(t) = m.get("tab") {
            self.view_tab = if t == "3d" {
                ViewTab::ThreeD
            } else {
                ViewTab::Map
            };
        }
        if let Some(v) = m.f64("spd") {
            self.speed_idx = (v as usize).min(SPEEDS.len() - 1);
        }
        if let Some(b) = m.get("base").and_then(|b| from_name(&Basemap::ALL, b)) {
            self.basemap = b;
        }
        self.sat_on = false;
        if let Some(s) = m.get("sat") {
            let mut it = s.split(',');
            if let Some(l) = it.next().and_then(|l| from_name(&SatLayer::ALL, l)) {
                self.sat_on = true;
                self.sat_layer = l;
            }
            if let Some(o) = it.next().and_then(|o| o.parse::<f32>().ok()) {
                self.sat_opacity = o.clamp(0.1, 1.0);
            }
        }
        if let Some(p) = m.get("pal") {
            for kv in p.split(';') {
                if let Some((b, w)) = kv.split_once(':')
                    && let (Some(b), Some(w)) =
                        (from_name(&Basemap::ALL, b), from_name(&WindPalette::ALL, w))
                {
                    self.wind_palettes.insert(b, w);
                }
            }
        }
        if let Some(c) = m.get("cb").and_then(|c| from_name(&ColorBy::ALL, c)) {
            self.color_by = c;
        }
        if let Some(d) = m.get("disp") {
            let has = |c: char| d.contains(c);
            self.show_error = has('e');
            self.show_wind_arrows = has('a');
            self.show_wind_field = has('w');
            self.wind_field_night_only = has('n');
            self.skip_daytime = has('d');
            self.hide_insignificant = has('i');
            self.trail_to_cursor = has('c');
            self.follow = has('f');
            self.view3d.follow = has('F');
        }
        if let Some(e) = m.f64("err") {
            self.max_error_km = e as f32;
        }
        if let Some(c) = m.get("chart").and_then(|c| from_name(&ChartVar::ALL, c)) {
            self.chart_var = c;
        }
        if let Some(c) = m.get("cm").and_then(|c| from_name(&CurtainMetric::ALL, c)) {
            self.curtain_metric = c;
        }
        if let Some(c) = m.flag("co") {
            self.chart_open = c;
        }
        if let Some(w) = m.get("wx") {
            let p: Vec<&str> = w.split(',').collect();
            let num = |i: usize| p.get(i).and_then(|v| v.parse::<f32>().ok());
            if let Some(v) = p
                .first()
                .and_then(|v| from_name(&[Model::Era5, Model::BestMatch], v))
            {
                self.opts.model = v;
            }
            if let Some(v) = p
                .get(1)
                .and_then(|v| from_name(&[WindLevel::Auto, WindLevel::M10, WindLevel::M100], v))
            {
                self.opts.level = v;
            }
            if let Some(v) = num(2) {
                self.opts.segments = v != 0.0;
            }
            if let Some(v) = num(3) {
                self.opts.night_only = v != 0.0;
            }
            if let Some(v) = num(4) {
                self.opts.max_segment_h = v;
            }
            if let Some(v) = num(5) {
                self.opts.field_margin_deg = v;
            }
            if let Some(v) = num(6) {
                self.opts.field_stride = (v as i32).max(1);
            }
        }
        let f3 = m.nums("f3d");
        if let Some(&a) = f3.first() {
            self.field_auto = a != 0.0;
        }
        if let Some(&st) = f3.get(1).filter(|v| v.is_finite()) {
            self.field_step_cells = (st as i32).clamp(1, 8);
            self.profiles.node_step = self.field_step_cells;
        }
        if let Some(&rad) = f3.get(2).filter(|v| v.is_finite()) {
            self.field_radius_deg = rad as f32;
        }
        if let Some(om) = m.get("w3d").and_then(|o| from_name(&OmModel::ALL, o)) {
            self.om_model = om;
        }
        let v3 = m.nums("v3");
        let g = |i: usize| v3.get(i).copied().filter(|v| v.is_finite());
        let v = &mut self.view3d;
        if let Some(x) = g(0) {
            v.particles_on = x != 0.0;
        }
        if let Some(x) = g(1) {
            v.particles_per_level = (x as usize).clamp(100, 2500);
        }
        if let Some(x) = g(2) {
            v.flow_hours_per_s = x as f32;
        }
        if let Some(x) = g(3) {
            v.show_planes = x != 0.0;
        }
        if let Some(x) = g(4) {
            v.show_raster = x != 0.0;
        }
        if let Some(x) = g(5) {
            v.show_drop_lines = x != 0.0;
        }
        if let Some(x) = g(6) {
            v.show_shadows = x != 0.0;
        }
        if let Some(x) = g(7) {
            v.ground_dim = x as f32;
        }
        if let Some(x) = g(8) {
            v.default_alt_m = x as f32;
        }
        if let Some(x) = g(9) {
            v.gap_factor = x as f32;
        }
        v.terrain = g(10).is_some_and(|x| x != 0.0);
        if let Some(x) = m.f64("rl") {
            v.raster_level = x as usize;
        }
    }

    /// Track-dependent state, applied once the link's data has loaded.
    fn apply_share_view(&mut self, m: &ShareMap) {
        if let Some(v) = m.get("accs") {
            for e in v.split(';') {
                let p: Vec<&str> = e.split('|').collect();
                if p.len() < 4 {
                    continue;
                }
                if let Some(s) = self.acc_sets.iter_mut().find(|s| Self::acc_key(s) == p[0]) {
                    s.start_ms = p[1].parse().ok();
                    s.clock_rate = p[2].parse::<f64>().unwrap_or(0.0) / 1e6;
                    s.track = Some(p[3].to_string()).filter(|t| !t.is_empty());
                    if s.start_ms.is_some() {
                        let k = Self::acc_key(s);
                        self.acc_start_guessed.remove(&k);
                        self.acc_start_text.remove(&k);
                    }
                }
            }
            self.acc_cache = None;
        }
        // Date filters first: the selected fix below is looked up in the filtered fixes.
        let mut changed = Vec::new();
        let wanted: HashMap<usize, (i64, i64)> = m
            .get("dr")
            .map(|d| {
                d.split(';')
                    .filter_map(|kv| {
                        let (key, r) = kv.rsplit_once(':')?;
                        let (a, b) = r.split_once('-')?;
                        Some((self.track_by_key(key)?, (a.parse().ok()?, b.parse().ok()?)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for i in 0..self.tracks.len() {
            let want = wanted.get(&i).copied();
            if self.tracks[i].date_filter != want {
                self.tracks[i].set_date_filter(want);
                changed.push(i);
            }
        }
        if !changed.is_empty() {
            self.recompute_time_range();
            self.curtain_cache = None;
        }
        if let Some(h) = m.get("hide") {
            let hidden: Vec<&str> = h.split(',').collect();
            for i in 0..self.tracks.len() {
                let key = self.track_key(i);
                self.tracks[i].visible = !hidden.contains(&key.as_str());
            }
        }
        if let Some(c) = m.get("col") {
            for kv in c.split(';') {
                if let Some((key, hex)) = kv.rsplit_once(':')
                    && hex.len() == 6
                    && let Ok(v) = u32::from_str_radix(hex, 16)
                    && let Some(i) = self.track_by_key(key)
                {
                    self.tracks[i].color = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
                }
            }
        }
        self.selected_track = m.get("iso").and_then(|k| self.track_by_key(k));
        self.selected = m.get("sel").and_then(|s| {
            let (key, t) = s.rsplit_once(':')?;
            let t: i64 = t.parse().ok()?;
            let ti = self.track_by_key(key)?;
            let fi = self.tracks[ti].fixes.iter().position(|f| f.t == t)?;
            Some((ti, fi))
        });
        if let Some(t) = m.f64("t") {
            self.t_now = t.clamp(self.t_min, self.t_max);
        }
        let mp = m.nums("map");
        if mp.len() == 3 && mp.iter().all(|v| v.is_finite()) {
            let _ = self.memory.set_zoom(mp[2]);
            self.memory.center_at(lon_lat(mp[1], mp[0]));
            self.fit_pending = false;
        }
        let c = m.nums("cam");
        if c.len() >= 6 && c[..6].iter().all(|v| v.is_finite()) {
            let cam = crate::view3d::Camera {
                yaw: c[0] as f32,
                pitch: c[1] as f32,
                dist: c[2] as f32,
                target: [c[3] as f32, c[4] as f32, c[5] as f32],
                ..Default::default()
            };
            let exag = c
                .get(6)
                .copied()
                .filter(|v| v.is_finite())
                .map(|v| v as f32);
            let auto = c.get(7).map(|v| *v != 0.0);
            let origin = match (c.get(8), c.get(9)) {
                (Some(la), Some(lo)) if la.is_finite() && lo.is_finite() => Some((*la, *lo)),
                _ => None,
            };
            self.view3d.set_camera_after_fit(cam, exag, auto, origin);
        }
        // Weather: re-download what the sender had.
        if m.flag("era5") == Some(true) {
            self.start_weather();
        }
        if m.get("w3d").is_some() {
            self.ensure_open_meteo();
            if let Some(lv) = m.get("lv") {
                let want: Vec<f32> = lv.split(',').filter_map(|x| x.parse().ok()).collect();
                let idx: Vec<usize> = (0..self.profiles.levels.len())
                    .filter(|&i| {
                        want.iter()
                            .any(|w| (w - self.profiles.levels[i]).abs() < 0.5)
                    })
                    .collect();
                if !idx.is_empty() {
                    self.display_levels = idx;
                    self.slice_cache = None;
                }
            }
            if m.flag("ann3d") == Some(true) {
                self.start_profiles();
            }
        }
        if let Some(x) = m.f64("rl") {
            self.view3d.raster_level = x as usize;
        }
        self.status("Opened a shared view");
    }

    /// Restore a shared link: settings now, data (demo / embedded / URLs), then the view.
    fn start_from_share(&mut self, m: &ShareMap) {
        self.apply_share_settings(m);
        let forced: HashMap<String, String> = m
            .get_all("fw")
            .filter_map(|s| s.rsplit_once(':'))
            .map(|(n, f)| (n.to_string(), f.to_string()))
            .collect();
        if m.flag("demo") == Some(true) {
            for (name, csv) in crate::demo::all() {
                let fw = forced.get(name).map(|s| s.as_str());
                self.add_file_from(
                    name.to_string(),
                    csv.into_bytes(),
                    fw,
                    Some(FileOrigin::Demo),
                );
            }
        }
        if let Some(e) = m.get("emb") {
            match share::unpack_files(e) {
                Ok(files) => {
                    for f in files {
                        let fw = (!f.firmware.is_empty()).then_some(f.firmware.as_str());
                        self.add_file_from(
                            f.name.clone(),
                            f.bytes.clone(),
                            fw,
                            Some(FileOrigin::Local),
                        );
                    }
                }
                Err(err) => self.status(format!("! shared link: {err}")),
            }
        }
        let urls: Vec<String> = m.get_all("data").map(|s| s.to_string()).collect();
        for u in &urls {
            let fw = forced.get(&share::file_name_from_url(u)).cloned();
            self.fetch_url(u.clone(), fw, true);
        }
        if urls.is_empty() {
            self.apply_share_view(m);
        } else {
            self.status(format!(
                "Shared link: downloading {} data file(s)…",
                urls.len()
            ));
            self.pending_share = Some((m.clone(), urls.len()));
        }
    }

    fn share_file_arrived(&mut self) {
        if let Some((_, n)) = &mut self.pending_share {
            *n = n.saturating_sub(1);
            if *n == 0
                && let Some((m, _)) = self.pending_share.take()
            {
                self.apply_share_view(&m);
            }
        }
    }

    /// Download a CSV from a URL into the app.
    fn fetch_url(&self, url: String, force: Option<String>, from_share: bool) {
        // Dropbox / GitHub page links → direct downloads (also fixes older shared links).
        let url = share::direct_download_url(&url);
        let tx = self.tx.clone();
        ehttp::fetch(ehttp::Request::get(&url), move |res| {
            let msg = match res {
                Ok(r) if r.ok && share::looks_like_html(&r.bytes) => Msg::FetchFailed {
                    url,
                    err: "got a web page instead of a CSV - use a direct download link \
                          (Dropbox and GitHub page links are converted automatically; \
                          Google Drive and OneDrive share links can't be read by browsers)"
                        .into(),
                    from_share,
                },
                Ok(r) if r.ok => Msg::RemoteFile {
                    url,
                    bytes: r.bytes,
                    force,
                    from_share,
                },
                Ok(r) => Msg::FetchFailed {
                    url,
                    err: format!("HTTP {} {}", r.status, r.status_text),
                    from_share,
                },
                Err(e) => Msg::FetchFailed {
                    url,
                    err: format!(
                        "{e} (the server may not allow downloads from other sites - CORS)"
                    ),
                    from_share,
                },
            };
            let _ = tx.send(msg);
        });
    }

    /// Keep the browser's address bar in sync with the view (like a map app), at most once a second.
    fn sync_url_hash(&mut self) {
        #[cfg(target_arch = "wasm32")]
        {
            if self.pending_share.is_some()
                || self.tracks.is_empty()
                || self.now - self.hash_synced_at < 1.0
            {
                return;
            }
            self.hash_synced_at = self.now;
            // Embed data in the live address only while it stays short enough for every browser.
            let embed = self.packed_data().len() <= 60_000;
            let frag = self.share_map(embed).encode();
            if frag != self.last_hash {
                if let Some(h) = web_sys::window().and_then(|w| w.history().ok()) {
                    let _ = h.replace_state_with_url(
                        &wasm_bindgen::JsValue::NULL,
                        "",
                        Some(&format!("#{frag}")),
                    );
                }
                self.last_hash = frag;
            }
        }
    }

    fn share_window(&mut self, ctx: &egui::Context) {
        if !self.share_dialog {
            return;
        }
        let has_local = self.files.iter().any(|f| f.origin == FileOrigin::Local);
        let has_url = self
            .files
            .iter()
            .any(|f| matches!(f.origin, FileOrigin::Url(_)));
        let packed_len = if has_local {
            self.packed_data().len()
        } else {
            0
        };
        let embed = self.share_embed && has_local;
        let url = self.share_url(embed);
        let cube = matches!(
            self.profiles.source,
            wind3d::Source::File(_) | wind3d::Source::Synthetic
        );
        let mut open = true;
        let mut copied = false;
        egui::Window::new("Share this view")
            .open(&mut open)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(
                    "Anyone opening this link sees the same data, time, map or 3D view and settings. \
                     Weather is downloaded again on their side.",
                );
                if has_local {
                    ui.checkbox(
                        &mut self.share_embed,
                        format!("Include my data files in the link ({:.0} KB)", packed_len as f32 / 1024.0),
                    );
                    if !self.share_embed {
                        ui.colored_label(
                            Color32::from_rgb(255, 180, 80),
                            "Without the data the link only restores the view for people who open the same files.",
                        );
                    } else if packed_len > 60_000 {
                        ui.colored_label(
                            Color32::from_rgb(255, 180, 80),
                            "Long link: fine in browsers and email, but some chat apps cut long links. \
                             Put the CSV online (e.g. GitHub) and use File → Open URL… for a short link.",
                        );
                    }
                }
                if has_url {
                    ui.weak("Files opened from a web address are referenced, not copied into the link.");
                }
                if cube {
                    ui.weak("The imported ERA5 cube / test field is not included; the link uses Open-Meteo winds.");
                }
                let mut text = url.as_str();
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace),
                );
                ui.horizontal(|ui| {
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(url.clone());
                        copied = true;
                    }
                    ui.weak(format!("{} characters", url.len()));
                });
            });
        if copied {
            self.status("Share link copied to the clipboard");
        }
        self.share_dialog = open;
    }

    fn url_window(&mut self, ctx: &egui::Context) {
        if !self.url_dialog {
            return;
        }
        let mut open = true;
        let mut go = false;
        egui::Window::new("Open CSV from a web address")
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.label("Link to a CSV file: a Dropbox share link (\"Copy link\"), a GitHub file, or any direct download address.");
                let r = ui.add(egui::TextEdit::singleline(&mut self.url_input).desired_width(f32::INFINITY));
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    go = true;
                }
                ui.weak("Dropbox and GitHub links work as copied. Other servers must allow downloads from other sites (CORS); Google Drive and OneDrive share links don't.");
                if ui.button("Load").clicked() {
                    go = true;
                }
            });
        let url = self.url_input.trim().to_string();
        if go && url.starts_with("http") {
            self.status(format!("Downloading {url}…"));
            self.fetch_url(url, None, false);
            open = false;
        }
        self.url_dialog = open;
    }

    /// Cut the after-fall-off tail of every visible track (via its date filter, so it can be
    /// undone and travels in share links).
    fn remove_fallen_tags(&mut self) {
        let mut cut = Vec::new();
        let mut kept = 0;
        for i in 0..self.tracks.len() {
            if !self.tracks[i].visible {
                continue;
            }
            let tr = &self.tracks[i];
            match crate::falloff::detect(tr, &self.falloff) {
                Some(f) => {
                    let n = tr.fixes.iter().filter(|x| x.t >= f.cut_t).count();
                    let start = tr
                        .date_filter
                        .map(|r| r.0)
                        .or(tr.full_time_range().map(|r| r.0))
                        .unwrap_or(f.cut_t);
                    if n == 0 || f.cut_t <= start {
                        kept += 1;
                        continue;
                    }
                    self.tracks[i].set_date_filter(Some((start, f.cut_t - 1)));
                    cut.push(format!(
                        "{} ({} fixes from {}, {})",
                        self.tracks[i].id,
                        n,
                        fmt_time(f.cut_t),
                        f.signal
                    ));
                }
                None => kept += 1,
            }
        }
        // refresh time range, selection and caches
        self.set_date_filter(&[], None);
        if cut.is_empty() {
            self.status(format!(
                "No fallen tags found among {kept} visible track(s)"
            ));
        } else {
            self.status(format!(
                "Removed fallen-tag tails from {} track(s): {}. {kept} track(s) unchanged.",
                cut.len(),
                cut.join("; ")
            ));
        }
    }

    /// Restrict tracks `idxs` to `range` (or restore all data), keeping the time cursor.
    fn set_date_filter(&mut self, idxs: &[usize], range: Option<(i64, i64)>) {
        for &i in idxs {
            self.tracks[i].set_date_filter(range);
        }
        let t = self.t_now;
        self.recompute_time_range();
        self.t_now = t.clamp(self.t_min, self.t_max);
        self.selected = None; // fix indices changed
        self.curtain_cache = None;
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
                .add_filter("Tracking & acceleration data", &["csv", "txt", "tsv", "gz"])
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
                .add_filter("Tracking & acceleration data", &["csv", "txt", "tsv", "gz"])
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
        let mut tracks: Vec<Track> = Vec::new();
        for (ti, tr) in self.tracks.iter().enumerate().filter(|(_, t)| t.visible) {
            let mut tr = tr.clone();
            if self.acc_cache.is_some() {
                let k = self.acc_params.k;
                for f in tr.fixes.iter_mut() {
                    let a = f.acc.as_deref();
                    let o = |v: Option<f32>, d: usize| {
                        v.map(|x| format!("{x:.d$}")).unwrap_or_default()
                    };
                    f.extra.push((
                        "acc_n_bursts".into(),
                        a.map(|a| a.n.to_string()).unwrap_or_default(),
                    ));
                    f.extra
                        .push(("acc_flight_frac".into(), o(a.map(|a| a.flight_frac), 3)));
                    f.extra
                        .push(("acc_dyn_sd_g".into(), o(a.map(|a| a.dyn_sd), 4)));
                    f.extra
                        .push(("acc_vedba_g".into(), o(a.map(|a| a.vedba), 4)));
                    f.extra
                        .push(("acc_wbf_hz".into(), o(a.and_then(|a| a.wbf_hz), 2)));
                    f.extra
                        .push(("acc_pitch_deg".into(), o(a.map(|a| a.pitch_deg), 1)));
                    if k >= 2 {
                        f.extra.push((
                            "acc_cluster".into(),
                            a.and_then(|a| a.cluster)
                                .map(|c| format!("C{}", c + 1))
                                .unwrap_or_default(),
                        ));
                        for c in 0..k {
                            f.extra.push((
                                format!("acc_frac_C{}", c + 1),
                                o(a.and_then(|a| a.cluster_frac.get(c).copied()), 3),
                            ));
                        }
                    }
                }
            }
            if let Some(c) = self.st_cache.as_ref().filter(|_| self.st_on) {
                let r = format!("{:.0}", c.a.params.radius_m);
                for (fi, f) in tr.fixes.iter_mut().enumerate() {
                    let (nt, no, near) =
                        c.a.per_fix.get(&(ti, fi)).copied().unwrap_or((0, 0, None));
                    f.extra.push(("structure_radius_m".into(), r.clone()));
                    f.extra
                        .push(("turbines_within_radius".into(), nt.to_string()));
                    f.extra
                        .push(("other_structures_within_radius".into(), no.to_string()));
                    f.extra.push((
                        "nearest_turbine_m".into(),
                        near.map(|d| format!("{d:.0}")).unwrap_or_default(),
                    ));
                }
            }
            tracks.push(tr);
        }
        let csv = crate::export::to_csv(tracks.iter());
        self.save_csv("wingtrack_annotated.csv", csv);
    }

    /// Save a CSV (file dialog on desktop, download in the browser).
    fn save_csv(&mut self, name: &str, csv: String) {
        let name = name.to_string();
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
            let msg = match web_download(&name, csv.as_bytes(), "text/csv") {
                Ok(()) => format!("Downloaded {name}"),
                Err(e) => format!("! download failed: {e}"),
            };
            self.status(msg);
        }
    }

    // ------------------------------------------------------------------ acceleration

    fn acc_read_opts(&self) -> crate::acc::ReadOptions {
        crate::acc::ReadOptions {
            eobs_slope: self.acc_slope,
            window_s: self.acc_params.window_s as f64,
        }
    }

    fn acc_key(s: &crate::acc::AccSet) -> String {
        format!("{}@{}", s.id, s.file)
    }

    fn add_acc_file(&mut self, name: String, bytes: Vec<u8>, origin: FileOrigin) {
        match crate::acc::read(&name, &bytes, &self.acc_read_opts()) {
            Ok(sets) => {
                let n: usize = sets.iter().map(|s| s.bursts.len()).sum();
                let ids: Vec<String> = sets.iter().map(|s| s.id.clone()).collect();
                let fmt = sets[0].format.label();
                self.acc_sets.retain(|s| s.file != name);
                self.acc_sets.extend(sets);
                self.acc_files.retain(|f| f.name != name);
                self.acc_files.push(AccFile {
                    name: name.clone(),
                    bytes,
                    origin,
                });
                self.acc_cache = None;
                self.status(format!(
                    "Loaded acceleration {name} ({fmt}): {} animal(s) [{}], {n} bursts / windows",
                    ids.len(),
                    ids.join(", ")
                ));
            }
            Err(e) => self.status(format!("! {name}: {e}")),
        }
    }

    /// Re-read every ACC file (window length or e-obs slope changed), keeping each set's
    /// track, start time and clock rate.
    fn reread_acc(&mut self) {
        let keep: HashMap<String, (Option<String>, Option<i64>, f64)> = self
            .acc_sets
            .iter()
            .map(|s| {
                (
                    Self::acc_key(s),
                    (s.track.clone(), s.start_ms, s.clock_rate),
                )
            })
            .collect();
        let opts = self.acc_read_opts();
        let mut sets = Vec::new();
        for f in &self.acc_files {
            if let Ok(v) = crate::acc::read(&f.name, &f.bytes, &opts) {
                sets.extend(v);
            }
        }
        for s in &mut sets {
            if let Some((t, st, cr)) = keep.get(&Self::acc_key(s)) {
                s.track = t.clone();
                s.start_ms = *st;
                s.clock_rate = *cr;
            }
        }
        self.acc_sets = sets;
        self.acc_cache = None;
    }

    /// Track for an ACC id: exact match on the track id or tag, else one containing the other.
    fn match_acc_track(&self, id: &str) -> Option<usize> {
        let norm = |s: &str| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        };
        let a = norm(id);
        if a.is_empty() {
            return None;
        }
        let ids: Vec<String> = self.tracks.iter().map(|t| norm(&t.id)).collect();
        ids.iter().position(|t| *t == a).or_else(|| {
            ids.iter().position(|t| {
                (a.len() >= 4 && t.contains(&a)) || (t.len() >= 4 && a.contains(t.as_str()))
            })
        })
    }

    /// Match sets to tracks, anchor FleaTag clocks, compute features and annotate fixes.
    fn ensure_acc_cache(&mut self) {
        if self.acc_sets.is_empty() {
            if self.acc_cache.take().is_some() {
                for tr in &mut self.tracks {
                    tr.fixes.iter_mut().for_each(|f| f.acc = None);
                }
            }
            return;
        }
        // auto-match unassigned sets once tracks exist
        for i in 0..self.acc_sets.len() {
            if self.acc_sets[i].track.is_none()
                && let Some(ti) = self.match_acc_track(&self.acc_sets[i].id)
            {
                self.acc_sets[i].track = Some(self.track_key(ti));
            }
            // FleaTag without a start: the track's first fix, flagged
            let s = &self.acc_sets[i];
            if s.relative
                && s.start_ms.is_none()
                && let Some(ti) = s.track.as_deref().and_then(|k| self.track_by_key(k))
                && let Some((t0, _)) = self.tracks[ti].full_time_range()
            {
                let key = Self::acc_key(s);
                self.acc_sets[i].start_ms = Some(t0 * 1000);
                self.acc_start_guessed.insert(key);
            }
        }
        use std::hash::{Hash, Hasher};
        let key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            let p = &self.acc_params;
            (
                p.band.0.to_bits(),
                p.band.1.to_bits(),
                p.flight_g.to_bits(),
                p.min_peak_frac.to_bits(),
                p.k,
            )
                .hash(&mut h);
            for s in &self.acc_sets {
                (
                    &s.id,
                    &s.file,
                    s.bursts.len(),
                    s.start_ms,
                    s.clock_rate.to_bits(),
                )
                    .hash(&mut h);
            }
            h.finish()
        };
        let anno_key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (key, self.visible_key(), self.tracks.len()).hash(&mut h);
            for s in &self.acc_sets {
                s.track.hash(&mut h);
            }
            h.finish()
        };
        if self
            .acc_cache
            .as_ref()
            .is_some_and(|c| c.anno_key == anno_key)
        {
            return;
        }
        let a = match self.acc_cache.take() {
            Some(c) if c.key == key => c.a,
            _ => crate::acc::analyse(&self.acc_sets, &self.acc_params),
        };
        let keys: Vec<String> = (0..self.tracks.len()).map(|i| self.track_key(i)).collect();
        for (tr, tk) in self.tracks.iter_mut().zip(keys) {
            tr.fixes.iter_mut().for_each(|f| f.acc = None);
            let sets: Vec<usize> = (0..self.acc_sets.len())
                .filter(|&i| self.acc_sets[i].track.as_deref() == Some(tk.as_str()))
                .collect();
            if sets.is_empty() {
                continue;
            }
            for (fi, fa) in crate::acc::summarise_track(tr, &sets, &a) {
                tr.fixes[fi].acc = Some(Box::new(fa));
            }
        }
        self.acc_cache = Some(AccCache { key, a, anno_key });
    }

    fn acc_panel(&mut self, ui: &mut egui::Ui) {
        use crate::acc::{BAND_PRESETS, Format};
        if self.acc_sets.is_empty() {
            ui.weak(
                "Open acceleration files like any other: Movebank e-obs bursts (eobs:accelerations-raw), \
                 Movebank / Technosmart / any per-sample x-y-z table, or FleaTag .txt / .csv exports. \
                 They are matched to tracks by id and summarised per fix.",
            );
            return;
        }
        // ---- datasets
        let tracks: Vec<(String, String)> = (0..self.tracks.len())
            .map(|i| (self.track_key(i), self.tracks[i].id.clone()))
            .collect();
        let mut remove = None;
        for i in 0..self.acc_sets.len() {
            let key = Self::acc_key(&self.acc_sets[i]);
            let guessed = self.acc_start_guessed.contains(&key);
            let s = &mut self.acc_sets[i];
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.strong(&s.id);
                let sr = s.bursts.first().map(|b| b.sr).unwrap_or(0.0);
                ui.weak(format!(
                    "{} · {} bursts · {sr:.0} Hz · {}",
                    s.format.label(),
                    s.bursts.len(),
                    s.file
                ));
                if ui
                    .small_button("✕")
                    .on_hover_text("Remove this animal's ACC data")
                    .clicked()
                {
                    remove = Some(i);
                }
            });
            ui.horizontal(|ui| {
                ui.label("track");
                let cur = s
                    .track
                    .as_ref()
                    .and_then(|k| tracks.iter().find(|t| &t.0 == k))
                    .map(|t| t.1.clone())
                    .unwrap_or_else(|| "(none)".into());
                egui::ComboBox::from_id_salt(("acc_trk", i))
                    .selected_text(cur)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut s.track, None, "(none)");
                        for (k, name) in &tracks {
                            ui.selectable_value(&mut s.track, Some(k.clone()), name);
                        }
                    });
            });
            if s.format == Format::FleaTag {
                let txt = self
                    .acc_start_text
                    .entry(key.clone())
                    .or_insert_with(|| s.start_ms.map(|t| fmt_time(t / 1000)).unwrap_or_default());
                ui.horizontal(|ui| {
                    ui.label("tag start (UTC)");
                    let r = ui.add(
                        egui::TextEdit::singleline(txt)
                            .desired_width(150.0)
                            .hint_text("YYYY-MM-DD HH:MM:SS"),
                    );
                    if r.lost_focus()
                        && let Some(t) = crate::table::parse_time_ms(txt)
                    {
                        s.start_ms = Some(t);
                        self.acc_start_guessed.remove(&key);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("clock rate");
                    let mut pct = s.clock_rate * 100.0;
                    if ui
                        .add(egui::DragValue::new(&mut pct).speed(0.05).range(-10.0..=10.0).suffix(" %"))
                        .on_hover_text("Real elapsed time = tag time × (1 + rate); FleaTags run ~1–2 % off (see hummingbird_activity_budget.R)")
                        .changed()
                    {
                        s.clock_rate = pct / 100.0;
                    }
                });
                if guessed {
                    ui.colored_label(
                        Color32::from_rgb(255, 190, 90),
                        "FleaTags have no clock - start set to the track's first fix; enter the real tag start",
                    );
                }
            }
        }
        if let Some(i) = remove {
            let s = self.acc_sets.remove(i);
            if !self.acc_sets.iter().any(|x| x.file == s.file) {
                self.acc_files.retain(|f| f.name != s.file);
            }
            self.acc_cache = None;
        }
        ui.separator();
        // ---- settings (applied with a button)
        let e = &mut self.acc_edit;
        ui.horizontal_wrapped(|ui| {
            ui.label("wingbeat band");
            egui::ComboBox::from_id_salt("acc_band")
                .selected_text("preset")
                .show_ui(ui, |ui| {
                    for (n, b) in BAND_PRESETS {
                        if ui.selectable_label(e.band == b, n).clicked() {
                            e.band = b;
                        }
                    }
                });
            ui.add(
                egui::DragValue::new(&mut e.band.0)
                    .speed(0.2)
                    .range(0.5..=200.0)
                    .suffix(" Hz"),
            );
            ui.label("–");
            ui.add(
                egui::DragValue::new(&mut e.band.1)
                    .speed(0.2)
                    .range(1.0..=250.0)
                    .suffix(" Hz"),
            );
        });
        ui.add(egui::Slider::new(&mut e.flight_g, 0.05..=5.0).logarithmic(true).text("flight: dynamic SD > (g)"))
            .on_hover_text("Bursts are flight above this (flea: 1 g for hummingbird bursts; bats on e-obs tags often 0.3–0.8 g)");
        ui.add(
            egui::Slider::new(&mut e.min_peak_frac, 0.0..=0.8).text("… and wingbeat peak share ≥"),
        )
        .on_hover_text("Also require a clear rhythmic peak in the band (0 = off)");
        ui.add(egui::Slider::new(&mut e.k, 0..=8).text("behaviour clusters (k-means, 0 = off)"));
        ui.add(
            egui::Slider::new(&mut e.window_s, 0.3..=10.0)
                .logarithmic(true)
                .suffix(" s")
                .text("window for continuous data"),
        );
        ui.horizontal(|ui| {
            let mut ov = self.acc_slope.is_some();
            if ui
                .checkbox(&mut ov, "e-obs g per count")
                .on_hover_text(
                    "Default by tag generation: ≤2241 → 0.001, ≤4117 → 0.0022, newer → 1/512",
                )
                .changed()
            {
                self.acc_slope = if ov { Some(1.0 / 512.0) } else { None };
                self.reread_acc();
            }
            if let Some(v) = self.acc_slope.as_mut()
                && ui
                    .add(egui::DragValue::new(v).speed(0.0001).range(0.0001..=0.05))
                    .lost_focus()
            {
                self.reread_acc();
            }
        });
        let changed = self.acc_edit != self.acc_params;
        if ui
            .add_enabled(changed, egui::Button::new("Apply"))
            .clicked()
        {
            let rewindow = self.acc_edit.window_s != self.acc_params.window_s;
            self.acc_params = self.acc_edit;
            if rewindow {
                self.reread_acc();
            }
        }
        // ---- clusters
        let Some(c) = &self.acc_cache else { return };
        let total: usize = c.a.per_set.iter().map(|v| v.len()).sum();
        let flight: usize = c.a.per_set.iter().flatten().filter(|x| x.1).count();
        ui.label(format!(
            "{total} bursts · {:.0} % flight",
            100.0 * flight as f32 / total.max(1) as f32
        ));
        if c.a.params.k >= 2 {
            egui::Grid::new("acc_clusters")
                .striped(true)
                .show(ui, |ui| {
                    for h in ["cluster", "bursts", "dyn SD", "wingbeat", "pitch", "flight"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for (i, ci) in c.a.clusters.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let (r, _) =
                                ui.allocate_exact_size(vec2(10.0, 10.0), egui::Sense::hover());
                            ui.painter().rect_filled(
                                r,
                                2.0,
                                CLUSTER_COLORS[i % CLUSTER_COLORS.len()],
                            );
                            ui.label(format!("C{}", i + 1));
                        });
                        ui.label(ci.n.to_string());
                        ui.label(format!("{:.2} g", ci.dyn_sd));
                        ui.label(
                            ci.wbf_hz
                                .map(|w| format!("{w:.1} Hz"))
                                .unwrap_or("-".into()),
                        );
                        ui.label(format!("{:.0}°", ci.pitch_deg));
                        ui.label(format!("{:.0} %", ci.flight_frac * 100.0));
                        ui.end_row();
                    }
                });
            ui.weak("Clusters are numbered from calmest (C1) to most dynamic; name them from these profiles.");
        }
        if ui
            .button("export burst features CSV")
            .on_hover_text(
                "One row per burst: all features, flight flag, cluster and the matched track",
            )
            .clicked()
        {
            let csv = {
                let tr_of = |si: usize| {
                    self.acc_sets[si]
                        .track
                        .as_deref()
                        .and_then(|k| self.track_by_key(k))
                        .map(|i| self.tracks[i].id.clone())
                        .unwrap_or_default()
                };
                crate::acc::bursts_csv(&self.acc_sets, &c.a, tr_of)
            };
            self.save_csv("wingtrack_acc_bursts.csv", csv);
        }
        ui.weak("Per-fix summaries (acc_* columns) are added to the annotated CSV export.");
    }

    /// Waveform and spectrum of the burst nearest the cursor for the focused track.
    fn acc_burst_chart(&mut self, ui: &mut Ui) {
        let Some(c) = &self.acc_cache else {
            ui.weak("Load acceleration data to see bursts.");
            return;
        };
        let focus = self
            .selected_track
            .or(self.selected.map(|s| s.0))
            .or_else(|| self.tracks.iter().position(|t| t.visible));
        let tk = focus.map(|i| self.track_key(i));
        let t_ms = (self.t_now * 1000.0) as i64;
        let mut best: Option<(i64, usize, usize)> = None;
        for (si, s) in self.acc_sets.iter().enumerate() {
            if tk.is_some() && s.track != tk {
                continue;
            }
            for (bi, (f, _, _)) in c.a.per_set[si].iter().enumerate() {
                let d = (f.t_ms - t_ms).abs();
                if best.is_none_or(|b| d < b.0) {
                    best = Some((d, si, bi));
                }
            }
        }
        let Some((_, si, bi)) = best else {
            ui.weak("No ACC data for this track.");
            return;
        };
        // per_set holds only bursts with absolute times, in order: map back by time
        let (f, fl, cl) = &c.a.per_set[si][bi];
        let set = &self.acc_sets[si];
        let Some(b) = set.bursts.iter().find(|b| set.abs_ms(b) == Some(f.t_ms)) else {
            return;
        };
        ui.label(format!(
            "{} · {} · {:.2} s · dyn SD {:.2} g · wingbeat {} · {}{}",
            set.id,
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(f.t_ms)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                .unwrap_or_default(),
            f.dur_s,
            f.dyn_sd,
            f.wbf_hz
                .map(|w| format!("{w:.1} Hz (peak {:.0} %)", f.peak_frac * 100.0))
                .unwrap_or("-".into()),
            if *fl { "flight" } else { "not flight" },
            if c.a.params.k >= 2 {
                format!(" · C{}", cl + 1)
            } else {
                String::new()
            }
        ));
        let sr = b.sr as f64;
        let axes: Vec<Vec<[f64; 2]>> = (0..3)
            .map(|k| {
                b.xyz
                    .iter()
                    .enumerate()
                    .map(|(i, v)| [i as f64 / sr * 1000.0, v[k] as f64])
                    .collect()
            })
            .collect();
        let dyn_: Vec<[f64; 3]> = b
            .xyz
            .iter()
            .map(|v| {
                [
                    (v[0] - f.mean[0]) as f64,
                    (v[1] - f.mean[1]) as f64,
                    (v[2] - f.mean[2]) as f64,
                ]
            })
            .collect();
        let (fr, pw) = crate::acc::spectrum(&dyn_, sr);
        let spec: Vec<[f64; 2]> = fr
            .iter()
            .zip(&pw)
            .map(|(x, p)| [*x, p[0] + p[1] + p[2]])
            .collect();
        let band = c.a.params.band;
        let w = ui.available_width();
        ui.horizontal(|ui| {
            Plot::new("acc_wave")
                .width(w * 0.6)
                .legend_if(!self.narrow)
                .x_axis_label("ms")
                .y_axis_label("g")
                .allow_scroll(false)
                .show(ui, |pui| {
                    for (k, (name, col)) in [
                        ("x", Color32::from_rgb(230, 97, 1)),
                        ("y", Color32::from_rgb(94, 60, 153)),
                        ("z", Color32::from_rgb(77, 175, 74)),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        pui.line(
                            Line::new(name, PlotPoints::from(axes[k].clone()))
                                .color(col)
                                .width(1.3),
                        );
                    }
                });
            Plot::new("acc_spec")
                .width(w * 0.38)
                .x_axis_label("Hz")
                .allow_scroll(false)
                .show(ui, |pui| {
                    pui.line(
                        Line::new("power (x+y+z)", PlotPoints::from(spec))
                            .color(Color32::from_gray(220)),
                    );
                    pui.vline(
                        egui_plot::VLine::new("band", band.0 as f64).color(Color32::from_gray(90)),
                    );
                    pui.vline(
                        egui_plot::VLine::new("band", band.1 as f64).color(Color32::from_gray(90)),
                    );
                    if let Some(w) = f.wbf_hz {
                        pui.vline(
                            egui_plot::VLine::new("wingbeat", w as f64)
                                .color(Color32::from_rgb(255, 160, 40)),
                        );
                    }
                });
        });
    }

    // ------------------------------------------------------------------ structures

    /// Queue OpenStreetMap downloads for the corridor around the visible tracks.
    fn plan_structures(&mut self) {
        if !self.st_on || self.tracks.is_empty() {
            return;
        }
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (self.visible_key(), self.st_params.radius_m.to_bits()).hash(&mut h);
            h.finish()
        };
        if self.st_planned == Some(key) {
            return;
        }
        self.st_planned = Some(key);
        let buffer = (self.st_params.radius_m as f64).max(2000.0);
        let queued: HashSet<crate::structures::Tile> =
            self.st_queue.iter().flatten().copied().collect();
        let tiles: Vec<crate::structures::Tile> =
            crate::structures::corridor_tiles(&self.tracks, buffer)
                .into_iter()
                .filter(|t| !self.st_store.tiles.contains(t) && !queued.contains(t))
                .collect();
        if tiles.is_empty() {
            return;
        }
        let n = tiles.len();
        for c in tiles.chunks(crate::structures::TILES_PER_QUERY) {
            self.st_queue.push_back(c.to_vec());
        }
        self.status(format!(
            "OpenStreetMap: wind turbines & tall structures for {n} tile(s) of {}° along the tracks",
            crate::structures::TILE_DEG
        ));
    }

    /// One Overpass request at a time (the public servers allow very few per IP).
    fn pump_structures(&mut self, ctx: &egui::Context) {
        if self.st_busy || self.st_queue.is_empty() {
            return;
        }
        if self.now < self.st_next_at {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
            return;
        }
        let Some(tiles) = self.st_queue.pop_front() else {
            return;
        };
        let server =
            crate::structures::OVERPASS[self.st_server % crate::structures::OVERPASS.len()];
        let mut req = crate::structures::overpass_request(server, &tiles, true);
        req.timeout = Some(std::time::Duration::from_secs(200));
        self.st_busy = true;
        let (tx, ctx2) = (self.tx.clone(), ctx.clone());
        ehttp::fetch(req, move |res| {
            let result = match res {
                Ok(r) if r.ok => r
                    .text()
                    .map(str::to_string)
                    .ok_or_else(|| "non-UTF8 body".into()),
                Ok(r) => Err(format!(
                    "HTTP {} {}",
                    r.status,
                    r.text()
                        .map(|t| t.chars().take(300).collect::<String>())
                        .unwrap_or_default()
                )),
                Err(e) => Err(e),
            };
            let _ = tx.send(Msg::Overpass { tiles, result });
            ctx2.request_repaint();
        });
    }

    fn on_overpass(&mut self, tiles: Vec<crate::structures::Tile>, result: Result<String, String>) {
        self.st_busy = false;
        match result.and_then(|b| crate::structures::parse_overpass(&b)) {
            Ok(v) => {
                let n = self.st_store.add(v);
                self.st_store.tiles.extend(tiles.iter().copied());
                if self.st_queue.is_empty() {
                    self.status(format!(
                        "OpenStreetMap structures loaded: {} in total (+{n})",
                        self.st_store.items.len()
                    ));
                }
            }
            Err(e) => {
                let tries = self.st_retry.entry(tiles.clone()).or_insert(0);
                *tries += 1;
                if *tries <= 2 {
                    // busy / timed out: other server, short pause, retry
                    self.st_server += 1;
                    self.st_next_at = self.now + 10.0 * *tries as f64;
                    self.st_queue.push_front(tiles);
                    self.status(format!("Overpass: {e} - retrying"));
                } else {
                    self.st_failed += tiles.len();
                    self.st_store.tiles.extend(tiles.iter().copied());
                    self.status(format!(
                        "! Overpass failed for {} tile(s): {e}",
                        tiles.len()
                    ));
                }
            }
        }
    }

    /// Recompute encounters when tracks, structures, annotations or parameters change.
    fn ensure_st_cache(&mut self) {
        if !self.st_on || self.st_store.is_empty() {
            self.st_cache = None;
            return;
        }
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            let p = &self.st_params;
            (
                self.visible_key(),
                self.st_store.version,
                p.radius_m.to_bits(),
                p.min_other_height_m.to_bits(),
                p.include_other,
            )
                .hash(&mut h);
            // annotations (surface pressure → height above ground)
            let annotated: usize = self
                .tracks
                .iter()
                .map(|t| {
                    t.fixes
                        .iter()
                        .filter(|f| f.weather.surface_pressure_hpa.is_some())
                        .count()
                })
                .sum();
            annotated.hash(&mut h);
            h.finish()
        };
        if self.st_cache.as_ref().is_some_and(|c| c.key == key) {
            return;
        }
        let a = crate::structures::analyse(&self.tracks, &self.st_store, &self.st_params);
        let hits = a.encounters.iter().map(|e| e.structure).collect();
        let near = a
            .per_fix
            .iter()
            .filter(|(_, v)| v.0 > 0)
            .map(|(k, _)| *k)
            .collect();
        self.st_cache = Some(StCache { key, a, hits, near });
    }

    fn open_structures_dialog(&self, ctx: &egui::Context) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(move || {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("Structure / turbine table", &["csv", "txt", "gz"])
                .pick_file()
            {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                match std::fs::read(&p) {
                    Ok(bytes) => {
                        let _ = tx.send(Msg::StructFile { name, bytes });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Status(format!("! {}: {e}", p.display())));
                    }
                }
                ctx.request_repaint();
            }
        });
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter("Structure / turbine table", &["csv", "txt", "gz"])
                .pick_file()
                .await
            {
                let bytes = f.read().await;
                let _ = tx.send(Msg::StructFile {
                    name: f.file_name(),
                    bytes,
                });
                ctx.request_repaint();
            }
        });
    }

    fn structures_panel(&mut self, ui: &mut egui::Ui) {
        use crate::structures::{self as st, Kind};
        ui.checkbox(&mut self.st_on, "show & analyse")
            .on_hover_text("Download wind turbines and tall structures along the tracks from OpenStreetMap and count encounters");
        if !self.st_on {
            ui.weak("Wind turbines and masts / towers / chimneys from OpenStreetMap, or a turbine register you import.");
            return;
        }
        let items = &self.st_store.items;
        let turb = items.iter().filter(|s| s.kind == Kind::WindTurbine).count();
        let with_dims = items
            .iter()
            .filter(|s| s.kind == Kind::WindTurbine && !s.dims().3)
            .count();
        ui.label(format!(
            "{turb} wind turbines ({with_dims} with hub height & rotor size) · {} other structures",
            items.len() - turb
        ));
        if !self.st_queue.is_empty() || self.st_busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(format!(
                    "OpenStreetMap: {} request(s) to go",
                    self.st_queue.len() + self.st_busy as usize
                ));
            });
        }
        if self.st_failed > 0 {
            ui.colored_label(
                Color32::from_rgb(255, 180, 80),
                format!(
                    "{} tile(s) failed to download - 'reload' retries",
                    self.st_failed
                ),
            );
        }
        let p = &mut self.st_params;
        ui.add(
            egui::Slider::new(&mut p.radius_m, 100.0..=20_000.0)
                .logarithmic(true)
                .suffix(" m")
                .text("encounter radius"),
        )
        .on_hover_text("A structure counts as encountered when a step (straight line between fixes) passes within this distance. Sigfox location errors are km-scale: see location_error_m in the export.");
        ui.horizontal(|ui| {
            ui.checkbox(&mut p.include_other, "masts, towers, chimneys");
            ui.add_enabled(
                p.include_other,
                egui::Slider::new(&mut p.min_other_height_m, 0.0..=300.0)
                    .suffix(" m")
                    .text("min. height"),
            );
        });
        ui.checkbox(&mut self.st_highlight, "highlight steps near turbines");
        ui.horizontal_wrapped(|ui| {
            if ui
                .button("Import register CSV…")
                .on_hover_text("Any table with latitude/longitude (+ height, hub_height, rotor_diameter): Marktstammdatenregister export, US Wind Turbine Database, a structures CSV from WingTrack…")
                .clicked()
            {
                self.open_structures_dialog(ui.ctx());
            }
            if ui.button("reload").on_hover_text("Forget downloaded tiles and fetch again").clicked() {
                self.st_store.clear();
                self.st_failed = 0;
                self.st_retry.clear();
                self.st_planned = None;
                self.st_queue.clear();
            }
        });
        let Some(c) = &self.st_cache else {
            return;
        };
        let sums = st::summaries(&self.tracks, &self.st_store, &c.a);
        let total: usize = c.hits.len();
        ui.label(format!(
            "{total} structure(s) within {:.0} m of the tracks",
            c.a.params.radius_m
        ));
        egui::ScrollArea::vertical().max_height(180.0).id_salt("st_sum").show(ui, |ui| {
            egui::Grid::new("st_grid").striped(true).show(ui, |ui| {
                ui.strong("track");
                ui.strong("turbines");
                ui.strong("/100 km");
                ui.strong("nearest");
                ui.strong("rotor zone").on_hover_text("Encounters with height above ground (tag pressure vs ERA5 surface pressure, needs Annotate) inside the rotor-swept band");
                ui.end_row();
                for sm in &sums {
                    ui.label(&self.tracks[sm.track].id);
                    ui.label(format!("{}{}", sm.turbines, if sm.other > 0 { format!(" (+{} other)", sm.other) } else { String::new() }));
                    ui.label(format!("{:.1}", sm.per_100km));
                    ui.label(sm.nearest_turbine_m.map(|d| format!("{d:.0} m")).unwrap_or("-".into()));
                    ui.label(if sm.with_agl > 0 { format!("{}/{}", sm.in_rotor_zone, sm.with_agl) } else { "-".into() });
                    ui.end_row();
                }
            });
        });
        let mut save: Option<(&str, String)> = None;
        ui.horizontal_wrapped(|ui| {
            ui.label("export:");
            if ui.button("encounters").on_hover_text("One row per track × structure within the radius: distance, time, altitude, rotor zone, structure metadata").clicked() {
                save = Some(("wingtrack_encounters.csv", st::encounters_csv(&self.tracks, &self.st_store, &c.a)));
            }
            if ui.button("per-track summary").clicked() {
                save = Some(("wingtrack_structure_summary.csv", st::summary_csv(&self.tracks, &self.st_store, &c.a)));
            }
            if ui.button("structures").on_hover_text("All loaded turbines / structures (re-importable)").clicked() {
                save = Some(("wingtrack_structures.csv", st::structures_csv(&self.st_store)));
            }
        });
        ui.weak("Per-fix counts (turbines_within_radius, nearest_turbine_m) are added to the annotated CSV export. Structures © OpenStreetMap contributors (ODbL).");
        if let Some((name, csv)) = save {
            self.save_csv(name, csv);
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
        let pts = weather::sample_points(&self.tracks, &self.opts);
        // The wind field around the animals is fetched lazily for the days in view.
        self.surface_field_on = true;
        self.surface_field_days.clear();
        if self.era5_source == Era5Source::Cds {
            if self.cds_key.trim().is_empty() {
                self.status(
                    "! Copernicus CDS selected but no API key entered (ERA5 panel → data source)"
                        .to_string(),
                );
                return;
            }
            let jobs = crate::cds::plan(&pts, &self.store.days, weather::latest_available_day());
            if jobs.is_empty() {
                let n = weather::annotate(&mut self.tracks, &self.store, &self.opts);
                self.status(format!("ERA5: everything cached - annotated {n} fixes"));
                return;
            }
            self.status(format!(
                "ERA5 via Copernicus CDS: {} grid-cell job(s) for {} sample points (each takes ~10–60 s at CDS; {CDS_PARALLEL} at a time)",
                jobs.len(),
                pts.len()
            ));
            self.reset_counters_if_idle();
            self.req_total += jobs.len();
            self.cds_pending.extend(jobs);
            return;
        }
        self.maybe_request_surface_field();
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
        // Annotation first: wind-field windows queued earlier can wait.
        self.enqueue_front(reqs.into_iter().map(Job::Surface).collect());
    }

    fn downloads_idle(&self) -> bool {
        self.queue.is_empty()
            && self.in_flight.is_empty()
            && self.cds_pending.is_empty()
            && self.cds_running.is_empty()
    }

    fn reset_counters_if_idle(&mut self) {
        if self.downloads_idle() {
            self.req_total = 0;
            self.req_done = 0;
            self.req_failed = 0;
        }
    }

    fn enqueue(&mut self, jobs: Vec<Job>) {
        if jobs.is_empty() {
            return;
        }
        self.reset_counters_if_idle();
        self.req_total += jobs.len();
        self.queue.extend(jobs);
    }

    fn enqueue_front(&mut self, jobs: Vec<Job>) {
        if jobs.is_empty() {
            return;
        }
        self.reset_counters_if_idle();
        self.req_total += jobs.len();
        for j in jobs.into_iter().rev() {
            self.queue.push_front(j);
        }
    }

    // ------------------------------------------------------------------ 3D wind

    /// Default display levels: those closest to ~300, 800 and 1500 m (standard atmosphere).
    fn reset_display_levels(&mut self) {
        let lv = &self.profiles.levels;
        let mut out: Vec<usize> = Vec::new();
        for h in [300.0, 800.0, 1500.0] {
            if let Some(i) = (0..lv.len()).min_by(|&a, &b| {
                let ha = geo::pressure_to_altitude_m(lv[a] as f64, 1013.25);
                let hb = geo::pressure_to_altitude_m(lv[b] as f64, 1013.25);
                (ha - h).abs().total_cmp(&(hb - h).abs())
            }) && !out.contains(&i)
            {
                out.push(i);
            }
        }
        out.sort_unstable();
        self.display_levels = out;
        self.view3d.raster_level = 0;
        self.slice_cache = None;
        self.curtain_cache = None;
    }

    fn ensure_open_meteo(&mut self) {
        if !matches!(
            self.profiles.source,
            wind3d::Source::File(_) | wind3d::Source::Synthetic
        ) {
            let before = self.profiles.levels.len();
            self.profiles.use_open_meteo(self.om_model);
            self.profiles.node_step = self.field_step_cells;
            if before != self.profiles.levels.len() || self.display_levels.is_empty() {
                self.reset_display_levels();
            }
        }
    }

    /// Bounding box of the visible tracks plus a margin, limited to 30° × 40°.
    /// Box around where the visible animals are at the cursor time (held 12 h past the
    /// ends of each track), padded by the 3D field radius and limited to 30° × 40°.
    fn field_bbox(&self) -> Option<(f64, f64, f64, f64)> {
        let t = self.t_now;
        let (mut s, mut n, mut w, mut e) = (90.0f64, -90.0f64, 180.0f64, -180.0f64);
        let mut any = false;
        for tr in self.tracks.iter().filter(|t| t.visible) {
            let Some((t0, t1)) = tr.time_range() else {
                continue;
            };
            if t < t0 as f64 - 43_200.0 || t > t1 as f64 + 43_200.0 {
                continue;
            }
            if let Some((la, lo, _)) = tr.position_at(t.clamp(t0 as f64, t1 as f64)) {
                s = s.min(la);
                n = n.max(la);
                w = w.min(lo);
                e = e.max(lo);
                any = true;
            }
        }
        if !any {
            return None;
        }
        let m = self.field_radius_deg as f64;
        let (s, n, w, e) = (
            (s - m).max(-85.0),
            (n + m).min(85.0),
            (w - m).max(-180.0),
            (e + m).min(180.0),
        );
        let (cl, co) = ((s + n) / 2.0, (w + e) / 2.0);
        Some((
            s.max(cl - 15.0),
            n.min(cl + 15.0),
            w.max(co - 20.0),
            e.min(co + 20.0),
        ))
    }

    /// Distinct 3D field nodes needed for the week from the cursor day.
    fn field_node_count(&self) -> usize {
        let day = weather::day_of(self.t_now as i64);
        let pts = weather::window_points(
            &self.tracks,
            day,
            7,
            self.field_radius_deg as f64,
            self.profiles.node_step,
        );
        let mut cells: Vec<weather::CellKey> =
            pts.iter().map(|p| weather::cell_of(p.0, p.1)).collect();
        cells.sort_unstable();
        cells.dedup();
        cells.len()
    }

    fn start_profiles(&mut self) {
        self.ensure_open_meteo();
        if matches!(
            self.profiles.source,
            wind3d::Source::File(_) | wind3d::Source::Synthetic
        ) {
            let n = wind3d::annotate(&mut self.tracks, &self.profiles);
            self.status(format!("3D wind (file): annotated {n} fixes"));
            return;
        }
        let pts = wind3d::annotation_points(&self.tracks, true, self.opts.max_segment_h);
        let reqs = self.profiles.plan(&pts, wind3d::latest_available_day());
        if reqs.is_empty() {
            let n = wind3d::annotate(&mut self.tracks, &self.profiles);
            self.status(format!("3D wind: everything cached - annotated {n} fixes"));
            return;
        }
        self.status(format!(
            "3D wind: {} request(s) along the tracks ({} levels, {})",
            reqs.len(),
            self.profiles.levels.len(),
            self.profiles.source.label()
        ));
        self.enqueue(reqs.into_iter().map(Job::Profile).collect());
    }

    /// Key that changes when the set of visible tracks changes (field windows depend on it).
    fn visible_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (i, t) in self.tracks.iter().enumerate() {
            if t.visible {
                (i, &t.id, t.fixes.len()).hash(&mut h);
            }
        }
        h.finish()
    }

    /// Days to request next: the cursor day, or two days ahead if that is already covered,
    /// so playback prefetches. Returns the first day of a 7-day window not yet requested.
    fn next_window_day(&self, done: &HashSet<(i64, u64)>) -> Option<i64> {
        let key = self.visible_key();
        let day = weather::day_of(self.t_now as i64);
        [day, day + 2]
            .into_iter()
            .find(|d| !done.contains(&(*d, key)))
    }

    /// Lazily download the 3D field around the animals for the week from the cursor day
    /// (Open-Meteo only). A cell needed on several days costs one call (≤ 14 days).
    fn maybe_request_field(&mut self) {
        if !self.field_auto
            || self.tracks.is_empty()
            || matches!(
                self.profiles.source,
                wind3d::Source::File(_) | wind3d::Source::Synthetic
            )
            || self.queue.len() > 20
        {
            return;
        }
        self.ensure_open_meteo();
        let Some(day) = self.next_window_day(&self.field_days_requested) else {
            return;
        };
        let key = self.visible_key();
        for d in day..day + 7 {
            self.field_days_requested.insert((d, key));
        }
        let pts = weather::window_points(
            &self.tracks,
            day,
            7,
            self.field_radius_deg as f64,
            self.profiles.node_step,
        );
        let reqs = self.profiles.plan(&pts, wind3d::latest_available_day());
        if !reqs.is_empty() {
            let w: f32 = reqs.iter().map(|r| r.weight()).sum();
            self.status(format!(
                "3D wind around the animals, {} – {}: {} request(s), ≈{w:.0} API calls",
                weather::day_str(day),
                weather::day_str(day + 6),
                reqs.len()
            ));
            self.enqueue(reqs.into_iter().map(Job::Profile).collect());
        }
    }

    /// Same for the 2D (surface ERA5) wind field, once the user has annotated at least once.
    fn maybe_request_surface_field(&mut self) {
        if !self.surface_field_on
            || !self.show_wind_field
            || self.opts.field_margin_deg <= 0.0
            || self.tracks.is_empty()
            || self.queue.len() > 20
        {
            return;
        }
        let Some(day) = self.next_window_day(&self.surface_field_days) else {
            return;
        };
        let key = self.visible_key();
        for d in day..day + 7 {
            self.surface_field_days.insert((d, key));
        }
        let pts = weather::window_points(
            &self.tracks,
            day,
            7,
            self.opts.field_margin_deg as f64,
            self.opts.field_stride,
        );
        let reqs = self
            .store
            .plan(&pts, self.opts.model, weather::latest_available_day());
        if !reqs.is_empty() {
            let w: f32 = reqs.iter().map(|r| r.weight()).sum();
            self.status(format!(
                "ERA5 wind field around the animals, {} – {}: {} request(s), ≈{w:.0} API calls",
                weather::day_str(day),
                weather::day_str(day + 6),
                reqs.len()
            ));
            self.enqueue(reqs.into_iter().map(Job::Surface).collect());
        }
    }

    fn open_cube_dialog(&self, ctx: &egui::Context) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(move || {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("ERA5 wind cube", &["csv", "gz", "txt"])
                .pick_file()
            {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                match std::fs::read(&p) {
                    Ok(bytes) => {
                        let _ = tx.send(Msg::Cube { name, bytes });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Status(format!("! {}: {e}", p.display())));
                    }
                }
                ctx.request_repaint();
            }
        });
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter("ERA5 wind cube", &["csv", "gz", "txt"])
                .pick_file()
                .await
            {
                let bytes = f.read().await;
                let _ = tx.send(Msg::Cube {
                    name: f.file_name(),
                    bytes,
                });
                ctx.request_repaint();
            }
        });
    }

    fn current_slice(&mut self) -> Option<&FieldSlice> {
        let bbox = self.field_bbox()?;
        if self.profiles.is_empty() || self.display_levels.is_empty() {
            return None;
        }
        let q = |x: f64| (x * 4.0).round() as i32;
        let key: SliceKey = (
            (self.t_now as i64) / 300,
            self.profiles.generation,
            self.display_levels.clone(),
            (q(bbox.0), q(bbox.1), q(bbox.2), q(bbox.3)),
        );
        if self.slice_cache.as_ref().is_none_or(|c| c.0 != key) {
            let slice = FieldSlice::build(
                &self.profiles,
                &self.display_levels,
                bbox,
                self.t_now as i64,
            );
            self.slice_cache = Some((key, slice));
        }
        self.slice_cache.as_ref().and_then(|c| c.1.as_ref())
    }

    /// Track used by single-track charts: isolated track, else selected fix's track, else first visible.
    fn chart_track(&self) -> Option<usize> {
        self.selected_track
            .or(self.selected.map(|s| s.0))
            .or_else(|| self.tracks.iter().position(|t| t.visible))
    }

    fn pump_weather(&mut self, ctx: &egui::Context) {
        // Stay under Open-Meteo's free limits (600 calls/min, 5000/h) with some headroom.
        const PER_MIN: f32 = 450.0;
        const PER_HOUR: f32 = 4200.0;
        let now = self.now;
        while self.sent_log.front().is_some_and(|(t, _)| now - t > 3600.0) {
            self.sent_log.pop_front();
        }
        // Watchdog: a request that never answers would otherwise block the queue forever.
        let stale: Vec<usize> = self
            .in_flight_t
            .iter()
            .filter(|(_, t)| now - **t > OM_TIMEOUT_S)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.in_flight_t.remove(&id);
            if let Some(job) = self.in_flight.remove(&id) {
                let n = self.retries.entry(job.url().to_string()).or_insert(0);
                *n += 1;
                if *n <= 2 {
                    self.status(format!(
                        "Open-Meteo did not answer within {OM_TIMEOUT_S:.0} s - retrying"
                    ));
                    self.queue.push_front(job);
                } else {
                    self.req_done += 1;
                    self.req_failed += 1;
                    match &job {
                        Job::Surface(req) => self.store.mark_done(req),
                        Job::Profile(req) => self.profiles.mark_done(req),
                    }
                    self.status("! Open-Meteo request timed out repeatedly - skipped".to_string());
                    self.on_download_complete();
                }
            }
        }
        if !self.queue.is_empty() && now < self.om_blocked_until {
            let n = self.queue.len();
            self.queue.clear();
            self.req_done += n;
            self.req_failed += n;
            self.wait_note = None;
            self.on_download_complete();
            return;
        }
        if now < self.pause_until {
            self.wait_note = Some(format!(
                "Open-Meteo rate limit: resuming in {:.0} s",
                self.pause_until - now
            ));
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
            return;
        }
        self.wait_note = None;
        // A key raises the limits a lot; keep a (looser) budget anyway.
        let scale = if self.om_key.trim().is_empty() {
            1.0
        } else {
            10.0
        };
        while self.in_flight.len() < 3 {
            let Some(w) = self.queue.front().map(|j| j.weight()) else {
                break;
            };
            let minute: f32 = self
                .sent_log
                .iter()
                .filter(|(t, _)| now - t < 60.0)
                .map(|x| x.1)
                .sum();
            let hour: f32 = self.sent_log.iter().map(|x| x.1).sum();
            // A single request heavier than the budget may still go when nothing else is pending.
            let empty_min = minute == 0.0;
            if (minute + w > PER_MIN * scale && !empty_min) || hour + w > PER_HOUR * scale {
                self.wait_note = Some(if hour + w > PER_HOUR * scale {
                    format!(
                        "waiting for the hourly Open-Meteo budget ({hour:.0} of {:.0} calls used in the last hour; free limit 5 000/h per IP)",
                        PER_HOUR * scale
                    )
                } else {
                    format!("pacing requests ({minute:.0} calls in the last minute)")
                });
                ctx.request_repaint_after(std::time::Duration::from_secs(2));
                break;
            }
            let Some(req) = self.queue.pop_front() else {
                break;
            };
            self.sent_log.push_back((now, w));
            let id = self.next_req_id;
            self.next_req_id += 1;
            let tx = self.tx.clone();
            let ctx2 = ctx.clone();
            let mut http = ehttp::Request::get(om_url(req.url(), &self.om_key));
            http.timeout = Some(std::time::Duration::from_secs_f64(OM_TIMEOUT_S));
            self.in_flight_t.insert(id, now);
            ehttp::fetch(http, move |res| {
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
                Msg::RemoteFile {
                    url,
                    bytes,
                    force,
                    from_share,
                } => {
                    let name = share::file_name_from_url(&url);
                    self.add_file_from(name, bytes, force.as_deref(), Some(FileOrigin::Url(url)));
                    if from_share {
                        self.share_file_arrived();
                    }
                }
                Msg::FetchFailed {
                    url,
                    err,
                    from_share,
                } => {
                    self.status(format!("! could not download {url}: {err}"));
                    if from_share {
                        self.share_file_arrived();
                    }
                }
                Msg::Status(s) => self.status(s),
                Msg::Cube { name, bytes } => match self.profiles.import_cube(&name, &bytes) {
                    Ok(msg) => {
                        self.status(format!("Imported wind cube {msg}"));
                        self.field_days_requested.clear();
                        self.reset_display_levels();
                        let n = wind3d::annotate(&mut self.tracks, &self.profiles);
                        self.status(format!("3D wind (file): annotated {n} fixes"));
                    }
                    Err(e) => self.status(format!("! wind cube: {e}")),
                },
                Msg::Weather { id, result } => {
                    let Some(job) = self.in_flight.remove(&id) else {
                        continue;
                    };
                    self.in_flight_t.remove(&id);
                    // Rate limited: wait and retry instead of giving up on these cells.
                    if let Err(e) = &result
                        && (e.starts_with("HTTP 429") || e.to_lowercase().contains("too many"))
                    {
                        let el = e.to_lowercase();
                        if el.contains("daily") {
                            // Nothing will succeed before midnight UTC: stop instead of retrying.
                            let day = 86_400.0;
                            let wall = chrono::Utc::now().timestamp() as f64;
                            self.om_blocked_until = self.now + (day - wall.rem_euclid(day)) + 60.0;
                            self.queue.push_front(job);
                            self.status(
                                "! Open-Meteo daily limit (10 000 calls) reached for your IP address - it is shared by everyone on your network. \
                                 Remaining requests skipped. Try again after midnight UTC, enter an Open-Meteo API key, or switch the ERA5 source to Copernicus CDS."
                                    .to_string(),
                            );
                            continue;
                        }
                        if el.contains("hourly") {
                            let wall = chrono::Utc::now().timestamp() as f64;
                            let wait = 3600.0 - wall.rem_euclid(3600.0) + 30.0;
                            self.pause_until = self.now + wait;
                            self.queue.push_front(job);
                            self.status(format!(
                                "Open-Meteo hourly limit reached for your IP - waiting {:.0} min for the next hour",
                                wait / 60.0
                            ));
                            continue;
                        }
                        let n = self.retries.entry(job.url().to_string()).or_insert(0);
                        *n += 1;
                        if *n <= 4 {
                            let wait = 65.0 * *n as f64;
                            self.pause_until = self.now + wait;
                            self.status(format!(
                                "Open-Meteo rate limit reached - pausing {wait:.0} s, then retrying"
                            ));
                            self.queue.push_front(job);
                            continue;
                        }
                    }
                    self.req_done += 1;
                    let res = match &job {
                        Job::Surface(req) => result.and_then(|body| self.store.ingest(req, &body)),
                        Job::Profile(req) => {
                            result.and_then(|body| self.profiles.ingest(req, &body))
                        }
                    };
                    if let Err(e) = res {
                        self.req_failed += 1;
                        match &job {
                            Job::Surface(req) => self.store.mark_done(req),
                            Job::Profile(req) => self.profiles.mark_done(req),
                        }
                        self.status(format!("! weather request failed: {e}"));
                    }
                    self.on_download_complete();
                }
                Msg::Cds { run, result } => self.on_cds_response(run, result),
                Msg::Overpass { tiles, result } => self.on_overpass(tiles, result),
                Msg::StructFile { name, bytes } => {
                    match crate::table::Table::from_bytes(&name, &bytes)
                        .and_then(|t| crate::structures::from_table(&t))
                    {
                        Ok(v) => {
                            let n = v.len();
                            let new = self.st_store.add(v);
                            self.st_on = true;
                            self.status(format!(
                                "{name}: imported {n} structures ({new} new) - kept alongside OpenStreetMap"
                            ));
                        }
                        Err(e) => self.status(format!("! {name}: {e}")),
                    }
                }
            }
        }
    }

    /// After each finished download: annotate every few results and once at the end.
    fn on_download_complete(&mut self) {
        self.since_annotate += 1;
        let finished = self.downloads_idle();
        if finished || self.since_annotate >= 4 {
            let n = weather::annotate(&mut self.tracks, &self.store, &self.opts);
            let n3 = if self.profiles.is_empty() {
                0
            } else {
                wind3d::annotate(&mut self.tracks, &self.profiles)
            };
            self.since_annotate = 0;
            self.curtain_cache = None;
            if finished {
                self.status(format!(
                                "Downloads done: surface {} cells / {} h ({n} fixes), pressure levels {} cells / {} h ({n3} fixes){}",
                                self.store.cells.len(),
                                self.store.hours(),
                                self.profiles.cells.len(),
                                self.profiles.hours(),
                                if self.req_failed > 0 {
                                    format!(" ({} requests failed)", self.req_failed)
                                } else {
                                    String::new()
                                }
                            ));
            }
        }
    }

    // ------------------------------------------------------------------ Copernicus CDS

    /// Data source picker and API keys (keys are stored on this device only).
    fn era5_source_ui(&mut self, ui: &mut egui::Ui) {
        let before = self.era5_source;
        egui::ComboBox::from_label("ERA5 source")
            .selected_text(self.era5_source.label())
            .show_ui(ui, |ui| {
                for s in [Era5Source::OpenMeteo, Era5Source::Cds] {
                    ui.selectable_value(&mut self.era5_source, s, s.label());
                }
            });
        if before != self.era5_source {
            crate::prefs::save(
                "era5_source",
                if self.era5_source == Era5Source::Cds {
                    "cds"
                } else {
                    ""
                },
            );
        }
        let show = self.show_keys;
        let key_field = |ui: &mut egui::Ui, v: &mut String, hint: &str| {
            ui.add(
                egui::TextEdit::singleline(v)
                    .password(!show)
                    .hint_text(hint)
                    .desired_width(210.0),
            )
            .changed()
        };
        match self.era5_source {
            Era5Source::Cds => {
                ui.horizontal(|ui| {
                    ui.label("CDS API key");
                    if key_field(ui, &mut self.cds_key, "personal access token") {
                        crate::prefs::save("cds_key", self.cds_key.trim());
                    }
                    ui.toggle_value(&mut self.show_keys, "👁")
                        .on_hover_text("Show / hide the key");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.hyperlink_to("get your key", crate::cds::PROFILE_PAGE);
                    ui.label("·");
                    ui.hyperlink_to(
                        "accept the ERA5 time-series licence (once)",
                        crate::cds::DATASET_PAGE,
                    );
                });
                ui.weak(
                    "Uses your own CDS quota (ERA5 hourly time series, one job per 0.25° cell; ~10–60 s each). \
                     Point annotation only: wind-field arrows and 3D wind still come from Open-Meteo. \
                     This dataset has no 100 m wind or cloud cover, so wind support uses 10 m wind. \
                     The key is stored only on this device, never in share links.",
                );
            }
            Era5Source::OpenMeteo => {
                egui::CollapsingHeader::new("Open-Meteo API key (optional)").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if key_field(ui, &mut self.om_key, "paid-plan key") {
                            crate::prefs::save("open_meteo_key", self.om_key.trim());
                        }
                        ui.toggle_value(&mut self.show_keys, "👁");
                    });
                    ui.weak(
                        "Free use is limited per IP address (600/min, 5 000/h, 10 000/day), shared by everyone on \
                         your network. A key (open-meteo.com/en/pricing) switches to the customer servers. \
                         Stored on this device only.",
                    );
                });
            }
        }
    }

    /// Start queued CDS jobs and send the next request of each running one.
    fn pump_cds(&mut self, ctx: &egui::Context) {
        while self.cds_running.len() < CDS_PARALLEL
            && let Some(job) = self.cds_pending.pop_front()
        {
            self.cds_next_id += 1;
            self.cds_running.push(CdsRun {
                id: self.cds_next_id,
                job,
                stage: CdsStage::Submit,
                busy: false,
                next_at: 0.0,
                polls: 0,
            });
        }
        let now = self.now;
        let key = self.cds_key.clone();
        for r in self.cds_running.iter_mut().filter(|r| !r.busy) {
            if now < r.next_at {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(r.next_at - now));
                continue;
            }
            let req = match &r.stage {
                CdsStage::Submit => crate::cds::submit(&key, &r.job),
                CdsStage::Poll(id) => crate::cds::status(&key, id),
                CdsStage::Results(id) => crate::cds::results(&key, id),
                // the download link is pre-signed: no key header
                CdsStage::Download(href) => ehttp::Request::get(href),
            };
            r.busy = true;
            let (tx, ctx2, run) = (self.tx.clone(), ctx.clone(), r.id);
            ehttp::fetch(req, move |res| {
                let result = res.map(|r| (r.status, r.bytes));
                let _ = tx.send(Msg::Cds { run, result });
                ctx2.request_repaint();
            });
        }
    }

    fn on_cds_response(&mut self, run: u64, result: Result<(u16, Vec<u8>), String>) {
        let Some(i) = self.cds_running.iter().position(|r| r.id == run) else {
            return;
        };
        self.cds_running[i].busy = false;
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let step: Result<Option<CdsStage>, String> = match result {
            Err(e) => {
                if matches!(self.cds_running[i].stage, CdsStage::Submit) && self.req_done == 0 {
                    // First contact failed without any HTTP status: blocked or offline.
                    Err(format!(
                        "could not reach the Copernicus CDS ({e}). {}",
                        if cfg!(target_arch = "wasm32") {
                            "The browser may block calls to cds.climate.copernicus.eu from other web sites (CORS); the desktop app is not affected."
                        } else {
                            "Check the internet connection."
                        }
                    ))
                } else {
                    Err(e)
                }
            }
            Ok((401 | 403, b)) if matches!(self.cds_running[i].stage, CdsStage::Submit) => {
                let msg = crate::cds::error_text(&text(&b));
                // A bad key or missing licence fails every job: stop them all.
                let n = self.cds_pending.len() + self.cds_running.len() - 1;
                self.cds_pending.clear();
                self.cds_running.retain(|r| r.id == run);
                self.req_done += n;
                self.req_failed += n;
                Err(format!(
                    "CDS refused the request: {msg} (key from {})",
                    crate::cds::PROFILE_PAGE
                ))
            }
            Ok((s, b)) if !(200..300).contains(&s) => {
                Err(format!("HTTP {s}: {}", crate::cds::error_text(&text(&b))))
            }
            Ok((_, b)) => {
                let r = &mut self.cds_running[i];
                match r.stage.clone() {
                    CdsStage::Submit | CdsStage::Poll(_) => {
                        crate::cds::parse_job(&text(&b)).and_then(|(id, st)| match st {
                            crate::cds::JobState::Done => Ok(Some(CdsStage::Results(id))),
                            crate::cds::JobState::Failed(m) => Err(m),
                            crate::cds::JobState::Waiting => {
                                r.polls += 1;
                                // 3 s, then gradually up to 15 s between polls
                                r.next_at = self.now + (3.0 + r.polls as f64).min(15.0);
                                Ok(Some(CdsStage::Poll(id)))
                            }
                        })
                    }
                    CdsStage::Results(_) => {
                        crate::cds::parse_results(&text(&b)).map(|h| Some(CdsStage::Download(h)))
                    }
                    CdsStage::Download(_) => crate::cds::payload_csv(&b)
                        .and_then(|csv| crate::cds::parse_csv(&csv))
                        .map(|series| {
                            let job = r.job.clone();
                            self.store.cells.entry(job.cell).or_default().extend(series);
                            for d in job.start_day..=job.end_day {
                                self.store.days.insert((job.cell, d));
                            }
                            None
                        }),
                }
            }
        };
        match step {
            Ok(Some(next)) => self.cds_running[i].stage = next,
            Ok(None) | Err(_) => {
                let r = self.cds_running.remove(i);
                if let Err(e) = step {
                    self.req_failed += 1;
                    for d in r.job.start_day..=r.job.end_day {
                        self.store.days.insert((r.job.cell, d));
                    }
                    self.status(format!("! CDS: {e}"));
                }
                self.req_done += 1;
                self.on_download_complete();
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
        let narrow = self.narrow;
        ui.horizontal_wrapped(|ui| {
            ui.toggle_value(&mut self.side_open, "Panel")
                .on_hover_text("Show or hide the side panel");
            if !narrow {
                ui.strong("WingTrack");
            }
            ui.separator();
            ui.selectable_value(&mut self.view_tab, ViewTab::Map, "2D map");
            ui.selectable_value(&mut self.view_tab, ViewTab::ThreeD, "3D winds");
            ui.separator();
            if ui
                .add_enabled(!self.tracks.is_empty(), egui::Button::new("Share"))
                .on_hover_text(
                    "Link that opens exactly this view - data, time, map, 3D camera and settings",
                )
                .clicked()
            {
                self.share_dialog = true;
            }
            if narrow {
                ui.menu_button("File", |ui| self.file_actions(ui));
                ui.menu_button("Layers", |ui| self.layer_actions(ui));
            } else {
                self.file_actions(ui);
                ui.separator();
                self.layer_actions(ui);
            }
        });
    }

    /// Open / demo / export / clear (inline on wide screens, a "File" menu on phones).
    fn file_actions(&mut self, ui: &mut Ui) {
        if ui
            .button("Open…")
            .on_hover_text("CSV from Wildcloud, Movebank, CTT or R")
            .clicked()
        {
            self.open_dialog(ui.ctx());
        }
        if ui
            .button("Load demo")
            .on_hover_text("Simulated NanoFox, TinyFox and CTT GPS files")
            .clicked()
        {
            self.load_demo();
        }
        if ui
            .add_enabled(
                !self.tracks.is_empty(),
                egui::Button::new("Export annotated CSV"),
            )
            .clicked()
        {
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
        if ui
            .add_enabled(!self.tracks.is_empty(), egui::Button::new("Clear"))
            .clicked()
        {
            self.tracks.clear();
            self.files.clear();
            self.acc_files.clear();
            self.acc_sets.clear();
            self.selected = None;
            self.selected_track = None;
            self.packed_cache = None;
        }
        if ui
            .button("Open URL…")
            .on_hover_text("Load a CSV from a web address (e.g. a GitHub raw link) - shared links then reference it instead of embedding it")
            .clicked()
        {
            self.url_dialog = true;
        }
    }

    /// Basemap and daily-satellite controls (inline on wide screens, a "Layers" menu on phones).
    fn layer_actions(&mut self, ui: &mut Ui) {
        egui::ComboBox::from_id_salt("basemap")
            .selected_text(self.basemap.label())
            .show_ui(ui, |ui| {
                for b in Basemap::ALL {
                    ui.selectable_value(&mut self.basemap, b, b.label());
                }
            });
        ui.checkbox(&mut self.sat_on, "Daily satellite").on_hover_text(
            "NASA GIBS imagery for the date under the time cursor - see cloud fronts the animals flew through",
        );
        if self.sat_on {
            egui::ComboBox::from_id_salt("sat")
                .selected_text(self.sat_layer.label())
                .show_ui(ui, |ui| {
                    for l in SatLayer::ALL {
                        ui.selectable_value(&mut self.sat_layer, l, l.label());
                    }
                });
            ui.add(egui::Slider::new(&mut self.sat_opacity, 0.1..=1.0).text("opacity"));
        }
    }

    fn side_panel(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // ---- files & firmware
            egui::CollapsingHeader::new(format!("Files & firmware ({})", self.files.len())).default_open(true).show(ui, |ui| {
                if self.files.is_empty() {
                    ui.weak("No files yet. Supported: Wildcloud TinyFox / NanoFox 30Days / FineScalePressure, Movebank Sigfox & GPS, CTT GPS, any lat/lon/time CSV - plus acceleration (Movebank e-obs, Technosmart, FleaTag).");
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
                    let mut cut = false;
                    let mut restore = false;
                    ui.horizontal(|ui| {
                        cut = ui
                            .button("✂ Remove fallen tags")
                            .on_hover_text(
                                "For every visible track, cut the end where the tag kept transmitting after falling off: \
                                 after the last active message, ≥ N messages with flat activity (TinyFox 24 h activity %, \
                                 NanoFox VeDBA; distance moved for other tags) up to the end. Same rule as \
                                 detect_tag_fell_off() in SigfoxTagPrep; a tag_fell_off column in the file is used directly. \
                                 Undo with 📅 → clear, or 'restore'.",
                            )
                            .clicked();
                        let p = &mut self.falloff;
                        ui.menu_button("⚙", |ui| {
                            ui.add(egui::Slider::new(&mut p.min_inactive_run, 1..=12).text("min. inactive messages"));
                            ui.add(egui::Slider::new(&mut p.activity_threshold, 0.0..=10.0).text("TinyFox activity % >"));
                            ui.add(egui::Slider::new(&mut p.vedba_threshold, 0.0..=500.0).text("NanoFox VeDBA >"));
                            ui.add(egui::Slider::new(&mut p.dist_threshold_km, 0.1..=10.0).text("other tags: moved > km"));
                            ui.checkbox(&mut p.keep_first_inactive, "keep the first inactive message");
                            ui.checkbox(&mut p.use_file_flag, "use the file's tag_fell_off column when present")
                                .on_hover_text("Movebank exports from SigfoxTagPrep carry detect_tag_fell_off() results");
                            if ui.button("defaults").clicked() {
                                *p = crate::falloff::Params::default();
                            }
                        });
                        if self.tracks.iter().any(|t| t.date_filter.is_some()) {
                            restore = ui
                                .small_button("restore")
                                .on_hover_text("Clear every date filter / fall-off cut")
                                .clicked();
                        }
                    });
                    if cut {
                        self.remove_fallen_tags();
                    }
                    if restore {
                        let all: Vec<usize> = (0..self.tracks.len()).collect();
                        self.set_date_filter(&all, None);
                        self.status("Restored all fixes (date filters cleared)".to_string());
                    }
                    ui.separator();
                }
                let mut zoom_to = None;
                let mut date_click = None;
                let mut date_action: Option<DateAction> = None;
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
                        let filtered = tr.date_filter.is_some();
                        let btn = egui::Button::new(if filtered { "📅 filtered" } else { "📅" })
                            .small()
                            .selected(filtered || self.date_edit == Some(i));
                        if ui.add(btn).on_hover_text("Keep only fixes inside a date range").clicked() {
                            date_click = Some(i);
                        }
                    });
                    let range = tr.time_range().map(|(a, b)| format!("{} to {}", fmt_date(a), fmt_date(b))).unwrap_or_default();
                    let hidden = if tr.excluded_fixes.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} outside the date range)", tr.excluded_fixes.len())
                    };
                    ui.weak(format!(
                        "   {} · {} fixes{hidden} · {}{}",
                        tr.tag,
                        tr.fixes.len(),
                        range,
                        tr.species.as_ref().map(|s| format!(" · {s}")).unwrap_or_default()
                    ));
                    if self.date_edit == Some(i) {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("from");
                                ui.add(egui::TextEdit::singleline(&mut self.date_from).desired_width(130.0).hint_text("YYYY-MM-DD [HH:MM]"));
                            });
                            ui.horizontal(|ui| {
                                ui.label("to   ");
                                ui.add(egui::TextEdit::singleline(&mut self.date_to).desired_width(130.0).hint_text("YYYY-MM-DD [HH:MM]"));
                            });
                            let range = parse_date_range(&self.date_from, &self.date_to);
                            if let Err(e) = &range {
                                ui.colored_label(egui::Color32::from_rgb(230, 120, 100), e);
                            }
                            ui.horizontal(|ui| {
                                if ui.add_enabled(range.is_ok(), egui::Button::new("apply")).clicked() {
                                    date_action = Some((vec![i], range.clone().ok()));
                                }
                                if ui
                                    .add_enabled(range.is_ok(), egui::Button::new("apply to all visible"))
                                    .on_hover_text("Use this range for every visible track (e.g. one season)")
                                    .clicked()
                                {
                                    date_action = Some((Vec::new(), range.clone().ok()));
                                }
                                if ui.add_enabled(tr.date_filter.is_some(), egui::Button::new("clear")).clicked() {
                                    date_action = Some((vec![i], None));
                                }
                            });
                            ui.weak("Dates are UTC; a bare date covers the whole day. Fixes and sensor data outside the range are set aside, not deleted.");
                        });
                    }
                }
                if let Some(i) = date_click {
                    if self.date_edit == Some(i) {
                        self.date_edit = None;
                    } else {
                        let tr = &self.tracks[i];
                        let (a, b) = tr.date_filter.or(tr.full_time_range()).unwrap_or((0, 0));
                        self.date_from = fmt_minute(a);
                        self.date_to = fmt_minute(b);
                        self.date_edit = Some(i);
                    }
                }
                if let Some((idxs, range)) = date_action {
                    let idxs = if idxs.is_empty() {
                        (0..self.tracks.len()).filter(|&j| self.tracks[j].visible).collect()
                    } else {
                        idxs
                    };
                    self.set_date_filter(&idxs, range);
                    self.date_edit = None;
                }
                if let Some(i) = zoom_to {
                    self.selected_track = Some(i);
                    self.fit_pending = true;
                    self.view3d.request_fit();
                    if self.narrow {
                        self.side_open = false; // show the map on phones
                    }
                }
            });

            // ---- acceleration
            egui::CollapsingHeader::new(format!("Acceleration ({})", self.acc_sets.len()))
                .default_open(!self.acc_sets.is_empty())
                .show(ui, |ui| {
                    self.acc_panel(ui);
                });

            // ---- wind turbines & structures
            egui::CollapsingHeader::new("Wind turbines & tall structures").default_open(false).show(ui, |ui| {
                self.structures_panel(ui);
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
                let old_field = (self.opts.field_margin_deg, self.opts.field_stride);
                ui.add(
                    egui::Slider::new(&mut self.opts.field_margin_deg, 0.0..=8.0)
                        .step_by(0.25)
                        .suffix("°")
                        .text("wind-field radius"),
                )
                .on_hover_text("Download wind arrows this far around each animal, for the week in view (fetched as you move through time). 0 = only at the fixes");
                let spacing = |s: i32| format!("{}°", s as f64 * weather::GRID);
                ui.add_enabled_ui(self.opts.field_margin_deg > 0.0, |ui| {
                    egui::ComboBox::from_label("wind-field spacing").selected_text(spacing(self.opts.field_stride)).show_ui(ui, |ui| {
                        for s in [1, 2, 4] {
                            ui.selectable_value(&mut self.opts.field_stride, s, spacing(s));
                        }
                    });
                });
                if old_field != (self.opts.field_margin_deg, self.opts.field_stride) {
                    self.surface_field_days.clear();
                }
                if self.opts.field_margin_deg > 0.0 && !self.tracks.is_empty() {
                    let day = weather::day_of(self.t_now as i64);
                    let pts = weather::window_points(&self.tracks, day, 7, self.opts.field_margin_deg as f64, self.opts.field_stride);
                    let mut cells: Vec<weather::CellKey> = pts.iter().map(|p| weather::cell_of(p.0, p.1)).collect();
                    cells.sort_unstable();
                    cells.dedup();
                    let n = cells.len();
                    let txt = if self.surface_field_on {
                        format!("≈{n} wind-field cells for this week (≈{n} API calls)")
                    } else {
                        format!("≈{n} wind-field cells per week, fetched after the first Annotate")
                    };
                    if n > 1500 {
                        ui.colored_label(Color32::from_rgb(255, 180, 80), txt + " - consider a smaller radius or wider spacing");
                    } else {
                        ui.weak(txt);
                    }
                }
                ui.separator();
                self.era5_source_ui(ui);
                let busy = !self.downloads_idle();
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
                    if !self.cds_running.is_empty() || !self.cds_pending.is_empty() {
                        let waiting = self.cds_running.iter().filter(|r| matches!(r.stage, CdsStage::Poll(_))).count();
                        ui.weak(format!(
                            "Copernicus CDS: {} running ({waiting} queued at CDS), {} waiting here",
                            self.cds_running.len(),
                            self.cds_pending.len()
                        ));
                    }
                    if let Some(n) = &self.wait_note {
                        ui.colored_label(Color32::from_rgb(255, 190, 90), n);
                    }
                    if self.req_failed > 0 {
                        ui.weak(format!("{} failed - see the log", self.req_failed));
                    }
                }
                if !self.store.is_empty() {
                    ui.weak(format!("cache: {} cells · {} hours", self.store.cells.len(), self.store.hours()));
                }
            });

            // ---- 3D wind
            egui::CollapsingHeader::new("3D wind (pressure levels)").default_open(true).show(ui, |ui| {
                self.wind3d_panel(ui);
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

    fn time_slider(&mut self, ui: &mut Ui) {
        let (lo, hi) = (self.t_min, self.t_max);
        let reserve = if self.narrow { 10.0 } else { 190.0 };
        ui.spacing_mut().slider_width = (ui.available_width() - reserve).max(100.0);
        ui.add(
            egui::Slider::new(&mut self.t_now, lo..=hi)
                .show_value(false)
                .trailing_fill(true),
        );
        if !self.narrow {
            ui.monospace(fmt_time(self.t_now as i64) + " UTC");
        }
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
            ui.toggle_value(&mut self.chart_open, "Chart")
                .on_hover_text("Show or hide the chart");
            if self.narrow {
                let short = chrono::DateTime::<chrono::Utc>::from_timestamp(self.t_now as i64, 0)
                    .map(|d| d.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                ui.monospace(short);
            } else {
                self.time_slider(ui);
            }
        });
        if self.narrow {
            ui.horizontal(|ui| self.time_slider(ui));
        }
        if !self.chart_open {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("chartvar")
                .selected_text(self.chart_var.label())
                .show_ui(ui, |ui| {
                    for v in ChartVar::ALL {
                        ui.selectable_value(&mut self.chart_var, v, v.label());
                    }
                });
            if self.chart_var == ChartVar::Curtain {
                egui::ComboBox::from_id_salt("curtain_metric")
                    .selected_text(self.curtain_metric.label())
                    .show_ui(ui, |ui| {
                        for m in CurtainMetric::ALL {
                            ui.selectable_value(&mut self.curtain_metric, m, m.label());
                        }
                    });
            }
            if !self.narrow {
                ui.weak(match self.selected_track {
                    Some(i) => format!(
                        "showing {}",
                        self.tracks.get(i).map(|t| t.id.as_str()).unwrap_or("")
                    ),
                    None => "showing all visible tracks (click a track name to isolate)".into(),
                });
            }
        });
        match self.chart_var {
            ChartVar::Curtain => return self.curtain_chart(ui),
            ChartVar::Profile => return self.profile_chart(ui),
            ChartVar::AccBurst => return self.acc_burst_chart(ui),
            _ => {}
        }

        let var = self.chart_var;
        let mut series: Vec<(String, Color32, Vec<[f64; 2]>)> = Vec::new();
        for (i, tr) in self.tracks.iter().enumerate() {
            if !tr.visible || self.selected_track.is_some_and(|s| s != i) {
                continue;
            }
            let tk = self.track_key(i);
            let pts: Vec<[f64; 2]> = match var {
                ChartVar::Acc(m) => match &self.acc_cache {
                    Some(c) => self
                        .acc_sets
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.track.as_deref() == Some(tk.as_str()))
                        .flat_map(|(si, _)| c.a.per_set[si].iter())
                        .filter_map(|(f, fl, cl)| {
                            m.of(f, *fl, *cl).map(|v| [f.t_ms as f64 / 86_400_000.0, v])
                        })
                        .collect(),
                    None => Vec::new(),
                },
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
            .legend_if(!self.narrow)
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
                    // per-burst ACC metrics are clouds of points, not lines
                    if !matches!(var, ChartVar::Acc(_)) {
                        pui.line(
                            Line::new(name.clone(), PlotPoints::from(pts.clone()))
                                .color(*c)
                                .width(1.5),
                        );
                    }
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
                acc_band: self.acc_params.band,
            },
            structs: self.st_cache.as_ref().filter(|_| self.st_on).map(|c| {
                (
                    crate::structures::Draw {
                        items: &self.st_store.items,
                        hits: &c.hits,
                        params: self.st_params,
                    },
                    self.st_highlight.then_some(&c.near),
                )
            }),
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
        let shown = map.with_plugin(layer).show(ui, |ui, _resp, proj, _mem| {
            let centre = proj.unproject(ui.max_rect().center().to_vec2());
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
            (centre.y(), centre.x())
        });
        self.map_center = Some(shown.inner);

        if let Some(sel) = interaction.clicked {
            self.selected = Some(sel);
            self.t_now = self.tracks[sel.0].fixes[sel.1].t as f64;
            self.playing = false;
        }
        if let Some((ti, fi, pos)) = interaction.hover {
            self.fix_tooltip(ui, ti, fi, pos);
        } else {
            self.selected_card(ui);
        }
    }

    /// On phones (no hover, side panel usually hidden) keep the tapped fix's summary on the map.
    fn selected_card(&self, ui: &Ui) {
        if !(self.narrow && !self.side_open) {
            return;
        }
        if let Some((ti, fi)) = self.selected
            && self.tracks.get(ti).is_some_and(|t| fi < t.fixes.len())
        {
            let r = ui.max_rect();
            self.fix_tooltip(ui, ti, fi, r.left_bottom() + vec2(-6.0, -150.0));
        }
    }

    fn fix_tooltip(&self, ui: &Ui, ti: usize, fi: usize, pos: Pos2) {
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
        if let Some(fw) = &f.weather.flight {
            if let Some(ws) = fw.wind_support {
                s.push_str(&format!(
                    "\nflight-level support {ws:+.1} m/s ({:.0} hPa)",
                    fw.matched_level_hpa
                ));
            }
            if let (Some(b), Some(bs)) = (fw.best_level_hpa, fw.best_wind_support) {
                s.push_str(&format!("\nbest level {b:.0} hPa: {bs:+.1} m/s"));
            }
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

    fn map3d(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let _ = self.current_slice();
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
                        date,
                    },
                    ctx.clone(),
                ),
            );
        }
        let basemap = self.basemap;
        let base = self
            .base_tiles
            .entry(basemap)
            .or_insert_with(|| HttpTiles::new(basemap, ctx.clone()));
        let attribution = base.attribution().text.to_string();
        let slice = self.slice_cache.as_ref().and_then(|c| c.1.as_ref());
        let slice_level_hpa: Vec<f32> = slice
            .map(|sl| sl.levels.iter().map(|&i| self.profiles.levels[i]).collect())
            .unwrap_or_default();
        let sat = if self.sat_on {
            self.sat_tiles
                .get_mut(&sat_key)
                .map(|t| (t as &mut dyn Tiles, self.sat_opacity))
        } else {
            None
        };
        let scene = Scene {
            tracks: &self.tracks,
            t_now: self.t_now,
            trail_to_cursor: self.trail_to_cursor,
            selected: self.selected,
            focus_track: self.selected_track,
            slice,
            slice_level_hpa,
            base: Some(base as &mut dyn Tiles),
            sat,
            source_label: self.profiles.source.label(),
            basemap_attribution: attribution,
            max_error_m: self.max_error_km * 1000.0,
            structures: self.st_cache.as_ref().filter(|_| self.st_on).map(|c| {
                crate::structures::Draw {
                    items: &self.st_store.items,
                    hits: &c.hits,
                    params: self.st_params,
                }
            }),
        };
        let out = self.view3d.show(ui, scene);
        if let Some(sel) = out.clicked {
            self.selected = Some(sel);
            self.t_now = self.tracks[sel.0].fixes[sel.1].t as f64;
            self.playing = false;
        }
        if let Some((ti, fi, pos)) = out.hover {
            self.fix_tooltip(ui, ti, fi, pos);
        } else {
            self.selected_card(ui);
        }
    }

    fn wind3d_panel(&mut self, ui: &mut Ui) {
        let is_file = matches!(
            self.profiles.source,
            wind3d::Source::File(_) | wind3d::Source::Synthetic
        );
        ui.weak(format!("source: {}", self.profiles.source.label()));
        if !self.profiles.levels.is_empty() {
            ui.weak(format!(
                "{} levels {:.0}–{:.0} hPa · {} cells · {} hours",
                self.profiles.levels.len(),
                self.profiles.levels.first().copied().unwrap_or(0.0),
                self.profiles.levels.last().copied().unwrap_or(0.0),
                self.profiles.cells.len(),
                self.profiles.hours()
            ));
        }
        let old_model = self.om_model;
        egui::ComboBox::from_label("Open-Meteo model")
            .selected_text(self.om_model.label())
            .show_ui(ui, |ui| {
                for m in OmModel::ALL {
                    ui.selectable_value(&mut self.om_model, m, m.label());
                }
            });
        if old_model != self.om_model && !is_file && !self.profiles.is_empty() {
            self.profiles.use_open_meteo(self.om_model);
            self.field_days_requested.clear();
            self.reset_display_levels();
        }
        let busy = !self.queue.is_empty() || !self.in_flight.is_empty();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.tracks.is_empty() && !busy, egui::Button::new("Download along tracks"))
                .on_hover_text("Pressure-level wind at every fix and every 3 h along each step; annotates flight-level and best-level wind support")
                .clicked()
            {
                self.start_profiles();
            }
            if ui
                .button("Import ERA5 cube…")
                .on_hover_text("CSV / CSV.gz from tools/era5_wind_cube.py (any pressure levels, e.g. 1000–500 hPa)")
                .clicked()
            {
                self.open_cube_dialog(ui.ctx());
            }
            if is_file && ui.button("Back to Open-Meteo").clicked() {
                self.profiles.clear();
                self.field_days_requested.clear();
                self.ensure_open_meteo();
                self.reset_display_levels();
            }
        });
        ui.add_enabled_ui(!is_file, |ui| {
            ui.checkbox(
                &mut self.field_auto,
                "auto-download the wind field around the animals (3D tab)",
            )
            .on_hover_text(
                "Fetched a week at a time from the cursor day, as you move through time",
            );
            let old_r = self.field_radius_deg;
            ui.add(
                egui::Slider::new(&mut self.field_radius_deg, 1.0..=10.0)
                    .step_by(0.5)
                    .suffix("°")
                    .text("field radius around animals"),
            );
            if old_r != self.field_radius_deg {
                self.field_days_requested.clear();
                self.slice_cache = None;
            }
            let old = self.field_step_cells;
            egui::ComboBox::from_label("field resolution")
                .selected_text(format!(
                    "{:.2}°",
                    self.field_step_cells as f64 * weather::GRID
                ))
                .show_ui(ui, |ui| {
                    for st in [1, 2, 4, 8] {
                        ui.selectable_value(
                            &mut self.field_step_cells,
                            st,
                            format!("{:.2}°", st as f64 * weather::GRID),
                        );
                    }
                });
            if old != self.field_step_cells {
                self.profiles.node_step = self.field_step_cells;
                self.field_days_requested.clear();
                self.slice_cache = None;
            }
            let n = self.field_node_count();
            if n > 0 {
                let txt = format!(
                    "≈ {n} field points this week (≈ {:.0} API calls)",
                    n as f32 * 2.5
                );
                if n > 1000 {
                    ui.colored_label(
                        Color32::from_rgb(240, 160, 60),
                        txt + " - consider a coarser resolution",
                    );
                } else {
                    ui.weak(txt);
                }
            }
        });
        if !self.profiles.levels.is_empty() {
            ui.label("levels shown in 3D:");
            let mean_z: HashMap<usize, f32> = self
                .slice_cache
                .as_ref()
                .and_then(|c| c.1.as_ref())
                .map(|sl| {
                    sl.levels
                        .iter()
                        .copied()
                        .zip(sl.mean_z.iter().copied())
                        .collect()
                })
                .unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                for i in 0..self.profiles.levels.len() {
                    let p = self.profiles.levels[i];
                    let h = mean_z
                        .get(&i)
                        .copied()
                        .unwrap_or(geo::pressure_to_altitude_m(p as f64, 1013.25) as f32);
                    let mut on = self.display_levels.contains(&i);
                    if ui
                        .checkbox(&mut on, format!("{p:.0} (~{h:.0} m)"))
                        .changed()
                    {
                        if on && self.display_levels.len() < 5 {
                            self.display_levels.push(i);
                        } else {
                            self.display_levels.retain(|&x| x != i);
                        }
                        self.display_levels.sort_unstable();
                        self.slice_cache = None;
                    }
                }
            });
            if !self.display_levels.is_empty() {
                let cur = self.view3d.raster_level.min(self.display_levels.len() - 1);
                egui::ComboBox::from_label("colour field = wind at")
                    .selected_text(format!(
                        "{:.0} hPa",
                        self.profiles.levels[self.display_levels[cur]]
                    ))
                    .show_ui(ui, |ui| {
                        for (k, &li) in self.display_levels.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.view3d.raster_level,
                                k,
                                format!("{:.0} hPa", self.profiles.levels[li]),
                            );
                        }
                    });
            }
        }
        ui.separator();
        let v = &mut self.view3d;
        ui.horizontal(|ui| {
            ui.checkbox(&mut v.auto_exag, "auto").on_hover_text(
                "Spread the levels so the stack is ~20 % as tall as the wind field is wide",
            );
            ui.add_enabled(
                !v.auto_exag,
                egui::Slider::new(&mut v.exag, 10.0..=3000.0)
                    .logarithmic(true)
                    .text("vertical exaggeration"),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut v.particles_on, "flowing particles");
            ui.checkbox(&mut v.show_raster, "speed colours");
            ui.checkbox(&mut v.show_planes, "level planes");
        });
        if v.particles_on {
            ui.add(
                egui::Slider::new(&mut v.particles_per_level, 100..=2500).text("particles / level"),
            );
            ui.add(
                egui::Slider::new(&mut v.flow_hours_per_s, 0.5..=12.0)
                    .text("flow speed (h of wind / s)"),
            );
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut v.show_drop_lines, "drop lines");
            ui.checkbox(&mut v.show_shadows, "ground shadows");
        });
        ui.toggle_value(&mut v.terrain, "⛰ 3D terrain").on_hover_text(
            "Drape the basemap over real topography (AWS Terrain Tiles), hill-shaded. Uses the same \
             vertical exaggeration as the flight altitudes, so heights above ground stay true.",
        );
        ui.add(egui::Slider::new(&mut v.ground_dim, 0.0..=0.9).text("dim basemap"));
        ui.add(
            egui::Slider::new(&mut v.default_alt_m, 0.0..=2000.0).text("altitude if unknown (m)"),
        );
        ui.add(
            egui::Slider::new(&mut v.gap_factor, 1.5..=8.0)
                .text("dotted if gap > × median interval"),
        );
    }

    fn curtain_chart(&mut self, ui: &mut Ui) {
        let Some(ti) = self.chart_track() else {
            ui.weak("No visible track.");
            return;
        };
        if self.profiles.is_empty() {
            ui.weak("No pressure-level wind yet - use \"Download along tracks\" or \"Import ERA5 cube…\" in the side panel.");
            return;
        }
        let key = (
            ti,
            self.profiles.generation,
            self.curtain_metric,
            self.tracks[ti].fixes.len(),
        );
        if self.curtain_cache.as_ref().is_none_or(|c| c.0 != key) {
            let c = wind3d::curtain(&self.profiles, &self.tracks[ti], self.curtain_metric, 300);
            self.curtain_cache = Some((key, c));
        }
        let Some(c) = self.curtain_cache.as_ref().and_then(|c| c.1.as_ref()) else {
            ui.weak("No wind data along this track yet.");
            return;
        };
        let tr = &self.tracks[ti];
        let metric = self.curtain_metric;
        let nl = self.profiles.levels.len();
        let dx = c.step_s as f64 / 86400.0;
        let mut cells: Vec<([[f64; 2]; 4], Color32)> = Vec::new();
        for (i, &t) in c.times.iter().enumerate() {
            let x0 = t as f64 / 86400.0 - dx / 2.0;
            let x1 = x0 + dx;
            for j in 0..nl {
                let (v, z) = (c.val[i][j], c.z[i][j]);
                if !v.is_finite() || !z.is_finite() {
                    continue;
                }
                let zp = if j > 0 { c.z[i][j - 1] } else { f32::NAN };
                let zn = if j + 1 < nl { c.z[i][j + 1] } else { f32::NAN };
                let mut lo = if zp.is_finite() {
                    (zp + z) / 2.0
                } else if zn.is_finite() {
                    z - (zn - z) / 2.0
                } else {
                    z - 100.0
                };
                if c.ground_z[i].is_finite() {
                    lo = lo.max(c.ground_z[i]);
                }
                let hi = if zn.is_finite() {
                    (z + zn) / 2.0
                } else if zp.is_finite() {
                    z + (z - zp) / 2.0
                } else {
                    z + 100.0
                };
                let col = match metric {
                    CurtainMetric::WindSpeed => crate::view3d::wind_color(v, 255),
                    _ => diverging(v / 10.0),
                };
                cells.push((
                    [
                        [x0, lo as f64],
                        [x1, lo as f64],
                        [x1, hi as f64],
                        [x0, hi as f64],
                    ],
                    col,
                ));
            }
        }
        let alt: Vec<[f64; 2]> = tr
            .fixes
            .iter()
            .filter_map(|f| {
                let h = f
                    .weather
                    .flight
                    .as_ref()
                    .and_then(|w| w.flight_height_m)
                    .or(f.best_alt())?;
                Some([f.t as f64 / 86400.0, h as f64])
            })
            .collect();
        let best: Vec<[f64; 2]> = c
            .times
            .iter()
            .zip(&c.best_z)
            .filter(|(_, z)| z.is_finite())
            .map(|(t, z)| [*t as f64 / 86400.0, *z as f64])
            .collect();
        let ground: Vec<[f64; 2]> = c
            .times
            .iter()
            .zip(&c.ground_z)
            .filter(|(_, z)| z.is_finite())
            .map(|(t, z)| [*t as f64 / 86400.0, *z as f64])
            .collect();
        let tcol = Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]);
        let id = tr.id.clone();
        if !self.narrow {
            ui.weak(format!(
            "{} along {id}'s path (colour: {}) · line = animal altitude · dashed = level with best wind support",
            metric.label(),
            if metric == CurtainMetric::WindSpeed { "speed 0–28 m/s" } else { "blue tailwind / red headwind, ±10 m/s" }
        ));
        }
        let t_now_d = self.t_now / 86400.0;
        let resp = Plot::new("curtain")
            .legend_if(!self.narrow)
            .allow_scroll(false)
            .y_axis_label("height (m ASL)")
            .x_axis_formatter(|m: GridMark, _r| fmt_date((m.value * 86400.0) as i64))
            .label_formatter(|h: &HoverPosition<'_>| {
                let p = match h {
                    HoverPosition::NearDataPoint { position, .. } => position,
                    HoverPosition::Elsewhere { position } => position,
                };
                Some(format!(
                    "{}\n{:.0} m",
                    fmt_time((p.x * 86400.0) as i64),
                    p.y
                ))
            })
            .show(ui, |pui| {
                for (k, (pts, col)) in cells.iter().enumerate() {
                    pui.polygon(
                        egui_plot::Polygon::new("", PlotPoints::from(pts.to_vec()))
                            .fill_color(*col)
                            .stroke(Stroke::NONE)
                            .allow_hover(false)
                            .id(("cell", k)),
                    );
                }
                if !ground.is_empty() {
                    pui.line(
                        Line::new("ground", PlotPoints::from(ground))
                            .color(Color32::from_gray(60))
                            .width(2.0),
                    );
                }
                if !best.is_empty() {
                    pui.line(
                        Line::new("best level", PlotPoints::from(best))
                            .color(Color32::from_rgb(255, 230, 120))
                            .style(egui_plot::LineStyle::dashed_loose())
                            .width(1.2),
                    );
                }
                if !alt.is_empty() {
                    pui.line(
                        Line::new(format!("{id} altitude"), PlotPoints::from(alt.clone()))
                            .color(Color32::WHITE)
                            .width(2.0),
                    );
                    pui.points(
                        Points::new(format!("{id} altitude"), PlotPoints::from(alt))
                            .color(tcol)
                            .radius(3.0),
                    );
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

    fn profile_chart(&mut self, ui: &mut Ui) {
        if self.profiles.is_empty() {
            ui.weak("No pressure-level wind yet - use \"Download along tracks\" or \"Import ERA5 cube…\" in the side panel.");
            return;
        }
        // Selected fix, else the chart track's position at the cursor.
        let target = if let Some((ti, fi)) = self.selected
            && let Some(f) = self.tracks.get(ti).and_then(|t| t.fixes.get(fi))
        {
            let h = f
                .weather
                .flight
                .as_ref()
                .and_then(|w| w.flight_height_m)
                .or(f.best_alt());
            Some((
                ti,
                f.lat,
                f.lon,
                f.t,
                f.reported_heading.or(f.bearing_deg),
                h,
            ))
        } else if let Some(ti) = self.chart_track()
            && let Some((la, lo, idx)) = self.tracks[ti].position_at(self.t_now)
        {
            let tr = &self.tracks[ti];
            let hd = tr.fixes.get(idx + 1).map(|b| {
                geo::bearing_deg(tr.fixes[idx].lat, tr.fixes[idx].lon, b.lat, b.lon) as f32
            });
            Some((ti, la, lo, self.t_now as i64, hd, None))
        } else {
            None
        };
        let Some((ti, la, lo, t, hd, h_animal)) = target else {
            ui.weak("Select a fix or move the cursor into a track's time range.");
            return;
        };
        let Some(prof) = self.profiles.sample(la, lo, t) else {
            ui.weak(format!(
                "No wind profile at {} for this location yet.",
                fmt_time(t)
            ));
            return;
        };
        let st = &self.profiles;
        let (mut sup, mut crs, mut spd) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..st.levels.len() {
            if !st.above_ground(&prof, i) || !prof.lv[i].z.is_finite() {
                continue;
            }
            let l = prof.lv[i];
            let z = l.z as f64;
            spd.push([l.speed() as f64, z]);
            if let Some(h) = hd {
                let (s, c) = geo::wind_components(l.u as f64, l.v as f64, h as f64);
                sup.push([s, z]);
                crs.push([c, z]);
            }
        }
        let ground = if prof.sp.is_finite() {
            st.height_of_pressure(&prof, prof.sp)
        } else {
            None
        };
        let tr = &self.tracks[ti];
        let tcol = Color32::from_rgb(tr.color[0], tr.color[1], tr.color[2]);
        ui.weak(format!(
            "Wind profile · {} · {} · {:.3}, {:.3}{}",
            tr.id,
            fmt_time(t),
            la,
            lo,
            hd.map(|h| format!(" · heading {h:.0}°"))
                .unwrap_or_else(|| " · no heading (support not defined)".into())
        ));
        Plot::new("profile")
            .legend_if(!self.narrow)
            .allow_scroll(false)
            .x_axis_label("m/s")
            .y_axis_label("height (m ASL)")
            .show(ui, |pui| {
                pui.vline(VLine::new("", 0.0).color(Color32::from_gray(90)));
                pui.line(
                    Line::new("wind speed", PlotPoints::from(spd.clone()))
                        .color(Color32::from_gray(190))
                        .width(1.5),
                );
                pui.points(
                    Points::new("wind speed", PlotPoints::from(spd))
                        .color(Color32::from_gray(190))
                        .radius(3.0),
                );
                if !sup.is_empty() {
                    pui.line(
                        Line::new("wind support", PlotPoints::from(sup.clone()))
                            .color(Color32::from_rgb(80, 160, 230))
                            .width(2.0),
                    );
                    pui.points(
                        Points::new("wind support", PlotPoints::from(sup))
                            .color(Color32::from_rgb(80, 160, 230))
                            .radius(3.0),
                    );
                    pui.line(
                        Line::new("crosswind", PlotPoints::from(crs.clone()))
                            .color(Color32::from_rgb(230, 150, 60))
                            .width(1.5),
                    );
                    pui.points(
                        Points::new("crosswind", PlotPoints::from(crs))
                            .color(Color32::from_rgb(230, 150, 60))
                            .radius(2.5),
                    );
                }
                if let Some(h) = h_animal {
                    pui.hline(
                        egui_plot::HLine::new("animal altitude", h as f64)
                            .color(tcol)
                            .width(2.0),
                    );
                }
                if let Some(g) = ground {
                    pui.hline(
                        egui_plot::HLine::new("ground", g as f64)
                            .color(Color32::from_gray(70))
                            .width(2.0),
                    );
                }
            });
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
            if let Some(a) = &f.acc {
                row(ui, "ACC bursts (step)", a.n.to_string());
                row(
                    ui,
                    "ACC flight share",
                    format!("{:.0} %", a.flight_frac * 100.0),
                );
                row(
                    ui,
                    "ACC dynamic SD / VeDBA",
                    format!("{:.2} / {:.2} g", a.dyn_sd, a.vedba),
                );
                row(ui, "ACC wingbeat", o(a.wbf_hz, "Hz", 1));
                row(ui, "ACC pitch", format!("{:.0}°", a.pitch_deg));
                if let Some(c) = a.cluster {
                    let fr: Vec<String> = a
                        .cluster_frac
                        .iter()
                        .enumerate()
                        .filter(|(_, x)| **x > 0.0)
                        .map(|(i, x)| format!("C{} {:.0}%", i + 1, x * 100.0))
                        .collect();
                    row(ui, "ACC cluster", format!("C{} ({})", c + 1, fr.join(", ")));
                }
            }
            if let Some(fw) = &w.flight {
                row(
                    ui,
                    "flight pressure",
                    format!("{:.1} hPa", fw.flight_pressure_hpa),
                );
                row(ui, "flight height (profile)", o(fw.flight_height_m, "m", 0));
                row(
                    ui,
                    "matched level",
                    format!("{:.0} hPa", fw.matched_level_hpa),
                );
                row(ui, "flight-level wind", format!("{:.1} m/s", fw.wind_speed));
                row(ui, "flight-level support", o(fw.wind_support, "m/s", 1));
                row(ui, "flight-level crosswind", o(fw.crosswind, "m/s", 1));
                row(ui, "flight-level airspeed", o(fw.airspeed, "m/s", 1));
                row(
                    ui,
                    "best level",
                    match (fw.best_level_hpa, fw.best_wind_support) {
                        (Some(l), Some(b)) => format!("{l:.0} hPa ({b:+.1} m/s)"),
                        _ => "-".into(),
                    },
                );
                row(
                    ui,
                    "at best level",
                    fw.at_best_wind
                        .map(|b| if b { "yes" } else { "no" })
                        .unwrap_or("-")
                        .into(),
                );
                row(ui, "3D wind source", fw.source.clone());
            }
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
        self.now = ctx.input(|i| i.time);
        self.sync_url_hash();
        self.process_messages();
        match self.view_tab {
            ViewTab::ThreeD => self.maybe_request_field(),
            ViewTab::Map => self.maybe_request_surface_field(),
        }
        self.pump_weather(ctx);
        self.pump_cds(ctx);
        self.plan_structures();
        self.pump_structures(ctx);
        self.ensure_st_cache();
        self.ensure_acc_cache();
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
        let screen = ctx.content_rect();
        self.narrow = screen.width() < 760.0;
        if !self.layout_init {
            // Phones start with the side panel hidden and a shorter chart.
            self.side_open = !self.narrow;
            self.layout_init = true;
        }
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        let side_w = if self.narrow {
            (screen.width() * 0.85).min(330.0)
        } else {
            330.0
        };
        let mut side_open = self.side_open;
        egui::Panel::left(if self.narrow { "side_narrow" } else { "side" })
            .resizable(true)
            .default_size(side_w)
            .size_range(180.0..=screen.width().max(200.0) * 0.95)
            .show_collapsible(ui, &mut side_open, |ui| self.side_panel(ui));
        self.side_open = side_open;
        let chart_h = if self.narrow {
            (screen.height() * 0.3).clamp(140.0, 220.0)
        } else {
            250.0
        };
        let bottom = if self.chart_open {
            egui::Panel::bottom(if self.narrow {
                "timeline_narrow"
            } else {
                "timeline"
            })
            .resizable(true)
            .default_size(chart_h)
        } else {
            egui::Panel::bottom("timeline_min").resizable(false)
        };
        self.timeline_rect = bottom.show(ui, |ui| self.timeline(ui)).response.rect;
        self.map_rect = egui::CentralPanel::no_frame()
            .show(ui, |ui| match self.view_tab {
                ViewTab::Map => self.map(ui),
                ViewTab::ThreeD => self.map3d(ui),
            })
            .response
            .rect;
        self.video_window(&ctx);
        self.share_window(&ctx);
        self.url_window(&ctx);
    }
}

/// Address of this page without the fragment (web), or the published web app (desktop).
fn page_base_url() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(loc) = web_sys::window().map(|w| w.location())
            && let (Ok(o), Ok(p)) = (loc.origin(), loc.pathname())
        {
            return format!("{o}{p}");
        }
    }
    share::WEB_APP_URL.to_string()
}

/// The share link the app was opened with: the page's `#…` on the web, or a link passed
/// as the first command-line argument on the desktop.
fn startup_link() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        let h = web_sys::window()?.location().hash().ok()?;
        (h.len() > 2).then_some(h)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::args().nth(1).filter(|a| a.contains('#'))
    }
}

/// Charts drop their legend on phone-width screens, where it would cover the plot.
trait PlotLegendExt {
    fn legend_if(self, on: bool) -> Self;
}

impl PlotLegendExt for Plot<'_> {
    fn legend_if(self, on: bool) -> Self {
        if on {
            self.legend(Legend::default())
        } else {
            self
        }
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
    /// Wingbeat band, for the wingbeat colour scale.
    acc_band: (f32, f32),
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
    /// Wind turbines & structures, plus steps to highlight.
    structs: Option<StructLayer<'a>>,
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
            ColorBy::AccFlight => f
                .acc
                .as_ref()
                .map(|a| viridis(a.flight_frac))
                .unwrap_or(none),
            ColorBy::AccWbf => f
                .acc
                .as_ref()
                .and_then(|a| a.wbf_hz)
                .map(|w| {
                    let (lo, hi) = self.acc_band;
                    viridis((w - lo) / (hi - lo).max(0.1))
                })
                .unwrap_or(none),
            ColorBy::AccCluster => f
                .acc
                .as_ref()
                .and_then(|a| a.cluster)
                .map(|c| CLUSTER_COLORS[c % CLUSTER_COLORS.len()])
                .unwrap_or(none),
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

        // Wind turbines & tall structures, and the steps that passed close to turbines.
        if let Some((sd, near)) = &self.structs {
            if let Some(near) = near {
                let halo = Color32::from_rgba_unmultiplied(255, 120, 20, 110);
                for &(ti, fi) in near.iter() {
                    let Some(tr) = self.tracks.get(ti).filter(|t| t.visible) else {
                        continue;
                    };
                    if fi == 0 || fi >= tr.fixes.len() {
                        continue;
                    }
                    let (a, b) = (&tr.fixes[fi - 1], &tr.fixes[fi]);
                    painter.line_segment(
                        [to_screen(a.lat, a.lon), to_screen(b.lat, b.lon)],
                        Stroke::new(8.0, halo),
                    );
                }
            }
            let ppm = projector.scale_pixel_per_meter(projector.unproject(clip.center().to_vec2()));
            let mut hovered: Option<(f32, usize, Pos2)> = None;
            for (i, s) in sd.items.iter().enumerate() {
                if !sd.params.counts(s) {
                    continue;
                }
                let p = to_screen(s.lat, s.lon);
                if !clip.expand(10.0).contains(p) {
                    continue;
                }
                let hit = sd.hits.contains(&i);
                let fill = if hit {
                    Color32::from_rgb(255, 140, 30)
                } else {
                    Color32::from_gray(235)
                };
                let (_, rotor, _, _) = s.dims();
                if s.kind == crate::structures::Kind::WindTurbine {
                    let r = (rotor / 2.0 * ppm).clamp(2.0, 12.0);
                    if r < 4.0 {
                        painter.circle(
                            p,
                            2.2,
                            fill,
                            Stroke::new(0.6, Color32::from_black_alpha(200)),
                        );
                    } else {
                        // three blades
                        for k in 0..3 {
                            let a = std::f32::consts::TAU * k as f32 / 3.0
                                - std::f32::consts::FRAC_PI_2;
                            painter.line_segment(
                                [p, p + vec2(a.cos(), a.sin()) * r],
                                Stroke::new(1.8, Color32::from_black_alpha(170)),
                            );
                            painter.line_segment(
                                [p, p + vec2(a.cos(), a.sin()) * r],
                                Stroke::new(1.0, fill),
                            );
                        }
                        painter.circle_filled(p, 1.8, fill);
                    }
                } else {
                    let r = 3.2;
                    painter.add(egui::Shape::convex_polygon(
                        vec![p + vec2(0.0, -r * 1.3), p + vec2(r, r), p + vec2(-r, r)],
                        if hit { fill } else { Color32::from_gray(170) },
                        Stroke::new(0.6, Color32::from_black_alpha(200)),
                    ));
                }
                if let Some(h) = hover_pos {
                    let d = h.distance(p);
                    if d < 7.0 && hovered.is_none_or(|x| d < x.0) {
                        hovered = Some((d, i, p));
                    }
                }
            }
            if let Some((_, i, p)) = hovered {
                let s = &sd.items[i];
                let (hub, rotor, total, est) = s.dims();
                let dims = if s.kind == crate::structures::Kind::WindTurbine {
                    format!("hub {hub:.0} m · rotor {rotor:.0} m · tip {total:.0} m")
                } else {
                    format!("{total:.0} m")
                };
                let txt = format!(
                    "{}{} - {dims}{}\n{} {}",
                    s.kind.label(),
                    s.name.as_ref().map(|n| format!(" {n}")).unwrap_or_default(),
                    if est { " (estimated)" } else { "" },
                    s.source,
                    s.id
                );
                let r = painter.text(
                    p + vec2(10.0, 10.0),
                    Align2::LEFT_TOP,
                    &txt,
                    FontId::proportional(12.0),
                    Color32::WHITE,
                );
                painter.rect_filled(r.expand(3.0), 3.0, Color32::from_black_alpha(170));
                painter.text(
                    p + vec2(10.0, 10.0),
                    Align2::LEFT_TOP,
                    &txt,
                    FontId::proportional(12.0),
                    Color32::WHITE,
                );
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
                // error radius for every non-GPS fix (Sigfox km-scale, Bluetooth tens of metres)
                if self.show_error
                    && f.source != LocationSource::Gps
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
        ColorBy::AccFlight => ("ACC flight share of the step", "0 %", "100 %", false),
        ColorBy::AccWbf => ("ACC wingbeat frequency", "band low", "band high", false),
        ColorBy::AccCluster => {
            colorbar(
                p,
                origin,
                "ACC behaviour cluster",
                "C1 calm",
                "dynamic",
                &|x| CLUSTER_COLORS[((x * 5.999) as usize).min(5)],
            );
            return true;
        }
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

/// Tracks to (re)filter (empty = all visible) and the range, `None` to clear.
type DateAction = (Vec<usize>, Option<(i64, i64)>);

/// `YYYY-MM-DD HH:MM` (UTC), the format the date-range editor shows.
fn fmt_minute(t: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// Parse the date-range editor. A bare date means the start (from) or end (to) of that day;
/// anything [`crate::table::parse_time`] understands is accepted too. The "to" bound is
/// inclusive to the end of its minute.
fn parse_date_range(from: &str, to: &str) -> Result<(i64, i64), String> {
    fn bound(s: &str, end: bool) -> Option<i64> {
        let s = s.trim();
        for f in ["%Y-%m-%d", "%d.%m.%Y"] {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(s, f) {
                let (h, m, sec) = if end { (23, 59, 59) } else { (0, 0, 0) };
                return d.and_hms_opt(h, m, sec).map(|dt| dt.and_utc().timestamp());
            }
        }
        let t = crate::table::parse_time(s)?;
        // "HH:MM" without seconds: include the whole minute at the end bound
        Some(if end && t % 60 == 0 { t + 59 } else { t })
    }
    let a = bound(from, false).ok_or("can't read the 'from' date")?;
    let b = bound(to, true).ok_or("can't read the 'to' date")?;
    if b < a {
        return Err("'to' is before 'from'".into());
    }
    Ok((a, b))
}

#[cfg(test)]
mod share_tests {
    use super::*;

    /// Synthetic Movebank e-obs ACC for a track: a burst every 10 min, flapping at 9 Hz in
    /// the first half of the deployment, at rest afterwards.
    fn eobs_for(tr: &Track) -> String {
        let (t0, t1) = tr.time_range().unwrap();
        let mut out = String::from(
            "event-id,timestamp,eobs:accelerations-raw,eobs:acceleration-axes,eobs:acceleration-sampling-frequency-per-axis,individual-local-identifier,tag-local-identifier\n",
        );
        let mid = (t0 + t1) / 2;
        let mut t = t0;
        let mut id = 0;
        while t <= t1 {
            let fly = t < mid;
            let raw: Vec<String> = (0..40)
                .map(|i| {
                    let ph = std::f64::consts::TAU * 9.0 * i as f64 / 33.0;
                    let z = 2048.0 + 512.0 + if fly { 700.0 * ph.sin() } else { 0.0 };
                    format!("2048 {} {}", 2048 + (i % 3), z.round())
                })
                .collect();
            out.push_str(&format!(
                "{id},{}.000,{},XYZ,33,{},5001\n",
                fmt_time(t),
                raw.join(" "),
                tr.id
            ));
            t += 600;
            id += 1;
        }
        out
    }

    #[test]
    fn acceleration_end_to_end() {
        let mut a = WingApp::blank();
        a.load_demo();
        let tr0 = a.tracks[0].clone();
        let n = a.tracks.len();
        a.add_file_from(
            "acc.csv".into(),
            eobs_for(&tr0).into_bytes(),
            None,
            Some(FileOrigin::Local),
        );
        assert_eq!(a.tracks.len(), n, "an ACC file adds no tracks");
        assert_eq!(a.acc_sets.len(), 1);
        a.acc_params.k = 2;
        a.ensure_acc_cache();
        assert_eq!(
            a.acc_sets[0].track.as_deref(),
            Some(a.track_key(0).as_str()),
            "matched by id"
        );
        let annotated: Vec<&crate::acc::FixAcc> = a.tracks[0]
            .fixes
            .iter()
            .filter_map(|f| f.acc.as_deref())
            .collect();
        assert!(annotated.len() > 3);
        let early = annotated.first().unwrap();
        let late = annotated.last().unwrap();
        assert!(
            early.flight_frac > 0.9 && late.flight_frac < 0.1,
            "{early:?} {late:?}"
        );
        assert!((early.wbf_hz.unwrap() - 9.0).abs() < 0.5);
        assert_ne!(early.cluster, late.cluster);
        assert!(a.tracks[1].fixes.iter().all(|f| f.acc.is_none()));
        // export columns
        let mut tr = a.tracks[0].clone();
        tr.fixes.retain(|f| f.acc.is_some());
        // share round trip of settings and set state
        a.acc_params.flight_g = 0.7;
        a.acc_sets[0].clock_rate = 0.015;
        let m = a.share_map(true);
        let mut b = WingApp::blank();
        b.load_demo();
        b.add_file_from(
            "acc.csv".into(),
            eobs_for(&tr0).into_bytes(),
            None,
            Some(FileOrigin::Local),
        );
        b.apply_share_settings(&m);
        b.apply_share_view(&m);
        assert_eq!(b.acc_params, a.acc_params);
        assert!((b.acc_sets[0].clock_rate - 0.015).abs() < 1e-9);
        assert_eq!(b.acc_sets[0].track, a.acc_sets[0].track);
        b.ensure_acc_cache();
        assert!(b.tracks[0].fixes.iter().any(|f| f.acc.is_some()));
    }

    #[test]
    #[ignore]
    fn fallen_tags_on_real_file() {
        // WT_FILE=moths.csv cargo test fallen_tags_on_real_file -- --ignored --nocapture
        let Ok(p) = std::env::var("WT_FILE") else {
            return;
        };
        let mut a = WingApp::blank();
        a.add_file_from(
            "m.csv".into(),
            std::fs::read(p).unwrap(),
            None,
            Some(FileOrigin::Local),
        );
        let before: usize = a.tracks.iter().map(|t| t.fixes.len()).sum();
        a.remove_fallen_tags();
        let after: usize = a.tracks.iter().map(|t| t.fixes.len()).sum();
        println!("fixes {before} -> {after}; {}", a.log.last().unwrap());
        a.remove_fallen_tags();
        println!("again: {}", a.log.last().unwrap());
        assert!(after < before);
    }

    #[test]
    fn date_range_editor_parsing() {
        let (a, b) = parse_date_range("2024-09-20", "2024-09-21").unwrap();
        assert_eq!(fmt_time(a), "2024-09-20 00:00:00");
        assert_eq!(fmt_time(b), "2024-09-21 23:59:59");
        let (a, b) = parse_date_range("2024-09-20 19:10", " 2024-09-20 22:00 ").unwrap();
        assert_eq!(fmt_time(a), "2024-09-20 19:10:00");
        assert_eq!(fmt_time(b), "2024-09-20 22:00:59");
        assert_eq!(fmt_minute(a), "2024-09-20 19:10");
        assert!(parse_date_range("2024-09-22", "2024-09-21").is_err());
        assert!(parse_date_range("soon", "2024-09-21").is_err());
    }

    fn without(m: &ShareMap, keys: &[&str]) -> Vec<(String, String)> {
        m.pairs
            .iter()
            .filter(|p| !keys.contains(&p.0.as_str()))
            .cloned()
            .collect()
    }

    #[test]
    fn share_link_restores_the_view() {
        let mut a = WingApp::blank();
        a.load_demo();
        a.add_file_from(
            "my_local.csv".into(),
            crate::demo::ctt_gps_csv().into_bytes(),
            Some("ctt-gps"),
            Some(FileOrigin::Local),
        );
        assert_eq!(a.tracks.len(), 5);
        a.view_tab = ViewTab::ThreeD;
        a.t_now = a.t_min + 0.37 * (a.t_max - a.t_min);
        a.speed_idx = 1;
        a.basemap = Basemap::CartoDark;
        a.sat_on = true;
        a.sat_layer = SatLayer::ViirsSnpp;
        a.sat_opacity = 0.5;
        a.wind_palettes
            .insert(Basemap::OpenStreetMap, WindPalette::Turbo);
        a.color_by = ColorBy::Altitude;
        a.show_error = false;
        a.follow = true;
        a.view3d.follow = true;
        a.max_error_km = 10.0;
        a.chart_var = ChartVar::Sensor(SensorKind::Pressure);
        a.curtain_metric = CurtainMetric::Crosswind;
        a.chart_open = false;
        a.opts.field_margin_deg = 2.0;
        a.opts.night_only = false;
        a.field_radius_deg = 5.0;
        a.field_step_cells = 2;
        a.view3d.particles_per_level = 900;
        a.view3d.ground_dim = 0.2;
        a.view3d.cam.yaw = 33.0;
        a.view3d.cam.pitch = 25.0;
        a.view3d.cam.dist = 1234.0;
        a.view3d.auto_exag = false;
        a.view3d.exag = 400.0;
        a.tracks[1].visible = false;
        a.tracks[0].color = [1, 2, 3];
        let (lo, hi) = a.tracks[2].time_range().unwrap();
        let dr = (lo + (hi - lo) / 4, hi - (hi - lo) / 4);
        a.set_date_filter(&[2], Some(dr));
        assert!(!a.tracks[2].excluded_fixes.is_empty());
        a.view3d.terrain = true;
        a.st_on = true;
        a.st_params.radius_m = 2500.0;
        a.st_params.include_other = false;
        a.st_highlight = false;
        a.selected_track = Some(2);
        a.selected = Some((0, 5));
        a.map_center = Some((45.5, 7.25));
        let _ = a.memory.set_zoom(6.5);

        let url = a.share_url(true);
        assert!(url.starts_with(share::WEB_APP_URL) && url.contains("#v=1&demo=1"));

        let mut b = WingApp::blank();
        b.start_from_share(&ShareMap::decode(&url));
        assert_eq!(b.tracks.len(), 5);
        assert_eq!(
            b.files
                .iter()
                .filter(|f| f.origin == FileOrigin::Local)
                .count(),
            1
        );
        assert_eq!(
            b.files
                .iter()
                .find(|f| f.name == "my_local.csv")
                .unwrap()
                .firmware,
            "ctt-gps"
        );
        assert!(!b.tracks[1].visible && b.tracks[0].color == [1, 2, 3]);
        assert_eq!(b.tracks[2].date_filter, Some(dr));
        assert_eq!(b.tracks[2].fixes.len(), a.tracks[2].fixes.len());
        assert!(b.view3d.terrain);
        assert!(
            b.st_on
                && b.st_params.radius_m == 2500.0
                && !b.st_params.include_other
                && !b.st_highlight
        );
        assert_eq!(b.selected, Some((0, 5)));
        assert_eq!(b.selected_track, Some(2));
        assert_eq!(b.t_now, a.t_now.round());
        assert_eq!(b.memory.zoom(), 6.5);
        let c = b.memory.detached().expect("map centred from the link");
        assert!((c.y() - 45.5).abs() < 1e-9 && (c.x() - 7.25).abs() < 1e-9);
        // Everything else encodes identically (map / cam are applied on the first frame).
        b.map_center = a.map_center;
        b.view3d.cam = a.view3d.cam.clone();
        b.view3d.exag = a.view3d.exag;
        b.view3d.auto_exag = a.view3d.auto_exag;
        assert_eq!(
            without(&a.share_map(true), &[]),
            without(&b.share_map(true), &[])
        );
    }

    #[test]
    fn link_without_data_or_with_bad_data_is_harmless() {
        let mut b = WingApp::blank();
        b.start_from_share(&ShareMap::decode(
            "#v=1&tab=3d&emb=%%%garbage&cb=Nope&t=abc",
        ));
        assert!(b.tracks.is_empty());
        assert_eq!(b.view_tab, ViewTab::ThreeD);
        assert_eq!(b.color_by, ColorBy::Individual);
    }
}
