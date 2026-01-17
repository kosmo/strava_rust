use axum::{extract::Query, extract::State, routing::get, Router};
use clap::Parser;
use dotenvy::dotenv;
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

mod map_server;

#[derive(Debug, Deserialize)]
struct Athlete {
    id: i64,
    username: Option<String>,
    firstname: Option<String>,
    lastname: Option<String>,
}

#[derive(Debug, Parser)]
#[command(name = "rust_strava", about = "Strava API Rust example")]
struct Cli {
    /// Exchange an OAuth authorization code for an access token
    #[arg(long)]
    exchange_code: Option<String>,

    /// Per-page for activities list
    #[arg(long, default_value_t = 5)]
    per_page: u32,

    /// Page number for activities list
    #[arg(long, default_value_t = 1)]
    page: u32,

    /// Start a web server to display GPX files on a Leaflet map
    #[arg(long)]
    serve_map: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenResponse {
    token_type: Option<String>,
    access_token: String,
    expires_at: Option<i64>,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActivitySummary {
    id: i64,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TypedStream<T> {
    #[serde(default)]
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct StreamSet {
    #[serde(default)]
    latlng: Option<TypedStream<[f64; 2]>>, // key_by_type=true shape: { latlng: { data: [[lat, lon], ...] } }
    #[serde(default)]
    time: Option<TypedStream<i64>>, // { time: { data: [..] } }
    #[serde(default)]
    altitude: Option<TypedStream<f64>>, // { altitude: { data: [..] } }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load environment variables from .env if present
    let _ = dotenv();
    let args = Cli::parse();

    // Serve map mode - start web server to display GPX files
    if args.serve_map {
        return map_server::serve_map_server().await;
    }

    // Credentials are read from environment (.env supported):
    // STRAVA_CLIENT_ID (numeric), STRAVA_CLIENT_SECRET
    // Optional fallback: STRAVA_ACCESS_TOKEN

    // Support exchanging an authorization code for an access token
    if let Some(code) = args.exchange_code {
        let client_id = env::var("STRAVA_CLIENT_ID").unwrap_or_else(|_| String::new());
        let client_secret = env::var("STRAVA_CLIENT_SECRET").unwrap_or_else(|_| String::new());
        if client_id.is_empty() || client_secret.is_empty() {
            eprintln!("Missing STRAVA_CLIENT_ID or STRAVA_CLIENT_SECRET. Populate .env first.");
            return Ok(());
        }

        let client = reqwest::Client::builder()
            .user_agent("rust-strava-example/0.1")
            .build()?;

        let resp = client
            .post("https://www.strava.com/oauth/token")
            .form(&serde_json::json!({
                "client_id": client_id,
                "client_secret": client_secret,
                "code": code,
                "grant_type": "authorization_code",
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eprintln!("Token exchange failed: status={} body={}", status, body);
            return Ok(());
        }

        let token: TokenResponse = resp.json().await?;
        println!("Access token: {}", token.access_token);
        if let Some(rt) = token.refresh_token {
            println!("Refresh token: {}", rt);
        }
        println!("Note: Save tokens securely. Do NOT commit them.");
        return Ok(());
    }

    // If no exchange requested, run a local OAuth authorize + callback to obtain a fresh token with required scopes.
    let client = reqwest::Client::builder()
        .user_agent("rust-strava-example/0.1")
        .build()?;

    let (tx, rx) = oneshot::channel::<String>();
    #[derive(Clone)]
    struct AppState {
        tx: Arc<Mutex<Option<oneshot::Sender<String>>>>,
    }
    let state = AppState {
        tx: Arc::new(Mutex::new(Some(tx))),
    };

    // Minimal axum server to capture `code` at /callback
    let app = Router::new()
        .route(
            "/callback",
            get(
                |State(state): State<AppState>,
                 Query(params): Query<std::collections::HashMap<String, String>>| async move {
                    if let Some(code) = params.get("code").cloned() {
                        if let Some(sender) = state.tx.lock().unwrap().take() {
                            let _ = sender.send(code);
                        }
                        "You can close this tab. Code captured."
                    } else {
                        "Missing ?code= parameter"
                    }
                },
            ),
        )
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    let server_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("Server error: {}", e);
        }
    });

    // Open authorize URL in browser (macOS)
    // Read credentials from environment (.env)
    let client_id = env::var("STRAVA_CLIENT_ID").unwrap_or_else(|_| String::new());
    let client_secret = env::var("STRAVA_CLIENT_SECRET").unwrap_or_else(|_| String::new());

    // Validate client_id looks numeric; Strava rejects invalid app IDs.
    if client_id.is_empty() || !client_id.chars().all(|c| c.is_ascii_digit()) {
        eprintln!("Invalid Client ID. Set STRAVA_CLIENT_ID to your numeric ID or fill CLIENT_ID constant.");
        return Ok(());
    }
    if client_secret.is_empty() {
        eprintln!("Missing Client Secret. Set STRAVA_CLIENT_SECRET or fill CLIENT_SECRET constant from Strava app settings.");
        return Ok(());
    }

    // Use localhost redirect and ensure it matches your Strava app settings exactly.
    let redirect_uri = "http://localhost:8080/callback";
    // Hardcode scopes to include private activities as requested
    let authorize_url = format!(
        "https://www.strava.com/oauth/authorize?client_id={}&response_type=code&redirect_uri={}&approval_prompt=auto&scope=read,activity:read,activity:read_all",
        client_id, redirect_uri
    );
    println!("Opening browser for OAuth: {}", authorize_url);
    let _ = Command::new("open").arg(&authorize_url).status();

    // Wait for the code, but don't hang forever: add a timeout.
    let code = match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(code)) => code,
        Ok(Err(_)) => {
            eprintln!("Callback channel closed before receiving code.");
            server_handle.abort();
            return Ok(());
        }
        Err(_) => {
            eprintln!("Timed out waiting for OAuth callback (no code received in 10s).");
            server_handle.abort();
            return Ok(());
        }
    };
    server_handle.abort();

    // Exchange code for access token
    let exchange_resp = client
        .post("https://www.strava.com/oauth/token")
        .form(&serde_json::json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "code": code,
            "grant_type": "authorization_code",
        }))
        .send()
        .await?;

    let token_value = if exchange_resp.status().is_success() {
        let token: TokenResponse = exchange_resp.json().await?;
        println!("Obtained access token via OAuth.");
        token.access_token
    } else {
        let status = exchange_resp.status();
        let body = exchange_resp.text().await.unwrap_or_default();
        eprintln!("Token exchange failed: status={} body={}", status, body);
        println!("Falling back to initial access token (scopes may be insufficient).");
        env::var("STRAVA_ACCESS_TOKEN").unwrap_or_default()
    };

    // Env token no longer used in this one-off test; we refresh or fallback to hardcoded values.

    // Reuse client; use token_value for subsequent calls

    // Example 1: Get current athlete profile
    let athlete_resp = client
        .get("https://www.strava.com/api/v3/athlete")
        .header(AUTHORIZATION, format!("Bearer {}", token_value))
        .header(USER_AGENT, "rust-strava-example/0.1")
        .send()
        .await?;

    if !athlete_resp.status().is_success() {
        let status = athlete_resp.status();
        let body = athlete_resp.text().await.unwrap_or_default();
        eprintln!(
            "Athlete request failed: status={} body={}\nHint: 401 means the token is invalid or expired, or scopes are missing. Ensure you authorized with at least 'read'.",
            status,
            body
        );
        return Ok(());
    }

    let athlete: Athlete = athlete_resp.json().await?;

    println!(
        "Authenticated as athlete id={} name={} {} username={}",
        athlete.id,
        athlete.firstname.as_deref().unwrap_or(""),
        athlete.lastname.as_deref().unwrap_or(""),
        athlete.username.as_deref().unwrap_or("")
    );

    // Example 2: List recent activities (first 5)
    let activities_resp = client
        .get("https://www.strava.com/api/v3/athlete/activities")
        .query(&[("per_page", args.per_page), ("page", args.page)])
        .header(AUTHORIZATION, format!("Bearer {}", token_value))
        .header(USER_AGENT, "rust-strava-example/0.1")
        .send()
        .await?;

    if !activities_resp.status().is_success() {
        let status = activities_resp.status();
        let body = activities_resp.text().await.unwrap_or_default();
        eprintln!(
            "Activities request failed: status={} body={}\nHint: 401 usually means the token lacks required scopes (e.g., 'activity:read'), is expired, or invalid. Regenerate via OAuth with correct scopes.",
            status,
            body
        );
        return Ok(());
    }

    let activities_json: serde_json::Value = activities_resp.json().await?;
    println!(
        "Recent activities (JSON):\n{}",
        serde_json::to_string_pretty(&activities_json)?
    );

    // Parse minimal activity summaries
    let activities: Vec<ActivitySummary> =
        serde_json::from_value(activities_json.clone()).unwrap_or_default();
    if activities.is_empty() {
        return Ok(());
    }

    // Create gpx output folder
    let mut out_dir = PathBuf::from("gpx");
    fs::create_dir_all(&out_dir)?;

    // For each activity, fetch streams and generate GPX
    for act in activities.iter() {
        let id = act.id;
        let name = act.name.as_deref().unwrap_or("");
        println!("Exporting GPX for activity {} - {}", id, name);

        // Fetch streams (latlng, time, altitude)
        let streams_resp = client
            .get(format!(
                "https://www.strava.com/api/v3/activities/{}/streams",
                id
            ))
            .query(&[("keys", "latlng,time,altitude"), ("key_by_type", "true")])
            .header(AUTHORIZATION, format!("Bearer {}", token_value))
            .header(USER_AGENT, "rust-strava-example/0.1")
            .send()
            .await?;

        if !streams_resp.status().is_success() {
            let status = streams_resp.status();
            let body = streams_resp.text().await.unwrap_or_default();
            eprintln!(
                "Streams request failed for {}: status={} body={}",
                id, status, body
            );
            continue;
        }

        let streams_json: serde_json::Value = streams_resp.json().await?;
        let streams: StreamSet = serde_json::from_value(streams_json).unwrap_or(StreamSet {
            latlng: None,
            time: None,
            altitude: None,
        });

        // Build GPX content
        let file_path = out_dir.join(format!("activity_{}.gpx", id));
        let gpx = build_gpx_xml(name, &streams);
        fs::write(&file_path, gpx)?;
        println!("Saved GPX: {}", file_path.display());
    }

    Ok(())
}

