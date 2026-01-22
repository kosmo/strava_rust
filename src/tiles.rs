use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[derive(Serialize)]
pub struct TileInfo {
    pub x: u32,
    pub y: u32,
    pub z: u32,
}

#[derive(Serialize)]
pub struct TilesResponse {
    pub tiles: Vec<TileInfo>,
    pub zoom: u32,
    pub total_count: usize,
}

pub const TILE_ZOOM: u32 = 14;

pub fn lat_lon_to_tile(lat: f64, lon: f64, zoom: u32) -> (u32, u32) {
    let n = 2_u32.pow(zoom) as f64;
    let x = ((lon + 180.0) / 360.0 * n).floor() as u32;
    let lat_rad = lat.to_radians();
    let y = ((1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor() as u32;
    (x, y)
}

// #[allow(dead_code)]
// pub fn tile_to_bounds(x: u32, y: u32, zoom: u32) -> (f64, f64, f64, f64) {
//     let n = 2_u32.pow(zoom) as f64;
//     let lon_min = x as f64 / n * 360.0 - 180.0;
//     let lon_max = (x + 1) as f64 / n * 360.0 - 180.0;

//     let lat_max = (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n))
//         .sinh()
//         .atan()
//         .to_degrees();
//     let lat_min = (std::f64::consts::PI * (1.0 - 2.0 * (y + 1) as f64 / n))
//         .sinh()
//         .atan()
//         .to_degrees();

//     (lat_min, lon_min, lat_max, lon_max)
// }

fn extract_attr(s: &str, attr: &str) -> Option<f64> {
    let pattern = format!("{}=\"", attr);
    let start = s.find(&pattern)? + pattern.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    rest[..end].parse().ok()
}

fn extract_all_points_from_gpx(content: &str) -> Vec<(f64, f64)> {
    let mut points = Vec::new();

    for line in content.lines() {
        if let Some(start) = line.find("<trkpt") {
            let segment = &line[start..];
            let lat = extract_attr(segment, "lat");
            let lon = extract_attr(segment, "lon");
            if let (Some(lat), Some(lon)) = (lat, lon) {
                points.push((lat, lon));
            }
        }
    }
    points
}

pub fn get_visited_tiles() -> TilesResponse {
    let gpx_dir = PathBuf::from("gpx");
    let mut visited_tiles: HashSet<(u32, u32)> = HashSet::new();

    if let Ok(entries) = fs::read_dir(&gpx_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".gpx") {
                    let path = entry.path();
                    if let Ok(content) = fs::read_to_string(&path) {
                        let points = extract_all_points_from_gpx(&content);
                        for (lat, lon) in points {
                            let (x, y) = lat_lon_to_tile(lat, lon, TILE_ZOOM);
                            visited_tiles.insert((x, y));
                        }
                    }
                }
            }
        }
    }

    let tiles: Vec<TileInfo> = visited_tiles
        .into_iter()
        .map(|(x, y)| TileInfo { x, y, z: TILE_ZOOM })
        .collect();

    let total_count = tiles.len();

    TilesResponse {
        tiles,
        zoom: TILE_ZOOM,
        total_count,
    }
}
