use axum::{
    extract::Path as AxumPath, extract::State, http::header, response::IntoResponse, routing::get, Json, Router,
};
use rusqlite::Connection;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

use crate::database;
use crate::tiles;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
}

#[derive(Serialize)]
struct GpxFileInfo {
    filename: String,
    modified: u64, // Unix timestamp in seconds
    distance_km: f64,
}

pub async fn serve_map_server() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize database
    let mut conn = database::init_db()?;
    
    // Process any new GPX files on startup
    println!("Processing GPX files...");
    let new_tiles = tiles::process_all_gpx_files(&mut conn)?;
    if new_tiles > 0 {
        println!("Added {} new tile entries", new_tiles);
    }
    
    let total_tiles = database::get_tile_count(&conn)?;
    println!("Total tiles in database: {}", total_tiles);
    
    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
    };
    
    let app = Router::new()
        .route("/", get(serve_map_html))
        .route("/gpx", get(list_gpx_files))
        .route("/gpx/:filename", get(serve_gpx_file))
        .route("/tiles", get(list_visited_tiles))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Map server running at http://127.0.0.1:8080");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_map_html() -> impl IntoResponse {
    let path = PathBuf::from("static/index.html");
    match fs::read_to_string(&path) {
        Ok(content) => (
            axum::http::StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            content,
        ),
        Err(_) => (
            axum::http::StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            "index.html not found".to_string(),
        ),
    }
}

async fn list_gpx_files() -> Json<Vec<GpxFileInfo>> {
    let gpx_dir = PathBuf::from("gpx");
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(&gpx_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".gpx") {
                    let path = entry.path();
                    let (modified, distance_km) = parse_gpx_info(&path);
                    files.push(GpxFileInfo {
                        filename: name.to_string(),
                        modified,
                        distance_km,
                    });
                }
            }
        }
    }
    // Sort by modified time, newest first
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    Json(files)
}

fn parse_gpx_info(path: &PathBuf) -> (u64, f64) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (0, 0.0),
    };

    let timestamp = extract_gpx_time(&content);
    let distance = calculate_distance_from_content(&content);

    (timestamp, distance)
}

fn extract_gpx_time(content: &str) -> u64 {
    // Try to find <time> element in metadata or first trackpoint
    if let Some(start) = content.find("<time>") {
        let rest = &content[start + 6..];
        if let Some(end) = rest.find("</time>") {
            let time_str = &rest[..end];
            return parse_iso8601(time_str);
        }
    }
    0
}

fn parse_iso8601(s: &str) -> u64 {
    // Parse ISO 8601 format: 2024-01-15T10:30:00Z or 2024-01-15T10:30:00+00:00
    let s = s.trim();

    // Remove timezone suffix for simpler parsing
    let s = s.trim_end_matches('Z');
    let s = if let Some(pos) = s.rfind('+') {
        &s[..pos]
    } else if let Some(pos) = s.rfind('-') {
        // Check if this is a date separator or timezone
        if pos > 10 {
            &s[..pos]
        } else {
            s
        }
    } else {
        s
    };

    // Parse: YYYY-MM-DDTHH:MM:SS
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return 0;
    }

    let date_parts: Vec<u32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u32> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();

    if date_parts.len() < 3 || time_parts.len() < 3 {
        return 0;
    }

    let year = date_parts[0];
    let month = date_parts[1];
    let day = date_parts[2];
    let hour = time_parts[0];
    let min = time_parts[1];
    let sec = time_parts[2];

    // Simple conversion to Unix timestamp (not accounting for leap seconds, etc.)
    // Days from 1970-01-01
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

    let secs = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
    secs.max(0) as u64
}

fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn calculate_distance_from_content(content: &str) -> f64 {
    let mut points: Vec<(f64, f64)> = Vec::new();

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

    let mut total_km = 0.0;
    for i in 1..points.len() {
        total_km += haversine_km(points[i - 1], points[i]);
    }
    (total_km * 100.0).round() / 100.0
}

fn extract_attr(s: &str, attr: &str) -> Option<f64> {
    let pattern = format!("{}=\"", attr);
    let start = s.find(&pattern)? + pattern.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    rest[..end].parse().ok()
}

fn haversine_km(p1: (f64, f64), p2: (f64, f64)) -> f64 {
    let r = 6371.0; // Earth radius in km
    let d_lat = (p2.0 - p1.0).to_radians();
    let d_lon = (p2.1 - p1.1).to_radians();
    let lat1 = p1.0.to_radians();
    let lat2 = p2.0.to_radians();

    let a = (d_lat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    r * c
}

async fn serve_gpx_file(AxumPath(filename): AxumPath<String>) -> impl IntoResponse {
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain")],
            "Invalid filename".to_string(),
        );
    }
    let path = PathBuf::from("gpx").join(&filename);
    match fs::read_to_string(&path) {
        Ok(content) => (
            axum::http::StatusCode::OK,
            [(header::CONTENT_TYPE, "application/gpx+xml")],
            content,
        ),
        Err(_) => (
            axum::http::StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            "File not found".to_string(),
        ),
    }
}

async fn list_visited_tiles(State(state): State<AppState>) -> Json<tiles::TilesResponse> {
    let conn = state.db.lock().unwrap();
    Json(tiles::get_visited_tiles(&conn))
}
