use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::database;

/// Calculate distance between two GPS coordinates using Haversine formula
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0; // Earth radius in km
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();

    let a =
        (d_lat / 2.0).sin().powi(2) + lat1_rad.cos() * lat2_rad.cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    R * c
}

/// Calculate total distance from a list of GPS points
fn calculate_distance_from_points(points: &[(f64, f64, i64)]) -> f64 {
    if points.len() < 2 {
        return 0.0;
    }

    let mut total = 0.0;
    for window in points.windows(2) {
        let (lat1, lon1, _) = window[0];
        let (lat2, lon2, _) = window[1];
        total += haversine_km(lat1, lon1, lat2, lon2);
    }
    total
}

#[derive(Serialize)]
pub struct TileInfo {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub first_visited_at: i64,
    pub activity_id: Option<String>,
    pub activity_title: Option<String>,
    pub gpx_filename: Option<String>,
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

/// Extract time attribute from a trkpt element
fn extract_time_from_trkpt(content: &str, start_pos: usize) -> Option<i64> {
    // Look for <time> tag after the trkpt
    let segment = &content[start_pos..];

    // Find the end of this trkpt (could be </trkpt> or next <trkpt)
    let end_pos = segment.find("</trkpt>").unwrap_or(segment.len().min(500));
    let trkpt_content = &segment[..end_pos];

    if let Some(time_start) = trkpt_content.find("<time>") {
        let rest = &trkpt_content[time_start + 6..];
        if let Some(time_end) = rest.find("</time>") {
            let time_str = &rest[..time_end];
            return Some(parse_iso8601(time_str));
        }
    }
    None
}

/// Parse ISO8601 timestamp to Unix epoch seconds
fn parse_iso8601(s: &str) -> i64 {
    let s = s.trim().trim_end_matches('Z');
    let s = if let Some(pos) = s.rfind('+') {
        &s[..pos]
    } else if let Some(pos) = s.rfind('-') {
        if pos > 10 {
            &s[..pos]
        } else {
            s
        }
    } else {
        s
    };

    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return 0;
    }

    let date_parts: Vec<i32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_str = parts[1].split('.').next().unwrap_or(parts[1]); // Remove fractional seconds
    let time_parts: Vec<i32> = time_str.split(':').filter_map(|p| p.parse().ok()).collect();

    if date_parts.len() < 3 || time_parts.len() < 2 {
        return 0;
    }

    let year = date_parts[0];
    let month = date_parts[1];
    let day = date_parts[2];
    let hour = time_parts[0];
    let min = time_parts[1];
    let sec = if time_parts.len() > 2 {
        time_parts[2]
    } else {
        0
    };

    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    let month_days = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    days += month_days[(month - 1) as usize] as i64;
    if month > 2 && is_leap_year(year) {
        days += 1;
    }
    days += (day - 1) as i64;

