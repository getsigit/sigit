//! siGit Code's [`Profile`] for `ed-acp`: who the agent is, its prompt and tools, its slash
//! commands, its sign-in, and how a host reaches the official MCP server.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, HttpHeader, McpServer, McpServerHttp,
    UnstructuredCommandInput,
};
use async_trait::async_trait;
use ed_acp::{
    AgentInfo, AuthProvider, DescribeCtx, LlmEnv, Profile, PromptCtx, SessionCtx, ToolCtx,
    ToolOutcome, Toolset, WorkspaceTools,
};
use serde_json::Value;

use crate::auth::{SiGitAuth, cloud_base_url};
use crate::credentials;
use crate::prompt::{CO_AUTHOR_TRAILER_PLACEHOLDER, INIT_PROMPT, SYSTEM_PROMPT};

pub const INFO: AgentInfo = AgentInfo {
    name: "sigit-agent",
    display_name: "siGit Code",
    version: env!("CARGO_PKG_VERSION"),
    env: LlmEnv {
        prefix: "SIGIT_AGENT",
        dir_name: "sigit-agent",
        default_model: "onde-nova",
    },
};

/// Name of the official server in a session, so its tools are `mcp__sigit__*`, which is what
/// [`SYSTEM_PROMPT`] tells the model to call.
const OFFICIAL_MCP: &str = "sigit";

/// The official siGit MCP server (issues, pull requests) as a session server for the signed-in
/// user, `None` when signed out.
///
/// This is passed per session rather than offered as a process-wide built-in server: a shared
/// server would keep the bearer it started with, and a user who signed out and in as someone
/// else would keep reaching the first account's issues. A host calls this when it opens a
/// session and passes the result in `mcpServers`.
pub fn official_mcp_servers() -> Vec<McpServer> {
    if matches!(
        std::env::var("SIGIT_MCP_OFFICIAL").as_deref(),
        Ok("off" | "0" | "false")
    ) {
        return Vec::new();
    }
    let Some(token) = credentials::load_token() else {
        return Vec::new();
    };
    let url = std::env::var("SIGIT_MCP_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| format!("{}/mcp", cloud_base_url()));
    vec![McpServer::Http(
        McpServerHttp::new(OFFICIAL_MCP, url).headers(vec![HttpHeader::new(
            "Authorization",
            format!("Bearer {token}"),
        )]),
    )]
}

/// What the model is told when the host cannot start processes.
const NO_SHELL_NOTE: &str = "\n\nThis host cannot run commands: there is no run_command tool, and no git or shell. The rule \
above about running commands yourself does not apply here. When something needs a command, \
say which one and let the user run it.";

/// siGit Code as an `ed-acp` agent.
#[derive(Debug, Clone, Copy)]
pub struct SiGitCode {
    shell: bool,
}

impl SiGitCode {
    /// With the workspace tools, `run_command` included.
    pub fn new() -> Self {
        Self { shell: true }
    }

    /// For hosts that cannot start processes (an iOS app, a sandboxed Mac app): the workspace
    /// tools without `run_command`, and a prompt that says so.
    pub fn without_shell() -> Self {
        Self { shell: false }
    }

    /// The model-facing tool list for this host, in the system prompt.
    fn host_note(&self) -> String {
        let tools = if self.shell {
            "read_file, write_file, edit_file, list_directory and run_command"
        } else {
            "read_file, write_file, edit_file and list_directory"
        };
        format!(
            "\n\nIn this host your file tools are {tools}, plus the tools of connected MCP \
             servers. Tools named above that are not in this list (such as read_website or \
             create_file) are not available; use write_file to create files."
        )
    }
}

impl Default for SiGitCode {
    fn default() -> Self {
        Self::new()
    }
}

/// `WorkspaceTools` minus `run_command`.
struct FileTools;

fn is_run_command(definition: &Value) -> bool {
    definition["function"]["name"] == "run_command"
}

#[async_trait]
impl Toolset for FileTools {
    fn definitions(&self) -> Vec<Value> {
        WorkspaceTools
            .definitions()
            .into_iter()
            .filter(|definition| !is_run_command(definition))
            .collect()
    }

    fn handles(&self, name: &str) -> bool {
        name != "run_command" && WorkspaceTools.handles(name)
    }

    fn describe(
        &self,
        name: &str,
        args: &Value,
        ctx: &DescribeCtx<'_>,
    ) -> (
        String,
        agent_client_protocol::schema::v1::ToolKind,
        Vec<agent_client_protocol::schema::v1::ToolCallLocation>,
    ) {
        WorkspaceTools.describe(name, args, ctx)
    }

    async fn execute(
        &self,
        ctx: &ToolCtx,
        tool_call_id: &str,
        name: &str,
        args: Value,
    ) -> anyhow::Result<ToolOutcome> {
        if name == "run_command" {
            anyhow::bail!("This host cannot run commands.");
        }
        WorkspaceTools.execute(ctx, tool_call_id, name, args).await
    }
}

/// The co-author trailer for a commit made from `surface`, in the format `sigit` uses.
fn co_author_trailer(surface: &str) -> String {
    format!(
        "Co-Authored-By: siGit Code v{}-{surface} <noreply@sigit.si>",
        INFO.version
    )
}

/// `/init`'s name and what it does, advertised to the client.
fn init_command() -> AvailableCommand {
    AvailableCommand::new("init", "Create or improve AGENTS.md for this repository").input(
        AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("")),
    )
}

