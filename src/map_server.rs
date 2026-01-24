use axum::{
    extract::Path as AxumPath, extract::Query, extract::State, http::header, 
    response::IntoResponse, routing::{get, post},
    Json, Router,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;

use crate::database;
use crate::strava;
use crate::tiles;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    // Store the current access token (refreshed via OAuth)
    access_token: Arc<RwLock<Option<String>>>,
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
        access_token: Arc::new(RwLock::new(None)),
    };

    let app = Router::new()
        .route("/", get(serve_map_html))
        .route("/gpx", get(list_gpx_files))
        .route("/gpx/:filename", get(serve_gpx_file))
        .route("/tiles", get(list_visited_tiles))
        .route("/gemeinden.geojson", get(serve_gemeinden_geojson))
        .route("/fetch-activities", post(fetch_activities))
        .route("/stats", get(get_stats))
        .route("/square-cluster", get(get_square_cluster))
        .route("/auth/start", get(auth_start))
        .route("/auth/callback", get(auth_callback))
        .route("/auth/status", get(auth_status))
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

async fn serve_gemeinden_geojson() -> impl IntoResponse {
    let path = PathBuf::from("static/gemeinden.geojson");
    match fs::read_to_string(&path) {
        Ok(content) => (
            axum::http::StatusCode::OK,
            [(header::CONTENT_TYPE, "application/geo+json")],
            content,
        ),
        Err(_) => (
            axum::http::StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain")],
            "gemeinden.geojson not found".to_string(),
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

#[derive(Deserialize)]
struct FetchParams {
    #[serde(default)]
    fetch_all: bool,
    #[serde(default = "default_per_page")]
    per_page: u32,
    #[serde(default = "default_page")]
    page: u32,
}

fn default_per_page() -> u32 { 50 }
fn default_page() -> u32 { 1 }

#[derive(Serialize)]
struct FetchResponse {
    success: bool,
    message: String,
    imported: u32,
    skipped: u32,
}

async fn fetch_activities(
    State(state): State<AppState>,
    Json(params): Json<FetchParams>,
) -> Json<FetchResponse> {
    // First, check if we have a token from OAuth in state
    let state_token = {
        let token_guard = state.access_token.read().unwrap();
        token_guard.clone()
    };
    
    // Use state token if available, otherwise fall back to environment
    let mut access_token = state_token.unwrap_or_else(|| {
        std::env::var("STRAVA_ACCESS_TOKEN").unwrap_or_default()
    });
    let refresh_token = std::env::var("STRAVA_REFRESH_TOKEN").unwrap_or_default();
    let client_id = std::env::var("STRAVA_CLIENT_ID").unwrap_or_default();
    let client_secret = std::env::var("STRAVA_CLIENT_SECRET").unwrap_or_default();

    if access_token.is_empty() && refresh_token.is_empty() {
        return Json(FetchResponse {
            success: false,
            message: "Nicht authentifiziert. Bitte zuerst 'Bei Strava anmelden' klicken.".to_string(),
            imported: 0,
            skipped: 0,
        });
    }

    // Create HTTP client
    let client = match strava::create_client() {
        Ok(c) => c,
        Err(e) => {
            return Json(FetchResponse {
                success: false,
                message: format!("HTTP Client Fehler: {}", e),
                imported: 0,
                skipped: 0,
            });
        }
    };

    // Try to fetch activities, refresh token if needed
    let activities = match strava::get_activities(&client, &access_token, params.per_page, params.page).await {
        Ok(a) => a,
        Err(e) => {
            let error_str = e.to_string();
            // Check if it's a 401 error and we have a refresh token
            if error_str.contains("401") && !refresh_token.is_empty() && !client_id.is_empty() && !client_secret.is_empty() {
                println!("Access token expired, attempting refresh...");
                
                // Try to refresh the token
                match strava::refresh_token(&client, &client_id, &client_secret, &refresh_token).await {
                    Ok(new_tokens) => {
                        println!("Token refreshed successfully!");
                        println!("New access token: {}", new_tokens.access_token);
                        if let Some(ref rt) = new_tokens.refresh_token {
                            println!("New refresh token: {}", rt);
                        }
                        println!("Please update your .env file with the new tokens.");
                        
                        access_token = new_tokens.access_token;
                        
                        // Retry with new token
                        match strava::get_activities(&client, &access_token, params.per_page, params.page).await {
                            Ok(a) => a,
                            Err(e2) => {
                                return Json(FetchResponse {
                                    success: false,
                                    message: format!("Strava API Fehler nach Token-Refresh: {}", e2),
                                    imported: 0,
                                    skipped: 0,
                                });
                            }
                        }
                    }
                    Err(refresh_err) => {
                        return Json(FetchResponse {
                            success: false,
                            message: format!("Token-Refresh fehlgeschlagen: {}. Bitte erneut via CLI authentifizieren.", refresh_err),
                            imported: 0,
                            skipped: 0,
                        });
                    }
                }
            } else {
                return Json(FetchResponse {
                    success: false,
                    message: format!("Strava API Fehler: {}", e),
                    imported: 0,
                    skipped: 0,
                });
            }
        }
    };

    if activities.is_empty() {
        return Json(FetchResponse {
            success: true,
            message: "Keine neuen Aktivitäten gefunden.".to_string(),
            imported: 0,
            skipped: 0,
        });
    }

    let out_dir = PathBuf::from("gpx");

    // Export activities as GPX - we handle database operations separately
    // to avoid holding non-Send types across await points
    let mut imported_count: u32 = 0;
    let mut skipped_count: u32 = 0;

    // First, check which activities are already imported (using a separate connection)
    let already_imported: std::collections::HashSet<i64> = if !params.fetch_all {
        match database::init_db() {
            Ok(conn) => {
                database::get_imported_activity_ids(&conn)
                    .unwrap_or_default()
                    .into_iter()
                    .collect()
            }
            Err(_) => std::collections::HashSet::new(),
        }
    } else {
        std::collections::HashSet::new()
    };

    // Filter activities
    let activities_to_import: Vec<_> = activities
        .into_iter()
        .filter(|act| {
            if already_imported.contains(&act.id) {
                skipped_count += 1;
                false
            } else {
                true
            }
        })
        .collect();

    if activities_to_import.is_empty() {
        return Json(FetchResponse {
            success: true,
            message: format!("Keine neuen Aktivitäten. {} bereits importiert.", skipped_count),
            imported: 0,
            skipped: skipped_count,
        });
    }

    // Now export each activity
    let mut imported_ids: Vec<(i64, Option<String>, f64)> = Vec::new();
    
    for act in &activities_to_import {
        let id = act.id;
        let name = act.name.as_deref().unwrap_or("");
        println!("Exporting GPX for activity {} - {}", id, name);

        match strava::get_activity_streams(&client, &access_token, id).await {
            Ok(streams) => {
                let file_path = out_dir.join(format!("activity_{}.gpx", id));
                let start_date = act.start_date.as_deref();
                let gpx = strava::build_gpx_xml(name, start_date, &streams);
                
                // Calculate distance from streams
                let distance_km = strava::calculate_distance_from_streams(&streams);
                
                if let Err(e) = std::fs::write(&file_path, &gpx) {
                    eprintln!("Failed to write GPX file: {}", e);
                    continue;
                }
                println!("Saved GPX: {} ({:.2} km)", file_path.display(), distance_km);
                imported_ids.push((id, act.name.clone(), distance_km));
                imported_count += 1;
            }
            Err(e) => {
                eprintln!("Failed to get streams for activity {}: {}", id, e);
                continue;
            }
        }
    }

    // Mark activities as imported in database (after all awaits are done)
    if !imported_ids.is_empty() {
        if let Ok(conn) = database::init_db() {
            for (id, name, distance_km) in &imported_ids {
                if let Err(e) = database::mark_activity_imported(&conn, *id, name.as_deref(), *distance_km) {
                    eprintln!("Warning: Failed to mark activity {} as imported: {}", id, e);
                }
            }
        }
    }

    // Process new GPX files to update tiles
    {
        let mut conn = state.db.lock().unwrap();
        if let Err(e) = tiles::process_all_gpx_files(&mut conn) {
            eprintln!("Fehler beim Verarbeiten der GPX-Dateien: {}", e);
        }
    }

    Json(FetchResponse {
        success: true,
        message: format!("{} Aktivitäten importiert, {} übersprungen", imported_count, skipped_count),
        imported: imported_count,
        skipped: skipped_count,
    })
}

// OAuth Authentication Handlers

#[derive(Serialize)]
struct AuthStartResponse {
    success: bool,
    auth_url: Option<String>,
    message: String,
}

async fn auth_start() -> Json<AuthStartResponse> {
    let client_id = std::env::var("STRAVA_CLIENT_ID").unwrap_or_default();
    
    if client_id.is_empty() {
        return Json(AuthStartResponse {
            success: false,
            auth_url: None,
            message: "STRAVA_CLIENT_ID nicht gesetzt.".to_string(),
        });
    }

    let redirect_uri = "http://localhost:8080/auth/callback";
    let auth_url = strava::get_authorize_url(&client_id, redirect_uri);
    
    Json(AuthStartResponse {
        success: true,
        auth_url: Some(auth_url),
        message: "Bitte im neuen Fenster bei Strava anmelden.".to_string(),
    })
}

#[derive(Deserialize)]
struct AuthCallbackParams {
    code: Option<String>,
    error: Option<String>,
}

async fn auth_callback(
    State(state): State<AppState>,
    Query(params): Query<AuthCallbackParams>,
) -> impl IntoResponse {
    if let Some(error) = params.error {
        return (
            axum::http::StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!(r#"<!DOCTYPE html>
<html><head><title>Authentifizierung fehlgeschlagen</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Fehler</h1>
<p>{}</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#, error),
        );
    }

    let code = match params.code {
        Some(c) => c,
        None => {
            return (
                axum::http::StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                r#"<!DOCTYPE html>
<html><head><title>Fehler</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Fehler</h1>
<p>Kein Autorisierungscode erhalten.</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#.to_string(),
            );
        }
    };

    let client_id = std::env::var("STRAVA_CLIENT_ID").unwrap_or_default();
    let client_secret = std::env::var("STRAVA_CLIENT_SECRET").unwrap_or_default();

    if client_id.is_empty() || client_secret.is_empty() {
        return (
            axum::http::StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            r#"<!DOCTYPE html>
<html><head><title>Fehler</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Konfigurationsfehler</h1>
<p>STRAVA_CLIENT_ID oder STRAVA_CLIENT_SECRET nicht gesetzt.</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#.to_string(),
        );
    }

    // Exchange code for token
    let client = match strava::create_client() {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(r#"<!DOCTYPE html>
<html><head><title>Fehler</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Fehler</h1>
<p>HTTP Client konnte nicht erstellt werden: {}</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#, e),
            );
        }
    };

    match strava::exchange_code(&client, &client_id, &client_secret, &code).await {
        Ok(token) => {
            // Store the token in state
            {
                let mut token_guard = state.access_token.write().unwrap();
                *token_guard = Some(token.access_token.clone());
            }
            
            println!("OAuth successful! Access token obtained.");
            if let Some(ref rt) = token.refresh_token {
                println!("Refresh token: {}", rt);
                println!("Speichere diesen Refresh Token in deiner .env Datei als STRAVA_REFRESH_TOKEN");
            }

            (
                axum::http::StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(r#"<!DOCTYPE html>
<html><head><title>Authentifizierung erfolgreich</title>
<script>
  // Notify parent window if this was opened as popup
  if (window.opener) {{
    window.opener.postMessage({{ type: 'strava-auth-success' }}, '*');
    setTimeout(() => window.close(), 2000);
  }}
</script>
</head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #28a745;">✅ Erfolgreich authentifiziert!</h1>
<p>Du kannst dieses Fenster jetzt schließen und Aktivitäten abrufen.</p>
<p style="font-size: 12px; color: #666;">Refresh Token (für .env): <code>{}</code></p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#, token.refresh_token.as_deref().unwrap_or("(keiner)")),
            )
        }
        Err(e) => {
            (
                axum::http::StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(r#"<!DOCTYPE html>
<html><head><title>Fehler</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Token-Austausch fehlgeschlagen</h1>
<p>{}</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#, e),
            )
        }
    }
}

#[derive(Serialize)]
struct AuthStatusResponse {
    authenticated: bool,
}

async fn auth_status(State(state): State<AppState>) -> Json<AuthStatusResponse> {
    let token_guard = state.access_token.read().unwrap();
    Json(AuthStatusResponse {
        authenticated: token_guard.is_some(),
    })
}

#[derive(Serialize)]
struct StatsResponse {
    total_distance_km: f64,
    activity_count: usize,
    max_square: u32,
    max_cluster: usize,
    eddington: u32,
}

async fn get_stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let conn = state.db.lock().unwrap();
    
    let total_distance = database::get_total_distance(&conn).unwrap_or(0.0);
    let activity_count = database::get_imported_activity_ids(&conn)
        .map(|ids| ids.len())
        .unwrap_or(0);
    let eddington = database::calculate_eddington_number(&conn).unwrap_or(0);
    
    // Calculate Yard and Übersquadrat (independently from all tiles)
    let tiles_response = tiles::get_visited_tiles(&conn);
    let max_cluster = tiles::calculate_max_cluster(&tiles_response.tiles);
    let all_coords: Vec<(u32, u32)> = tiles_response.tiles.iter().map(|t| (t.x, t.y)).collect();
    let max_square = tiles::calculate_max_square_from_coords(&all_coords);
    
    Json(StatsResponse {
        total_distance_km: (total_distance * 100.0).round() / 100.0,
        activity_count,
        max_square: max_square.size,
        max_cluster: max_cluster.size,
        eddington,
    })
}

#[derive(Serialize)]
struct SquareClusterResponse {
    max_square: SquareGeometry,
    max_cluster: ClusterGeometry,
    zoom: u32,
}

#[derive(Serialize)]
struct SquareGeometry {
    size: u32,
    bounds: [[f64; 2]; 2], // [[south, west], [north, east]]
}

#[derive(Serialize)]
struct ClusterGeometry {
    size: usize,
    tiles: Vec<[[f64; 2]; 2]>, // Array of tile bounds
}

async fn get_square_cluster(State(state): State<AppState>) -> Json<SquareClusterResponse> {
    let conn = state.db.lock().unwrap();
    let tiles_response = tiles::get_visited_tiles(&conn);
    
    // Calculate Yard and Übersquadrat (independently from all tiles)
    let max_cluster = tiles::calculate_max_cluster(&tiles_response.tiles);
    let all_coords: Vec<(u32, u32)> = tiles_response.tiles.iter().map(|t| (t.x, t.y)).collect();
    let max_square = tiles::calculate_max_square_from_coords(&all_coords);
    
    // Convert square to bounds
    let square_bounds = if max_square.size > 0 {
        let (lat_min, lon_min, _, _) = tiles::tile_to_bounds(
            max_square.top_left_x, 
            max_square.top_left_y + max_square.size - 1, 
            tiles::TILE_ZOOM
        );
        let (_, _, lat_max, lon_max) = tiles::tile_to_bounds(
            max_square.top_left_x + max_square.size - 1, 
            max_square.top_left_y, 
            tiles::TILE_ZOOM
        );
        [[lat_min, lon_min], [lat_max, lon_max]]
    } else {
        [[0.0, 0.0], [0.0, 0.0]]
    };
    
    // Convert cluster tiles to bounds
    let cluster_tiles: Vec<[[f64; 2]; 2]> = max_cluster.tiles
        .iter()
        .map(|(x, y)| {
            let (lat_min, lon_min, lat_max, lon_max) = tiles::tile_to_bounds(*x, *y, tiles::TILE_ZOOM);
            [[lat_min, lon_min], [lat_max, lon_max]]
        })
        .collect();
    
    Json(SquareClusterResponse {
        max_square: SquareGeometry {
            size: max_square.size,
            bounds: square_bounds,
        },
        max_cluster: ClusterGeometry {
            size: max_cluster.size,
            tiles: cluster_tiles,
        },
        zoom: tiles::TILE_ZOOM,
    })
}