    days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn extract_all_points_with_time_from_gpx(content: &str) -> Vec<(f64, f64, i64)> {
    let mut points = Vec::new();
    let mut default_time: Option<i64> = None;

    // Try to extract default time from metadata
    if let Some(start) = content.find("<metadata>") {
        if let Some(end) = content.find("</metadata>") {
            let metadata = &content[start..end];
            if let Some(time_start) = metadata.find("<time>") {
                let rest = &metadata[time_start + 6..];
                if let Some(time_end) = rest.find("</time>") {
                    let time_str = &rest[..time_end];
                    default_time = Some(parse_iso8601(time_str));
                }
            }
        }
    }

    let mut search_start = 0;
    while let Some(pos) = content[search_start..].find("<trkpt") {
        let abs_pos = search_start + pos;
        let segment = &content[abs_pos..];

        let lat = extract_attr(segment, "lat");
        let lon = extract_attr(segment, "lon");

        if let (Some(lat), Some(lon)) = (lat, lon) {
            // Try to get time from this specific trackpoint
            let time = extract_time_from_trkpt(content, abs_pos)
                .or(default_time)
                .unwrap_or(0);
            points.push((lat, lon, time));
        }

        search_start = abs_pos + 6;
    }

    points
}

/// Extract the track name from GPX content
fn extract_track_name(content: &str) -> Option<String> {
    if let Some(start) = content.find("<name>") {
        let rest = &content[start + 6..];
        if let Some(end) = rest.find("</name>") {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Extract activity ID from filename (e.g., "activity_15409133734.gpx" -> "15409133734")
fn extract_activity_id(filename: &str) -> Option<String> {
    let name = filename.strip_suffix(".gpx")?;
    if let Some(id) = name.strip_prefix("activity_") {
        Some(id.to_string())
    } else {
        Some(name.to_string())
    }
}

/// Process a single GPX file and store tiles in the database
pub fn process_gpx_file(
    conn: &mut Connection,
    filename: &str,
    content: &str,
) -> Result<usize, String> {
    // Check if already processed
    if database::is_file_processed(conn, filename).map_err(|e| e.to_string())? {
        return Ok(0);
    }

    let points = extract_all_points_with_time_from_gpx(content);
    let activity_title = extract_track_name(content).unwrap_or_else(|| filename.to_string());

    // Calculate distance from GPS points
    let distance_km = calculate_distance_from_points(&points);
    let activity_id = extract_activity_id(filename).unwrap_or_default();

    // Collect tiles with their earliest timestamp
    let mut tile_times: HashMap<(u32, u32), i64> = HashMap::new();

    for (lat, lon, time) in points {
        let (x, y) = lat_lon_to_tile(lat, lon, TILE_ZOOM);
        tile_times
            .entry((x, y))
            .and_modify(|t| *t = (*t).min(time))
            .or_insert(time);
    }

    // Prepare batch insert
    let tiles: Vec<database::TileData> = tile_times
        .into_iter()
        .map(|((x, y), time)| database::TileData {
            x,
            y,
            z: TILE_ZOOM,
            visited_at: time,
            activity_id: activity_id.clone(),
            activity_title: activity_title.clone(),
            gpx_filename: filename.to_string(),
        })
        .collect();

    let count = tiles.len();

    // Insert tiles
    database::insert_tiles_batch(conn, &tiles).map_err(|e| e.to_string())?;

    // Mark file as processed
    database::mark_file_processed(conn, filename).map_err(|e| e.to_string())?;

    // Store activity with distance in imported_activities
    if let Ok(activity_id_num) = activity_id.parse::<i64>() {
        if let Err(e) = database::mark_activity_imported(
            conn,
            activity_id_num,
            Some(&activity_title),
            distance_km,
        ) {
            eprintln!(
                "Warning: Failed to mark activity {} as imported: {}",
                activity_id, e
            );
        }
    }

    Ok(count)
}

/// Process all GPX files in the gpx directory
pub fn process_all_gpx_files(conn: &mut Connection) -> Result<usize, String> {
    let gpx_dir = PathBuf::from("gpx");
    let mut total_new_tiles = 0;

    if let Ok(entries) = fs::read_dir(&gpx_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".gpx") {
                    let path = entry.path();
                    if let Ok(content) = fs::read_to_string(&path) {
                        match process_gpx_file(conn, name, &content) {
                            Ok(count) => {
                                if count > 0 {
                                    println!("Processed {}: {} tiles", name, count);
                                    total_new_tiles += count;
                                }
                            }
                            Err(e) => {
                                eprintln!("Error processing {}: {}", name, e);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(total_new_tiles)
}

/// Get visited tiles from the database
pub fn get_visited_tiles(conn: &Connection) -> TilesResponse {
    let tiles = match database::get_all_tiles(conn) {
        Ok(records) => records
            .into_iter()
            .map(|r| TileInfo {
                x: r.x,
                y: r.y,
                z: r.z,
                first_visited_at: r.first_visited_at,
                activity_id: r.activity_id,
                activity_title: r.activity_title,
                gpx_filename: r.gpx_filename,
            })
            .collect(),
        Err(e) => {
            eprintln!("Error getting tiles from database: {}", e);
            Vec::new()
        }
    };

    let total_count = tiles.len();

    TilesResponse {
        tiles,
        zoom: TILE_ZOOM,
        total_count,
    }
}

/// Result for max square calculation
#[derive(Serialize, Clone)]
pub struct MaxSquareResult {
    pub size: u32,
    pub top_left_x: u32,
    pub top_left_y: u32,
}

/// Result for Yard calculation  
#[derive(Serialize, Clone)]
pub struct MaxClusterResult {
    pub size: usize,
    pub tiles: Vec<(u32, u32)>,
}

/// Calculate the Yard:
/// 1. Find all tiles that are surrounded on all 4 sides by other visited tiles
/// 2. From those, find the largest connected cluster (BFS)
pub fn calculate_max_cluster(tiles: &[TileInfo]) -> MaxClusterResult {
    use std::collections::{HashSet, VecDeque};

    if tiles.is_empty() {
        return MaxClusterResult {
            size: 0,
            tiles: vec![],
        };
    }

    // Step 1: Find all tiles surrounded on all 4 sides
    let all_visited: HashSet<(u32, u32)> = tiles.iter().map(|t| (t.x, t.y)).collect();

    let surrounded_tiles: HashSet<(u32, u32)> = tiles
        .iter()
        .filter(|t| {
            let x = t.x;
            let y = t.y;

            let has_left = x > 0 && all_visited.contains(&(x - 1, y));
            let has_right = all_visited.contains(&(x + 1, y));
            let has_up = y > 0 && all_visited.contains(&(x, y - 1));
            let has_down = all_visited.contains(&(x, y + 1));

            has_left && has_right && has_up && has_down
        })
        .map(|t| (t.x, t.y))
        .collect();

    if surrounded_tiles.is_empty() {
        return MaxClusterResult {
            size: 0,
            tiles: vec![],
        };
    }

    // Step 2: Find the largest connected cluster within surrounded tiles using BFS
    let mut unvisited = surrounded_tiles.clone();
    let mut max_cluster: Vec<(u32, u32)> = vec![];

    while !unvisited.is_empty() {
        let start = *unvisited.iter().next().unwrap();
        let mut queue = VecDeque::new();
        let mut cluster = vec![];

        queue.push_back(start);
        unvisited.remove(&start);

        while let Some((x, y)) = queue.pop_front() {
            cluster.push((x, y));

            // Check 4 orthogonal neighbors (only within surrounded tiles)
            let mut neighbors = Vec::new();
            if x > 0 {
                neighbors.push((x - 1, y));
            }
            neighbors.push((x + 1, y));
            if y > 0 {
                neighbors.push((x, y - 1));
            }
            neighbors.push((x, y + 1));

            for neighbor in neighbors {
                if unvisited.remove(&neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }

        if cluster.len() > max_cluster.len() {
            max_cluster = cluster;
        }
    }

    MaxClusterResult {
        size: max_cluster.len(),
        tiles: max_cluster,
    }
}

/// Calculate the largest square (Übersquadrat) within a set of tiles
/// This should be called with the tiles from the max cluster (Yard)
pub fn calculate_max_square_from_coords(tile_coords: &[(u32, u32)]) -> MaxSquareResult {
    use std::collections::HashSet;

    if tile_coords.is_empty() {
        return MaxSquareResult {
            size: 0,
            top_left_x: 0,
            top_left_y: 0,
        };
    }

    // Create a set of tiles for O(1) lookup
    let visited: HashSet<(u32, u32)> = tile_coords.iter().copied().collect();

    // Find bounds
    let min_x = tile_coords.iter().map(|(x, _)| *x).min().unwrap();
    let max_x = tile_coords.iter().map(|(x, _)| *x).max().unwrap();
    let min_y = tile_coords.iter().map(|(_, y)| *y).min().unwrap();
    let max_y = tile_coords.iter().map(|(_, y)| *y).max().unwrap();

    let width = (max_x - min_x + 1) as usize;
    let height = (max_y - min_y + 1) as usize;

    // DP table for largest square ending at each cell
    let mut dp = vec![vec![0u32; width]; height];
    let mut max_size = 0u32;
    let mut max_pos = (min_x, min_y);

    for y in 0..height {
        for x in 0..width {
            let abs_x = min_x + x as u32;
            let abs_y = min_y + y as u32;

            if visited.contains(&(abs_x, abs_y)) {
                if x == 0 || y == 0 {
                    dp[y][x] = 1;
                } else {
                    dp[y][x] = dp[y - 1][x].min(dp[y][x - 1]).min(dp[y - 1][x - 1]) + 1;
                }

                if dp[y][x] > max_size {
                    max_size = dp[y][x];
                    // Top-left corner of the square
                    max_pos = (abs_x - max_size + 1, abs_y - max_size + 1);
                }
            }
        }
    }

    MaxSquareResult {
        size: max_size,
        top_left_x: max_pos.0,
        top_left_y: max_pos.1,
    }
}

/// Convert tile coordinates to lat/lon bounds
pub fn tile_to_bounds(x: u32, y: u32, zoom: u32) -> (f64, f64, f64, f64) {
    let n = 2_u32.pow(zoom) as f64;
    let lon_min = x as f64 / n * 360.0 - 180.0;
    let lon_max = (x + 1) as f64 / n * 360.0 - 180.0;

    let lat_max = (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n))
        .sinh()
        .atan()
        .to_degrees();
    let lat_min = (std::f64::consts::PI * (1.0 - 2.0 * (y + 1) as f64 / n))
        .sinh()
        .atan()
        .to_degrees();

    (lat_min, lon_min, lat_max, lon_max)
}
