//! Core data model shared by all firmware parsers, the weather annotator and the UI.
//!
//! Every parser turns its input into a list of [`Track`]s. A track is one animal / tag
//! deployment and holds location [`Fix`]es plus non-location [`SensorSample`]s (VeDBA,
//! temperature, pressure bins that Sigfox tags send alongside or instead of positions).

/// How a fix was located. Drives how much the UI and the annotator trust it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationSource {
    /// Satellite GPS fix (Movebank GPS, CTT, ...).
    Gps,
    /// Sigfox "Atlas" network location estimate (radius of hundreds of metres to tens of km).
    SigfoxAtlas,
    /// Position of a receiving station (e.g. a single Sigfox base station or radio node).
    Station,
    /// Anything else (e.g. capture site, manually entered).
    Other,
}

impl LocationSource {
    pub fn label(self) -> &'static str {
        match self {
            LocationSource::Gps => "GPS",
            LocationSource::SigfoxAtlas => "Sigfox Atlas",
            LocationSource::Station => "Station",
            LocationSource::Other => "Other",
        }
    }
}

/// Where an altitude value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AltitudeSource {
    /// GPS height (above ellipsoid / MSL as delivered by the tag).
    Gps,
    /// Barometric altitude from an instantaneous pressure reading.
    PressureInstant,
    /// Barometric altitude from a *minimum* pressure over a window
    /// (NanoFox: 3 h, TinyFox: 24 h) i.e. the maximum altitude reached in that window.
    PressureWindowMin,
}

impl AltitudeSource {
    pub fn label(self) -> &'static str {
        match self {
            AltitudeSource::Gps => "GPS height",
            AltitudeSource::PressureInstant => "barometric",
            AltitudeSource::PressureWindowMin => "barometric (window max)",
        }
    }
}

/// Environmental annotation of a single fix (filled by [`crate::weather`]).
#[derive(Clone, Debug, Default)]
pub struct WeatherAnno {
    /// Wind u/v (m/s, *towards* east / north) at the level used for this fix.
    pub u: Option<f32>,
    pub v: Option<f32>,
    /// Which level the wind was taken from ("10m" / "100m" / "imported").
    pub level: Option<&'static str>,
    pub wind_speed: Option<f32>,
    /// Meteorological wind direction (degrees the wind blows *from*).
    pub wind_from_deg: Option<f32>,
    /// Tailwind (+) / headwind (−) component along the track heading (m/s).
    pub wind_support: Option<f32>,
    /// Crosswind component (m/s, + = from the left / pushing to the right).
    pub crosswind: Option<f32>,
    /// Airspeed estimate = |ground velocity − wind velocity| (m/s).
    pub airspeed: Option<f32>,
    pub temp_2m: Option<f32>,
    pub msl_hpa: Option<f32>,
    pub surface_pressure_hpa: Option<f32>,
    pub precipitation_mm: Option<f32>,
    pub cloud_cover: Option<f32>,
    /// Mean wind support / crosswind along the segment that *ended* at this fix,
    /// sampled hourly along the interpolated path (night hours only if requested).
    pub seg_wind_support: Option<f32>,
    pub seg_crosswind: Option<f32>,
    pub seg_hours: Option<u16>,
    /// Barometric altitude recomputed with the local ERA5 mean-sea-level pressure.
    pub alt_msl_corrected: Option<f32>,
}

/// One location fix.
#[derive(Clone, Debug)]
pub struct Fix {
    /// Unix seconds, UTC.
    pub t: i64,
    pub lat: f64,
    pub lon: f64,
    pub source: LocationSource,
    /// Horizontal error radius in metres (Sigfox radius, GPS HDOP-derived, ...).
    pub error_m: Option<f32>,
    pub alt_m: Option<f32>,
    pub alt_source: Option<AltitudeSource>,
    /// Pressure (hPa) the altitude was derived from, if barometric.
    pub pressure_hpa: Option<f32>,
    /// Ground speed / heading reported by the tag (GPS units only).
    pub reported_speed_ms: Option<f32>,
    pub reported_heading: Option<f32>,
    pub temp_c: Option<f32>,
    /// Activity, in the units the tag delivers (VeDBA sum, activity %, ...).
    pub activity: Option<f32>,
    /// Derived from consecutive fixes.
    pub step_m: Option<f32>,
    pub dt_s: Option<f32>,
    pub ground_speed_ms: Option<f32>,
    /// Bearing (degrees from north) of the step *arriving* at this fix.
    pub bearing_deg: Option<f32>,
    /// True if the step is larger than the combined location error (i.e. real movement).
    pub step_significant: bool,
    pub weather: WeatherAnno,
    /// Extra columns kept for export / display (name, value).
    pub extra: Vec<(String, String)>,
}

impl Fix {
    pub fn new(t: i64, lat: f64, lon: f64, source: LocationSource) -> Self {
        Fix {
            t,
            lat,
            lon,
            source,
            error_m: None,
            alt_m: None,
            alt_source: None,
            pressure_hpa: None,
            reported_speed_ms: None,
            reported_heading: None,
            temp_c: None,
            activity: None,
            step_m: None,
            dt_s: None,
            ground_speed_ms: None,
            bearing_deg: None,
            step_significant: true,
            weather: WeatherAnno::default(),
            extra: Vec::new(),
        }
    }

