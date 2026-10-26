# WingTrack — agent guide

WingTrack is an interactive animal flight tracker written in Rust. It targets Sigfox tags (TinyFox, NanoFox, uWasp), GPS tags and CTT cellular GPS tags, overlaying daily satellite imagery and ERA5 wind/weather reanalysis. It runs as a native desktop app (Linux, macOS, Windows) and, from the same source, in the browser via WebAssembly.

This file is written for AI coding agents. Assume the reader knows nothing about the project.

## Technology stack

- **Language:** Rust, edition 2024, minimum toolchain 1.95.
- **Build system:** Cargo.
- **UI framework:** egui + eframe. The map is rendered with `walkers`; charts with `egui_plot`.
- **Web build:** `trunk`, target `wasm32-unknown-unknown`.
- **HTTP:** `ehttp` (works on native and WASM).
- **CSV parsing:** `csv` + custom tolerant header normalisation in `src/table.rs`.
- **Date/time:** `chrono`.
- **Video export:** `gif` crate for animated GIF; MP4 is produced by piping raw RGBA frames to `ffmpeg` (desktop only).
- **Weather data:** Open-Meteo Historical Weather API (ERA5 / best-match) for 10 m / 100 m wind and surface weather;
  Open-Meteo Historical Forecast API (GFS / ICON / IFS pressure levels 1000–700 hPa) or a user-exported ERA5
  "wind cube" (CSV/CSV.gz, any pressure levels) for 3D winds. `flate2` reads gzipped cubes.
- **Tile sources:** Esri World Imagery / Topo, OpenStreetMap, CARTO dark; NASA GIBS for daily MODIS/VIIRS imagery.

## Repository layout

```
.
├── Cargo.toml              # Rust package manifest
├── Cargo.lock              # Pinned dependencies
├── index.html              # trunk entry point for the WASM build
├── .github/workflows/ci.yml# CI: test, clippy, desktop builds, GitHub Pages deploy
├── README.md               # User-facing documentation
├── assets/screenshot.png   # README asset
├── src/
│   ├── main.rs             # Native / WASM entry points
│   ├── lib.rs              # Public module exports
│   ├── app.rs              # egui application: map, timeline, charts, export, video
│   ├── model.rs            # Core data model: Track, Fix, SensorSample, WeatherAnno
│   ├── table.rs            # Tolerant CSV reader (delimiter sniffing, header normalisation, timestamp parsing)
│   ├── firmware/           # One module per data source
│   │   ├── mod.rs          # Firmware trait, registry, shared helpers
│   │   ├── wildcloud.rs    # TinyFox / NanoFox 30Days / NanoFox FineScalePressure
│   │   ├── movebank.rs     # Movebank Sigfox and GPS
│   │   ├── ctt.rs          # CTT cellular GPS
│   │   ├── generic.rs      # Any lat/lon/time CSV
│   │   └── tests.rs        # Parser unit tests
│   ├── weather.rs          # Open-Meteo request planning, cache, ERA5 annotation
│   ├── geo.rs              # Great-circle maths, solar elevation, barometric altitude, wind vectors
│   ├── tiles.rs            # Basemap and NASA GIBS tile sources
│   ├── video.rs            # Frame capture/cropping, MP4 (ffmpeg) and GIF encoding
│   ├── wind3d.rs           # Pressure-level profiles: Open-Meteo + ERA5-cube sources, flight-level annotation, curtains
│   ├── view3d.rs           # CPU-projected 3D view: ground tiles, wind raster, level planes, particles, tracks at altitude
│   ├── export.rs           # Annotated CSV export (Movebank-like column names)
│   └── demo.rs             # Simulated demo datasets in raw export formats
└── tools/era5_wind_cube.py # Python: export an ERA5 pressure-level wind cube from download_era5.py folders
```

## Build, run, and test commands

All commands run from the repository root.

### Native desktop

```bash
# Build and run (release mode is strongly recommended; debug is very slow)
cargo run --release

# Compile only
cargo build --release

# Run with the simulated demo files already loaded
WINGTRACK_DEMO=1 cargo run --release        # macOS / Linux / Git Bash
$env:WINGTRACK_DEMO=1; cargo run --release  # PowerShell
set WINGTRACK_DEMO=1 && cargo run --release # cmd

# Offline synthetic wind field for UI testing (not ERA5)
WINGTRACK_SYNTHETIC_WIND=1 cargo run --release
```

The compiled binary is written to `target/release/wingtrack` (`target/release/wingtrack.exe` on Windows).

### WebAssembly

```bash
rustup target add wasm32-unknown-unknown
cargo install --locked trunk
trunk serve --release
```

