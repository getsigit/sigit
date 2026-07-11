//! Configurable subagent types for the `task` tool.
//!
//! A subagent type is a Markdown file with YAML frontmatter:
//!
//! ```markdown
//! ---
//! name: code-reviewer
//! description: Reviews a diff for correctness bugs. Use when the user asks for a review.
//! tools: read_file, search_files, glob
//! ---
//!
//! You are a meticulous code reviewer. Focus on correctness bugs first...
//! ```
//!
//! Discovered from, in priority order (earlier wins on name clashes):
//!
//! - `<cwd>/.sigit/agents/` and `<cwd>/.claude/agents/` (project-local)
//! - `$SIGIT_CONFIG_DIR/agents/` (default `~/.config/sigit/agents/`)
//! - `~/.claude/agents/` (shared with the broader Claude Code ecosystem)
//!
//! `name` and `description` are required (same slug rules as
//! [`crate::skills`]); the Markdown body becomes the subagent's system
//! prompt, replacing [`crate::tools::SUBAGENT_SYSTEM_PROMPT`] for that run.
//!
//! `tools` is optional and, when present, is a comma-separated allow-list.
//! It can only ever *narrow* the subagent's toolset — [`crate::tools`] owns
//! the actual read-only allow-list ([`crate::tools::SUBAGENT_TOOL_NAMES`])
//! and intersects it against this list, so a type file cannot grant itself
//! `edit_file`, `run_command`, or any other mutating tool. That security
//! boundary is why the intersection logic lives in `tools.rs` next to the
//! constant it's protecting, not here — this module only discovers and
//! parses files, it never decides what a subagent may execute.

use std::path::{Path, PathBuf};

use crate::frontmatter::{extract_frontmatter, parse_frontmatter_fields, strip_frontmatter};
use crate::skills::validate_name;

/// A discovered subagent type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentType {
    /// The `name` from frontmatter — the value passed as `task`'s
    /// `subagent_type` argument.
    pub name: String,
    /// The `description` from frontmatter: what the type is for and when to
    /// use it, embedded in the `task` tool's description.
    pub description: String,
    /// Raw `tools` list from frontmatter, if present. Unfiltered — see
    /// [`crate::tools`] for the security-relevant intersection against the
    /// base read-only allow-list.
    pub tools: Option<Vec<String>>,
    /// The Markdown body (frontmatter stripped): this type's system prompt.
    pub system_prompt: String,
    /// Where this type's file lives on disk, for diagnostics.
    pub path: PathBuf,
}

/// Discover all valid subagent types across the known roots. Earlier roots
/// win when two types share a `name`. Sorted by name for stable output.
pub fn discover_agent_types() -> Vec<AgentType> {
    let mut types: Vec<AgentType> = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();

    for root in agent_type_roots() {
        collect_agent_types_from_root(&root, &mut types, &mut seen_names);
    }

    types.sort_by(|a, b| a.name.cmp(&b.name));
    types
}

/// Re-discover types and find one by name.
pub fn resolve_agent_type(name: &str) -> Option<AgentType> {
    discover_agent_types().into_iter().find(|t| t.name == name)
}

/// Human-readable list of discovered subagent types, for the `/agents` slash
/// command.
pub fn format_agent_types_list() -> String {
    let types = discover_agent_types();
    if types.is_empty() {
        return "No subagent types found. Add a Markdown file under \
                .sigit/agents/ or .claude/agents/ in your project (e.g. \
                .sigit/agents/code-reviewer.md), or under \
                ~/.config/sigit/agents/. Without one, `task` uses the \
                default general-purpose research subagent."
            .to_string();
    }

    let mut lines = vec![format!("{} subagent type(s) available:", types.len())];
    for agent_type in &types {
        lines.push(format!("- {}: {}", agent_type.name, agent_type.description));
    }
    lines.push(String::new());
    lines.push(
        "Pass one of these names as `subagent_type` when calling `task`; omit it for the \
         default general-purpose research subagent."
            .to_string(),
    );
    lines.join("\n")
}

/// The subagent-type directories to scan, in priority order.
fn agent_type_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    // Project-local types win over user-global ones.
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".sigit").join("agents"));
        roots.push(cwd.join(".claude").join("agents"));
    }

    // User-global siGit config dir (honours SIGIT_CONFIG_DIR).
    if let Some(config_dir) = sigit_config_dir() {
        roots.push(config_dir.join("agents"));
    }

    // Shared with the broader Claude Code ecosystem.
    if let Some(home) = home_dir() {
        roots.push(home.join(".claude").join("agents"));
    }

    roots
}

