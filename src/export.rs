//! Annotated CSV export — one row per fix, Movebank-like column names so the file can go
//! straight back into R (`read.csv` → `move2::mt_as_move2(...)`).

use crate::model::Track;
use crate::table::fmt_time;
use std::collections::BTreeSet;

fn o<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

fn f2(v: Option<f32>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_default()
}

pub fn to_csv<'a>(tracks: impl IntoIterator<Item = &'a Track> + Clone) -> String {
    let extra_keys: BTreeSet<String> = tracks
        .clone()
        .into_iter()
        .flat_map(|t| {
            t.fixes
                .iter()
                .flat_map(|f| f.extra.iter().map(|e| e.0.clone()))
        })
        .collect();
    let mut w = csv::Writer::from_writer(Vec::new());
    let mut header: Vec<String> = [
        "individual_local_identifier",
        "tag_type",
        "firmware_profile",
        "species",
        "timestamp",
        "location_lat",
        "location_long",
        "location_source",
        "location_error_m",
        "altitude_m",
        "altitude_source",
        "altitude_era5_msl_corrected_m",
        "pressure_hpa",
        "step_m",
        "dt_s",
        "ground_speed_ms",
        "bearing_deg",
        "step_significant",
        "tag_temperature_c",
        "activity",
        "wind_level",
        "wind_u_ms",
        "wind_v_ms",
        "wind_speed_ms",
        "wind_from_deg",
        "wind_support_ms",
        "crosswind_ms",
        "airspeed_ms",
        "segment_wind_support_ms",
        "segment_crosswind_ms",
        "segment_hours",
        "era5_t2m_c",
        "era5_msl_hpa",
        "era5_surface_pressure_hpa",
        "era5_precipitation_mm",
        "era5_cloud_cover_pct",
        // pressure-level wind; names follow SigfoxTagPrep::annotate_era5()
        "flight_pressure_hPa",
        "flight_height_m",
        "matched_pressure_level",
        "u_flight",
        "v_flight",
        "wind_speed_flight",
        "wind_support_flight",
        "crosswind_flight",
        "airspeed_flight",
        "best_wind_level",
        "best_wind_level_height_m",
        "best_wind_support",
        "at_best_wind",
        "wind3d_source",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    header.extend(extra_keys.iter().cloned());
    let _ = w.write_record(&header);
    for tr in tracks {
        for f in &tr.fixes {
            let wa = &f.weather;
            let mut rec = vec![
                tr.id.clone(),
                tr.tag.clone(),
                tr.firmware.clone(),
                tr.species.clone().unwrap_or_default(),
                fmt_time(f.t),
                format!("{:.6}", f.lat),
                format!("{:.6}", f.lon),
                f.source.label().to_string(),
                f2(f.error_m),
                f2(f.alt_m),
                f.alt_source
                    .map(|a| a.label().to_string())
                    .unwrap_or_default(),
                f2(wa.alt_msl_corrected),
                f2(f.pressure_hpa),
                f2(f.step_m),
                o(f.dt_s),
                f2(f.ground_speed_ms),
                f2(f.bearing_deg),
                f.step_significant.to_string(),
                f2(f.temp_c),
                f2(f.activity),
                o(wa.level),
                f2(wa.u),
                f2(wa.v),
                f2(wa.wind_speed),
                f2(wa.wind_from_deg),
                f2(wa.wind_support),
                f2(wa.crosswind),
                f2(wa.airspeed),
                f2(wa.seg_wind_support),
                f2(wa.seg_crosswind),
                o(wa.seg_hours),
                f2(wa.temp_2m),
                f2(wa.msl_hpa),
                f2(wa.surface_pressure_hpa),
                f2(wa.precipitation_mm),
                f2(wa.cloud_cover),
                o(wa.flight.as_ref().map(|w| w.flight_pressure_hpa)),
                f2(wa.flight.as_ref().and_then(|w| w.flight_height_m)),
                o(wa.flight.as_ref().map(|w| w.matched_level_hpa)),
                f2(wa.flight.as_ref().map(|w| w.u)),
                f2(wa.flight.as_ref().map(|w| w.v)),
                f2(wa.flight.as_ref().map(|w| w.wind_speed)),
                f2(wa.flight.as_ref().and_then(|w| w.wind_support)),
                f2(wa.flight.as_ref().and_then(|w| w.crosswind)),
                f2(wa.flight.as_ref().and_then(|w| w.airspeed)),
                o(wa.flight.as_ref().and_then(|w| w.best_level_hpa)),
                f2(wa.flight.as_ref().and_then(|w| w.best_level_height_m)),
                f2(wa.flight.as_ref().and_then(|w| w.best_wind_support)),
                o(wa.flight
                    .as_ref()
                    .and_then(|w| w.at_best_wind)
                    .map(|b| if b { "TRUE" } else { "FALSE" })),
                wa.flight
                    .as_ref()
                    .map(|w| w.source.clone())
                    .unwrap_or_default(),
            ];
            for k in &extra_keys {
                rec.push(
                    f.extra
                        .iter()
                        .find(|e| &e.0 == k)
                        .map(|e| e.1.clone())
                        .unwrap_or_default(),
                );
            }
            let _ = w.write_record(&rec);
        }
    }
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip_through_generic_parser() {
        let csv = crate::demo::ctt_gps_csv();
        let t = crate::table::Table::from_str("ctt.csv", &csv).unwrap();
        let (out, _) = crate::firmware::parse(&t, None).unwrap();
        let exported = super::to_csv(out.tracks.iter());
        let t2 = crate::table::Table::from_str("export.csv", &exported).unwrap();
        let (back, id) = crate::firmware::parse(&t2, None).unwrap();
        assert_eq!(id, "movebank-gps");
        assert_eq!(back.tracks[0].fixes.len(), out.tracks[0].fixes.len());
    }
}
