fn main() {
    // Read STRAVA_CLIENT_ID and STRAVA_CLIENT_SECRET from the .env file in the
    // project root and bake them into the binary at compile time.
    // The build fails with a clear error when either value is missing or empty.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let env_path = manifest_dir.join(".env");

    let (client_id, client_secret) = read_credentials_from_env(&env_path);

    if client_id.is_empty() {
        panic!(
            "\n\n\
            ❌  STRAVA_CLIENT_ID fehlt!\n\
            Trage STRAVA_CLIENT_ID=<deine_id> in die Datei .env ein.\n\n"
        );
    }
    if client_secret.is_empty() {
        panic!(
            "\n\n\
            ❌  STRAVA_CLIENT_SECRET fehlt!\n\
            Trage STRAVA_CLIENT_SECRET=<dein_secret> in die Datei .env ein.\n\n"
        );
    }

    // Bake values into the binary (accessible via env!() in source code)
    println!("cargo:rustc-env=STRAVA_CLIENT_ID={}", client_id);
    println!("cargo:rustc-env=STRAVA_CLIENT_SECRET={}", client_secret);

    // Re-run this build script whenever .env changes
    println!("cargo:rerun-if-changed=.env");

    tauri_build::build()
}

/// Parse KEY=VALUE lines from a .env file.
/// Ignores comments and blank lines. Falls back to the process environment.
fn read_credentials_from_env(env_path: &std::path::Path) -> (String, String) {
    let mut client_id = std::env::var("STRAVA_CLIENT_ID").unwrap_or_default();
    let mut client_secret = std::env::var("STRAVA_CLIENT_SECRET").unwrap_or_default();

    if let Ok(contents) = std::fs::read_to_string(env_path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.starts_with('#') || !line.contains('=') {
                continue;
            }
            let (key, value) = line.split_once('=').unwrap();
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'');
            match key {
                "STRAVA_CLIENT_ID" => client_id = value.to_string(),
                "STRAVA_CLIENT_SECRET" => client_secret = value.to_string(),
                _ => {}
            }
        }
    }

    (client_id, client_secret)
}