#[async_trait]
impl Profile for SiGitCode {
    fn info(&self) -> AgentInfo {
        INFO
    }

    fn system_prompt(&self, ctx: &PromptCtx<'_>) -> String {
        let mut prompt = SYSTEM_PROMPT.replace(
            CO_AUTHOR_TRAILER_PLACEHOLDER,
            &co_author_trailer(ctx.surface),
        );
        prompt.push_str(&self.host_note());
        if !self.shell {
            prompt.push_str(NO_SHELL_NOTE);
        }
        prompt.push_str(&format!("\n\nWorking directory: {}", ctx.cwd.display()));
        if ctx.roots.len() > 1 {
            let roots: Vec<String> = ctx.roots.iter().map(|r| r.display().to_string()).collect();
            prompt.push_str(&format!("\nProject roots: {}", roots.join(", ")));
        }
        prompt
    }

    fn toolsets(&self) -> Vec<Arc<dyn Toolset>> {
        if self.shell {
            vec![Arc::new(WorkspaceTools)]
        } else {
            vec![Arc::new(FileTools)]
        }
    }

    fn slash_commands(&self, _ctx: &SessionCtx<'_>) -> Vec<AvailableCommand> {
        vec![init_command()]
    }

    fn expand_slash(&self, text: &str, _ctx: &SessionCtx<'_>) -> Option<String> {
        let rest = text.trim_start().strip_prefix("/init")?;
        // `/initialize` is not `/init`.
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            Some(INIT_PROMPT.to_string())
        } else {
            None
        }
    }

    fn auth(&self) -> Option<Arc<dyn AuthProvider>> {
        Some(Arc::new(SiGitAuth))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn names(toolsets: &[Arc<dyn Toolset>]) -> Vec<String> {
        toolsets
            .iter()
            .flat_map(|t| t.definitions())
            .map(|d| d["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_shell_variant_offers_run_command_and_the_other_does_not() {
        let with = names(&SiGitCode::new().toolsets());
        let without = names(&SiGitCode::without_shell().toolsets());
        assert!(with.contains(&"run_command".to_string()));
        assert!(!without.contains(&"run_command".to_string()));
        assert!(without.contains(&"edit_file".to_string()));
        assert!(!FileTools.handles("run_command"));
        assert!(FileTools.handles("read_file"));
    }

    #[test]
    fn the_prompt_carries_the_surface_trailer_and_names_the_host() {
        let mcp = ed_acp::ed_mcp::McpToolset::empty(ed_acp::ed_mcp::ToolsetConfig::new(
            ed_acp::ed_mcp::ClientInfo::new("t", "0"),
        ));
        let roots = vec![Path::new("/work/a").to_path_buf()];
        let ctx = PromptCtx {
            surface: "app",
            cwd: Path::new("/work/a"),
            roots: &roots,
            mcp: &mcp,
        };
        let prompt = SiGitCode::without_shell().system_prompt(&ctx);
        assert!(prompt.contains("siGit Code v"), "{prompt}");
        assert!(prompt.contains("-app <noreply@sigit.si>"));
        assert!(!prompt.contains(CO_AUTHOR_TRAILER_PLACEHOLDER));
        assert!(prompt.contains("cannot run commands"));
        assert!(prompt.contains("Working directory: /work/a"));
        let with_shell = SiGitCode::new().system_prompt(&ctx);
        assert!(!with_shell.contains("cannot run commands"));
    }

    #[test]
    fn init_expands_to_the_init_prompt_and_only_init() {
        let mcp = ed_acp::ed_mcp::McpToolset::empty(ed_acp::ed_mcp::ToolsetConfig::new(
            ed_acp::ed_mcp::ClientInfo::new("t", "0"),
        ));
        let ctx = SessionCtx { mcp: &mcp };
        let profile = SiGitCode::new();
        assert_eq!(
            profile.expand_slash("/init", &ctx).as_deref(),
            Some(INIT_PROMPT)
        );
        assert!(profile.expand_slash(" /init now", &ctx).is_some());
        assert!(profile.expand_slash("/initialize", &ctx).is_none());
        assert!(profile.expand_slash("hello", &ctx).is_none());
    }

    #[test]
    fn the_profile_brings_its_own_sign_in() {
        assert!(SiGitCode::new().auth().is_some());
    }
}
