//! Account commands: `login`, `logout`, `whoami`.
//!
//! These authenticate against the siGit account API and store a session token
//! locally. The token is the credential used for siGit Code Cloud requests.
//!
//! Base URL: `$SIGIT_API_URL`, else `https://sigit.si`.

use serde::Deserialize;

use crate::credentials::{self, Credentials};

/// Default account API host. Override with `SIGIT_API_URL` (dev: `http://localhost:8088`).
const DEFAULT_API_URL: &str = "https://sigit.si";

fn api_base() -> String {
    std::env::var("SIGIT_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string())
}

// ── sigit.si /api/v1 response shapes ─────────────────────────────────────────────

/// Sign-in response. A successful sign-in carries an `access_token`; an
/// unverified account reports a `status`; failures arrive as an `error`.
#[derive(Debug, Deserialize)]
struct SignInResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MeResponse {
    #[serde(default)]
    email: Option<String>,
}

// ── Commands ──────────────────────────────────────────────────────────────────────

/// `sigit login`: prompt for credentials, authenticate, and store the token.
pub async fn login() -> anyhow::Result<()> {
    let base = api_base();
    println!("Sign in to siGit Code Cloud ({base})");

    let email = prompt("Email: ")?;
    let password = rpassword::prompt_password("Password: ")?;
    if email.trim().is_empty() || password.is_empty() {
        anyhow::bail!("email and password are required");
    }

    let url = format!("{}/api/v1/users/sign_in", base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "email": email.trim(), "password": password }))
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("could not reach siGit Code Cloud: {error}"))?;

    let status = response.status();
    let parsed: SignInResponse = response
        .json()
        .await
        .map_err(|error| anyhow::anyhow!("unexpected response from siGit Code Cloud: {error}"))?;

    if let Some(token) = parsed.access_token.filter(|token| !token.trim().is_empty()) {
        credentials::store(&Credentials {
            access_token: token,
            email: Some(email.trim().to_string()),
        })
        .map_err(|error| anyhow::anyhow!("could not save session: {error}"))?;
        println!("✓ Signed in as {}. siGit Code Cloud is ready.", email.trim());
        return Ok(());
    }

    // No token: surface the most specific message available.
    if let Some(message) = parsed.error.and_then(|error| error.message) {
        anyhow::bail!("sign-in failed: {message}");
    }
    if let Some(account_status) = parsed.status {
        anyhow::bail!(
            "sign-in incomplete (status: {account_status}). Check your email to verify your account."
        );
    }
    anyhow::bail!("sign-in failed (HTTP {})", status.as_u16());
}

/// `sigit logout`: clear the local session, notifying the server best-effort.
pub async fn logout() -> anyhow::Result<()> {
    if let Some(token) = credentials::load_token() {
        let url = format!("{}/api/v1/users/sign_out", api_base().trim_end_matches('/'));
        // Best-effort: a failed server call must not block local logout.
        let _ = reqwest::Client::new()
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await;
    }
    if credentials::clear() {
        println!("✓ Signed out of siGit Code Cloud.");
    } else {
        println!("Not signed in.");
    }
    Ok(())
}

/// `sigit whoami`: show the signed-in account, verifying the token if reachable.
pub async fn whoami() -> anyhow::Result<()> {
    let Some(creds) = credentials::load() else {
        println!("Not signed in. Run `sigit login` to use siGit Code Cloud.");
        return Ok(());
    };

    let url = format!("{}/api/v1/me", api_base().trim_end_matches('/'));
    match reqwest::Client::new()
        .get(&url)
        .bearer_auth(&creds.access_token)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            let email = response
                .json::<MeResponse>()
                .await
                .ok()
                .and_then(|me| me.email)
                .or(creds.email)
                .unwrap_or_else(|| "(unknown)".to_string());
            println!("Signed in to siGit Code Cloud as {email}.");
        }
        Ok(response) => {
            println!(
                "Session may be expired (HTTP {}). Run `sigit login` again.",
                response.status().as_u16()
            );
        }
        Err(_) => {
            // Offline: fall back to the cached email.
            let email = creds.email.unwrap_or_else(|| "(unknown)".to_string());
            println!("Signed in as {email} (could not reach siGit Code Cloud to verify).");
        }
    }
    Ok(())
}

/// Print a prompt and read one trimmed line from stdin.
fn prompt(label: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}
