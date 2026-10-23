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
}

impl SatLayer {
    pub const ALL: [SatLayer; 3] = [
        SatLayer::ModisTerra,
        SatLayer::ModisAqua,
        SatLayer::ViirsSnpp,
    ];
    pub fn label(self) -> &'static str {
        match self {
            SatLayer::ModisTerra => "MODIS Terra true colour (~10:30 local)",
            SatLayer::ModisAqua => "MODIS Aqua true colour (~13:30 local)",
            SatLayer::ViirsSnpp => "VIIRS SNPP true colour (~13:30 local)",
        }
    }
    fn layer(self) -> &'static str {
        match self {
            SatLayer::ModisTerra => "MODIS_Terra_CorrectedReflectance_TrueColor",
            SatLayer::ModisAqua => "MODIS_Aqua_CorrectedReflectance_TrueColor",
            SatLayer::ViirsSnpp => "VIIRS_SNPP_CorrectedReflectance_TrueColor",
        }
    }
}

/// One GIBS layer for one UTC date (`YYYY-MM-DD`).
pub struct Gibs {
    pub layer: SatLayer,
    pub date: String,
}

impl TileSource for Gibs {
    fn tile_url(&self, t: TileId) -> String {
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
        9
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
