use axum::{
    extract::Path as AxumPath,
    extract::Query,
    extract::State,
    http::header,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;

use crate::config;
use crate::database;
use crate::strava;
use crate::tiles;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    config: Arc<RwLock<config::Config>>,
}

#[derive(Serialize)]
struct GpxFileInfo {
    filename: String,
    modified: u64, // Unix timestamp in seconds
    distance_km: f64,
    elevation_gain_m: i32,
    title: String,
}

pub async fn serve_map_server() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize database (runs schema + data migrations automatically)
    let (mut conn, needs_tile_recount) = database::init_db()?;

    let gpx_dir = data_dir().join("gpx");

    if needs_tile_recount {
        println!("DB migration: recalculating tile visit counts from all GPX files...");
    }

    // Process any new GPX files on startup (also handles post-migration recount)
    println!("Processing GPX files...");
    let new_tiles = tiles::process_all_gpx_files(&mut conn, &gpx_dir)?;
    if new_tiles > 0 {
        println!("Added/updated {} tile entries", new_tiles);
    }

    let total_tiles = database::get_tile_count(&conn)?;
    println!("Total tiles in database: {}", total_tiles);

    let cfg = config::load(&data_dir());
    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        config: Arc::new(RwLock::new(cfg)),
    };

    let app = Router::new()
        .route("/", get(serve_map_html))
        .route("/gpx", get(list_gpx_files))
        .route("/gpx/:filename", get(serve_gpx_file))
        .route("/tiles", get(list_visited_tiles))
        .route("/gemeinden.geojson", get(serve_gemeinden_geojson))
        .route(
            "/sachsen_gemeinden.geojson",
            get(serve_sachsen_gemeinden_geojson),
        )
        .route("/sachsen_kreise.geojson", get(serve_sachsen_kreise_geojson))
        .route(
            "/thueringen_gemeinden.geojson",
            get(serve_thueringen_gemeinden_geojson),
        )
        .route(
            "/thueringen_kreise.geojson",
            get(serve_thueringen_kreise_geojson),
        )
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

/// Returns the directory for runtime data files (gpx/, tiles.db).
/// - Debug: current working directory (convenient during development)
/// - Release on macOS: ~/Library/Application Support/rust-strava/
fn data_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    {
        PathBuf::from(".")
    }
    #[cfg(not(debug_assertions))]
    {
        let base = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        let dir = base
            .join("Library")
            .join("Application Support")
            .join("rust-strava");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}

async fn serve_map_html() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../static/index.html").to_string(),
    )
}

async fn serve_gemeinden_geojson() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "application/geo+json")],
        include_str!("../static/gemeinden.geojson").to_string(),
    )
}

async fn serve_sachsen_gemeinden_geojson() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "application/geo+json")],
        include_str!("../static/sachsen_gemeinden.geojson").to_string(),
    )
}

async fn serve_sachsen_kreise_geojson() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "application/geo+json")],
        include_str!("../static/sachsen_kreise.geojson").to_string(),
    )
}

async fn serve_thueringen_gemeinden_geojson() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "application/geo+json")],
        include_str!("../static/thueringen_gemeinden.geojson").to_string(),
    )
}

async fn serve_thueringen_kreise_geojson() -> impl IntoResponse {
    (
        axum::http::StatusCode::OK,
        [(header::CONTENT_TYPE, "application/geo+json")],
        include_str!("../static/thueringen_kreise.geojson").to_string(),
    )
}

async fn list_gpx_files() -> Json<Vec<GpxFileInfo>> {
    let gpx_dir = data_dir().join("gpx");
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(&gpx_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".gpx") {
                    let path = entry.path();
                    let (modified, distance_km, elevation_gain_m, title) = parse_gpx_info(&path);
                    files.push(GpxFileInfo {
                        filename: name.to_string(),
                        modified,
                        distance_km,
                        elevation_gain_m,
                        title,
                    });
                }
            }
        }
    }
    // Sort by modified time, newest first
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    Json(files)
}

fn parse_gpx_info(path: &PathBuf) -> (u64, f64, i32, String) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (0, 0.0, 0, String::new()),
    };

    let timestamp = extract_gpx_time(&content);
    let distance = calculate_distance_from_content(&content);
    let elevation_gain = calculate_elevation_gain(&content);
    let title = extract_gpx_name(&content);

    (timestamp, distance, elevation_gain, title)
}

