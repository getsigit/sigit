//! The checks the launch splash waits on. Each runs on its own task and
//! reports as soon as it has an answer, so a slow network never holds up the
//! model scan and the splash can show them landing one by one.
//!
//! None of this blocks the first frame: the scan of the model caches and the
//! `git` call used to run inside `App::new`, before anything was drawn.

use std::time::Duration;

use tokio::sync::mpsc;

use crate::account::{self, SessionState};
use crate::models::{ModelCacheHealth, ModelPickerItem, build_model_picker_items};

/// How long the account check may spend on the network.
const ACCOUNT_TIMEOUT: Duration = Duration::from_millis(2500);
/// How long `git remote get-url` may take before the workspace goes unnamed.
const GIT_TIMEOUT: Duration = Duration::from_millis(1500);

/// One check's answer.
pub(crate) enum Report {
    Account(SessionState),
    Models {
        items: Vec<ModelPickerItem>,
        /// The saved on-device selection, if it resolves to a known model.
        selected: Option<String>,
    },
    /// `owner/name` of the repository on the sigit.si host, when there is one.
    Workspace(Option<String>),
    /// MCP discovery finished. `None` if it left no state behind.
    Tools(Option<crate::mcp::Overview>),
}

/// Start every check and return the channel they report on.
pub(crate) fn spawn() -> mpsc::UnboundedReceiver<Report> {
    let (tx, rx) = mpsc::unbounded_channel();

    let account_tx = tx.clone();
    tokio::spawn(async move {
        let state = account::check_session(ACCOUNT_TIMEOUT).await;
        let _ = account_tx.send(Report::Account(state));
    });

    let models_tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let items = build_model_picker_items();
        let selected = crate::setup::load_selected_model_name();
        let _ = models_tx.send(Report::Models { items, selected });
    });

    // MCP discovery used to run before the terminal was set up, so a slow
    // server meant a blank screen for up to its handshake timeout. It runs here
    // instead, and a turn started before it finishes simply lacks those tools.
    let tools_tx = tx.clone();
    tokio::spawn(async move {
        crate::mcp::init().await;
        let _ = tools_tx.send(Report::Tools(crate::mcp::overview()));
    });

    tokio::spawn(async move {
        let _ = tx.send(Report::Workspace(detect_repo().await));
    });

    rx
}

/// `owner/name` when the current directory's `origin` remote lives on the
/// sigit.si host (per `SIGIT_API_URL`). Any failure (no git, no repo, no
/// origin, a hung `git`) is `None`.
async fn detect_repo() -> Option<String> {
    let output = tokio::time::timeout(
        GIT_TIMEOUT,
        tokio::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout);
    crate::chat::parse_repo_remote(url.trim(), &crate::chat::sigit_host())
}

/// What one line of the launch checklist says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Pending,
    /// Fine, and worth a few words.
    Ok(String),
    /// Nothing wrong, nothing to report either (not signed in, no models yet).
    Note(String),
    /// Needs the user's attention.
    Warn(String),
}

impl Check {
    pub(crate) fn is_done(&self) -> bool {
        !matches!(self, Check::Pending)
    }
}

/// How many lines the checklist has.
pub(crate) const CHECK_ROWS: usize = 4;

/// The lines under the wordmark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Checks {
    pub account: Check,
    pub models: Check,
    pub workspace: Check,
    pub tools: Check,
}

impl Checks {
    pub(crate) fn new() -> Self {
        Self {
            account: Check::Pending,
            models: Check::Pending,
            workspace: Check::Pending,
            tools: Check::Pending,
        }
    }

    /// Every check has reported.
    pub(crate) fn all_done(&self) -> bool {
        self.account.is_done()
            && self.models.is_done()
            && self.workspace.is_done()
            && self.tools.is_done()
    }

    pub(crate) fn rows(&self) -> [(&'static str, &Check); CHECK_ROWS] {
        [
            ("Account", &self.account),
            ("Models", &self.models),
            ("Workspace", &self.workspace),
            ("Tools", &self.tools),
        ]
    }
}

pub(crate) fn account_check(state: &SessionState) -> Check {
    match state {
        SessionState::SignedOut => Check::Note("not signed in".into()),
        SessionState::Valid { email } => Check::Ok(email.clone()),
        SessionState::Offline { email } => Check::Ok(format!("{email} (offline)")),
        SessionState::Expired { .. } => Check::Warn("session expired, run /login".into()),
    }
}

pub(crate) fn models_check(items: &[ModelPickerItem]) -> Check {
    let ready = items
        .iter()
        .filter(|item| item.cloud_tier.is_none() && item.cache_health == ModelCacheHealth::Complete)
        .count();
    match ready {
        0 => Check::Note("none on device, /models to get one".into()),
        1 => Check::Ok("1 on device".into()),
        n => Check::Ok(format!("{n} on device")),
    }
}

pub(crate) fn tools_check(overview: Option<&crate::mcp::Overview>) -> Check {
    match overview {
        None => Check::Note("not available".into()),
        Some(o) if o.servers == 0 => Check::Note("no MCP servers configured".into()),
        Some(o) if o.unavailable == 0 => {
            Check::Ok(format!("{} from {} MCP server(s)", o.tools, o.servers))
        }
        Some(o) => Check::Warn(format!(
            "{} MCP server(s) unavailable, see /mcp ({} tools)",
            o.unavailable, o.tools
        )),
    }
}

pub(crate) fn workspace_check(repo: Option<&str>) -> Check {
    match repo {
        Some(repo) => Check::Ok(repo.to_string()),
        None => Check::Note(
            std::env::current_dir()
                .ok()
                .and_then(|dir| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "no repository".into()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_are_done_only_when_every_one_has_reported() {
        let mut checks = Checks::new();
        assert!(!checks.all_done());
        checks.account = Check::Note("not signed in".into());
        checks.models = Check::Ok("2 on device".into());
        assert!(!checks.all_done());
        checks.workspace = workspace_check(Some("getsigit/sigit"));
        assert!(!checks.all_done());
        checks.tools = tools_check(None);
        assert!(checks.all_done());
    }

    #[test]
    fn account_states_map_to_lines() {
        assert_eq!(
            account_check(&SessionState::Valid {
                email: "a@b.c".into()
            }),
            Check::Ok("a@b.c".into())
        );
        assert_eq!(
            account_check(&SessionState::Offline {
                email: "a@b.c".into()
            }),
            Check::Ok("a@b.c (offline)".into())
        );
        assert!(matches!(
            account_check(&SessionState::Expired { status: 401 }),
            Check::Warn(_)
        ));
        assert!(matches!(
            account_check(&SessionState::SignedOut),
            Check::Note(_)
        ));
    }

    #[test]
    fn no_models_is_a_note_not_a_warning() {
        assert!(matches!(models_check(&[]), Check::Note(_)));
    }
}
