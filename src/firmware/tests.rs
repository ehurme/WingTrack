use super::*;
use crate::model::{LocationSource, SensorKind};
use crate::table::Table;

fn tbl(name: &str, s: &str) -> Table {
    Table::from_str(name, s).expect("table")
}

#[test]
fn wildcloud_nanofox_30days() {
    let csv = "Device,Time (UTC),Position,Radius (m) (Source/Status),Sequence Number,Base Stations (ID, RSSI, Reps),\
VeDBA sum 0 min ago,VeDBA sum 36 min ago,VeDBA sum 72 min ago,VeDBA sum 108 min ago,VeDBA sum 144 min ago,\
Average temperature 0 min ago,Average temperature 36 min ago,Average temperature 72 min ago,Average temperature 108 min ago,Average temperature 144 min ago,\
Min pressure of last 3 hrs (mbar),Min temperature of last 3 hrs (°C)\n\
ABC1,\"14.06.2025, 21:00:00\",\"47.70, 9.10\",7000 (2/1),12,\"4A1F,-118,2;3B22,-101,1\",900,800,700,60,50,30,29,28,15,14,920.5,>10\n\
ABC1,\"15.06.2025, 00:00:00\",\"47.50, 8.80\",1500 (2/1),13,,40,40,40,40,40,14,14,14,14,14,965.0,>5 / <=10\n";
    let t = tbl("wc.csv", csv);
    assert_eq!(rank(&t)[0].0, "wildcloud-nanofox-30days");
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "wildcloud-nanofox-30days");
    let tr = &out.tracks[0];
    assert_eq!(tr.id, "ABC1");
    assert_eq!(tr.tag, "NanoFox");
    assert_eq!(tr.fixes.len(), 2);
    let f = &tr.fixes[0];
    assert_eq!(f.source, LocationSource::SigfoxAtlas);
    assert_eq!(f.error_m, Some(7000.0));
    assert!((f.lat - 47.70).abs() < 1e-9);
    assert_eq!(f.activity, Some(2510.0));
    assert!(f.alt_m.unwrap() > 700.0, "920 hPa ~ 800 m");
    assert!(f.extra.iter().any(|(k, v)| k == "max_rssi" && v == "-101"));
    assert_eq!(
        tr.sensors
            .iter()
            .filter(|s| s.kind == SensorKind::Vedba)
            .count(),
        10
    );
    // Steps computed: ~ 33 km SW
    let g = &tr.fixes[1];
    assert!(g.step_m.unwrap() > 25_000.0);
    assert!(g.bearing_deg.unwrap() > 200.0 && g.bearing_deg.unwrap() < 260.0);
}

#[test]
fn wildcloud_fsp_and_tinyfox_detect() {
    let fsp = "Device,Time (UTC),Position,Radius (m) (Source/Status),VeDBA sum 0 min ago,Pressure 0 min ago,Pressure 36 min ago,Pressure 72 min ago,Min temperature of last 3 hrs (°C),Max temperature of last 3 hrs (°C)\n\
X,\"01.09.2025, 03:00:00\",\"46.0, 7.0\",3000 (1/1),500,955.2,930.1,940.0,12.5,31.0\n";
    let t = tbl("fsp.csv", fsp);
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "wildcloud-nanofox-fsp");
    let f = &out.tracks[0].fixes[0];
    assert_eq!(f.pressure_hpa, Some(955.2));
    assert_eq!(f.alt_source, Some(AltitudeSource::PressureInstant));
    assert_eq!(
        out.tracks[0]
            .sensors
            .iter()
            .filter(|s| s.kind == SensorKind::Pressure)
            .count(),
        3
    );

    let tiny = "Device,Time (UTC),Position,Radius (m) (Source/Status),Total VeDBA,24h Active (%),24h Min. Temperature (°C),24h Max. Temperature (°C),24h Min. Pressure (mbar)\n\
T1,\"01.09.2025, 12:00:00\",\"47.0, 9.0\",2500 (1/1),1000,5,8,30,960\n\
T1,\"02.09.2025, 12:00:00\",\"46.5, 8.4\",2500 (1/1),1600,12,9,32,930\n";
    let t = tbl("tiny.csv", tiny);
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "wildcloud-tinyfox");
    let tr = &out.tracks[0];
    assert_eq!(tr.tag, "TinyFox");
    assert_eq!(tr.fixes[0].activity, None);
    assert_eq!(
        tr.fixes[1].activity,
        Some(600.0),
        "cumulative → daily difference"
    );
    assert_eq!(
        tr.fixes[1].alt_source,
        Some(AltitudeSource::PressureWindowMin)
    );
}

#[test]
fn movebank_sigfox_long_format() {
    let csv = "event-id,visible,timestamp,location-long,location-lat,sigfox-computed-location-radius,sensor-type,individual-local-identifier,tag-local-identifier,barometric-pressure,external-temperature,vedba\n\
1,true,2025-09-01 03:00:00.000,9.0,47.0,5000,sigfox-geolocation,Bat_A,1B2C,,,\n\
2,true,2025-09-01 02:30:00.000,,,,barometric,Bat_A,1B2C,925,,\n\
3,true,2025-09-01 01:00:00.000,,,,barometric,Bat_A,1B2C,960,,\n\
4,true,2025-09-01 02:00:00.000,,,,accessory-measurements,Bat_A,1B2C,,,800\n\
5,true,2025-09-01 06:00:00.000,8.5,46.6,2000,sigfox-geolocation,Bat_A,1B2C,,,\n";
    let t = tbl("mb.csv", csv);
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "movebank-sigfox");
    let tr = &out.tracks[0];
    assert_eq!(tr.id, "Bat_A");
    assert_eq!(tr.fixes.len(), 2);
    assert_eq!(tr.fixes[0].error_m, Some(5000.0));
    // 3-h minimum pressure from separate sensor rows
    assert_eq!(tr.fixes[0].pressure_hpa, Some(925.0));
    assert!(tr.sensors.iter().any(|s| s.kind == SensorKind::Vedba));
}

