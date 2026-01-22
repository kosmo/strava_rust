# Rust Strava API Example

This is a minimal Rust example showing how to call the Strava API using an access token.

## Setup

1. Install Rust toolchain:

   - macOS: https://www.rust-lang.org/tools/install

2. Create a Strava access token:

   - Follow https://developers.strava.com/docs/getting-started/ to register an app and obtain an OAuth token.
   - Do not commit your token.

3. Create `.env` with your Strava app credentials:

```bash
STRAVA_CLIENT_ID=your_numeric_id
STRAVA_CLIENT_SECRET=your_client_secret
# Optional fallback token if exchange fails
STRAVA_ACCESS_TOKEN=
```

## Run

```bash
cd rust_strava
cargo run
```

The program:

- Calls `GET https://www.strava.com/api/v3/athlete` to verify authentication and print your athlete name.
- Calls `GET https://www.strava.com/api/v3/athlete/activities?per_page=5` and prints recent activities as prettified JSON.

## Notes

- Uses `reqwest` with Rustls TLS, `tokio` runtime, and `dotenvy` to load `.env`.
- For production, implement full OAuth flow and token refresh as needed.

## Get a Valid Token (OAuth)

Strava uses OAuth2. To obtain a valid token with the right scopes:

1. Create a Strava application and note `Client ID` and `Client Secret`.
2. Authorize the user using this URL (replace `CLIENT_ID` and `REDIRECT_URI`):

```
https://www.strava.com/oauth/authorize?client_id=CLIENT_ID&response_type=code&redirect_uri=REDIRECT_URI&approval_prompt=auto&scope=read,activity:read
```

3. After the user approves, Strava redirects to `REDIRECT_URI?code=...`. Copy that `code`.
4. Exchange the code for a token using the built-in helper:

```bash
export STRAVA_CLIENT_ID=YOUR_CLIENT_ID
export STRAVA_CLIENT_SECRET=YOUR_CLIENT_SECRET
cargo run -- --exchange-code "PASTE_AUTH_CODE_HERE"
```

This prints `Access token` (and a `Refresh token` if present). Save securely and set:

```bash
export STRAVA_ACCESS_TOKEN="PASTE_ACCESS_TOKEN"
```

Then run the example:

```bash
cargo run -- --per-page 5 --page 1
```

If you need private activities, the app will request `activity:read_all` automatically during authorization.
