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
- **Weather data:** Open-Meteo Historical Weather API (ERA5 / best-match).
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
│   ├── export.rs           # Annotated CSV export (Movebank-like column names)
│   └── demo.rs             # Simulated demo datasets in raw export formats
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
6. `app.rs` renders the map, timeline, charts and export UI; it drives asynchronous HTTP weather fetches and video recording.
7. `export.rs` writes a single CSV with all derived columns.

### Key abstractions

- `firmware::Firmware` trait (`src/firmware/mod.rs:26`): the extension point for new tag types. Implement `id`, `label`, `description`, `detect` and `parse`, then add the struct to `registry()`.
- `model::Fix`: one location fix. Carries source (`Gps`, `SigfoxAtlas`, `Station`, `Other`), error radius, altitude source, pressure, reported speed/heading, activity, step metrics and a `WeatherAnno`.
- `model::SensorSample`: non-location measurement (VeDBA, temperature, pressure, activity) with a timestamp so it can be plotted.
- `weather::WeatherStore`: cell → hour → values cache. Bins points into 0.25° ERA5 grid cells; requests are grouped by calendar month, up to 25 cells per call.
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

## Current state (working tree)

As of the latest exploration, the working tree contains significant uncommitted work-in-progress changes (not yet committed to `main`):

- Video export support is being added in `src/video.rs` and wired into `src/app.rs`.
- `Cargo.toml`/`Cargo.lock` add the `gif` crate.
- `src/weather.rs` adds a wind-field grid (`field_cells`, `field_points`) so ERA5 wind arrows extend beyond the track fixes.
- `README.md` has been expanded with user documentation.

These changes do **not currently compile**. The failures observed are:

```text
error[E0599]: no method named `wind_palette` found for mutable reference `&mut WingApp`
    --> src/app.rs:1234, 1251, 1528

error[E0283]: type annotations needed for `Track::new("a", "NanoFox".into(), "")`
    --> src/weather.rs:721
```

The `wind_palette()` method is referenced in `src/app.rs` but is not defined, and `src/weather.rs` tests use `.into()` where the compiler cannot infer the target type. Run `cargo test --lib` and `cargo clippy --all-targets -- -D warnings` after fixing these before declaring the branch ready.

The last known-good commit on `main` is `7c708d1 functional on nanofox`.

## Useful environment variables

| Variable | Effect |
| --- | --- |
| `WINGTRACK_DEMO=1` | Load simulated demo files on startup |
| `WINGTRACK_SYNTHETIC_WIND=1` | Fill the weather store with a rotating synthetic wind field for offline UI testing |

## External resources

- Repository: https://github.com/ehurme/wingtrack
- Open-Meteo Historical Weather API: https://open-meteo.com/en/docs/historical-weather-api
- NASA GIBS: https://earthdata.nasa.gov/gibs
- egui: https://github.com/emilk/egui
- walkers: https://github.com/podusowski/walkers
