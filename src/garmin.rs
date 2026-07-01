//! Garmin Connect API client.
//!
//! Authentication flow:
//! 1. A Tauri WebviewWindow opens the Garmin SSO login page.
//! 2. An `initialization_script` watches for `?ticket=ST-…` in the URL while on
//!    a garmin.com page, then navigates to `garmin-auth://callback?ticket=…`.
//! 3. `on_navigation` in the window builder intercepts that custom-scheme URL,
//!    extracts the ticket, and calls `exchange_ticket()`.
//! 4. `exchange_ticket` uses diauth.garmin.com (the current working endpoint)
//!    to obtain Bearer + refresh tokens.

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::Deserialize;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

const USER_AGENT_STR: &str = "GCM-Android-5.23";

/// A single activity entry from the Garmin activitylist-service.
#[derive(Debug, Deserialize)]
pub struct GarminActivity {
    #[serde(rename = "activityId")]
    pub activity_id: i64,
    #[serde(rename = "activityName", default)]
    pub activity_name: String,
    /// Distance in meters
    #[serde(default)]
    pub distance: f64,
    #[serde(rename = "elevationGain", default)]
    pub elevation_gain: f64,
    /// Activity type, e.g. `road_biking`, `mountain_biking`, `cycling`.
    #[serde(rename = "activityType", default)]
    pub activity_type: GarminActivityType,
}

/// The `activityType` object of a Garmin activity. Only the `typeKey`
/// (e.g. `road_biking`, `mountain_biking`) is relevant for us.
#[derive(Debug, Deserialize, Default)]
pub struct GarminActivityType {
    #[serde(rename = "typeKey", default)]
    pub type_key: String,
}

#[derive(Deserialize)]
struct DiTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
}

/// Create a reqwest client with the Garmin mobile user-agent.
pub fn create_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT_STR)
        .build()
}