// ...existing code (build_gpx_xml, xml_escape functions - keep these)...

fn build_gpx_xml(name: &str, streams: &StreamSet) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<gpx version=\"1.1\" creator=\"rust-strava-example\" xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://www.topografix.com/GPX/1/1 http://www.topografix.com/GPX/1/1/gpx.xsd\">\n");
    xml.push_str(&format!(
        "  <trk>\n    <name>{}</name>\n    <trkseg>\n",
        xml_escape(name)
    ));

    let points = streams.latlng.as_ref().map(|v| v.data.len()).unwrap_or(0);
    for i in 0..points {
        let (lat, lon) = streams
            .latlng
            .as_ref()
            .and_then(|v| v.data.get(i))
            .map(|p| (p[0], p[1]))
            .unwrap_or((0.0, 0.0));
        let ele = streams
            .altitude
            .as_ref()
            .and_then(|v| v.data.get(i))
            .copied();
        xml.push_str(&format!(
            "      <trkpt lat=\"{:.7}\" lon=\"{:.7}\">\n",
            lat, lon
        ));
        if let Some(e) = ele {
            xml.push_str(&format!("        <ele>{:.2}</ele>\n", e));
        }
        xml.push_str("      </trkpt>\n");
    }

    xml.push_str("    </trkseg>\n  </trk>\n</gpx>\n");
    xml
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
