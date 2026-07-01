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
use crate::garmin;
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
    /// Garmin activity type key (e.g. `road_biking`), empty if unknown.
    activity_type: String,
}

pub async fn serve_map_server(
    _app_handle: tauri::AppHandle,
) -> Result<(), Box<dyn std::error::Error>> {
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

    // One-time recompute of stored elevation gain using the noise-filtered
    // algorithm, so previously imported activities match the new calculation.
    recalculate_imported_elevations(&mut conn, &gpx_dir);

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
        .route("/garmin/auth/login", post(garmin_auth_login))
        .route("/garmin/auth/status", get(garmin_auth_status))
        .route("/fetch-garmin-activities", post(fetch_garmin_activities))
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

    // Load DB names (numeric_id → name) so we can override GPX <name> tags.
    let db_names: std::collections::HashMap<String, String> = database::init_db()
        .ok()
        .and_then(|(conn, _)| database::get_all_activity_names(&conn).ok())
        .unwrap_or_default();

    // Load DB activity types (numeric_id → type_key) for display in the info block.
    let db_types: std::collections::HashMap<String, String> = database::init_db()
        .ok()
        .and_then(|(conn, _)| database::get_all_activity_types(&conn).ok())
        .unwrap_or_default();

    // First pass: collect all Garmin files and build two maps:
    //   start_timestamp → garmin_title   (for title matching)
    //   start_timestamp → true           (for deduplication: skip matching Strava files)
    let mut garmin_title_by_time: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();
    if let Ok(entries) = fs::read_dir(&gpx_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let name = match fname.to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.starts_with("garmin_") || !name.ends_with(".gpx") {
                continue;
            }
            let content = match fs::read_to_string(entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let ts = extract_gpx_time(&content);
            if ts == 0 {
                continue;
            }
            let stem = name.trim_end_matches(".gpx");
            let db_key = stem.strip_prefix("garmin_").unwrap_or(stem);
            let title = db_names
                .get(db_key)
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| extract_gpx_name(&content));
            if !title.is_empty() {
                garmin_title_by_time.insert(ts, title);
            }
        }
    }

    // Second pass: collect all GPX files, skipping Strava duplicates.
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(&gpx_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let name = match fname.to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.ends_with(".gpx") {
                continue;
            }
            let content = match fs::read_to_string(entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let stem = name.trim_end_matches(".gpx");
            let is_strava = stem.starts_with("activity_");

            // Use activity start time for sorting (consistent, independent of file mtime).
            let modified = extract_gpx_time(&content);
            let distance_km = calculate_distance_from_content(&content);
            let elevation_gain_m = calculate_elevation_gain(&content);
            let gpx_title = extract_gpx_name(&content);

            let db_key = stem
                .strip_prefix("activity_")
                .or_else(|| stem.strip_prefix("garmin_"))
                .unwrap_or(stem);

            // For Strava files: find matching Garmin title by timestamp.
            let garmin_match = if is_strava {
                let ts = modified; // same variable, activity start time
                if ts > 0 {
                    garmin_title_by_time.get(&ts).cloned().or_else(|| {
                        (1u64..=60).find_map(|d| {
                            garmin_title_by_time
                                .get(&(ts + d))
                                .or_else(|| garmin_title_by_time.get(&(ts.saturating_sub(d))))
                                .cloned()
                        })
                    })
                } else {
                    None
                }
            } else {
                None
            };

            // Skip Strava file when a Garmin file covers the same activity.
            if is_strava && garmin_match.is_some() {
                continue;
            }

            let title = db_names
                .get(db_key)
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or(gpx_title);

            let activity_type = db_types.get(db_key).cloned().unwrap_or_default();

            files.push(GpxFileInfo {
                filename: name.to_string(),
                modified,
                distance_km,
                elevation_gain_m,
                title,
                activity_type,
            });
        }
    }
    // Sort by modified time, newest first
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    Json(files)
}