/// Minimal Base-64 encoder (avoids adding a crate dependency).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut i = 0;
    while i < input.len() {
        let b0 = input[i] as u32;
        let b1 = if i + 1 < input.len() {
            input[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < input.len() {
            input[i + 2] as u32
        } else {
            0
        };
        out.push(TABLE[((b0 >> 2) & 0x3F) as usize] as char);
        out.push(TABLE[(((b0 & 0x3) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if i + 1 < input.len() {
            TABLE[(((b1 & 0xF) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if i + 2 < input.len() {
            TABLE[(b2 & 0x3F) as usize] as char
        } else {
            '='
        });
        i += 3;
    }
    out
}

/// Exchange a CAS service ticket for DI OAuth2 (access_token, refresh_token).
///
/// Uses `diauth.garmin.com` — the current working endpoint as of 2025/2026.
/// Tries multiple `client_id` values in order, which is what python-garminconnect does.
pub async fn exchange_ticket(ticket: &str) -> Result<(String, String), Error> {
    let client = create_client()?;

    // Client IDs to try in order (matching what garminconnect 0.3.x uses)
    let client_ids = [
        "GARMIN_CONNECT_MOBILE_ANDROID_DI_2025Q2",
        "GARMIN_CONNECT_MOBILE_ANDROID_DI_2024Q4",
        "GARMIN_CONNECT_MOBILE_ANDROID_DI",
    ];

    let mut last_err = String::new();

    for client_id in &client_ids {
        // Basic auth: base64("client_id:")
        let basic = base64_encode(format!("{}:", client_id).as_bytes());

        let body = format!(
            "client_id={}&service_ticket={}&grant_type={}&service_url={}",
            urlencoding(client_id),
            urlencoding(ticket),
            urlencoding(
                "https://connectapi.garmin.com/di-oauth2-service/oauth/grant/service_ticket"
            ),
            urlencoding("https://connect.garmin.com/app"),
        );

        let resp = client
            .post("https://diauth.garmin.com/di-oauth2-service/oauth/token")
            .header(AUTHORIZATION, format!("Basic {}", basic))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(USER_AGENT, USER_AGENT_STR)
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            let tok: DiTokenResponse = resp.json().await?;
            let refresh = tok.refresh_token.unwrap_or_default();
            println!(
                "Garmin DI token exchange succeeded with client_id={}",
                client_id
            );
            return Ok((tok.access_token, refresh));
        }

        let body_text = resp.text().await.unwrap_or_default();
        println!(
            "Garmin DI token exchange failed (client_id={}): HTTP {} – {}",
            client_id, status, body_text
        );
        last_err = format!("HTTP {} – {}", status, body_text);
    }

    Err(format!("All client_ids failed. Last error: {}", last_err).into())
}

/// Refresh an existing access token.
pub async fn refresh_access_token(
    client_id: &str,
    refresh_token: &str,
) -> Result<(String, String), Error> {
    let client = create_client()?;
    let basic = base64_encode(format!("{}:", client_id).as_bytes());

    let body = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        urlencoding(client_id),
        urlencoding(refresh_token),
    );

    let resp = client
        .post("https://diauth.garmin.com/di-oauth2-service/oauth/token")
        .header(AUTHORIZATION, format!("Basic {}", basic))
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(USER_AGENT, USER_AGENT_STR)
        .body(body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Token refresh failed: HTTP {} – {}", status, body).into());
    }

    let tok: DiTokenResponse = resp.json().await?;
    let new_refresh = tok
        .refresh_token
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| refresh_token.to_string());
    Ok((tok.access_token, new_refresh))
}

/// Fetch a page of activities. `start` is zero-based; `limit` ≤ 100.
pub async fn get_activities(
    client: &reqwest::Client,
    access_token: &str,
    start: u32,
    limit: u32,
) -> Result<Vec<GarminActivity>, Error> {
    let resp = client
        .get("https://connectapi.garmin.com/activitylist-service/activities/search/activities")
        .query(&[("start", start), ("limit", limit)])
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header(USER_AGENT, USER_AGENT_STR)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Activity list failed: HTTP {} – {}", status, body).into());
    }

    Ok(resp.json().await?)
}

/// Download the GPX export of a single activity.
pub async fn download_gpx(
    client: &reqwest::Client,
    access_token: &str,
    activity_id: i64,
) -> Result<String, Error> {
    let url = format!(
        "https://connectapi.garmin.com/download-service/export/gpx/activity/{}",
        activity_id
    );

    let resp = client
        .get(&url)
        .header(AUTHORIZATION, format!("Bearer {}", access_token))
        .header(USER_AGENT, USER_AGENT_STR)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "GPX download failed for {}: HTTP {} – {}",
            activity_id, status, body
        )
        .into());
    }

    Ok(resp.text().await?)
}

/// Headless login: POST username + password to the Garmin SSO form, then
/// exchange the resulting CAS service ticket for DI OAuth2 tokens.
///
/// Requires the `cookies` feature of reqwest (cookie jar for session handling).
pub async fn login(email: &str, password: &str) -> Result<(String, String), Error> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT_STR)
        .cookie_store(true)
        // Don't auto-follow redirects so we can read the Location header on the
        // POST response and extract the service ticket from it.
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let sso_base = "https://sso.garmin.com/sso/signin";
    let service_url = "https://connect.garmin.com/app";

    let params = [
        ("service", service_url),
        ("gauthHost", "https://sso.garmin.com/sso"),
        ("source", "https://connect.garmin.com/signin/"),
        ("redirectAfterAccountLoginUrl", service_url),
        ("redirectAfterAccountCreationUrl", service_url),
        ("locale", "en_US"),
        ("id", "gauth-widget"),
        ("clientId", "GarminConnect"),
        ("rememberMeShown", "true"),
        ("rememberMeChecked", "false"),
        ("createAccountShown", "true"),
        ("openCreateAccount", "false"),
        ("displayNameShown", "false"),
        ("consumeServiceTicket", "false"),
        ("initialFocus", "true"),
        ("embedWidget", "false"),
        ("generateExtraServiceTicket", "true"),
        ("generateTwoExtraServiceTickets", "false"),
        ("generateNoServiceTicket", "false"),
    ];

    // Step 1: GET the signin page → establishes session cookie + returns CSRF token.
    let get_resp = client.get(sso_base).query(&params).send().await?;
    let get_status = get_resp.status();
    if !get_status.is_success() {
        return Err(format!("Garmin SSO GET fehlgeschlagen: HTTP {}", get_status).into());
    }
    let html = get_resp.text().await?;
    let csrf = extract_csrf(&html).ok_or(
        "CSRF-Token nicht im Garmin-Login-Formular gefunden – \
         prüfe ob sso.garmin.com erreichbar ist",
    )?;

    println!("Garmin SSO: CSRF-Token gefunden, sende Zugangsdaten…");

    // Step 2: POST credentials.
    let form_body = format!(
        "username={}&password={}&_csrf={}&embed=false&rememberme=on",
        urlencoding(email),
        urlencoding(password),
        urlencoding(&csrf),
    );

    let post_resp = client
        .post(sso_base)
        .query(&params)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header("Referer", sso_base)
        .body(form_body)
        .send()
        .await?;

    let post_status = post_resp.status();

    // Step 3: Extract the service ticket from the 302 Location header.
    let ticket = if post_status.is_redirection() {
        let location = post_resp
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .ok_or("Kein Location-Header nach SSO-Login-POST")?;
        println!("Garmin SSO Redirect → {}", location);
        let loc_url = reqwest::Url::parse(location)
            .map_err(|e| format!("Ungültige Redirect-URL '{}': {}", location, e))?;
        loc_url
            .query_pairs()
            .find(|(k, _)| k == "ticket")
            .map(|(_, v)| v.into_owned())
            .ok_or_else(|| format!("Kein 'ticket' in Redirect-URL: {}", location))?
    } else {
        // Some SSO versions embed the ticket in the response body.
        let body_text = post_resp.text().await?;
        extract_ticket_from_html(&body_text).ok_or_else(|| {
            if body_text.to_lowercase().contains("invalid")
                || body_text.to_lowercase().contains("incorrect")
            {
                "Falsche E-Mail-Adresse oder falsches Passwort".to_string()
            } else {
                format!(
                    "Login lieferte HTTP {} – kein Ticket (falsche Zugangsdaten?)",
                    post_status
                )
            }
        })?
    };

    println!("Garmin SSO: Ticket erhalten, tausche gegen OAuth2-Token…");
    exchange_ticket(&ticket).await
}

