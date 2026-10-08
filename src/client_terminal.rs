//! Running `run_command` in the ACP client's terminal instead of a pipe.
//!
//! An editor that advertises `terminal` in its `initialize` request can run a
//! command itself (`terminal/create`) and show it live in the tool call when
//! the agent embeds the terminal as `terminal` content. The user watches the
//! output as it arrives and can stop the command from the editor, where a
//! command sigit runs itself only shows its output once it has finished.
//!
//! What `run_command` promises the model stays the same on this path: the
//! command runs through the platform shell in the requested directory, it is
//! killed (`terminal/kill`) once [`COMMAND_TIMEOUT`](crate::tools) passes, the
//! output is capped, and a commit it creates still gets the co-author trailer,
//! which `tools.rs` checks on the disk before and after the run. Its stdin is
//! not sigit's: the editor owns the terminal, so the JSON-RPC pipe that
//! `spawn_shell` has to keep away from a child is not in reach here.
//!
//! The seam has the same shape as `client_fs`: `main.rs` registers a
//! [`ClientTerminal`] from `initialize`, and `tools.rs` asks [`route_for`]
//! whether a command should go through it. Only foreground commands are
//! routed. Background ones stay local because `command_output` and
//! `kill_command` read their pipes, and a command whose directory is outside
//! the session's roots stays local because nothing says the editor's machine
//! can see it. A client that cannot create the terminal costs a log line and
//! the command runs locally; once the terminal exists, a failure is the tool
//! call's, since running the command a second time could repeat its effects.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;

/// How long the client gets to answer a request that is not the wait for the
/// command itself. Creating, reading, killing and releasing a terminal is
/// local work for an editor.
const CLIENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How a command in the client's terminal ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitStatus {
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
}

/// What `terminal/output` returned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub output: String,
    /// The client dropped the start of the output to stay under the limit.
    pub truncated: bool,
}

/// The client's side of ACP's terminal methods. Errors are plain strings that
/// end up in the log or the tool result.
#[async_trait]
pub trait ClientTerminal: Send + Sync {
    /// `terminal/create`: start `command args…` in `cwd`, returning the
    /// terminal id.
    async fn create(
        &self,
        session_id: &str,
        command: &str,
        args: &[String],
        cwd: &Path,
        output_byte_limit: u64,
    ) -> Result<String, String>;
    /// Show the terminal in the tool call `tool_call_id` (a `tool_call_update`
    /// carrying `terminal` content). A notification, so it cannot fail in a
    /// way worth reporting.
    async fn embed(&self, session_id: &str, tool_call_id: &str, terminal_id: &str);
    async fn wait_for_exit(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<ExitStatus, String>;
    async fn output(&self, session_id: &str, terminal_id: &str) -> Result<Output, String>;
    async fn kill(&self, session_id: &str, terminal_id: &str) -> Result<(), String>;
    async fn release(&self, session_id: &str, terminal_id: &str) -> Result<(), String>;
}

/// The connected client's terminal. Capabilities are fixed for the connection,
/// and the process serves one connection, so this is set once from
/// `initialize`.
static CLIENT: RwLock<Option<Arc<dyn ClientTerminal>>> = RwLock::new(None);

/// Record the client's terminal when it advertised one, and forget any
/// earlier one when it did not.
pub fn register(terminal: Arc<dyn ClientTerminal>, advertised: bool) {
    if let Ok(mut guard) = CLIENT.write() {
        *guard = advertised.then_some(terminal);
    }
}

/// `SIGIT_CLIENT_TERMINAL=off` keeps `run_command` local even when the client
/// offers a terminal, for a client whose terminal misbehaves in a way the
/// fallback cannot see.
pub fn disabled_by_env() -> bool {
    std::env::var("SIGIT_CLIENT_TERMINAL")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false"
            )
        })
        .unwrap_or(false)
}

/// A command's way through the client, for the session that asked.
pub struct Route {
    terminal: Arc<dyn ClientTerminal>,
    session_id: String,
}

/// The client route for a command run in `cwd`, or `None` when it should run
/// locally: no client terminal is registered, no session is live, or `cwd` is
/// outside the session's roots.
pub fn route_for(session_id: Option<&str>, cwd: &Path) -> Option<Route> {
    let terminal = CLIENT.read().ok()?.clone()?;
    let session_id = session_id?;
    if !cwd.is_absolute() || !crate::client_fs::in_roots(cwd, &crate::workspace::project_dirs()) {
        return None;
    }
    Some(Route {
        terminal,
        session_id: session_id.to_string(),
    })
}

