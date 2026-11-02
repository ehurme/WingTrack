#!/usr/bin/env python3
"""Build compact per-year IBTrACS files for WingTrack's cyclone layer.

NOAA's IBTrACS servers don't allow browser (CORS) requests, so CI downloads the
best-track archive from NCEI and publishes small per-year CSVs next to the web app on
GitHub Pages (same origin, so the app can always read them):

    python3 tools/ibtracs_years.py dist/data/ibtracs              # download from NCEI
    python3 tools/ibtracs_years.py out/ --source ibtracs.csv      # local file (tests)

Keeps main tracks (drops spurs), one row per position with the columns
SID, NAME, BASIN, ISO_TIME, LAT, LON, USA_WIND, WMO_WIND, USA_PRES, WMO_PRES, USA_SSHS,
NATURE (a storm crossing New Year appears in both years; the app merges by SID), and
writes index.json with the years and the build date. Standard library only.
"""

import argparse
import csv
import datetime as dt
import io
import json
import os
import sys
import urllib.request

BASE = (
    "https://www.ncei.noaa.gov/data/international-best-track-archive-for-climate-stewardship-ibtracs/"
    "v04r01/access/csv/"
)
FILES = ["ibtracs.since1980.list.v04r01.csv", "ibtracs.last3years.list.v04r01.csv"]
KEEP = [
    "SID", "NAME", "BASIN", "ISO_TIME", "LAT", "LON", "USA_WIND", "WMO_WIND",
    "USA_PRES", "WMO_PRES", "USA_SSHS", "NATURE",
]


def open_source(src):
    if src:
        return open(src, newline="", encoding="utf-8", errors="replace"), src
    last = None
    for f in FILES:
        url = BASE + f
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "WingTrack CI (github.com/ehurme/WingTrack)"})
            resp = urllib.request.urlopen(req, timeout=300)
            return io.TextIOWrapper(resp, encoding="utf-8", errors="replace", newline=""), url
        except Exception as e:  # try the next file
            last = e
            print(f"could not fetch {url}: {e}", file=sys.stderr)
    raise SystemExit(f"IBTrACS download failed: {last}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out_dir")
    ap.add_argument("--source", help="local IBTrACS CSV instead of downloading")
    a = ap.parse_args()
    fh, src = open_source(a.source)
    rd = csv.reader(fh)
    header = [h.strip() for h in next(rd)]
    idx = {h: i for i, h in enumerate(header)}
    missing = [k for k in ["SID", "ISO_TIME", "LAT", "LON"] if k not in idx]
    if missing:
        raise SystemExit(f"not an IBTrACS file (missing {missing})")
    tt = idx.get("TRACK_TYPE")
    years = {}
    n = 0
    for row in rd:
        if len(row) < len(header):
            continue
        try:
            float(row[idx["LAT"]])
        except ValueError:
            continue  # units row
        if tt is not None and row[tt].strip().lower().startswith("spur"):
            continue
        y = row[idx["ISO_TIME"]][:4]
        if not y.isdigit():
            continue
        years.setdefault(y, []).append([row[idx[k]].strip() if k in idx else "" for k in KEEP])
        n += 1
    os.makedirs(a.out_dir, exist_ok=True)
    for y, rows in sorted(years.items()):
        with open(os.path.join(a.out_dir, f"{y}.csv"), "w", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            w.writerow(KEEP)
            w.writerows(rows)
    with open(os.path.join(a.out_dir, "index.json"), "w", encoding="utf-8") as f:
        json.dump(
            {
                "source": src,
                "built": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%MZ"),
                "years": sorted(int(y) for y in years),
                "positions": n,
            },
            f,
        )
    print(f"IBTrACS: {n} positions in {len(years)} year files → {a.out_dir}")


if __name__ == "__main__":
    main()