#[test]
fn movebank_gps() {
    let csv = "event-id,visible,timestamp,location-long,location-lat,ground-speed,heading,height-above-ellipsoid,gps:hdop,individual-local-identifier,individual-taxon-canonical-name\n\
1,true,2024-05-01 10:00:00.000,8.0,47.0,10.5,180,500,1.2,Stork1,Ciconia ciconia\n\
2,true,2024-05-01 10:05:00.000,8.0,46.97,11,182,520,0.9,Stork1,Ciconia ciconia\n\
3,false,2024-05-01 10:10:00.000,8.0,46.94,11,182,520,0.9,Stork1,Ciconia ciconia\n";
    let t = tbl("gps.csv", csv);
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "movebank-gps");
    let tr = &out.tracks[0];
    assert_eq!(tr.fixes.len(), 2);
    assert_eq!(tr.species.as_deref(), Some("Ciconia ciconia"));
    assert_eq!(tr.fixes[0].reported_speed_ms, Some(10.5));
    assert_eq!(tr.fixes[1].alt_source, Some(AltitudeSource::Gps));
    assert!((tr.fixes[0].error_m.unwrap() - 6.0).abs() < 1e-4);
}

#[test]
fn ctt_gps() {
    let csv = "id (imei),alias,Time of Fix,Lat,Lng,Altitude,Speed,COG,HDOP,VDOP,Satellite Count,Fix Time,Error,Nav Type,Activity Index,Battery,Temp\n\
861,Bird-1,2024-04-02 08:00:00,40.1,-75.2,120,20,45,1.1,1.5,9,22,0,3,50,3950,18\n\
861,Bird-1,2024-04-02 10:00:00,0,0,0,0,0,0,0,0,90,1,1,0,3950,18\n\
861,Bird-1,2024-04-02 12:00:00,40.3,-75.0,300,0,0,1.0,1.4,8,25,0,3,10,3940,20\n";
    let t = tbl("ctt.csv", csv);
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "ctt-gps");
    let tr = &out.tracks[0];
    assert_eq!(tr.id, "861");
    assert_eq!(tr.fixes.len(), 2, "failed fix dropped");
    assert!(
        (tr.fixes[0].reported_speed_ms.unwrap() - 10.29).abs() < 0.01,
        "knots → m/s"
    );
    assert!(!out.warnings.is_empty());
}

#[test]
fn ctt_legacy_header() {
    let csv = "GPS_YYYY-MM-DD_HH:MM:SS,Latitude,Longitude,Altitude,Speed,Course,Hdop,Satellites\n\
2023-05-01 12:00:00,51.0,4.0,30,0,0,1.0,7\n2023-05-01 14:00:00,51.1,4.1,40,5,20,1.0,7\n";
    let t = tbl("CTT-123.csv", csv);
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "ctt-gps");
    assert_eq!(out.tracks[0].id, "CTT-123");
}

#[test]
fn generic_flattened_r_with_imported_era5() {
    let csv = "individual_local_identifier,timestamp,lat,lon,vedba_sum,min_pressure,era5_u100,era5_v100,wind_support_flight\n\
N1,2025-09-01T03:00:00Z,47.0,9.0,1200,930,3.0,-4.0,2.5\n\
N1,2025-09-02T03:00:00Z,46.5,8.3,800,950,1.0,1.0,-0.5\n";
    let t = tbl("flat.csv", csv);
    let (out, id) = parse(&t, None).unwrap();
    assert_eq!(id, "generic");
    let f = &out.tracks[0].fixes[0];
    assert_eq!(f.weather.level, Some("imported"));
    assert_eq!(f.weather.wind_support, Some(2.5));
    assert!((f.weather.wind_speed.unwrap() - 5.0).abs() < 1e-5);
    assert!(f.alt_m.is_some());
}

#[test]
fn force_override_and_unknown() {
    let t = tbl("x.csv", "a,b\n1,2\n");
    assert!(parse(&t, None).is_err());
    let csv = "individual_local_identifier,timestamp,lat,lon\nA,2025-01-01 00:00:00,1,2\n";
    let t = tbl("y.csv", csv);
    let (_, id) = parse(&t, Some("generic")).unwrap();
    assert_eq!(id, "generic");
    assert!(parse(&t, Some("nope")).is_err());
}

#[test]
fn demo_files_parse() {
    for (name, csv) in crate::demo::all() {
        let t = tbl(name, &csv);
        let (out, id) = parse(&t, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        let expect = if name.contains("nanofox") {
            "wildcloud-nanofox-30days"
        } else if name.contains("tinyfox") {
            "wildcloud-tinyfox"
        } else {
            "ctt-gps"
        };
        assert_eq!(id, expect, "{name}");
        assert!(out.tracks.iter().all(|tr| tr.fixes.len() > 5), "{name}");
    }
}