/// How a run through the client went.
#[derive(Debug, PartialEq, Eq)]
pub enum Run {
    /// The client could not start the terminal; nothing ran, so the caller
    /// may run the command itself.
    NotStarted(String),
    /// The command ran and exited (on its own, or because the user stopped it
    /// from the editor).
    Exited {
        terminal_id: String,
        status: ExitStatus,
        output: Output,
    },
    /// The command was still running at the timeout and was killed.
    TimedOut { terminal_id: String, output: Output },
    /// The terminal started but the client failed a later request. The
    /// command may have run, so it must not be run again.
    Failed { terminal_id: String, error: String },
}

impl Route {
    /// Run `command_str` through the platform shell in `cwd`, embedding the
    /// terminal in `tool_call_id` when there is one, and wait for it for at
    /// most `timeout`. The terminal is released before this returns; a client
    /// keeps showing a released terminal that was embedded in a tool call.
    pub async fn run(
        &self,
        command_str: &str,
        cwd: &Path,
        tool_call_id: Option<&str>,
        timeout: Duration,
        output_byte_limit: u64,
    ) -> Run {
        let session = self.session_id.as_str();
        let (shell, args) = shell_invocation(command_str);
        let created = tokio::time::timeout(
            CLIENT_REQUEST_TIMEOUT,
            self.terminal
                .create(session, shell, &args, cwd, output_byte_limit),
        )
        .await;
        let terminal_id = match created {
            Ok(Ok(id)) => id,
            Ok(Err(error)) => return Run::NotStarted(error),
            Err(_) => return Run::NotStarted("terminal/create timed out".to_string()),
        };
        let id = terminal_id.as_str();

        if let Some(tool_call_id) = tool_call_id {
            self.terminal.embed(session, tool_call_id, id).await;
        }

        let waited = tokio::time::timeout(timeout, self.terminal.wait_for_exit(session, id)).await;
        let run = match waited {
            Ok(Ok(status)) => match self.read_output(id).await {
                Ok(output) => Run::Exited {
                    terminal_id: terminal_id.clone(),
                    status,
                    output,
                },
                Err(error) => Run::Failed {
                    terminal_id: terminal_id.clone(),
                    error,
                },
            },
            Ok(Err(error)) => Run::Failed {
                terminal_id: terminal_id.clone(),
                error: format!("terminal/wait_for_exit failed: {error}"),
            },
            Err(_) => {
                // Kill before reading, so the output is what the command
                // printed up to the timeout and nothing more.
                if let Err(error) = self
                    .ask("terminal/kill", self.terminal.kill(session, id))
                    .await
                {
                    log::warn!("could not kill terminal {id}: {error}");
                }
                Run::TimedOut {
                    terminal_id: terminal_id.clone(),
                    output: self.read_output(id).await.unwrap_or_default(),
                }
            }
        };

        if let Err(error) = self
            .ask("terminal/release", self.terminal.release(session, id))
            .await
        {
            log::warn!("could not release terminal {id}: {error}");
        }
        run
    }

    async fn read_output(&self, terminal_id: &str) -> Result<Output, String> {
        self.ask(
            "terminal/output",
            self.terminal.output(&self.session_id, terminal_id),
        )
        .await
    }

    /// One client request, bounded by [`CLIENT_REQUEST_TIMEOUT`].
    async fn ask<T>(
        &self,
        method: &str,
        request: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        match tokio::time::timeout(CLIENT_REQUEST_TIMEOUT, request).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(format!("{method} failed: {error}")),
            Err(_) => Err(format!("{method} timed out")),
        }
    }
}