fn extract_gpx_name(content: &str) -> String {
    if let Some(start) = content.find("<name>") {
        let rest = &content[start + 6..];
        if let Some(end) = rest.find("</name>") {
            return rest[..end].trim().to_string();
        }
    }
    String::new()
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

fn calculate_elevation_gain(content: &str) -> i32 {
    let mut elevations: Vec<f64> = Vec::new();

    // Extract all elevation values from <ele> tags
    let mut remaining = content;
    while let Some(start) = remaining.find("<ele>") {
        let after_tag = &remaining[start + 5..];
        if let Some(end) = after_tag.find("</ele>") {
            let ele_str = &after_tag[..end];
            if let Ok(ele) = ele_str.trim().parse::<f64>() {
                elevations.push(ele);
            }
        }
        remaining = &remaining[start + 5..];
    }

    // Sum only positive elevation changes (climbing)
    let mut total_gain = 0.0;
    for i in 1..elevations.len() {
        let diff = elevations[i] - elevations[i - 1];
        if diff > 0.0 {
            total_gain += diff;
        }
    }

    total_gain.round() as i32
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
    let path = data_dir().join("gpx").join(&filename);
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

fn default_per_page() -> u32 {
    50
}
fn default_page() -> u32 {
    1
}

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
    let client_id = config::CLIENT_ID;
    let client_secret = config::CLIENT_SECRET;
    let (mut access_token, refresh_token) = {
        let cfg = state.config.read().unwrap();
        (
            cfg.access_token.clone().unwrap_or_default(),
            cfg.refresh_token.clone().unwrap_or_default(),
        )
    };

    if access_token.is_empty() && refresh_token.is_empty() {
        return Json(FetchResponse {
            success: false,
            message: "Nicht authentifiziert. Bitte zuerst 'Bei Strava anmelden' klicken."
                .to_string(),
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
    let activities = match strava::get_activities(
        &client,
        &access_token,
        params.per_page,
        params.page,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            let error_str = e.to_string();
            // Check if it's a 401 error and we have a refresh token
            if error_str.contains("401") && !refresh_token.is_empty() {
                println!("Access token expired, attempting refresh...");

                // Try to refresh the token
                match strava::refresh_token(&client, &client_id, &client_secret, &refresh_token)
                    .await
                {
                    Ok(new_tokens) => {
                        println!("Token refreshed successfully!");
                        access_token = new_tokens.access_token.clone();
                        // Persist refreshed tokens to config
                        {
                            let mut cfg = state.config.write().unwrap();
                            cfg.access_token = Some(new_tokens.access_token.clone());
                            if let Some(ref rt) = new_tokens.refresh_token {
                                cfg.refresh_token = Some(rt.clone());
                            }
                            let _ = config::save(&data_dir(), &cfg);
                        }

                        // Retry with new token
                        match strava::get_activities(
                            &client,
                            &access_token,
                            params.per_page,
                            params.page,
                        )
                        .await
                        {
                            Ok(a) => a,
                            Err(e2) => {
                                return Json(FetchResponse {
                                    success: false,
                                    message: format!(
                                        "Strava API Fehler nach Token-Refresh: {}",
                                        e2
                                    ),
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

    let out_dir = data_dir().join("gpx");

    // Ensure the gpx directory exists before writing any files
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        return Json(FetchResponse {
            success: false,
            message: format!("GPX-Verzeichnis konnte nicht erstellt werden: {}", e),
            imported: 0,
            skipped: 0,
        });
    }

    // Export activities as GPX - we handle database operations separately
    // to avoid holding non-Send types across await points
    let mut imported_count: u32 = 0;
    let mut skipped_count: u32 = 0;

    // Check which activities are already imported AND whose GPX file actually exists on disk.
    // An activity that is in the DB but has no GPX file will be re-fetched.
    let already_imported: std::collections::HashSet<i64> = if !params.fetch_all {
        match database::init_db() {
            Ok((conn, _)) => database::get_imported_activity_ids(&conn)
                .unwrap_or_default()
                .into_iter()
                .filter(|id| out_dir.join(format!("activity_{}.gpx", id)).exists())
                .collect(),
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
            message: format!(
                "Keine neuen Aktivitäten. {} bereits importiert.",
                skipped_count
            ),
            imported: 0,
            skipped: skipped_count,
        });
    }

    // Now export each activity
    let mut imported_ids: Vec<(i64, Option<String>, f64, i32)> = Vec::new();

    for act in &activities_to_import {
        let id = act.id;
        let name = act.name.as_deref().unwrap_or("");
        println!("Exporting GPX for activity {} - {}", id, name);

        match strava::get_activity_streams(&client, &access_token, id).await {
            Ok(streams) => {
                let file_path = out_dir.join(format!("activity_{}.gpx", id));
                let start_date = act.start_date.as_deref();
                let gpx = strava::build_gpx_xml(name, start_date, &streams);

                // Calculate distance and elevation from streams
                let distance_km = strava::calculate_distance_from_streams(&streams);
                let elevation_gain_m = strava::calculate_elevation_gain_from_streams(&streams);

                if let Err(e) = std::fs::write(&file_path, &gpx) {
                    eprintln!("Failed to write GPX file: {}", e);
                    continue;
                }
                println!(
                    "Saved GPX: {} ({:.2} km, {} hm)",
                    file_path.display(),
                    distance_km,
                    elevation_gain_m
                );
                imported_ids.push((id, act.name.clone(), distance_km, elevation_gain_m));
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
        if let Ok((conn, _)) = database::init_db() {
            for (id, name, distance_km, elevation_gain_m) in &imported_ids {
                if let Err(e) = database::mark_activity_imported(
                    &conn,
                    *id,
                    name.as_deref(),
                    *distance_km,
                    *elevation_gain_m,
                ) {
                    eprintln!("Warning: Failed to mark activity {} as imported: {}", id, e);
                }
            }
        }
    }

    // Process new GPX files to update tiles
    {
        let mut conn = state.db.lock().unwrap();
        if let Err(e) = tiles::process_all_gpx_files(&mut conn, &data_dir().join("gpx")) {
            eprintln!("Fehler beim Verarbeiten der GPX-Dateien: {}", e);
        }
    }

    Json(FetchResponse {
        success: true,
        message: format!(
            "{} Aktivitäten importiert, {} übersprungen",
            imported_count, skipped_count
        ),
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

async fn auth_start(_state: State<AppState>) -> Json<AuthStartResponse> {
    let redirect_uri = "http://localhost:8080/auth/callback";
    let auth_url = strava::get_authorize_url(config::CLIENT_ID, redirect_uri);

    // Open the auth URL in the system browser (Tauri WebView blocks window.open for external URLs)
    let _ = std::process::Command::new("open").arg(&auth_url).spawn();

    Json(AuthStartResponse {
        success: true,
        auth_url: Some(auth_url),
        message: "Bitte im System-Browser bei Strava anmelden.".to_string(),
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
            format!(
                r#"<!DOCTYPE html>
<html><head><title>Authentifizierung fehlgeschlagen</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Fehler</h1>
<p>{}</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#,
                error
            ),
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
</body></html>"#
                    .to_string(),
            );
        }
    };

    let client_id = config::CLIENT_ID;
    let client_secret = config::CLIENT_SECRET;

    // Exchange code for token
    let client = match strava::create_client() {
        Ok(c) => c,
        Err(e) => {
            return (
                axum::http::StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(
                    r#"<!DOCTYPE html>
<html><head><title>Fehler</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Fehler</h1>
<p>HTTP Client konnte nicht erstellt werden: {}</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#,
                    e
                ),
            );
        }
    };

    match strava::exchange_code(&client, &client_id, &client_secret, &code).await {
        Ok(token) => {
            // Store tokens in config and persist to disk
            {
                let mut cfg = state.config.write().unwrap();
                cfg.access_token = Some(token.access_token.clone());
                if let Some(ref rt) = token.refresh_token {
                    cfg.refresh_token = Some(rt.clone());
                }
                let _ = config::save(&data_dir(), &cfg);
            }
            println!("OAuth successful! Tokens saved to config.json");

            (
                axum::http::StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                r#"<!DOCTYPE html>
<html><head><title>Authentifizierung erfolgreich</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #28a745;">✅ Erfolgreich authentifiziert!</h1>
<p>Tokens wurden gespeichert. Du kannst dieses Fenster schließen.</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#
                    .to_string(),
            )
        }
        Err(e) => (
            axum::http::StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!(
                r#"<!DOCTYPE html>
<html><head><title>Fehler</title></head>
<body style="font-family: sans-serif; padding: 40px; text-align: center;">
<h1 style="color: #dc3545;">❌ Token-Austausch fehlgeschlagen</h1>
<p>{}</p>
<p><a href="/">Zurück zur Karte</a></p>
</body></html>"#,
                e
            ),
        ),
    }
}

#[derive(Serialize)]
struct AuthStatusResponse {
    authenticated: bool,
}

async fn auth_status(State(state): State<AppState>) -> Json<AuthStatusResponse> {
    let cfg = state.config.read().unwrap();
    Json(AuthStatusResponse {
        authenticated: cfg.access_token.is_some(),
    })
}

#[derive(Serialize)]
struct StatsResponse {
    total_distance_km: f64,
    total_elevation_m: i64,
    activity_count: usize,
    max_square: u32,
    max_cluster: usize,
    eddington: u32,
}

async fn get_stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let conn = state.db.lock().unwrap();

    let total_distance = database::get_total_distance(&conn).unwrap_or(0.0);
    let total_elevation = database::get_total_elevation_gain(&conn).unwrap_or(0);
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
        total_elevation_m: total_elevation,
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
            tiles::TILE_ZOOM,
        );
        let (_, _, lat_max, lon_max) = tiles::tile_to_bounds(
            max_square.top_left_x + max_square.size - 1,
            max_square.top_left_y,
            tiles::TILE_ZOOM,
        );
        [[lat_min, lon_min], [lat_max, lon_max]]
    } else {
        [[0.0, 0.0], [0.0, 0.0]]
    };

    // Convert cluster tiles to bounds
    let cluster_tiles: Vec<[[f64; 2]; 2]> = max_cluster
        .tiles
        .iter()
        .map(|(x, y)| {
            let (lat_min, lon_min, lat_max, lon_max) =
                tiles::tile_to_bounds(*x, *y, tiles::TILE_ZOOM);
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
