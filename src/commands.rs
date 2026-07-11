//! User-defined slash commands for siGit Code.
//!
//! A command is a Markdown file with optional YAML frontmatter:
//!
//! ```markdown
//! ---
//! description: Review a PR for correctness bugs
//! argument-hint: <pr-number>
//! ---
//!
//! Review PR #$1 for correctness bugs. Focus on the diff, not style.
//! ```
//!
//! Discovered from, in priority order (earlier wins on name clashes):
//!
//! - `<cwd>/.sigit/commands/` and `<cwd>/.claude/commands/` (project-local)
//! - `$SIGIT_CONFIG_DIR/commands/` (default `~/.config/sigit/commands/`)
//! - `~/.claude/commands/` (shared with the broader Claude Code ecosystem)
//!
//! Subdirectories namespace the command name with `:`, e.g.
//! `.sigit/commands/git/commit.md` is invoked as `/git:commit`.
//!
//! Unlike [`crate::skills`] (progressive disclosure via a tool call),
//! invoking a custom command works like the built-in `/init`: the caller
//! resolves the command, renders its body against the typed arguments, and
//! feeds the result to the model as if the user had typed it directly —
//! going through the ordinary tool-calling turn and permission checks. There
//! is no separate execution sandbox; a command is just a canned prompt.

use std::path::{Path, PathBuf};

use crate::frontmatter::{extract_frontmatter, parse_frontmatter_fields, strip_frontmatter};

/// A discovered custom command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// The invocation name, without a leading `/` (e.g. `"review"` or
    /// `"git:commit"` for a command nested under a `git/` subdirectory).
    pub name: String,
    /// Optional one-line description from frontmatter, shown in `/commands`
    /// and advertised to ACP clients so they forward the command at all.
    pub description: Option<String>,
    /// Optional `argument-hint` from frontmatter (e.g. `"<pr-number>"`),
    /// shown as the input hint for ACP clients.
    pub argument_hint: Option<String>,
    /// The Markdown body (frontmatter stripped): the prompt template.
    pub body: String,
    /// Where this command's file lives on disk, for diagnostics.
    pub path: PathBuf,
}

/// Discover all custom commands across the known roots. Earlier roots win
/// when two commands share a `name`. Sorted by name for stable output.
pub fn discover_commands() -> Vec<CustomCommand> {
    let mut commands: Vec<CustomCommand> = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();

    for root in command_roots() {
        collect_commands_from_root(&root, &root, &mut commands, &mut seen_names);
    }

    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

/// Re-discover commands and find one by name. Accepts the name with or
/// without a leading `/` so callers can pass the raw token straight out of
/// slash-command parsing.
pub fn resolve_command(name: &str) -> Option<CustomCommand> {
    let name = name.strip_prefix('/').unwrap_or(name);
    discover_commands().into_iter().find(|c| c.name == name)
}

/// Render a command body against the raw argument string the user typed
/// after the command name (everything on the line past the first
/// whitespace; `None` if nothing followed).
///
/// Supports `$ARGUMENTS` (the whole argument string) and positional `$1`
/// through `$9` (whitespace-split). If the body uses neither placeholder and
/// arguments were supplied anyway, they're appended so nothing typed is
/// silently dropped.
pub fn render(body: &str, arguments: Option<&str>) -> String {
    let arguments = arguments.unwrap_or("");
    let mut result = body.to_string();
    let mut substituted = false;

    if result.contains("$ARGUMENTS") {
        result = result.replace("$ARGUMENTS", arguments);
        substituted = true;
    }

    let positional: Vec<&str> = arguments.split_whitespace().collect();
    for (i, arg) in positional.iter().enumerate() {
        let placeholder = format!("${}", i + 1);
        if result.contains(&placeholder) {
            result = result.replace(&placeholder, arg);
            substituted = true;
        }
    }

    if !substituted && !arguments.is_empty() {
        result.push_str("\n\n");
        result.push_str(arguments);
    }

    result
}

/// Human-readable list of discovered commands, for the `/commands` slash
/// command.
pub fn format_commands_list() -> String {
    let commands = discover_commands();
    if commands.is_empty() {
        return "No custom commands found. Add a Markdown file under \
                .sigit/commands/ or .claude/commands/ in your project \
                (e.g. .sigit/commands/review.md), or under \
                ~/.config/sigit/commands/."
            .to_string();
    }

    let mut lines = vec![format!("{} custom command(s) available:", commands.len())];
    for command in &commands {
        let hint = command
            .argument_hint
            .as_deref()
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        let description = command.description.as_deref().unwrap_or("(no description)");
        lines.push(format!("- /{}{hint} — {description}", command.name));
    }
    lines.join("\n")
}

/// The command directories to scan, in priority order.
fn command_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    // Project-local commands win over user-global ones.
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".sigit").join("commands"));
        roots.push(cwd.join(".claude").join("commands"));
    }

    // User-global siGit config dir (honours SIGIT_CONFIG_DIR).
    if let Some(config_dir) = sigit_config_dir() {
        roots.push(config_dir.join("commands"));
    }

    // Shared with the broader Claude Code ecosystem.
    if let Some(home) = home_dir() {
        roots.push(home.join(".claude").join("commands"));
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

/// Recursively scan a root for `*.md` files, appending newly-seen commands.
/// `root` stays fixed across the recursion so nested names are relative to
/// it; `dir` is the directory currently being walked.
fn collect_commands_from_root(
    root: &Path,
    dir: &Path,
    commands: &mut Vec<CustomCommand>,
    seen_names: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Most roots won't exist; that's expected, not an error.
        Err(_) => return,
    };

    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // Stable, predictable discovery order regardless of filesystem order.
    paths.sort();

    for path in paths {
        if path.is_dir() {
            collect_commands_from_root(root, &path, commands, seen_names);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        let Some(name) = command_name(root, &path) else {
            continue;
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                log::warn!("skipping command at {}: {error}", path.display());
                continue;
            }
        };

        let command = parse_command(&contents, &name, &path);

        // First root to define a name wins; later duplicates are ignored.
        if seen_names.iter().any(|n| n == &command.name) {
            log::debug!(
                "command \"{}\" at {} shadowed by an earlier definition",
                command.name,
                path.display()
            );
            continue;
        }

        seen_names.push(command.name.clone());
        commands.push(command);
    }
}

