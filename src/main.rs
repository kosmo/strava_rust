use axum::{extract::Query, extract::State, routing::get, Router};
use clap::Parser;
use dotenvy::dotenv;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

mod map_server;
mod strava;
mod tiles;

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

        let client = strava::create_client()?;

        match strava::exchange_code(&client, &client_id, &client_secret, &code).await {
            Ok(token) => {
                println!("Access token: {}", token.access_token);
                if let Some(rt) = token.refresh_token {
                    println!("Refresh token: {}", rt);
                }
                println!("Note: Save tokens securely. Do NOT commit them.");
            }
            Err(e) => {
                eprintln!("{}", e);
            }
        }
        return Ok(());
    }

    // If no exchange requested, run a local OAuth authorize + callback to obtain a fresh token with required scopes.
    let client = strava::create_client()?;

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
    let authorize_url = strava::get_authorize_url(&client_id, redirect_uri);
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
    let token_value = match strava::exchange_code(&client, &client_id, &client_secret, &code).await
    {
        Ok(token) => {
            println!("Obtained access token via OAuth.");
            token.access_token
        }
        Err(e) => {
            eprintln!("{}", e);
            println!("Falling back to initial access token (scopes may be insufficient).");
            env::var("STRAVA_ACCESS_TOKEN").unwrap_or_default()
        }
    };

    // Reuse client; use token_value for subsequent calls

    // Example 1: Get current athlete profile
    let athlete = match strava::get_athlete(&client, &token_value).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{}", e);
            return Ok(());
        }
    };

    println!(
        "Authenticated as athlete id={} name={} {} username={}",
        athlete.id,
        athlete.firstname.as_deref().unwrap_or(""),
        athlete.lastname.as_deref().unwrap_or(""),
        athlete.username.as_deref().unwrap_or("")
    );

    // Example 2: List recent activities
    let activities =
        match strava::get_activities(&client, &token_value, args.per_page, args.page).await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("{}", e);
                return Ok(());
            }
        };

    if activities.is_empty() {
        return Ok(());
    }

    // Export activities as GPX files
    let out_dir = PathBuf::from("gpx");
    strava::export_activities_as_gpx(&client, &token_value, &activities, &out_dir).await?;

    Ok(())
}