Then open http://127.0.0.1:8080.

### Tests

```bash
# Library unit tests only (parsers, weather batching/annotation, geodesy, export round-trip, video encoding)
cargo test --lib

# Clippy (CI-enforced)
cargo clippy --all-targets -- -D warnings
```

### Video export

MP4 export needs `ffmpeg` on PATH. GIF export works everywhere. On Windows the app spawns `ffmpeg` with the `CREATE_NO_WINDOW` flag.

## Code organisation and architecture

### Data flow

1. `table.rs` reads raw bytes into a `Table` with normalised headers and tolerant timestamp parsing.
2. `firmware/mod.rs` ranks all `Firmware` profiles by `detect()` confidence and calls the best match's `parse()`.
3. Each parser produces `model::Track`s: one track per animal/tag, containing `Fix`es (locations) and `SensorSample`s (non-location sensor streams).
4. `Track::finalize()` sorts and deduplicates fixes/sensors and computes step distances, bearings, ground speeds and `step_significant`.
5. `weather.rs` plans Open-Meteo requests, caches responses in `WeatherStore`, and annotates every fix with wind, weather, along-step wind support and ERA5-corrected barometric altitude.
6. `wind3d.rs` keeps pressure-level profiles in a `ProfileStore` (cell → hour → per-level u, v, geopotential height,
   temperature, plus surface pressure) and annotates fixes with `FlightWind` (flight-level and best-level wind,
   named as in `SigfoxTagPrep::annotate_era5()`). `view3d.rs` draws the *3D winds* tab from a `FieldSlice`.
7. `app.rs` renders the map, timeline, charts and export UI; it drives asynchronous HTTP weather fetches and video recording.
8. `export.rs` writes a single CSV with all derived columns (including the flight-level ones).

### Key abstractions

- `firmware::Firmware` trait (`src/firmware/mod.rs:26`): the extension point for new tag types. Implement `id`, `label`, `description`, `detect` and `parse`, then add the struct to `registry()`.
- `model::Fix`: one location fix. Carries source (`Gps`, `SigfoxAtlas`, `Station`, `Other`), error radius, altitude source, pressure, reported speed/heading, activity, step metrics and a `WeatherAnno`.
- `model::SensorSample`: non-location measurement (VeDBA, temperature, pressure, activity) with a timestamp so it can be plotted.
- `weather::WeatherStore`: cell → hour → values cache. Bins points into 0.25° ERA5 grid cells; requests are grouped by calendar month, up to 25 cells per call.
- `wind3d::ProfileStore`: pressure-level cache shared by both 3D-wind sources. `sample()` tries the exact 0.25° cell,
  then bilinear interpolation between regional-field nodes spaced `node_step` cells apart. Levels below the ground
  (level pressure > surface pressure) are masked.
- `view3d::View3D`: camera, particles and display options; `show()` takes a `Scene` borrowed from the app each frame.
  Everything is projected on the CPU and painted with egui meshes/lines, so it needs no GPU code and runs on WASM.
