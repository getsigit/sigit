//! Account access for siGit Code Cloud, surfaced as the `/login`, `/logout`,
//! and `/whoami` slash commands in both the TUI and ACP sessions.
//!
//! These functions authenticate against the siGit account API and store a
//! session token locally. The token is the credential used for siGit Code Cloud
//! requests. They perform no console I/O, so each slash surface can render the
//! returned message however it likes.
//!
//! Base URL: `$SIGIT_API_URL`, else `https://sigit.si`.

use serde::Deserialize;

use crate::credentials::{self, Credentials};

/// Default account API host. Override with `SIGIT_API_URL` (dev: `http://localhost:8088`).
const DEFAULT_API_URL: &str = "https://sigit.si";

fn api_base() -> String {
    std::env::var("SIGIT_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string())
}

// Sign-in returns an `AccountStatus`, one of:
//   "NotFound"                            (a bare JSON string)
//   {"Ready":{"access_token":"…"}}
//   {"Incomplete":{"status":<u32>}}
// Failures return {"error_code":<i32>,"message":"…"}. Parsed from a
// `serde_json::Value` rather than a struct because of the bare-string variant.

#[derive(Debug, Deserialize)]
struct MeResponse {
    #[serde(default)]
    email: Option<String>,
}

/// Authenticate with email and password, storing the session token on success.
/// Returns the signed-in email, or a human-readable error message.
pub async fn authenticate(email: &str, password: &str) -> Result<String, String> {
    let email = email.trim();
    if email.is_empty() || password.is_empty() {
        return Err("email and password are required".to_string());
    }

    let url = format!("{}/api/v1/auth/sign_in", api_base().trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "email": email, "password": password }))
        .send()
        .await
        .map_err(|error| format!("could not reach siGit Code Cloud: {error}"))?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("unexpected response from siGit Code Cloud: {error}"))?;

    if status.is_success() {
        // AccountStatus::Ready
        if let Some(token) = body
            .get("Ready")
            .and_then(|ready| ready.get("access_token"))
            .and_then(|token| token.as_str())
            .filter(|token| !token.trim().is_empty())
        {
            credentials::store(&Credentials {
                access_token: token.to_string(),
                email: Some(email.to_string()),
            })?;
            return Ok(email.to_string());
        }
        // AccountStatus::Incomplete
        if body.get("Incomplete").is_some() {
            return Err(
                "your account is not verified yet. Check your email to confirm it, then sign in again."
                    .to_string(),
            );
        }
        // AccountStatus::NotFound (a bare JSON string)
        if body.as_str() == Some("NotFound") {
            return Err(format!("no siGit account found for {email}."));
        }
        return Err("unexpected sign-in response from siGit Code Cloud".to_string());
    }

    // ErrorResponse { error_code, message }
    let message = body
        .get("message")
        .and_then(|message| message.as_str())
        .unwrap_or("sign-in failed");
    Err(format!("sign-in failed: {message}"))
}

/// Clear the local session, notifying the server best-effort. Returns a message
/// suitable for display.
pub async fn end_session() -> String {
    if let Some(token) = credentials::load_token() {
        let url = format!("{}/api/v1/auth/sign_out", api_base().trim_end_matches('/'));
        // A failed server call must not block local sign-out.
        let _ = reqwest::Client::new()
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await;
    }
    if credentials::clear() {
        "Signed out of siGit Code Cloud.".to_string()
    } else {
        "Not signed in.".to_string()
    }
}

/// One-line description of the current session, verifying the token if reachable.
pub async fn status_line() -> String {
    let Some(creds) = credentials::load() else {
        return "Not signed in. Use `/login <email> <password>` to use siGit Code Cloud."
            .to_string();
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
            format!("Signed in to siGit Code Cloud as {email}.")
        }
        Ok(response) => format!(
            "Session may be expired (HTTP {}). Use `/login` again.",
            response.status().as_u16()
        ),
        Err(_) => {
            let email = creds.email.unwrap_or_else(|| "(unknown)".to_string());
            format!("Signed in as {email} (could not reach siGit Code Cloud to verify).")
        }
    }
}

/// Split a `/login` argument into `(email, password)`. The password is the rest
/// of the line after the first whitespace, so it may contain spaces.
pub fn parse_login_args(arg: &str) -> Option<(String, String)> {
    let mut parts = arg.trim().splitn(2, char::is_whitespace);
    let email = parts.next().unwrap_or("").trim();
    let password = parts.next().unwrap_or("").trim();
    if email.is_empty() || password.is_empty() {
        None
    } else {
        Some((email.to_string(), password.to_string()))
    }
}
