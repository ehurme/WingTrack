#!/usr/bin/env python3
"""
Export an ERA5 pressure-level "wind cube" for WingTrack's 3D wind view.

Reads the folder layout written by SigfoxTagPrep's ``python/download_era5.py``::

    <era5_dir>/pressure_levels/era5_wind_<LEVEL>hPa_<YYYY>_<MM>.grib|.nc   (u, v, t[, z])
    <era5_dir>/single_levels/era5_single_<YYYY>_<MM>.grib|.nc             (sp, msl, ...)

and writes one long table (gzip-compressed CSV) that WingTrack loads with
*3D wind → Import ERA5 cube…*::

    time,latitude,longitude,level,u,v,gh,t,sp
    2025-09-01T00:00:00Z,47.0,9.0,950,3.12,-4.80,512.3,12.41,965.2

Columns: level in hPa, u/v in m/s, gh = geopotential height (m above MSL),
t in °C, sp = surface pressure (hPa, used to hide levels below the ground).

If the pressure-level files contain geopotential (``z``) it is used directly
(add ``"geopotential"`` to ``pressure_level_variables`` in era5_config.json).
Otherwise heights are integrated hypsometrically from mean-sea-level pressure
and the temperature profile (dry air; error typically < 20 m below 3 km).

Examples
--------
    # area and time span taken from a track file (Movebank / WingTrack export / any lat-lon-time CSV)
    python era5_wind_cube.py --era5-dir //10.0.16.7/grpdechmann/Postdoc-EdwardHurme/EnvData \\
        --tracks wingtrack_annotated.csv --step 0.5 --out nyctalus_2025_cube.csv.gz

    # explicit box and dates, all levels between 1000 and 500 hPa, 3-hourly
    python era5_wind_cube.py --era5-dir EnvData --bbox 40 56 -2 20 \\
        --start 2025-08-15 --end 2025-10-15 --levels 1000 950 900 850 800 700 600 500 \\
        --hours 3 --step 1.0 --out autumn_2025.csv.gz

Requirements: xarray, pandas, numpy, cfgrib (for GRIB) or netCDF4/h5netcdf (for NetCDF).
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

import numpy as np
import pandas as pd
import xarray as xr

G0 = 9.80665
RD = 287.05

LAT_ALIASES = ["location_lat", "location-lat", "latitude", "lat"]
LON_ALIASES = ["location_long", "location-long", "longitude", "lon", "long", "lng"]
TIME_ALIASES = ["timestamp", "time", "datetime", "time (utc)", "time_utc"]


# --------------------------------------------------------------------------- reading


def _open(path: Path) -> xr.Dataset:
    """Open one ERA5 file (GRIB or NetCDF) and normalise coordinate names."""
    if path.suffix in (".grib", ".grb", ".grib2"):
        try:
            ds = xr.open_dataset(path, engine="cfgrib", backend_kwargs={"indexpath": ""})
        except Exception:  # mixed-level GRIB → several hypercubes
            import cfgrib

            parts = cfgrib.open_datasets(str(path), backend_kwargs={"indexpath": ""})
            ds = xr.merge([p.drop_vars([c for c in p.coords if c not in ("time", "latitude", "longitude")]) for p in parts],
                          compat="override")
    else:
        ds = xr.open_dataset(path)
    ren = {}
    if "valid_time" in ds.coords and "time" not in ds.dims:
        ren["valid_time"] = "time"
    for c in ("pressure_level", "isobaricInhPa", "level", "plev"):
        if c in ds.coords:
            ren[c] = "level"
            break
    ds = ds.rename(ren)
    # Drop helper coords cfgrib adds (number, step, surface, ...)
    keep = {"time", "latitude", "longitude", "level"}
    ds = ds.drop_vars([c for c in ds.coords if c not in keep and c not in ds.dims], errors="ignore")
    if "longitude" in ds.coords and float(ds.longitude.max()) > 180:
        ds = ds.assign_coords(longitude=((ds.longitude + 180) % 360) - 180).sortby("longitude")
    return ds


def _subset(ds: xr.Dataset, bbox, t0, t1, stride: int, hours: int) -> xr.Dataset:
    s, n, w, e = bbox
    lat_desc = bool(ds.latitude[0] > ds.latitude[-1])
    ds = ds.sel(latitude=slice(n, s) if lat_desc else slice(s, n), longitude=slice(w, e))
    ds = ds.sel(time=slice(t0, t1))
    if stride > 1:
        ds = ds.isel(latitude=slice(None, None, stride), longitude=slice(None, None, stride))
    if hours > 1:
        ds = ds.where(ds.time.dt.hour % hours == 0, drop=True)
    return ds


def _months(t0: pd.Timestamp, t1: pd.Timestamp):
    m = pd.Timestamp(year=t0.year, month=t0.month, day=1)
    while m <= t1:
        yield m.year, m.month
        m = (m + pd.offsets.MonthBegin(1)).normalize()


def _find(folder: Path, stem: str) -> Path | None:
    for ext in (".grib", ".nc", ".grb", ".grib2"):
        p = folder / f"{stem}{ext}"
        if p.exists():
            return p
    return None


def _pick(df: pd.DataFrame, aliases: list[str]) -> str | None:
    low = {c.lower().strip(): c for c in df.columns}
    for a in aliases:
        if a in low:
            return low[a]
    return None


def bbox_from_tracks(path: Path, margin: float):
    df = pd.read_csv(path, sep=None, engine="python")
    la, lo, tt = _pick(df, LAT_ALIASES), _pick(df, LON_ALIASES), _pick(df, TIME_ALIASES)
    if la is None or lo is None or tt is None:
        sys.exit(f"{path}: need latitude, longitude and timestamp columns (got {list(df.columns)})")
    lat = pd.to_numeric(df[la], errors="coerce")
    lon = pd.to_numeric(df[lo], errors="coerce")
    t = pd.to_datetime(df[tt], errors="coerce", utc=True, dayfirst=False)
    ok = lat.notna() & lon.notna() & t.notna()
    if not ok.any():
        sys.exit(f"{path}: no valid rows")
    bbox = (lat[ok].min() - margin, lat[ok].max() + margin, lon[ok].min() - margin, lon[ok].max() + margin)
    return bbox, t[ok].min().tz_convert(None).floor("D"), t[ok].max().tz_convert(None).ceil("D")


# --------------------------------------------------------------------------- heights


def hypsometric_heights(levels: np.ndarray, temp_k: np.ndarray, msl_hpa: np.ndarray) -> np.ndarray:
    """Geopotential height (m) of each pressure level, integrating up from MSL.

    levels: (L,) hPa sorted high→low pressure; temp_k: (..., L); msl_hpa: (...)"""
    z = np.empty_like(temp_k)
    p_prev = msl_hpa
    z_prev = np.zeros_like(msl_hpa)
    t_prev = temp_k[..., 0]
    for k, p in enumerate(levels):
        t_mean = 0.5 * (t_prev + temp_k[..., k])
        z_k = z_prev + RD * t_mean / G0 * np.log(p_prev / p)
        z[..., k] = z_k
        p_prev, z_prev, t_prev = np.full_like(msl_hpa, p), z_k, temp_k[..., k]
    return z


# --------------------------------------------------------------------------- main


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--era5-dir", required=True, type=Path, help="folder with pressure_levels/ and single_levels/")
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--tracks", type=Path, help="CSV whose lat/lon/time extent defines the cube")
    g.add_argument("--bbox", nargs=4, type=float, metavar=("S", "N", "W", "E"))
    ap.add_argument("--start", help="YYYY-MM-DD (with --bbox)")
    ap.add_argument("--end", help="YYYY-MM-DD (with --bbox)")
    ap.add_argument("--margin", type=float, default=2.0, help="degrees added around the tracks (default 2)")
    ap.add_argument("--step", type=float, default=0.5, help="output grid spacing in degrees (multiple of 0.25; default 0.5)")
    ap.add_argument("--hours", type=int, default=1, help="keep every N-th hour (default 1)")
    ap.add_argument("--levels", nargs="*", type=int, help="pressure levels (hPa); default: all files found")
    ap.add_argument("--out", type=Path, required=True, help="output .csv or .csv.gz")
    a = ap.parse_args(argv)

    if a.tracks:
        bbox, t0, t1 = bbox_from_tracks(a.tracks, a.margin)
    else:
        if not (a.start and a.end):
            ap.error("--bbox needs --start and --end")
        bbox, t0, t1 = tuple(a.bbox), pd.Timestamp(a.start), pd.Timestamp(a.end) + pd.Timedelta(days=1)
    stride = max(1, int(round(a.step / 0.25)))
    plev_dir, sl_dir = a.era5_dir / "pressure_levels", a.era5_dir / "single_levels"

    if a.levels:
        levels = sorted(set(a.levels), reverse=True)
    else:
        levels = sorted({int(p.name.split("_")[2].replace("hPa", "")) for p in plev_dir.glob("era5_wind_*hPa_*")}, reverse=True)
    if not levels:
        sys.exit(f"no pressure-level files in {plev_dir}")
    print(f"box S{bbox[0]:.2f} N{bbox[1]:.2f} W{bbox[2]:.2f} E{bbox[3]:.2f}, {t0:%Y-%m-%d} – {t1:%Y-%m-%d}, "
          f"levels {levels}, step {stride * 0.25:.2f}°, every {a.hours} h")

    frames = []
    for year, month in _months(t0, t1):
        per_level = []
        for lev in levels:
            f = _find(plev_dir, f"era5_wind_{lev}hPa_{year}_{month:02d}")
            if f is None:
                print(f"  missing {lev} hPa {year}-{month:02d} — skipped")
                continue
            ds = _subset(_open(f), bbox, t0, t1, stride, a.hours)
            if "level" in ds.dims:
                ds = ds.squeeze("level", drop=True)
            ds = ds.drop_vars([v for v in ds.data_vars if v not in ("u", "v", "t", "z")]).expand_dims(level=[lev])
            per_level.append(ds)
        if not per_level:
            continue
        pl = xr.concat(per_level, dim="level").sortby("level", ascending=False)
        sf = _find(sl_dir, f"era5_single_{year}_{month:02d}")
        sp = msl = None
        if sf is not None:
            sl = _subset(_open(sf), bbox, t0, t1, stride, a.hours)
            sp = sl["sp"] / 100.0 if "sp" in sl else None
            msl = sl["msl"] / 100.0 if "msl" in sl else None
        if "z" in pl:
            gh = pl["z"] / G0
        elif "t" in pl and msl is not None:
            tk = pl["t"].transpose("time", "latitude", "longitude", "level").values
            mslv = msl.reindex_like(pl["t"].isel(level=0), method="nearest").transpose("time", "latitude", "longitude").values
            z = hypsometric_heights(np.array(pl.level.values, dtype=float), tk, mslv)
            gh = xr.DataArray(z, coords=pl["t"].transpose("time", "latitude", "longitude", "level").coords,
                              dims=("time", "latitude", "longitude", "level"))
        else:
            gh = None
            print("  no geopotential and no msl/t — WingTrack will use standard-atmosphere heights")
        out = xr.Dataset({"u": pl["u"], "v": pl["v"]})
        if "t" in pl:
            out["t"] = pl["t"] - 273.15
        if gh is not None:
            out["gh"] = gh
        df = out.to_dataframe().reset_index()
        if sp is not None:
            spd = sp.to_dataframe(name="sp").reset_index()[["time", "latitude", "longitude", "sp"]]
            df = df.merge(spd, on=["time", "latitude", "longitude"], how="left")
        frames.append(df)
        print(f"  {year}-{month:02d}: {len(df):,} rows")

    if not frames:
        sys.exit("nothing exported")
    df = pd.concat(frames, ignore_index=True).dropna(subset=["u", "v"])
    df["time"] = pd.to_datetime(df["time"]).dt.strftime("%Y-%m-%dT%H:%M:%SZ")
    cols = [c for c in ["time", "latitude", "longitude", "level", "u", "v", "gh", "t", "sp"] if c in df]
    df = df[cols].round({"latitude": 3, "longitude": 3, "u": 2, "v": 2, "gh": 1, "t": 2, "sp": 1})
    a.out.parent.mkdir(parents=True, exist_ok=True)
    df.to_csv(a.out, index=False, compression="gzip" if a.out.suffix == ".gz" else None)
    print(f"wrote {a.out} ({len(df):,} rows, {a.out.stat().st_size / 1e6:.1f} MB)")


if __name__ == "__main__":
    main()
