//! Basemaps and date-specific satellite imagery (NASA GIBS) as walkers tile sources.

use walkers::TileId;
use walkers::sources::{Attribution, TileSource};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Basemap {
    EsriImagery,
    OpenStreetMap,
    CartoDark,
    EsriTopo,
}

impl Basemap {
    pub const ALL: [Basemap; 4] = [
        Basemap::EsriImagery,
        Basemap::OpenStreetMap,
        Basemap::EsriTopo,
        Basemap::CartoDark,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Basemap::EsriImagery => "Satellite (Esri World Imagery)",
            Basemap::OpenStreetMap => "OpenStreetMap",
            Basemap::CartoDark => "Dark (CARTO)",
            Basemap::EsriTopo => "Topographic (Esri)",
        }
    }
}

impl TileSource for Basemap {
    fn tile_url(&self, t: TileId) -> String {
        match self {
            Basemap::EsriImagery => format!(
                "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{}/{}/{}",
                t.zoom, t.y, t.x
            ),
            Basemap::EsriTopo => format!(
                "https://server.arcgisonline.com/ArcGIS/rest/services/World_Topo_Map/MapServer/tile/{}/{}/{}",
                t.zoom, t.y, t.x
            ),
            Basemap::OpenStreetMap => format!(
                "https://tile.openstreetmap.org/{}/{}/{}.png",
                t.zoom, t.x, t.y
            ),
            Basemap::CartoDark => {
                format!(
                    "https://a.basemaps.cartocdn.com/dark_all/{}/{}/{}.png",
                    t.zoom, t.x, t.y
                )
            }
        }
    }
    fn attribution(&self) -> Attribution {
        let (text, url) = match self {
            Basemap::EsriImagery => ("Esri, Maxar, Earthstar Geographics", "https://www.esri.com"),
            Basemap::EsriTopo => (
                "Esri, HERE, Garmin, OpenStreetMap contributors",
                "https://www.esri.com",
            ),
            Basemap::OpenStreetMap => (
                "© OpenStreetMap contributors",
                "https://www.openstreetmap.org/copyright",
            ),
            Basemap::CartoDark => (
                "© OpenStreetMap contributors © CARTO",
                "https://carto.com/attributions",
            ),
        };
        Attribution {
            text,
            url,
            logo_light: None,
            logo_dark: None,
        }
    }
    fn max_zoom(&self) -> u8 {
        match self {
            Basemap::CartoDark => 20,
            _ => 19,
        }
    }
}

/// Daily satellite layers from NASA GIBS (Web Mercator, "best" endpoint).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SatLayer {
    ModisTerra,
    ModisAqua,
    ViirsSnpp,
    /// GPM IMERG precipitation rate, every 30 min (WMS).
    ImergPrecip,
    /// AIRS surface air temperature, daily daytime / night-time pass (WMS).
    AirsAirTempDay,
    AirsAirTempNight,
}

impl SatLayer {
    pub const ALL: [SatLayer; 6] = [
        SatLayer::ModisTerra,
        SatLayer::ModisAqua,
        SatLayer::ViirsSnpp,
        SatLayer::ImergPrecip,
        SatLayer::AirsAirTempDay,
        SatLayer::AirsAirTempNight,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SatLayer::ModisTerra => "MODIS Terra true colour (~10:30 local)",
            SatLayer::ModisAqua => "MODIS Aqua true colour (~13:30 local)",
            SatLayer::ViirsSnpp => "VIIRS SNPP true colour (~13:30 local)",
            SatLayer::ImergPrecip => "GPM IMERG precipitation rate (30 min)",
            SatLayer::AirsAirTempDay => "AIRS surface air temperature (daytime pass)",
            SatLayer::AirsAirTempNight => "AIRS surface air temperature (night pass)",
        }
    }
    fn layer(self) -> &'static str {
        match self {
            SatLayer::ModisTerra => "MODIS_Terra_CorrectedReflectance_TrueColor",
            SatLayer::ModisAqua => "MODIS_Aqua_CorrectedReflectance_TrueColor",
            SatLayer::ViirsSnpp => "VIIRS_SNPP_CorrectedReflectance_TrueColor",
            SatLayer::ImergPrecip => "IMERG_Precipitation_Rate_30min",
            SatLayer::AirsAirTempDay => "AIRS_L3_Surface_Air_Temperature_Daily_Day",
            SatLayer::AirsAirTempNight => "AIRS_L3_Surface_Air_Temperature_Daily_Night",
        }
    }
    /// Weather layers are coloured data drawn over the basemap (WMS, transparent).
    pub fn is_weather(self) -> bool {
        matches!(
            self,
            SatLayer::ImergPrecip | SatLayer::AirsAirTempDay | SatLayer::AirsAirTempNight
        )
    }
    /// GIBS time for a moment: the UTC date, or the 30-min IMERG step it falls in.
    pub fn time_key(self, t_unix: i64) -> String {
        match self {
            SatLayer::ImergPrecip => {
                let t = t_unix.div_euclid(1800) * 1800;
                chrono::DateTime::<chrono::Utc>::from_timestamp(t, 0)
                    .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_default()
            }
            _ => crate::table::fmt_date(t_unix),
        }
    }
    /// Legend: (title, low label, high label, colours low → high), for weather layers.
    pub fn legend(self) -> Option<(&'static str, &'static str, &'static str)> {
        match self {
            SatLayer::ImergPrecip => Some(("IMERG precipitation (mm/h)", "0.1", "≥ 50")),
            SatLayer::AirsAirTempDay | SatLayer::AirsAirTempNight => {
                Some(("AIRS surface air temperature", "cold", "warm"))
            }
            _ => None,
        }
    }
}

