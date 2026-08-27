//! Browser sign-in for siGit Code Cloud.
//!
//! The alternative to typing a password into a chat box. sigit is a public
//! OAuth client: it opens the browser at sigit.si, the person signs in and
//! approves there, and the authorization code comes back here. PKCE (RFC 7636)
//! is what proves the code belongs to this process, since a shipped CLI can
//! hold no client secret.
//!
//! The code comes home one of two ways:
//!
//! - **Loopback** (RFC 8252 §7.3). We bind `127.0.0.1:0`, let the OS pick the
//!   port, and hand that URL to the server as the redirect URI. The server
//!   registration lists `http://127.0.0.1/callback` with no port, which the
//!   provider matches port-agnostically, so no per-run registration is needed.
//!   Used by the editor's "Sign in" button, where there is nowhere to type.
//! - **Paste** (the out-of-band URN). No listener, no open port. The browser
//!   lands on a page showing the code and the person carries it back by hand.
//!   This is the path that survives ssh, a remote editor, or a locked-down box
//!   where binding a port fails.
//!
//! The token that comes out is stored by [`crate::credentials`] exactly like
//! the one `/login` produces, so everything downstream is unchanged.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::credentials::{self, Credentials};

/// Client id of the siGit Code registration on sigit.si. Public clients have
/// no secret and their id is not one either, so it ships as a constant rather
/// than being configured per install. Pinned server-side by
/// `rake oauth:register_cli`.
const OAUTH_CLIENT_ID: &str = "sigit-code-cli";

/// `user:read` for whoami, `code:agent` for inference and the MCP tools.
const OAUTH_SCOPE: &str = "user:read code:agent";

/// Non-standard redirect URI that asks the server to display the code instead
/// of redirecting anywhere.
const OOB_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

/// How long to hold the loopback listener open waiting for the browser. The
/// authorization code itself expires server-side in 10 minutes; this is the
/// shorter local patience for someone who walked away.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Default account host. Override with `SIGIT_API_URL` (dev: `http://localhost:8088`).
const DEFAULT_API_URL: &str = "https://sigit.si";

fn api_base() -> String {
    std::env::var("SIGIT_API_URL")
        .unwrap_or_else(|_| DEFAULT_API_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// An authorization in flight: the URL to send the browser to, and the secrets
/// needed to redeem whatever code comes back.
pub struct Flow {
    authorize_url: String,
    redirect_uri: String,
    verifier: String,
    state: String,
    /// Present only for the loopback variant.
    listener: Option<TcpListener>,
}

impl Flow {
    /// The URL the person needs to visit. Worth showing even when we opened it
    /// ourselves: the browser we launch is not always the one they're signed
    /// in to.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// True when a code will arrive on its own, false when it has to be pasted.
    pub fn is_loopback(&self) -> bool {
        self.listener.is_some()
    }
}

/// Start a loopback authorization, falling back to the paste variant when no
/// local port can be bound.
pub fn begin() -> Flow {
    match TcpListener::bind(("127.0.0.1", 0)).and_then(|listener| {
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(false)?;
        Ok((listener, port))
    }) {
        Ok((listener, port)) => {
            let redirect_uri = format!("http://127.0.0.1:{port}/callback");
            build_flow(redirect_uri, Some(listener))
        }
        Err(error) => {
            log::warn!("no loopback port for browser sign-in ({error}); using the paste flow");
            build_flow(OOB_REDIRECT_URI.to_string(), None)
        }
    }
}

/// Start a paste authorization outright, without touching the network stack.
pub fn begin_paste() -> Flow {
    build_flow(OOB_REDIRECT_URI.to_string(), None)
}

fn build_flow(redirect_uri: String, listener: Option<TcpListener>) -> Flow {
    let verifier = random_token();
    let state = random_token();
    let challenge = code_challenge(&verifier);

    let authorize_url = format!(
        "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        api_base(),
        percent_encode(OAUTH_CLIENT_ID),
        percent_encode(&redirect_uri),
        percent_encode(OAUTH_SCOPE),
        percent_encode(&state),
        percent_encode(&challenge),
    );

    Flow {
        authorize_url,
        redirect_uri,
        verifier,
        state,
        listener,
    }
}

/// Hand the authorize URL to the platform's browser. Best-effort: a headless
/// box has nothing to open, which is not a reason to abandon the flow, since
/// the URL can still be carried to a browser elsewhere.
pub fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        // `start` is a cmd builtin, and its first quoted argument is taken as
        // the window title, hence the empty one.
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };

    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Block on the loopback listener until the browser redirects back, then
/// redeem the code and store the session. Returns the signed-in email.
///
/// Runs the socket work on a blocking thread: the wait is minutes long and
/// must not sit on an async worker.
pub async fn complete_loopback(flow: Flow) -> Result<String, String> {
    let Flow {
        redirect_uri,
        verifier,
        state,
        listener,
        ..
    } = flow;
    let listener = listener.ok_or_else(|| "this sign-in has no local listener".to_string())?;

    let expected_state = state.clone();
    let code = tokio::task::spawn_blocking(move || wait_for_code(listener, &expected_state))
        .await
        .map_err(|error| format!("sign-in was interrupted: {error}"))??;

    exchange(&code, &verifier, &redirect_uri).await
}

/// Redeem a code the person pasted in. Returns the signed-in email.
pub async fn complete_paste(flow: &Flow, code: &str) -> Result<String, String> {
    let code = code.trim();
    if code.is_empty() {
        return Err("no code was entered".to_string());
    }
    exchange(code, &flow.verifier, &flow.redirect_uri).await
}

/// Accept connections until one carries our callback. Anything else on the
/// port (a browser probe, a stray scan) is answered and ignored rather than
/// ending the wait.
fn wait_for_code(listener: TcpListener, expected_state: &str) -> Result<String, String> {
    let deadline = std::time::Instant::now() + CALLBACK_TIMEOUT;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("could not watch for the browser: {error}"))?;

    loop {
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for the browser".to_string());
        }

        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => return Err(format!("could not accept the browser: {error}")),
        };
        stream.set_nonblocking(false).ok();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

        // The request line is all we need, and it arrives in the first packet.
        let mut buffer = [0_u8; 4096];
        let read = stream.read(&mut buffer).unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]);
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
        else {
            respond(&mut stream, "Bad request.");
            continue;
        };

        let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (key, value) in parse_query(query) {
            match key.as_str() {
                "code" => code = Some(value),
                "state" => state = Some(value),
                "error" => error = Some(value),
                _ => {}
            }
        }

        if let Some(error) = error {
            respond(
                &mut stream,
                "Sign-in was cancelled. You can close this tab.",
            );
            return Err(format!("sign-in was declined ({error})"));
        }

        let Some(code) = code else {
            respond(&mut stream, "Nothing to do here.");
            continue;
        };

        // The state proves this callback belongs to the flow we started, not
        // to a link someone else sent to this browser.
        if state.as_deref() != Some(expected_state) {
            respond(&mut stream, "That sign-in did not come from siGit Code.");
            return Err("the sign-in response did not match this request".to_string());
        }

        respond(
            &mut stream,
            "Signed in to siGit Code. You can close this tab.",
        );
        return Ok(code);
    }
}