fn sigit_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("SIGIT_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    home_dir().map(|home| home.join(".config").join("sigit"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Scan a single root (non-recursive, matching [`crate::skills`]) for `*.md`
/// files, appending newly-seen types.
fn collect_agent_types_from_root(
    root: &Path,
    types: &mut Vec<AgentType>,
    seen_names: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // Most roots won't exist; that's expected, not an error.
        Err(_) => return,
    };

    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                log::warn!("skipping agent type at {}: {error}", path.display());
                continue;
            }
        };

        let agent_type = match parse_agent_type(&contents, &path) {
            Ok(agent_type) => agent_type,
            Err(error) => {
                log::warn!("skipping invalid agent type at {}: {error}", path.display());
                continue;
            }
        };

        if seen_names.iter().any(|n| n == &agent_type.name) {
            log::debug!(
                "agent type \"{}\" at {} shadowed by an earlier definition",
                agent_type.name,
                path.display()
            );
            continue;
        }

        seen_names.push(agent_type.name.clone());
        types.push(agent_type);
    }
}

/// Parse a subagent-type file into an [`AgentType`]. `name` and
/// `description` are required, mirroring [`crate::skills`]'s `SKILL.md`
/// validation.
fn parse_agent_type(contents: &str, path: &Path) -> Result<AgentType, String> {
    let frontmatter = extract_frontmatter(contents)
        .ok_or_else(|| "missing YAML frontmatter (expected leading `---` block)".to_string())?;

    let fields = parse_frontmatter_fields(frontmatter);

    let name = fields
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.clone())
        .ok_or_else(|| "frontmatter is missing required field `name`".to_string())?;
    validate_name(&name)?;

    let description = fields
        .iter()
        .find(|(k, _)| k == "description")
        .map(|(_, v)| v.clone())
        .ok_or_else(|| "frontmatter is missing required field `description`".to_string())?;
    if description.is_empty() {
        return Err("`description` must not be empty".to_string());
    }

    let tools = fields
        .iter()
        .find(|(k, _)| k == "tools")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
        .map(|raw| {
            raw.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect::<Vec<String>>()
        });

    let system_prompt = strip_frontmatter(contents).trim().to_string();
    if system_prompt.is_empty() {
        return Err("body (the system prompt) must not be empty".to_string());
    }

    Ok(AgentType {
        name,
        description,
        tools,
        system_prompt,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("sigit-subagents-test-{name}-{nanos}"))
    }

    #[test]
    fn parse_agent_type_reads_frontmatter_and_body() {
        let contents = "---\nname: code-reviewer\ndescription: Reviews diffs\ntools: read_file, search_files\n---\n\nYou are a reviewer.\n";
        let agent_type = parse_agent_type(contents, Path::new("/x/code-reviewer.md")).unwrap();
        assert_eq!(agent_type.name, "code-reviewer");
        assert_eq!(agent_type.description, "Reviews diffs");
        assert_eq!(
            agent_type.tools,
            Some(vec!["read_file".to_string(), "search_files".to_string()])
        );
        assert_eq!(agent_type.system_prompt, "You are a reviewer.");
    }

    #[test]
    fn parse_agent_type_without_tools_field_is_none() {
        let contents = "---\nname: generalist\ndescription: does anything\n---\n\nBe helpful.\n";
        let agent_type = parse_agent_type(contents, Path::new("/x/generalist.md")).unwrap();
        assert!(agent_type.tools.is_none());
    }

    #[test]
    fn parse_agent_type_requires_name_description_and_body() {
        let dir = Path::new("/tmp/x.md");
        assert!(parse_agent_type("---\ndescription: x\n---\nbody", dir).is_err());
        assert!(parse_agent_type("---\nname: x\n---\nbody", dir).is_err());
        assert!(parse_agent_type("---\nname: x\ndescription: y\n---\n\n", dir).is_err());
        assert!(parse_agent_type("---\nname: Bad Name\ndescription: y\n---\nbody", dir).is_err());
    }

    #[test]
    fn discover_and_resolve_roundtrip() {
        let root = unique_dir("roundtrip");
        let agents_root = root.join(".sigit").join("agents");
        fs::create_dir_all(&agents_root).unwrap();
        fs::write(
            agents_root.join("code-reviewer.md"),
            "---\nname: code-reviewer\ndescription: Reviews diffs\ntools: read_file, search_files\n---\n\nYou are a reviewer.\n",
        )
        .unwrap();

        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();

        let types = discover_agent_types();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "code-reviewer");

        let resolved = resolve_agent_type("code-reviewer").expect("resolve by name");
        assert_eq!(resolved.system_prompt, "You are a reviewer.");
        assert!(resolve_agent_type("nope").is_none());

        std::env::set_current_dir(prev).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn format_agent_types_list_reports_none_found() {
        let root = unique_dir("empty");
        fs::create_dir_all(&root).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();

        assert!(format_agent_types_list().contains("No subagent types found"));

        std::env::set_current_dir(prev).unwrap();
        let _ = fs::remove_dir_all(&root);
    }
}