/// Web-Mercator bounds (metres) of a tile.
fn tile_bounds_3857(t: TileId) -> (f64, f64, f64, f64) {
    const R: f64 = 20_037_508.342_789_244;
    let n = (1u64 << t.zoom) as f64;
    let size = 2.0 * R / n;
    let minx = -R + t.x as f64 * size;
    let maxy = R - t.y as f64 * size;
    (minx, maxy - size, minx + size, maxy)
}

/// One GIBS layer for one UTC date (`YYYY-MM-DD`).
pub struct Gibs {
    pub layer: SatLayer,
    pub date: String,
}

impl TileSource for Gibs {
    fn tile_url(&self, t: TileId) -> String {
        if self.layer.is_weather() {
            // WMS GetMap for the tile's box: works for any layer / time step
            let (x0, y0, x1, y1) = tile_bounds_3857(t);
            return format!(
                "https://gibs.earthdata.nasa.gov/wms/epsg3857/best/wms.cgi?SERVICE=WMS&REQUEST=GetMap&VERSION=1.1.1&LAYERS={}&STYLES=&SRS=EPSG:3857&BBOX={x0:.2},{y0:.2},{x1:.2},{y1:.2}&WIDTH=256&HEIGHT=256&FORMAT=image/png&TRANSPARENT=true&TIME={}",
                self.layer.layer(),
                self.date
            );
        }
        format!(
            "https://gibs.earthdata.nasa.gov/wmts/epsg3857/best/{}/default/{}/GoogleMapsCompatible_Level9/{}/{}/{}.jpg",
            self.layer.layer(),
            self.date,
            t.zoom,
            t.y,
            t.x
        )
    }
    fn attribution(&self) -> Attribution {
        Attribution {
            text: "Imagery: NASA EOSDIS GIBS / Worldview",
            url: "https://earthdata.nasa.gov/gibs",
            logo_light: None,
            logo_dark: None,
        }
    }
    fn max_zoom(&self) -> u8 {
        // weather products are coarse (0.1°–1°): stop requesting finer tiles
        if self.layer.is_weather() { 7 } else { 9 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls() {
        let g = Gibs {
            layer: SatLayer::ModisTerra,
            date: "2025-09-03".into(),
        };
        let u = g.tile_url(TileId {
            x: 8,
            y: 5,
            zoom: 4,
        });
        assert!(u.ends_with("MODIS_Terra_CorrectedReflectance_TrueColor/default/2025-09-03/GoogleMapsCompatible_Level9/4/5/8.jpg"));
        let t = SatLayer::ImergPrecip.time_key(1_726_860_000 + 1000); // 2024-09-20 19:20 + 16 min
        assert_eq!(t, "2024-09-20T19:30:00Z");
        let w = Gibs {
            layer: SatLayer::ImergPrecip,
            date: t,
        }
        .tile_url(TileId {
            x: 0,
            y: 0,
            zoom: 0,
        });
        assert!(
            w.contains("LAYERS=IMERG_Precipitation_Rate_30min")
                && w.contains("TIME=2024-09-20T19:30:00Z")
        );
        assert!(w.contains("BBOX=-20037508.34,-20037508.34,20037508.34,20037508.34"));
        assert_eq!(
            SatLayer::AirsAirTempDay.time_key(1_726_860_000),
            "2024-09-20"
        );
        assert!(
            Basemap::OpenStreetMap
                .tile_url(TileId {
                    x: 1,
                    y: 2,
                    zoom: 3
                })
                .ends_with("/3/1/2.png")
        );
    }
}
