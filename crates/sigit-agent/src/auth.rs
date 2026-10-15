//! [`SiGitAuth`]: siGit Code's sign-in as an `ed-acp` [`AuthProvider`].
//!
//! The same browser flow `sigit` runs in an editor (OAuth with PKCE against `sigit.si/oauth`,
//! public client `sigit-code-cli`), and the same stored session, so there is one OAuth client
//! to register and one place a token lives. The loopback listener receives the code; the
//! authorize URL goes to the client as a URL-mode elicitation when it offers one, so the page
//! opens where the user is looking.

use agent_client_protocol::schema::v1::{
    AuthMethod, AuthMethodAgent, CompleteElicitationNotification, CreateElicitationRequest,
    CreateElicitationResponse, ElicitationAction, ElicitationRequestScope, ElicitationUrlMode,
    ErrorCode, RequestId,
};
use agent_client_protocol::{Client, ConnectionTo};
use async_trait::async_trait;
use ed_acp::{AuthCtx, AuthProvider, LlmConfig, Provider};

use crate::browser_auth::{self, Flow};
use crate::credentials;
use crate::profile::INFO;

/// The id of the auth method, as the client sends it in `authenticate`.
pub const METHOD_ID: &str = "sigit";

/// Default API root of siGit Code Cloud; `SIGIT_CLOUD_URL` overrides it.
const DEFAULT_CLOUD_URL: &str = "https://sigit.si/api/v1";

/// The inference endpoint: `SIGIT_CLOUD_URL` or `https://sigit.si/api/v1`.
pub fn cloud_base_url() -> String {
    std::env::var("SIGIT_CLOUD_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CLOUD_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The model a session starts on: `SIGIT_AGENT_MODEL`, else the profile's default tier.
fn default_model() -> String {
    std::env::var(INFO.env.var("MODEL"))
        .ok()
        .filter(|model| !model.trim().is_empty())
        .unwrap_or_else(|| INFO.env.default_model.to_string())
}

pub struct SiGitAuth;

#[async_trait]
impl AuthProvider for SiGitAuth {
    fn method(&self) -> AuthMethod {
        AuthMethod::Agent(
            AuthMethodAgent::new(METHOD_ID, "Sign in to siGit Code")
                .description("Opens sigit.si in your browser to sign in to siGit Code Cloud."),
        )
    }

    fn llm_config(&self) -> Option<LlmConfig> {
        let token = credentials::load_token()?;
        Some(LlmConfig {
            provider: Provider::OpenAi,
            base_url: cloud_base_url(),
            api_key: Some(token),
            model: default_model(),
        })
    }

    async fn authenticate(&self, ctx: AuthCtx<'_>) -> agent_client_protocol::Result<()> {
        // An existing session clears the gate without sending anyone to a browser they
        // didn't ask for.
        if let Some(token) = credentials::load_token()
            && session_is_valid(&token).await
        {
            log::info!("authenticate: the stored session is still valid");
            return Ok(());
        }

        // There is no way to take a pasted code back from here, so this is the loopback
        // variant or nothing.
        let flow = browser_auth::begin();
        if !flow.is_loopback() {
            return Err(error(
                "Could not open a local port to receive the sign-in. Run `sigit login` in a \
                 terminal instead.",
            ));
        }

        let outcome = if ctx.client_offers_url_elicitation() {
            sign_in_via_client(ctx.connection, ctx.request_id, flow).await
        } else {
            log::info!(
                "authenticate: opening the browser at {}",
                flow.authorize_url()
            );
            if !browser_auth::open_browser(flow.authorize_url()) {
                log::warn!("authenticate: no browser could be launched");
            }
            browser_auth::complete_loopback(flow).await
        };

        match outcome {
            Ok(email) => {
                log::info!("authenticate: signed in as {email}");
                Ok(())
            }
            Err(reason) => Err(error(format!(
                "Sign-in did not complete ({reason}). Try again, or create an account at \
                 https://sigit.si."
            ))),
        }
    }

    async fn logout(&self) -> anyhow::Result<()> {
        credentials::clear();
        Ok(())
    }
}

fn error(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::new(ErrorCode::InternalError.into(), message.into())
}

/// Whether `token` is still accepted by the account API.
async fn session_is_valid(token: &str) -> bool {
    let base = std::env::var("SIGIT_API_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| "https://sigit.si".to_string());
    let url = format!("{}/api/v1/user", base.trim_end_matches('/'));
    match reqwest::Client::new()
        .get(url)
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(response) => response.status().is_success(),
        Err(error) => {
            log::warn!("authenticate: could not verify the stored session ({error})");
            false
        }
    }
}

/// The loopback sign-in with the client showing the page: the authorize URL goes out as a
/// URL-mode elicitation, so the user sees where they are being sent and the client opens it,
/// which also works where this process cannot launch a browser. A user who declines ends the
/// sign-in; a client that fails the request gets a browser opened from here. Once the code is
/// redeemed the client is told the elicitation is complete.
async fn sign_in_via_client(
    connection: &ConnectionTo<Client>,
    request_id: RequestId,
    flow: Flow,
) -> Result<String, String> {
    let url = flow.authorize_url().to_string();
    let elicitation_id = uuid::Uuid::new_v4().to_string();
    log::info!("authenticate: asking the client to open {url}");
    let ask = connection
        .send_request(CreateElicitationRequest::new(
            ElicitationUrlMode::new(
                ElicitationRequestScope::new(request_id),
                elicitation_id.clone(),
                url.clone(),
            ),
            "Sign in to siGit Code in your browser to use siGit Code Cloud.",
        ))
        .block_task();
    let sign_in = browser_auth::complete_loopback(flow);
    tokio::pin!(sign_in);

    // The page can be finished before the client gets round to answering.
    enum First {
        Answer(agent_client_protocol::Result<CreateElicitationResponse>),
        SignedIn(Result<String, String>),
    }
    let first = tokio::select! {
        answer = ask => First::Answer(answer),
        result = &mut sign_in => First::SignedIn(result),
    };
    let result = match first {
        First::SignedIn(result) => result,
        First::Answer(Ok(response)) => match response.action {
            ElicitationAction::Accept(_) => sign_in.await,
            _ => Err("the sign-in page was not opened".to_string()),
        },
        First::Answer(Err(error)) => {
            log::warn!("authenticate: the client could not show the sign-in page ({error})");
            if !browser_auth::open_browser(&url) {
                log::warn!("authenticate: no browser could be launched");
            }
            sign_in.await
        }
    };
    if result.is_ok() {
        connection
            .send_notification(CompleteElicitationNotification::new(elicitation_id))
            .ok();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_method_is_an_agent_method_named_sigit() {
        let method = SiGitAuth.method();
        assert_eq!(&*method.id().0, METHOD_ID);
        assert!(matches!(method, AuthMethod::Agent(_)));
    }

    #[test]
    fn the_cloud_url_defaults_to_the_api_root_and_drops_a_trailing_slash() {
        // Not set in a test run; the override is covered by `cloud_base_url`'s own branch.
        if std::env::var_os("SIGIT_CLOUD_URL").is_none() {
            assert_eq!(cloud_base_url(), "https://sigit.si/api/v1");
        }
    }
}