/// The shell and arguments that run `command_str`, the same shell
/// `spawn_shell` uses locally. ACP passes `command` and `args` separately, so
/// the whole command line travels as one argument and the client has nothing
/// to split or quote.
fn shell_invocation(command_str: &str) -> (&'static str, Vec<String>) {
    #[cfg(unix)]
    let (shell, flag) = ("sh", "-c");
    #[cfg(windows)]
    let (shell, flag) = ("cmd", "/C");
    (shell, vec![flag.to_string(), command_str.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// What a `terminal/create` asked for: command, args, cwd, byte limit.
    type Created = (String, Vec<String>, PathBuf, u64);

    /// A client terminal that records what it was asked and answers from a
    /// script.
    #[derive(Default)]
    struct FakeTerminal {
        refuse_create: bool,
        /// `None` never answers the wait, which is how a hung command looks.
        exit: Option<ExitStatus>,
        output: &'static str,
        calls: Mutex<Vec<String>>,
        created: Mutex<Option<Created>>,
    }

    impl FakeTerminal {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }
    }

    #[async_trait]
    impl ClientTerminal for FakeTerminal {
        async fn create(
            &self,
            session_id: &str,
            command: &str,
            args: &[String],
            cwd: &Path,
            output_byte_limit: u64,
        ) -> Result<String, String> {
            self.record(format!("create {session_id}"));
            if self.refuse_create {
                return Err("terminals are disabled".to_string());
            }
            *self.created.lock().unwrap() = Some((
                command.to_string(),
                args.to_vec(),
                cwd.to_path_buf(),
                output_byte_limit,
            ));
            Ok("term-1".to_string())
        }

        async fn embed(&self, _session_id: &str, tool_call_id: &str, terminal_id: &str) {
            self.record(format!("embed {terminal_id} in {tool_call_id}"));
        }

        async fn wait_for_exit(
            &self,
            _session_id: &str,
            terminal_id: &str,
        ) -> Result<ExitStatus, String> {
            self.record(format!("wait {terminal_id}"));
            match &self.exit {
                Some(status) => Ok(status.clone()),
                None => std::future::pending().await,
            }
        }

        async fn output(&self, _session_id: &str, terminal_id: &str) -> Result<Output, String> {
            self.record(format!("output {terminal_id}"));
            Ok(Output {
                output: self.output.to_string(),
                truncated: false,
            })
        }

        async fn kill(&self, _session_id: &str, terminal_id: &str) -> Result<(), String> {
            self.record(format!("kill {terminal_id}"));
            Ok(())
        }

        async fn release(&self, _session_id: &str, terminal_id: &str) -> Result<(), String> {
            self.record(format!("release {terminal_id}"));
            Ok(())
        }
    }

    fn route(terminal: &Arc<FakeTerminal>) -> Route {
        Route {
            terminal: Arc::clone(terminal) as Arc<dyn ClientTerminal>,
            session_id: "session-1".to_string(),
        }
    }

    #[tokio::test]
    async fn a_command_runs_in_an_embedded_terminal_that_is_released() {
        let terminal = Arc::new(FakeTerminal {
            exit: Some(ExitStatus {
                exit_code: Some(0),
                signal: None,
            }),
            output: "hello\n",
            ..FakeTerminal::default()
        });
        let cwd = std::env::temp_dir();

        let run = route(&terminal)
            .run(
                "echo hello",
                &cwd,
                Some("call_1"),
                Duration::from_secs(5),
                1000,
            )
            .await;

        assert_eq!(
            run,
            Run::Exited {
                terminal_id: "term-1".to_string(),
                status: ExitStatus {
                    exit_code: Some(0),
                    signal: None
                },
                output: Output {
                    output: "hello\n".to_string(),
                    truncated: false
                },
            }
        );
        assert_eq!(
            terminal.calls(),
            [
                "create session-1",
                "embed term-1 in call_1",
                "wait term-1",
                "output term-1",
                "release term-1",
            ]
        );
        let (command, args, sent_cwd, limit) = terminal.created.lock().unwrap().clone().unwrap();
        let (shell, expected_args) = shell_invocation("echo hello");
        assert_eq!(command, shell);
        assert_eq!(args, expected_args);
        assert_eq!(args.last().map(String::as_str), Some("echo hello"));
        assert_eq!(sent_cwd, cwd);
        assert_eq!(limit, 1000);
    }

    #[tokio::test]
    async fn a_command_past_the_timeout_is_killed_then_read() {
        let terminal = Arc::new(FakeTerminal {
            exit: None,
            output: "partial\n",
            ..FakeTerminal::default()
        });

        let run = route(&terminal)
            .run(
                "sleep 999",
                &std::env::temp_dir(),
                None,
                Duration::from_millis(50),
                1000,
            )
            .await;

        assert_eq!(
            run,
            Run::TimedOut {
                terminal_id: "term-1".to_string(),
                output: Output {
                    output: "partial\n".to_string(),
                    truncated: false
                },
            }
        );
        // No tool call to embed in, and the kill comes before the read.
        assert_eq!(
            terminal.calls(),
            [
                "create session-1",
                "wait term-1",
                "kill term-1",
                "output term-1",
                "release term-1",
            ]
        );
    }

    #[tokio::test]
    async fn a_terminal_the_client_cannot_create_runs_nothing() {
        let terminal = Arc::new(FakeTerminal {
            refuse_create: true,
            ..FakeTerminal::default()
        });

        let run = route(&terminal)
            .run(
                "true",
                &std::env::temp_dir(),
                Some("call_1"),
                Duration::from_secs(5),
                1000,
            )
            .await;

        assert_eq!(run, Run::NotStarted("terminals are disabled".to_string()));
        assert_eq!(terminal.calls(), ["create session-1"]);
    }
}
