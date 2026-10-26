# WingTrack

An interactive animal flight tracker written in Rust — in the spirit of
[BirdTracker](https://dp-00.github.io/BirdTracker/), but built for**Sigfox tags (TinyFox, NanoFox, uWasp)**. It overlays
**daily satellite imagery** and annotates every fix with **ERA5 wind and weather**, including
wind support along each step — the number that matters for Sigfox fixes taken hours apart at the roost.

Runs as a native desktop app (Linux / macOS / Windows) and, from the same code, in the browser
(WebAssembly, deployable to GitHub Pages).

![WingTrack with the demo data](assets/screenshot.png)

*Demo data shown with an offline test wind field (`WINGTRACK_SYNTHETIC_WIND=1`); with network access the
basemap, daily satellite imagery and real ERA5 data load from the services below.*

## Features

- **Firmware-aware import** — auto-detects the format of each CSV, with a per-file override:

  | Profile                                | What it reads                                                                                                                                                                           |
  | -------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | Wildcloud · TinyFox                   | `Total VeDBA` (cumulative → per-message difference), 24 h activity %, 24 h min/max temperature, 24 h min pressure → max altitude                                                    |
  | Wildcloud · NanoFox 30Days            | 5 × 36-min VeDBA and temperature bins (placed at their window centres), 3 h min pressure → max altitude                                                                               |
  | Wildcloud · NanoFox FineScalePressure | 5 × VeDBA bins + 5 instantaneous pressure readings → barometric altitude at transmission                                                                                              |
  | Movebank · Sigfox                     | `wildcloud_to_movebank()` uploads: Atlas locations + radius, separate VeDBA / pressure / temperature rows (3 h min pressure joined to fixes like `add_min_pressure_to_locations()`) |
  | Movebank · GPS                        | `height-above-msl/ellipsoid`, `ground-speed`, `heading`, `gps:hdop`; drops rows flagged invisible/outlier                                                                       |
  | CTT · cellular GPS                    | CTT portal exports (Time of Fix, Lat/Lng, Altitude, Speed in knots, COG, HDOP, satellites, Nav Type/Error codes) and legacy `GPS_YYYY-MM-DD_HH:MM:SS` files                           |
  | Generic                                | any lat/lon/time CSV, e.g. `flatten_movedata()` output; `era5_*` / `wind_support_flight` columns from `annotate_era5()` are shown as *imported* weather                       |

  Column matching mirrors `harmonize_wc_columns()` in [SigfoxTagPrep](https://github.com/ehurme/SigfoxTagPrep)
  (comma/semicolon/tab delimiters, BOMs, `14.06.2025, 17:56:24` timestamps, `7000 (2/1)` radius strings, strongest RSSI from the base-station field).
- **Sigfox-aware display** — location-error circles from the Atlas radius, a max-radius filter,
  dashed steps where the movement is smaller than the combined location error, and an option to hide them.
- **Satellite layers** — Esri World Imagery / OSM / topo / dark basemaps, plus **NASA GIBS daily imagery
  (MODIS Terra, MODIS Aqua, VIIRS)** that switches date with the time cursor, so you can see the fronts the animals crossed.
- **ERA5 wind & weather** (via the [Open-Meteo Historical Weather API](https://open-meteo.com/en/docs/historical-weather-api)):

  - 10 m / 100 m wind, 2 m temperature, MSL & surface pressure, precipitation, cloud cover per fix
  - level chosen from height above ground (barometric: tag pressure vs ERA5 surface pressure; GPS: altitude vs ERA5-derived ground elevation)
  - **wind support, crosswind, airspeed** relative to the step heading
  - **along-step wind support**: hourly samples along the great-circle path between fixes, optionally **night hours only** (sun < −6°)
  - **barometric altitude re-computed with the local ERA5 MSLP** instead of the 1013.25 hPa standard atmosphere
  - ERA5 wind field (arrows) for the hour under the cursor; requests batched by month, 25 grid cells per call, and cached
- **Timeline & charts** — play/pause at 10 min/s … 1 week/s, step fix-by-fix (← →, Space),
  linked chart of altitude, speeds, wind components, ERA5 variables, location error or raw sensor
  streams (VeDBA, tag temperature, pressure, activity). Click the chart to jump in time; click a fix on the map to inspect it,
  with a day/twilight/night indicator for the selected animal.
- **Export** — one CSV with every derived column (`wind_support_ms`, `segment_wind_support_ms`, `altitude_era5_msl_corrected_m`, …)
  and Movebank-style names, ready for R:

  ```r
  library(move2)
  d <- read.csv("wingtrack_annotated.csv")
  m <- mt_as_move2(d, coords = c("location_long", "location_lat"), time_column = "timestamp",
                   track_id_column = "individual_local_identifier", crs = 4326,
                   na.fail = FALSE)
  ```

## Running

```bash
# cd to wingtrack folder
cargo run --release
WINGTRACK_DEMO=1 cargo run --release          # start with the simulated demo files loaded

# Tests (parsers, weather batching/annotation, geodesy, export round-trip)
cargo test --lib

# Web (needs: rustup target add wasm32-unknown-unknown; cargo install trunk)
trunk serve --release                          # http://127.0.0.1:8080
```

Pushing to `main` on GitHub runs `.github/workflows/ci.yml`: tests + clippy, desktop builds for
Linux/macOS/Windows (downloadable artifacts), and a web build deployed to GitHub Pages
(enable *Settings → Pages → Source: GitHub Actions*).

Load data with **Open…** or by dragging CSV files onto the window; **Load demo** loads three simulated
files (a NanoFox 30Days Wildcloud export with two Leisler's bats, a TinyFox export, a CTT GPS export) — simulated tracks, not real data.

## Adding a tag type / firmware

Implement the `Firmware` trait in `src/firmware/` and add it to `registry()`:

```rust
pub trait Firmware: Send + Sync {
    fn id(&self) -> &'static str;          // stable id, written to exports
    fn label(&self) -> &'static str;       // shown in the per-file dropdown
    fn description(&self) -> &'static str;
    fn detect(&self, t: &Table) -> f32;    // 0..1 confidence from the header (and rows)
    fn parse(&self, t: &Table) -> Result<ParseOutput, String>;
}
```

`Table` gives tolerant column lookup on normalised headers (`"Radius (m) (Source/Status)"` → `radius_m_source_status`).
Fixes carry location source and error, altitude and its source (GPS, instantaneous or window-minimum pressure),
and sensor samples go into `Track::sensors` so they appear in the chart automatically.

## Notes and limits

- ERA5 single levels stop at **100 m**; bats often fly higher. For pressure-level winds matched to flight
  altitude use `annotate_era5()` in SigfoxTagPrep and load the result — WingTrack shows those columns as imported weather.
- Open-Meteo is free for non-commercial use (≈10 000 calls/day; a multi-location month request counts as several calls).
  Along-step sampling adds grid cells between fixes; turn it off for very large GPS datasets.
- ERA5 is available with a ~5–6 day delay; newer fixes stay un-annotated.
- GIBS true-colour layers are daytime overpasses — useful for synoptic weather, not for the night itself.
- Coordinates and times are treated as WGS84 and UTC.

## Code layout

| File               | Role                                                                                |
| ------------------ | ----------------------------------------------------------------------------------- |
| `src/model.rs`   | `Track`, `Fix`, `SensorSample`, weather annotation struct                     |
| `src/table.rs`   | CSV reader (delimiter sniffing, header normalisation, timestamp formats)            |
| `src/firmware/`  | one module per data source; `mod.rs` holds the trait, registry and shared helpers |
| `src/weather.rs` | ERA5 request planning, response cache, wind support / along-step annotation         |
| `src/geo.rs`     | great-circle maths, solar elevation, barometric altitude, wind vectors              |
| `src/tiles.rs`   | basemaps and NASA GIBS date-specific tile sources                                   |
| `src/app.rs`     | egui UI: map plugin, timeline, charts, export                                       |
| `src/demo.rs`    | simulated demo files in raw export formats                                          |

MIT licence. Map data © OpenStreetMap contributors, Esri, CARTO; imagery NASA EOSDIS GIBS; weather ERA5 (Copernicus) via Open-Meteo.