- Wind fields (2D surface and 3D pressure levels) are fetched lazily around the animals with
  `weather::window_points()` (cells within a radius of each animal's position, a week at a time from the cursor day).
  `app.rs::pump_weather()` throttles requests by their estimated Open-Meteo call weight (`weather::call_weight`:
  locations × max(1, vars/10) × max(1, days/14)) to 450/min and 4200/h, and re-queues jobs after HTTP 429.
- `table::Table`: tolerant column lookup by aliases (`t.col(&[...])`) and prefix (`t.col_prefix(...)`).

## Development conventions

- **Edition and MSRV:** Rust 2024, `rust-version = "1.95"` in `Cargo.toml`.
- **Spelling:** British English in comments and documentation (`colour`, `behaviour`, `centre`), although egui API names use American spelling.
- **Formatting:** Use `cargo fmt`. CI runs `cargo clippy --all-targets -- -D warnings`.
- **Documentation:** Modules have `//!` doc comments explaining their role. Keep them in sync with code changes.
- **Error handling:** Parser errors are returned as `String`. Background tasks send status messages through an `mpsc` channel to the UI thread.
- **Platform conditionals:** Use `#[cfg(not(target_arch = "wasm32"))]` for native-only code (file dialogs, `ffmpeg`, threads) and `#[cfg(target_arch = "wasm32")]` for web-only code (`rfd` async file dialog, `web_sys` downloads).
- **Dependencies:** confirm a crate is in `Cargo.toml` before using it; match the version and feature set already in use. The project avoids heavy web frameworks and uses egui's immediate-mode GUI idioms.

## Testing strategy

- Unit tests live inline in `#[cfg(test)] mod tests` blocks within each source file.
- `src/firmware/tests.rs` exercises every firmware profile with small synthetic CSVs, including real-world header variants (semicolon delimiters, unit suffixes, BOMs).
- `src/table.rs` tests header normalisation, delimiter sniffing, split-header merging and timestamp formats.
- `src/geo.rs` tests haversine distances, bearings, solar elevation, wind components and barometric altitude.
- `src/weather.rs` tests request planning, JSON ingestion, annotation and synthetic-field generation.
- `src/export.rs` tests a round-trip through the generic parser.
- `src/video.rs` tests frame cropping/downscaling, GIF round-trip and (if `ffmpeg` is available) MP4 encoding.
- `src/tiles.rs` tests tile URL generation.
- `src/wind3d.rs` tests Open-Meteo request URLs/planning, JSON ingestion, ln-p and height interpolation, underground
  masking, flight-level annotation, curtains, field slices and CSV/gzip cube import. An ignored test reads a real cube:
  `WINGTRACK_CUBE=cube.csv.gz cargo test --lib -- --ignored cube_file --nocapture`.
- `src/view3d.rs` tests the camera projection and tile maths.
- There are no integration tests or UI tests; the desktop and web apps are exercised manually.

## CI and deployment

`.github/workflows/ci.yml` runs on every push to `main` and on pull requests:

1. **test** job on Ubuntu: `cargo test --lib` and `cargo clippy --all-targets -- -D warnings`.
2. **desktop** job on Ubuntu/macOS/Windows: `cargo build --release` and uploads binaries as artifacts (`wingtrack-<os>`).
3. **web** job on Ubuntu: installs the WASM target and `trunk`, runs `trunk build --release --public-url ./`, and uploads `dist/` as a Pages artifact.
4. **deploy-pages** job deploys the web artifact to GitHub Pages (only from `main`).

Enable GitHub Pages via *Settings → Pages → Source: GitHub Actions*.

## Security considerations

- The app fetches remote tile images, satellite imagery and Open-Meteo weather data over HTTPS. No API keys are required.
- File I/O is limited to user-selected CSV files via `rfd` and explicit export paths.
- `ffmpeg` is spawned as a child process for MP4 export; the path is fixed to `"ffmpeg"` and no user input reaches the command line beyond the output file path chosen by the save dialog.
- Web downloads use `Blob`/`URL.createObjectURL` in the browser; no server-side component exists.
- No secrets, credentials or private keys are stored in the repository. `.gitignore` excludes `/target` and `/dist`.

## How to add a firmware profile

1. Add a new module under `src/firmware/`.
2. Implement `firmware::Firmware`:
   - `id()`: stable identifier used in overrides and exports.
   - `label()`: shown in the per-file dropdown.
   - `description()`: one-line tooltip.
   - `detect(t: &Table) -> f32`: confidence 0..1 from headers/rows.
   - `parse(t: &Table) -> Result<ParseOutput, String>`: build `Track`s.
3. Add the struct to `firmware::registry()` in `src/firmware/mod.rs`.
4. Add a unit test in `src/firmware/tests.rs` with a minimal representative CSV.
5. Update this `AGENTS.md` and `README.md` if the feature is user-facing.

## Current state

`main` builds and passes `cargo test --lib` and `cargo clippy --all-targets -- -D warnings`. The 3D-winds feature
(`wind3d.rs`, `view3d.rs`, curtain/profile charts, `tools/era5_wind_cube.py`) was added on top of the
"wind tracks and video" commit.

## Useful environment variables

| Variable | Effect |
| --- | --- |
| `WINGTRACK_DEMO=1` | Load simulated demo files on startup |
| `WINGTRACK_SYNTHETIC_WIND=1` | Fill the weather store with a rotating synthetic wind field, and the pressure-level store with a sheared synthetic profile field, for offline UI testing (not real data) |
| `WINGTRACK_3D=1` / `=profile` | With the synthetic wind: open the *3D winds* tab with the curtain (or profile) chart |
| `WINGTRACK_CUBE=path` | Path for the ignored real-cube import test |

## External resources

- Repository: https://github.com/ehurme/wingtrack
- Open-Meteo Historical Weather API: https://open-meteo.com/en/docs/historical-weather-api
- Open-Meteo Historical Forecast API (pressure levels): https://open-meteo.com/en/docs/historical-forecast-api
- NASA GIBS: https://earthdata.nasa.gov/gibs
- egui: https://github.com/emilk/egui
- walkers: https://github.com/podusowski/walkers
