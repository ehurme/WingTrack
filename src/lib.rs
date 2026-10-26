//! WingTrack — interactive animal flight tracker for Sigfox (TinyFox / NanoFox / uWasp),
//! GPS and CTT tags, with daily satellite imagery and ERA5 wind / weather annotation.
//!
//! The crate is split so the data side can be used without the GUI:
//! * [`table`] + [`firmware`] — read CSV exports and turn them into [`model::Track`]s
//! * [`weather`] — download ERA5 via Open-Meteo and annotate fixes with wind support etc.
//! * [`export`] — write annotated tracks back to CSV (readable by R / SigfoxTagPrep)
//! * [`video`] — MP4 / GIF export of the animated map and charts
//! * [`app`] — the egui application (native and web)

pub mod app;
pub mod demo;
pub mod export;
pub mod firmware;
pub mod geo;
pub mod model;
pub mod table;
pub mod tiles;
pub mod video;
pub mod view3d;
pub mod weather;
pub mod wind3d;

pub use app::WingApp;
