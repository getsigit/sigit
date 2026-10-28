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
struct UserResponse {
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

/// Run an interactive sign-in against the terminal, prompting for email and a
/// hidden password. Used by `sigit login`, which the editor launches in an
/// embedded terminal for ACP terminal-based authentication. Returns the signed-in
/// email or a human-readable error.
pub async fn interactive_login() -> Result<String, String> {
    use std::io::{Write, stdin, stdout};

    print!("siGit Code email: ");
    stdout().flush().ok();
    let mut email = String::new();
    stdin()
        .read_line(&mut email)
        .map_err(|error| format!("could not read email: {error}"))?;

    let password = rpassword::prompt_password("siGit Code password: ")
        .map_err(|error| format!("could not read password: {error}"))?;

    authenticate(email.trim(), &password).await
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

/// What the stored session amounts to right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// No credentials are stored.
    SignedOut,
    /// The server accepted the token.
    Valid { email: String },
    /// The server rejected the token (HTTP status).
    Expired { status: u16 },
    /// Credentials are stored but the server could not be reached in time.
    Offline { email: String },
}

/// Look up the stored session and verify its token, giving up on the network
/// after `timeout`.
pub async fn check_session(timeout: std::time::Duration) -> SessionState {
    let Some(creds) = credentials::load() else {
        return SessionState::SignedOut;
    };

    let url = format!("{}/api/v1/user", api_base().trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_default();
    match client
        .get(&url)
        .bearer_auth(&creds.access_token)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            let email = response
                .json::<UserResponse>()
                .await
                .ok()
                .and_then(|me| me.email)
                .or(creds.email)
                .unwrap_or_else(|| "(unknown)".to_string());
            SessionState::Valid { email }
        }
        Ok(response) => SessionState::Expired {
            status: response.status().as_u16(),
        },
        Err(_) => SessionState::Offline {
            email: creds.email.unwrap_or_else(|| "(unknown)".to_string()),
        },
    }
}

/// One-line description of the current session, verifying the token if reachable.
pub async fn status_line() -> String {
    match check_session(std::time::Duration::from_secs(10)).await {
        SessionState::SignedOut => {
            "Not signed in. Use `/login` to use siGit Code Cloud.".to_string()
        }
        SessionState::Valid { email } => format!("Signed in to siGit Code Cloud as {email}."),
        SessionState::Expired { status } => {
            format!("Session may be expired (HTTP {status}). Use `/login` again.")
        }
        SessionState::Offline { email } => {
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