/// Derive a namespaced command name from a file's path relative to its root:
/// `git/commit.md` under root `.sigit/commands` becomes `"git:commit"`.
fn command_name(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let without_ext = relative.with_extension("");
    let parts: Vec<String> = without_ext
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some(parts.join(":"))
}

/// Parse a command file into a [`CustomCommand`]. Frontmatter is entirely
/// optional — a command with no frontmatter at all is just its raw body with
/// no description or argument hint.
fn parse_command(contents: &str, name: &str, path: &Path) -> CustomCommand {
    let mut description = None;
    let mut argument_hint = None;

    if let Some(frontmatter) = extract_frontmatter(contents) {
        let fields = parse_frontmatter_fields(frontmatter);
        description = fields
            .iter()
            .find(|(k, _)| k == "description")
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty());
        argument_hint = fields
            .iter()
            .find(|(k, _)| k == "argument-hint")
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty());
    }

    let body = strip_frontmatter(contents).trim().to_string();

    CustomCommand {
        name: name.to_string(),
        description,
        argument_hint,
        body,
        path: path.to_path_buf(),
    }
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
        std::env::temp_dir().join(format!("sigit-commands-test-{name}-{nanos}"))
    }

    #[test]
    fn render_substitutes_arguments_placeholder() {
        let body = "Review PR: $ARGUMENTS";
        assert_eq!(render(body, Some("#42")), "Review PR: #42");
        assert_eq!(render(body, None), "Review PR: ");
    }

    #[test]
    fn render_substitutes_positional_placeholders() {
        let body = "Compare $1 against $2.";
        assert_eq!(
            render(body, Some("main feature-x")),
            "Compare main against feature-x."
        );
    }

    #[test]
    fn render_appends_arguments_when_no_placeholder_present() {
        let body = "Run the linter.";
        assert_eq!(render(body, Some("--fix")), "Run the linter.\n\n--fix");
        // No arguments given, nothing appended.
        assert_eq!(render(body, None), "Run the linter.");
    }

    #[test]
    fn command_name_namespaces_by_subdirectory() {
        let root = Path::new("/proj/.sigit/commands");
        assert_eq!(
            command_name(root, &root.join("review.md")),
            Some("review".to_string())
        );
        assert_eq!(
            command_name(root, &root.join("git").join("commit.md")),
            Some("git:commit".to_string())
        );
    }

    #[test]
    fn parse_command_reads_frontmatter_and_body() {
        let contents =
            "---\ndescription: Review a PR\nargument-hint: <pr-number>\n---\n\nReview PR #$1.\n";
        let command = parse_command(contents, "review", Path::new("/x/review.md"));
        assert_eq!(command.description.as_deref(), Some("Review a PR"));
        assert_eq!(command.argument_hint.as_deref(), Some("<pr-number>"));
        assert_eq!(command.body, "Review PR #$1.");
    }

    #[test]
    fn parse_command_without_frontmatter_is_just_the_body() {
        let contents = "Just do the thing.\n";
        let command = parse_command(contents, "thing", Path::new("/x/thing.md"));
        assert!(command.description.is_none());
        assert!(command.argument_hint.is_none());
        assert_eq!(command.body, "Just do the thing.");
    }

    #[test]
    fn discover_and_resolve_roundtrip() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = unique_dir("roundtrip");
        let commands_root = root.join(".sigit").join("commands");
        fs::create_dir_all(commands_root.join("git")).unwrap();
        fs::write(
            commands_root.join("review.md"),
            "---\ndescription: Review the diff\n---\n\nReview: $ARGUMENTS\n",
        )
        .unwrap();
        fs::write(
            commands_root.join("git").join("commit.md"),
            "Commit with message: $ARGUMENTS\n",
        )
        .unwrap();

        // discover_commands() reads the current directory, so run from `root`.
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();

        let commands = discover_commands();
        assert_eq!(commands.len(), 2);
        let review = commands.iter().find(|c| c.name == "review").unwrap();
        assert_eq!(review.description.as_deref(), Some("Review the diff"));
        let commit = commands.iter().find(|c| c.name == "git:commit").unwrap();
        assert_eq!(commit.body, "Commit with message: $ARGUMENTS");

        let resolved = resolve_command("/review").expect("resolve by slash-prefixed name");
        assert_eq!(resolved.name, "review");
        let resolved_bare = resolve_command("git:commit").expect("resolve by bare name");
        assert_eq!(resolved_bare.name, "git:commit");
        assert!(resolve_command("nope").is_none());

        std::env::set_current_dir(prev).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn format_commands_list_reports_none_found() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = unique_dir("empty");
        fs::create_dir_all(&root).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();

        assert!(format_commands_list().contains("No custom commands found"));

        std::env::set_current_dir(prev).unwrap();
        let _ = fs::remove_dir_all(&root);
    }
}
