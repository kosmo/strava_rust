use axum::{
    extract::Path as AxumPath, http::header, response::Html, response::IntoResponse, routing::get,
    Json, Router,
};
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use tokio::net::TcpListener;

#[derive(Serialize)]
struct GpxFileInfo {
    filename: String,
    modified: u64, // Unix timestamp in seconds
}

pub async fn serve_map_server() -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route("/", get(serve_map_html))
        .route("/gpx", get(list_gpx_files))
        .route("/gpx/:filename", get(serve_gpx_file));

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
                    let modified = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    files.push(GpxFileInfo {
                        filename: name.to_string(),
                        modified,
                    });
                }
            }
        }
    }
    // Sort by modified time, newest first
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    Json(files)
}

async fn serve_gpx_file(AxumPath(filename): AxumPath<String>) -> impl IntoResponse {
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return (axum::http::StatusCode::BAD_REQUEST, [(header::CONTENT_TYPE, "text/plain")], "Invalid filename".to_string());
    }
    let path = PathBuf::from("gpx").join(&filename);
    match fs::read_to_string(&path) {
        Ok(content) => (axum::http::StatusCode::OK, [(header::CONTENT_TYPE, "application/gpx+xml")], content),
        Err(_) => (axum::http::StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain")], "File not found".to_string()),
    }
}