/// Find the `_csrf` hidden-input value in Garmin's SSO HTML.
fn extract_csrf(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<input") {
        let abs = pos + rel;
        let tag_end = lower[abs..]
            .find('>')
            .map(|e| abs + e + 1)
            .unwrap_or(lower.len());
        let tag_lower = &lower[abs..tag_end];
        if tag_lower.contains("name=\"_csrf\"") || tag_lower.contains("name='_csrf'") {
            let tag_orig = &html[abs..tag_end];
            if let Some(v) = extract_attr_value(tag_orig, "value") {
                return Some(v);
            }
        }
        pos = abs + 1;
    }
    None
}

fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    let tag_l = tag.to_lowercase();
    for q in &['"', '\''] {
        let needle = format!("{}={}", attr, q);
        if let Some(p) = tag_l.find(&needle) {
            let start = p + needle.len();
            let end = tag[start..].find(*q)?;
            return Some(tag[start..start + end].to_string());
        }
    }
    None
}

fn extract_ticket_from_html(html: &str) -> Option<String> {
    let marker = "ticket=ST-";
    let pos = html.find(marker)?;
    let start = pos + 7; // skip "ticket="
    let rest = &html[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
}

/// Percent-encode a string for use in `application/x-www-form-urlencoded` bodies.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