/// Replace the first `<name>…</name>` in the given GPX file with `new_name`.
/// Tries both `activity_{id}.gpx` and `garmin_{id}.gpx` in `gpx_dir`.
fn update_gpx_name(gpx_dir: &PathBuf, activity_id: &str, new_name: &str) {
    let candidates = [
        gpx_dir.join(format!("activity_{}.gpx", activity_id)),
        gpx_dir.join(format!("garmin_{}.gpx", activity_id)),
    ];
    for path in &candidates {
        if !path.exists() {
            continue;
        }
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("update_gpx_name: read failed for {}: {}", path.display(), e);
                continue;
            }
        };
        let updated = replace_gpx_name(&content, new_name);
        if let Err(e) = fs::write(path, &updated) {
            eprintln!(
                "update_gpx_name: write failed for {}: {}",
                path.display(),
                e
            );
        } else {
            println!("GPX <name> aktualisiert: {}", path.display());
        }
    }
}

/// Swap the content of the first `<name>…</name>` element in a GPX string.
fn replace_gpx_name(content: &str, new_name: &str) -> String {
    // Escape XML special characters in the new name.
    let escaped = new_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if let Some(start) = content.find("<name>") {
        if let Some(end) = content[start..].find("</name>") {
            let before = &content[..start + 6]; // up to and including "<name>"
            let after = &content[start + end..]; // from "</name>" onwards
            return format!("{}{}{}", before, escaped, after);
        }
    }
    // No <name> tag found – insert one right before <trk> or at the end.
    if let Some(pos) = content.find("<trk>") {
        let (before, after) = content.split_at(pos);
        return format!("{}  <name>{}</name>\n{}", before, escaped, after);
    }
    content.to_string()
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
    // Strip fractional seconds (e.g. "51.000" → "51") before parsing
    let time_str = parts[1].split('.').next().unwrap_or(parts[1]);
    let time_parts: Vec<u32> = time_str.split(':').filter_map(|p| p.parse().ok()).collect();

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

/// Recompute and persist the elevation gain of every imported activity from its
/// GPX file using the current noise-filtered algorithm. Runs only once, guarded
/// by a marker in the `meta` table so it doesn't re-run on every startup.
fn recalculate_imported_elevations(conn: &mut rusqlite::Connection, gpx_dir: &PathBuf) {
    // Bump this when the elevation algorithm changes to trigger another recompute.
    const ELEVATION_ALGO_VERSION: &str = "3";

    if database::get_meta(conn, "elevation_algo_version").as_deref() == Some(ELEVATION_ALGO_VERSION)
    {
        return;
    }

    let imported = match database::get_all_imported(conn) {
        Ok(v) => v,
        Err(_) => return,
    };

    println!(
        "Recalculating elevation gain for {} imported activities...",
        imported.len()
    );

    let mut updated = 0;
    for (activity_id, _source) in &imported {
        // The same activity_id may be stored as a Strava or Garmin GPX file.
        let candidates = [
            gpx_dir.join(format!("activity_{}.gpx", activity_id)),
            gpx_dir.join(format!("garmin_{}.gpx", activity_id)),
        ];
        let content = candidates.iter().find_map(|p| fs::read_to_string(p).ok());
        let content = match content {
            Some(c) => c,
            None => continue,
        };

        let elevation = calculate_elevation_gain(&content);
        if database::update_activity_elevation(conn, activity_id, elevation).is_ok() {
            updated += 1;
        }
    }

    println!("Updated elevation gain for {} activities", updated);
    let _ = database::set_meta(conn, "elevation_algo_version", ELEVATION_ALGO_VERSION);
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

    // Sum positive elevation changes using a noise-filtering threshold so the
    // result matches Strava/Garmin instead of over-counting GPS noise.
    strava::elevation_gain_threshold(&elevations)
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
    let already_imported: std::collections::HashSet<String> = if !params.fetch_all {
        match database::init_db() {
            Ok((conn, _)) => database::get_imported_activity_ids(&conn, "strava")
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
            if already_imported.contains(&act.id.to_string()) {
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
    let mut imported_ids: Vec<(String, Option<String>, f64, i32)> = Vec::new();

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
                imported_ids.push((
                    id.to_string(),
                    act.name.clone(),
                    distance_km,
                    elevation_gain_m,
                ));
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
                    id,
                    "strava",
                    name.as_deref(),
                    *distance_km,
                    *elevation_gain_m,
                    None,
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
    let activity_count = database::get_imported_activity_ids(&conn, "strava")
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

// ── Garmin Connect auth & sync ────────────────────────────────────────────────

#[derive(Serialize)]
struct GarminAuthStartResponse {
    success: bool,
    message: String,
}

#[derive(Serialize)]
struct GarminAuthStatusResponse {
    authenticated: bool,
    has_credentials: bool,
}

#[derive(Deserialize)]
struct GarminCredentials {
    email: String,
    password: String,
}

/// Save Garmin credentials and immediately perform a headless SSO login in the
/// background.  No browser window is opened.
async fn garmin_auth_login(
    State(state): State<AppState>,
    Json(creds): Json<GarminCredentials>,
) -> Json<GarminAuthStartResponse> {
    // Persist credentials first so auto-reauth works on future launches.
    {
        let mut cfg = state.config.write().unwrap();
        cfg.garmin_email = Some(creds.email.clone());
        cfg.garmin_password = Some(creds.password.clone());
        let _ = config::save(&data_dir(), &cfg);
    }

    match garmin::login(&creds.email, &creds.password).await {
        Ok((access_token, refresh_token)) => {
            {
                let mut cfg = state.config.write().unwrap();
                cfg.garmin_access_token = Some(access_token);
                cfg.garmin_refresh_token = Some(refresh_token);
                let _ = config::save(&data_dir(), &cfg);
            }
            println!("Garmin Connect: Login erfolgreich, Tokens gespeichert ✓");
            Json(GarminAuthStartResponse {
                success: true,
                message: "Erfolgreich bei Garmin Connect angemeldet.".to_string(),
            })
        }
        Err(e) => Json(GarminAuthStartResponse {
            success: false,
            message: format!("Login fehlgeschlagen: {}", e),
        }),
    }
}

async fn garmin_auth_status(State(state): State<AppState>) -> Json<GarminAuthStatusResponse> {
    let cfg = state.config.read().unwrap();
    Json(GarminAuthStatusResponse {
        authenticated: cfg.garmin_access_token.is_some(),
        has_credentials: cfg.garmin_email.is_some() && cfg.garmin_password.is_some(),
    })
}

async fn fetch_garmin_activities(
    State(state): State<AppState>,
    Json(params): Json<FetchParams>,
) -> Json<FetchResponse> {
    let (mut access_token, mut refresh_token) = {
        let cfg = state.config.read().unwrap();
        (
            cfg.garmin_access_token.clone().unwrap_or_default(),
            cfg.garmin_refresh_token.clone().unwrap_or_default(),
        )
    };

    // If no access token, attempt a silent re-login with stored credentials.
    if access_token.is_empty() {
        let (email, password) = {
            let cfg = state.config.read().unwrap();
            (
                cfg.garmin_email.clone().unwrap_or_default(),
                cfg.garmin_password.clone().unwrap_or_default(),
            )
        };
        if email.is_empty() || password.is_empty() {
            return Json(FetchResponse {
                success: false,
                message: "Nicht bei Garmin Connect angemeldet. Bitte E-Mail und Passwort eingeben."
                    .to_string(),
                imported: 0,
                skipped: 0,
            });
        }
        match garmin::login(&email, &password).await {
            Ok((at, rt)) => {
                {
                    let mut cfg = state.config.write().unwrap();
                    cfg.garmin_access_token = Some(at.clone());
                    cfg.garmin_refresh_token = Some(rt.clone());
                    let _ = config::save(&data_dir(), &cfg);
                }
                access_token = at;
                refresh_token = rt;
            }
            Err(e) => {
                return Json(FetchResponse {
                    success: false,
                    message: format!("Garmin Login fehlgeschlagen: {}", e),
                    imported: 0,
                    skipped: 0,
                })
            }
        }
    }

    let client = match garmin::create_client() {
        Ok(c) => c,
        Err(e) => {
            return Json(FetchResponse {
                success: false,
                message: format!("HTTP Client Fehler: {}", e),
                imported: 0,
                skipped: 0,
            })
        }
    };

    // Fetch activity list; try refreshing token on 401.
    let start = (params.page.saturating_sub(1)) * params.per_page;
    let activities = match garmin::get_activities(&client, &access_token, start, params.per_page)
        .await
    {
        Ok(a) => a,
        Err(e) if e.to_string().contains("401") && !refresh_token.is_empty() => {
            println!("Garmin: access token expired, refreshing…");
            // Try a known working client_id stored alongside the refresh token
            // (we always use the same one for refresh).
            let client_id = "GARMIN_CONNECT_MOBILE_ANDROID_DI_2025Q2";
            match garmin::refresh_access_token(client_id, &refresh_token).await {
                Ok((new_access, new_refresh)) => {
                    {
                        let mut cfg = state.config.write().unwrap();
                        cfg.garmin_access_token = Some(new_access.clone());
                        cfg.garmin_refresh_token = Some(new_refresh);
                        let _ = config::save(&data_dir(), &cfg);
                    }
                    access_token = new_access;
                    match garmin::get_activities(&client, &access_token, start, params.per_page)
                        .await
                    {
                        Ok(a) => a,
                        Err(e2) => {
                            return Json(FetchResponse {
                                success: false,
                                message: format!("Garmin API Fehler nach Token-Refresh: {}", e2),
                                imported: 0,
                                skipped: 0,
                            })
                        }
                    }
                }
                Err(re) => {
                    // Refresh failed – try full re-login with stored credentials.
                    let (email, password) = {
                        let cfg = state.config.read().unwrap();
                        (
                            cfg.garmin_email.clone().unwrap_or_default(),
                            cfg.garmin_password.clone().unwrap_or_default(),
                        )
                    };
                    if !email.is_empty() && !password.is_empty() {
                        match garmin::login(&email, &password).await {
                            Ok((at, rt)) => {
                                {
                                    let mut cfg = state.config.write().unwrap();
                                    cfg.garmin_access_token = Some(at.clone());
                                    cfg.garmin_refresh_token = Some(rt.clone());
                                    let _ = config::save(&data_dir(), &cfg);
                                }
                                access_token = at;
                                match garmin::get_activities(
                                    &client,
                                    &access_token,
                                    start,
                                    params.per_page,
                                )
                                .await
                                {
                                    Ok(a) => a,
                                    Err(e2) => {
                                        return Json(FetchResponse {
                                            success: false,
                                            message: format!(
                                                "Garmin API Fehler nach Re-Login: {}",
                                                e2
                                            ),
                                            imported: 0,
                                            skipped: 0,
                                        })
                                    }
                                }
                            }
                            Err(le) => {
                                return Json(FetchResponse {
                                    success: false,
                                    message: format!(
                                        "Token-Refresh und Re-Login fehlgeschlagen: {} / {}",
                                        re, le
                                    ),
                                    imported: 0,
                                    skipped: 0,
                                })
                            }
                        }
                    } else {
                        return Json(FetchResponse {
                            success: false,
                            message: format!("Token-Refresh fehlgeschlagen: {}", re),
                            imported: 0,
                            skipped: 0,
                        });
                    }
                }
            }
        }
        Err(e) => {
            return Json(FetchResponse {
                success: false,
                message: format!("Garmin API Fehler: {}", e),
                imported: 0,
                skipped: 0,
            })
        }
    };

    if activities.is_empty() {
        return Json(FetchResponse {
            success: true,
            message: "Keine Garmin-Aktivitäten gefunden.".to_string(),
            imported: 0,
            skipped: 0,
        });
    }

    let out_dir = data_dir().join("gpx");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        return Json(FetchResponse {
            success: false,
            message: format!("GPX-Verzeichnis konnte nicht erstellt werden: {}", e),
            imported: 0,
            skipped: 0,
        });
    }

    // Build a combined set of all already-imported activity IDs (any source).
    let existing_ids: std::collections::HashSet<String> = if !params.fetch_all {
        match database::init_db() {
            Ok((conn, _)) => {
                let mut ids: std::collections::HashSet<String> =
                    database::get_imported_activity_ids(&conn, "garmin")
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                ids.extend(
                    database::get_imported_activity_ids(&conn, "strava")
                        .unwrap_or_default()
                        .into_iter(),
                );
                ids
            }
            Err(_) => std::collections::HashSet::new(),
        }
    } else {
        std::collections::HashSet::new()
    };

    let mut imported_count: u32 = 0;
    let mut title_updated_count: u32 = 0;
    let mut type_updated_count: u32 = 0;
    let mut imported_records: Vec<(String, String, f64, i32, String)> = Vec::new();

    for act in &activities {
        let id_str = act.activity_id.to_string();

        // Activity already imported (from any source) → update title from Garmin if changed
        if existing_ids.contains(&id_str) {
            if !act.activity_name.is_empty() {
                if let Ok((conn, _)) = database::init_db() {
                    let old_name = database::get_activity_name(&conn, &id_str)
                        .unwrap_or(None)
                        .unwrap_or_default();
                    if old_name != act.activity_name {
                        if let Err(e) =
                            database::update_activity_name(&conn, &id_str, &act.activity_name)
                        {
                            eprintln!(
                                "Failed to update activity name for {}: {}",
                                act.activity_id, e
                            );
                        } else {
                            update_gpx_name(&out_dir, &id_str, &act.activity_name);
                            println!(
                                "Titel von Garmin übernommen: {} → \"{}\"",
                                id_str, act.activity_name
                            );
                            title_updated_count += 1;
                        }
                    }
                }
            }
            // Backfill / refresh the activity type from Garmin.
            if !act.activity_type.type_key.is_empty() {
                if let Ok((conn, _)) = database::init_db() {
                    let old_type = database::get_activity_type(&conn, &id_str)
                        .unwrap_or(None)
                        .unwrap_or_default();
                    if old_type != act.activity_type.type_key {
                        let _ = database::update_activity_type(
                            &conn,
                            &id_str,
                            &act.activity_type.type_key,
                        );
                        type_updated_count += 1;
                    }
                }
            }
            continue;
        }

        match garmin::download_gpx(&client, &access_token, act.activity_id).await {
            Ok(gpx) => {
                let file_path = out_dir.join(format!("garmin_{}.gpx", act.activity_id));
                if let Err(e) = std::fs::write(&file_path, &gpx) {
                    eprintln!("Failed to write Garmin GPX {}: {}", act.activity_id, e);
                    continue;
                }
                let distance_km = (act.distance / 1000.0 * 100.0).round() / 100.0;
                let elevation_gain_m = act.elevation_gain.round() as i32;
                println!(
                    "Garmin GPX gespeichert: {} ({:.2} km, {} hm)",
                    file_path.display(),
                    distance_km,
                    elevation_gain_m
                );
                imported_records.push((
                    id_str,
                    act.activity_name.clone(),
                    distance_km,
                    elevation_gain_m,
                    act.activity_type.type_key.clone(),
                ));
                imported_count += 1;
            }
            Err(e) => eprintln!("Garmin GPX download failed for {}: {}", act.activity_id, e),
        }
    }

    // Mark in DB and process tiles
    if !imported_records.is_empty() {
        if let Ok((conn, _)) = database::init_db() {
            for (id, name, dist, elev, ty) in &imported_records {
                let _ = database::mark_activity_imported(
                    &conn,
                    id,
                    "garmin",
                    Some(name.as_str()),
                    *dist,
                    *elev,
                    if ty.is_empty() { None } else { Some(ty.as_str()) },
                );
            }
        }
        let mut conn = state.db.lock().unwrap();
        let _ = tiles::process_all_gpx_files(&mut conn, &out_dir);
    }

    let message = match (imported_count, title_updated_count) {
        (0, 0) => {
            if type_updated_count > 0 {
                format!("{} Aktivitätsarten von Garmin übernommen", type_updated_count)
            } else {
                "Keine Änderungen.".to_string()
            }
        }
        (i, 0) => format!("{} Garmin-Aktivitäten importiert", i),
        (0, t) => format!("{} Titel von Garmin aktualisiert", t),
        (i, t) => format!(
            "{} Garmin-Aktivitäten importiert, {} Titel aktualisiert",
            i, t
        ),
    };
    Json(FetchResponse {
        success: true,
        message,
        imported: imported_count,
        // `skipped` doubles as the “something changed, refresh the UI” signal for
        // the frontend: title updates + newly backfilled activity types.
        skipped: title_updated_count + type_updated_count,
    })
}
