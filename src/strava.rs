use chrono::{DateTime, Duration, Utc};
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const USER_AGENT_VALUE: &str = "rust-strava-example/0.1";

#[derive(Debug, Deserialize)]
pub struct Athlete {
    pub id: i64,
    pub username: Option<String>,
    pub firstname: Option<String>,
    pub lastname: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenResponse {
    pub token_type: Option<String>,
    pub access_token: String,
    pub expires_at: Option<i64>,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ActivitySummary {
    pub id: i64,
    pub name: Option<String>,
    pub start_date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TypedStream<T> {
    #[serde(default)]
    pub data: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub struct StreamSet {
    #[serde(default)]
    pub latlng: Option<TypedStream<[f64; 2]>>,
    #[serde(default)]
    pub time: Option<TypedStream<i64>>,
    #[serde(default)]
    pub altitude: Option<TypedStream<f64>>,
}

/// Creates a new reqwest client with the appropriate user agent
pub fn create_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT_VALUE)
        .build()
}

/// Exchange an authorization code for an access token
pub async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
) -> Result<TokenResponse, Box<dyn std::error::Error + Send + Sync>> {
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
        return Err(format!("Token exchange failed: status={} body={}", status, body).into());
    }

    let token: TokenResponse = resp.json().await?;
    Ok(token)
}

/// Refresh an expired access token using a refresh token
pub async fn refresh_token(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .post("https://www.strava.com/oauth/token")
        .form(&serde_json::json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "refresh_token": refresh_token,
            "grant_type": "refresh_token",
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Token refresh failed: status={} body={}", status, body).into());
    }

    let token: TokenResponse = resp.json().await?;
    Ok(token)
}

/// Get the OAuth authorization URL
pub fn get_authorize_url(client_id: &str, redirect_uri: &str) -> String {
    format!(
        "https://www.strava.com/oauth/authorize?client_id={}&response_type=code&redirect_uri={}&approval_prompt=auto&scope=read,activity:read,activity:read_all",
        client_id, redirect_uri
    )
}

/// Fetch the authenticated athlete's profile
pub async fn get_athlete(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Athlete, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get("https://www.strava.com/api/v3/athlete")
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header(USER_AGENT, USER_AGENT_VALUE)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Athlete request failed: status={} body={}\nHint: 401 means the token is invalid or expired, or scopes are missing.",
            status, body
        ).into());
    }

    let athlete: Athlete = resp.json().await?;
    Ok(athlete)
}

/// Fetch a list of activities for the authenticated athlete
pub async fn get_activities(
    client: &reqwest::Client,
    access_token: &str,
    per_page: u32,
    page: u32,
) -> Result<Vec<ActivitySummary>, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get("https://www.strava.com/api/v3/athlete/activities")
        .query(&[("per_page", per_page), ("page", page)])
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header(USER_AGENT, USER_AGENT_VALUE)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Activities request failed: status={} body={}\nHint: 401 usually means the token lacks required scopes.",
            status, body
        ).into());
    }

    let activities_json: serde_json::Value = resp.json().await?;
    println!(
        "Recent activities (JSON):\n{}",
        serde_json::to_string_pretty(&activities_json)?
    );

    let activities: Vec<ActivitySummary> =
        serde_json::from_value(activities_json).unwrap_or_default();
    Ok(activities)
}

/// Fetch streams (latlng, time, altitude) for a specific activity
pub async fn get_activity_streams(
    client: &reqwest::Client,
    access_token: &str,
    activity_id: i64,
) -> Result<StreamSet, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .get(format!(
            "https://www.strava.com/api/v3/activities/{}/streams",
            activity_id
        ))
        .query(&[("keys", "latlng,time,altitude"), ("key_by_type", "true")])
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header(USER_AGENT, USER_AGENT_VALUE)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Streams request failed for {}: status={} body={}",
            activity_id, status, body
        )
        .into());
    }

    let streams_json: serde_json::Value = resp.json().await?;
    let streams: StreamSet = serde_json::from_value(streams_json).unwrap_or(StreamSet {
        latlng: None,
        time: None,
        altitude: None,
    });
    Ok(streams)
}

/// Export activities as GPX files to the specified directory
/// If fetch_all is false, already imported activities are skipped
/// Returns (imported_count, skipped_count)
pub async fn export_activities_as_gpx(
    client: &reqwest::Client,
    access_token: &str,
    activities: &[ActivitySummary],
    out_dir: &PathBuf,
    db_conn: Option<&rusqlite::Connection>,
    fetch_all: bool,
) -> Result<(u32, u32), Box<dyn std::error::Error + Send + Sync>> {
    fs::create_dir_all(out_dir)?;

    let mut imported_count: u32 = 0;
    let mut skipped_count: u32 = 0;

    for act in activities.iter() {
        let id = act.id;
        let name = act.name.as_deref().unwrap_or("");
        
        // Check if activity was already imported (unless --fetch-all is set)
        if !fetch_all {
            if let Some(conn) = db_conn {
                if crate::database::is_activity_imported(conn, id).unwrap_or(false) {
                    println!("Skipping already imported activity {} - {}", id, name);
                    skipped_count += 1;
                    continue;
                }
            }
        }

        println!("Exporting GPX for activity {} - {}", id, name);

        match get_activity_streams(client, access_token, id).await {
            Ok(streams) => {
                let file_path = out_dir.join(format!("activity_{}.gpx", id));
                let start_date = act.start_date.as_deref();
                let gpx = build_gpx_xml(name, start_date, &streams);
                fs::write(&file_path, gpx)?;
                println!("Saved GPX: {}", file_path.display());
                
                // Mark activity as imported in database
                if let Some(conn) = db_conn {
                    if let Err(e) = crate::database::mark_activity_imported(conn, id, act.name.as_deref()) {
                        eprintln!("Warning: Failed to mark activity {} as imported: {}", id, e);
                    }
                }
                imported_count += 1;
            }
            Err(e) => {
                eprintln!("Failed to get streams for activity {}: {}", id, e);
                continue;
            }
        }
    }

    println!("\nImport summary: {} imported, {} skipped (already imported)", imported_count, skipped_count);
    Ok((imported_count, skipped_count))
}

/// Build GPX XML content from activity data and streams
pub fn build_gpx_xml(name: &str, start_date: Option<&str>, streams: &StreamSet) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<gpx version=\"1.1\" creator=\"rust-strava-example\" xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:schemaLocation=\"http://www.topografix.com/GPX/1/1 http://www.topografix.com/GPX/1/1/gpx.xsd\">\n");

    let start_time: Option<DateTime<Utc>> = start_date.and_then(|d| d.parse().ok());

    if let Some(date) = start_date {
        xml.push_str("  <metadata>\n");
        xml.push_str(&format!("    <time>{}</time>\n", xml_escape(date)));
        xml.push_str("  </metadata>\n");
    }

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
        let point_time: Option<DateTime<Utc>> = start_time.and_then(|st| {
            streams
                .time
                .as_ref()
                .and_then(|t| t.data.get(i))
                .map(|&secs| st + Duration::seconds(secs))
        });

        xml.push_str(&format!(
            "      <trkpt lat=\"{:.7}\" lon=\"{:.7}\">\n",
            lat, lon
        ));
        if let Some(e) = ele {
            xml.push_str(&format!("        <ele>{:.2}</ele>\n", e));
        }
        if let Some(t) = point_time {
            xml.push_str(&format!("        <time>{}</time>\n", t.to_rfc3339()));
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