fn respond(stream: &mut std::net::TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>siGit Code</title>\
         <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\">\
         <p>{message}</p></body>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Trade the authorization code for an access token and store it.
async fn exchange(code: &str, verifier: &str, redirect_uri: &str) -> Result<String, String> {
    let url = format!("{}/oauth/token", api_base());
    let response = reqwest::Client::new()
        .post(&url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", OAUTH_CLIENT_ID),
            ("redirect_uri", redirect_uri),
            ("code", code),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|error| format!("could not reach siGit Code Cloud: {error}"))?;

    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("unexpected response from siGit Code Cloud: {error}"))?;

    if !status.is_success() {
        // OAuth error bodies are `{"error":…,"error_description":…}`.
        let detail = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(|value| value.as_str())
            .unwrap_or("the code could not be exchanged");
        return Err(format!("sign-in failed: {detail}"));
    }

    let token = body
        .get("access_token")
        .and_then(|value| value.as_str())
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| "siGit Code Cloud returned no access token".to_string())?;

    // Store before the profile lookup: the token is the thing worth keeping,
    // and the email is only there to label it.
    credentials::store(&Credentials {
        access_token: token.to_string(),
        email: None,
    })?;

    let email = fetch_email(token).await;
    if email.is_some() {
        credentials::store(&Credentials {
            access_token: token.to_string(),
            email: email.clone(),
        })?;
    }

    Ok(email.unwrap_or_else(|| "(unknown)".to_string()))
}

/// Read the account email for display. OAuth tokens are served by
/// `/api/v1/user`, not the deprecated `/api/v1/me`.
async fn fetch_email(token: &str) -> Option<String> {
    let url = format!("{}/api/v1/user", api_base());
    let response = reqwest::Client::new()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response
        .json::<serde_json::Value>()
        .await
        .ok()?
        .get("email")?
        .as_str()
        .map(str::to_string)
}

/// 32 random bytes, base64url without padding: a PKCE verifier and a state
/// value both want an unguessable ASCII string of exactly this shape.
fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    rand::fill(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// S256: base64url(sha256(verifier)), unpadded.
fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Percent-encode a query-string value. Everything outside the unreserved set
/// is escaped, which is stricter than necessary and never wrong.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Split a query string into decoded key/value pairs.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_challenge_matches_rfc_7636_example() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn authorize_url_carries_pkce_and_the_bound_port() {
        let flow = begin();
        assert!(flow.is_loopback(), "a loopback port should be available");
        assert!(flow.authorize_url().contains("code_challenge_method=S256"));
        assert!(flow.authorize_url().contains("response_type=code"));
        // The redirect URI is encoded, so look for the escaped form.
        assert!(
            flow.authorize_url().contains("http%3A%2F%2F127.0.0.1%3A"),
            "authorize url should carry the loopback redirect: {}",
            flow.authorize_url()
        );
    }

    #[test]
    fn paste_flow_asks_the_server_to_display_the_code() {
        let flow = begin_paste();
        assert!(!flow.is_loopback());
        assert!(
            flow.authorize_url()
                .contains("urn%3Aietf%3Awg%3Aoauth%3A2.0%3Aoob")
        );
    }

    #[test]
    fn verifier_and_state_differ_between_flows() {
        let first = begin_paste();
        let second = begin_paste();
        assert_ne!(first.verifier, second.verifier);
        assert_ne!(first.state, second.state);
        assert_eq!(first.verifier.len(), 43, "32 bytes as unpadded base64url");
    }

    #[test]
    fn query_parsing_decodes_escapes() {
        let pairs = parse_query("code=abc%2Fdef&state=x+y");
        assert_eq!(pairs[0], ("code".to_string(), "abc/def".to_string()));
        assert_eq!(pairs[1], ("state".to_string(), "x y".to_string()));
    }
}