    /// The best altitude estimate available (ERA5-corrected barometric first).
    pub fn best_alt(&self) -> Option<f32> {
        self.weather.alt_msl_corrected.or(self.alt_m)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SensorKind {
    Vedba,
    Temperature,
    Pressure,
    Activity,
}

impl SensorKind {
    pub fn label(self) -> &'static str {
        match self {
            SensorKind::Vedba => "VeDBA",
            SensorKind::Temperature => "Tag temperature (°C)",
            SensorKind::Pressure => "Pressure (hPa)",
            SensorKind::Activity => "Activity (%)",
        }
    }
}

/// A non-location measurement (e.g. one of the five 36-min VeDBA bins of a NanoFox message).
#[derive(Clone, Debug)]
pub struct SensorSample {
    /// Centre of the measurement window, unix seconds UTC.
    pub t: i64,
    pub kind: SensorKind,
    pub value: f32,
}

/// One animal / tag deployment.
#[derive(Clone, Debug)]
pub struct Track {
    pub id: String,
    /// Tag hardware (TinyFox, NanoFox, uWasp, CTT, GPS, ...).
    pub tag: String,
    /// Firmware profile that parsed it (see [`crate::firmware`]).
    pub firmware: String,
    pub species: Option<String>,
    pub fixes: Vec<Fix>,
    pub sensors: Vec<SensorSample>,
    pub visible: bool,
    pub color: [u8; 3],
    /// Source file name (set by the app; used to re-parse with a different firmware profile).
    pub file: String,
}

impl Track {
    pub fn new(id: impl Into<String>, tag: impl Into<String>, firmware: impl Into<String>) -> Self {
        Track {
            id: id.into(),
            tag: tag.into(),
            firmware: firmware.into(),
            species: None,
            fixes: Vec::new(),
            sensors: Vec::new(),
            visible: true,
            color: [200, 200, 200],
            file: String::new(),
        }
    }

    pub fn time_range(&self) -> Option<(i64, i64)> {
        let first = self.fixes.first()?.t;
        let last = self.fixes.last()?.t;
        Some((first, last))
    }

    /// Sort fixes/sensors by time, drop exact duplicates, and compute step metrics.
    pub fn finalize(&mut self) {
        self.fixes.sort_by_key(|f| f.t);
        self.fixes.dedup_by(|a, b| {
            a.t == b.t && (a.lat - b.lat).abs() < 1e-9 && (a.lon - b.lon).abs() < 1e-9
        });
        self.sensors.sort_by_key(|s| (s.kind, s.t));
        self.sensors.dedup_by(|a, b| a.kind == b.kind && a.t == b.t);
        self.compute_steps();
    }

    pub fn compute_steps(&mut self) {
        for i in 0..self.fixes.len() {
            if i == 0 {
                let f = &mut self.fixes[0];
                f.step_m = None;
                f.dt_s = None;
                f.ground_speed_ms = f.reported_speed_ms;
                f.bearing_deg = f.reported_heading;
                continue;
            }
            let (a, b) = {
                let (l, r) = self.fixes.split_at_mut(i);
                (&l[i - 1], &mut r[0])
            };
            let d = crate::geo::haversine_m(a.lat, a.lon, b.lat, b.lon);
            let dt = (b.t - a.t) as f64;
            b.step_m = Some(d as f32);
            b.dt_s = Some(dt as f32);
            b.ground_speed_ms = b.reported_speed_ms.or(if dt > 0.0 {
                Some((d / dt) as f32)
            } else {
                None
            });
            b.bearing_deg = b.reported_heading.or(Some(crate::geo::bearing_deg(
                a.lat, a.lon, b.lat, b.lon,
            ) as f32));
            let err = a.error_m.unwrap_or(0.0) + b.error_m.unwrap_or(0.0);
            b.step_significant = (d as f32) > err.max(1.0);
        }
    }

    /// Interpolated position at time `t` (None outside the track's time range).
    pub fn position_at(&self, t: f64) -> Option<(f64, f64, usize)> {
        let fx = &self.fixes;
        if fx.is_empty() || t < fx[0].t as f64 || t > fx[fx.len() - 1].t as f64 {
            return None;
        }
        let idx = fx.partition_point(|f| (f.t as f64) <= t);
        if idx == 0 {
            return Some((fx[0].lat, fx[0].lon, 0));
        }
        if idx >= fx.len() {
            let l = &fx[fx.len() - 1];
            return Some((l.lat, l.lon, fx.len() - 1));
        }
        let a = &fx[idx - 1];
        let b = &fx[idx];
        let span = (b.t - a.t) as f64;
        let w = if span > 0.0 {
            (t - a.t as f64) / span
        } else {
            0.0
        };
        let (lat, lon) = crate::geo::interpolate(a.lat, a.lon, b.lat, b.lon, w);
        Some((lat, lon, idx - 1))
    }
}

/// Result of parsing one file.
#[derive(Debug, Default)]
pub struct ParseOutput {
    pub tracks: Vec<Track>,
    pub warnings: Vec<String>,
}
