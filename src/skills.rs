//! Agent Skills support for siGit Code.
//!
//! Implements the open [Agent Skills](https://agentskills.io) format: a skill is
//! a directory containing a `SKILL.md` file with YAML frontmatter (`name` +
//! `description`, plus optional fields) followed by Markdown instructions. Skills
//! may bundle `scripts/`, `references/`, and `assets/` the agent loads on demand.
//!
//! Loading follows the spec's *progressive disclosure*:
//!
//! 1. **Discovery** — at turn-build time we scan the skill roots and load only
//!    each skill's `name` and `description` into the `skill` tool's description,
//!    so the model knows what's available for a small context cost.
//! 2. **Activation** — when a task matches, the model calls the `skill` tool with
//!    a name; [`activate_skill`] reads the full `SKILL.md` body into context.
//! 3. **Execution** — the model follows the instructions, reading bundled files
//!    (under the reported skill directory) with the normal file/command tools.
//!
//! Skills are discovered from, in priority order (earlier wins on name clashes):
//!
//! - `<cwd>/.sigit/skills/` and `<cwd>/.claude/skills/` (project-local)
//! - `$SIGIT_CONFIG_DIR/skills/` (default `~/.config/sigit/skills/`)
//! - `~/.claude/skills/` (shared with the broader ecosystem)

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::frontmatter::{extract_frontmatter, parse_frontmatter_fields, strip_frontmatter};

/// The agent-facing tool name used to activate a skill.
pub const SKILL_TOOL_NAME: &str = "skill";

/// Hard cap on how many skills we advertise, to bound the tool description size.
const MAX_ADVERTISED_SKILLS: usize = 100;

/// A discovered skill: its identifying metadata plus where it lives on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The `name` from frontmatter. Lowercase alphanumeric + single hyphens.
    pub name: String,
    /// The `description` from frontmatter: what the skill does and when to use it.
    pub description: String,
    /// Optional `license` field.
    pub license: Option<String>,
    /// Optional `compatibility` field (environment requirements).
    pub compatibility: Option<String>,
    /// The skill's root directory (the one holding `SKILL.md`).
    pub dir: PathBuf,
}

impl Skill {
    /// Absolute path to this skill's `SKILL.md`.
    fn skill_md(&self) -> PathBuf {
        self.dir.join("SKILL.md")
    }
}

/// JSON Schema for the `skill` tool's arguments.
pub fn skill_tool_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "The name of the skill to activate, exactly as listed in this tool's description."
            }
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

/// Build the `skill` tool description, embedding the discovery list (each skill's
/// `name` and `description`) so the model can decide when to activate one.
pub fn skill_tool_description(skills: &[Skill]) -> String {
    let mut out = String::from(
        "Activate an Agent Skill to load its full instructions into context. \
         Skills are reusable, on-demand capabilities — specialized knowledge and \
         step-by-step workflows packaged as a folder. Only the name and description \
         of each skill are loaded up front; calling this tool with a skill's `name` \
         reads its full instructions (and tells you the skill's directory, so you \
         can read any bundled scripts, references, or assets with the file and \
         command tools). Activate a skill as soon as the user's task matches one of \
         the descriptions below; follow its instructions over your defaults.\n\n\
         Available skills:\n",
    );
    for skill in skills.iter().take(MAX_ADVERTISED_SKILLS) {
        out.push_str("- ");
        out.push_str(&skill.name);
        out.push_str(": ");
        out.push_str(&skill.description);
        out.push('\n');
    }
    out
}

/// Human-readable list of discovered skills, for the `/skills` slash command.
pub fn format_skills_list() -> String {
    let skills = discover_skills();
    if skills.is_empty() {
        return "No skills found. Add a skill folder (with a SKILL.md) under \
                .sigit/skills/ or .claude/skills/ in your project, or under \
                ~/.config/sigit/skills/. See https://agentskills.io."
            .to_string();
    }

    let mut lines = vec![format!("{} skill(s) available:", skills.len())];
    for skill in &skills {
        lines.push(format!("- {}: {}", skill.name, skill.description));
    }
    lines.push(String::new());
    lines.push(
        "I activate a skill automatically when your task matches its description.".to_string(),
    );
    lines.join("\n")
}

/// Execute the `skill` tool: parse the requested name and return the full
/// `SKILL.md` body, prefixed with the skill's directory so relative references
/// (e.g. `scripts/foo.py`, `references/REFERENCE.md`) can be resolved.
pub fn activate_skill(arguments: &str) -> String {
    let args: Value = match serde_json::from_str(arguments) {
        Ok(v) => v,
        Err(err) => return format!("Error: failed to parse arguments: {err}"),
    };

    let name = match args.get("name").and_then(Value::as_str) {
        Some(n) => n.trim(),
        None => return "Error: missing required parameter \"name\"".to_string(),
    };

    // Re-discover so activation always reflects the skills on disk right now.
    let skills = discover_skills();
    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        if skills.is_empty() {
            return format!("Error: no skill named \"{name}\" is available (no skills found).");
        }
        let available = skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return format!("Error: no skill named \"{name}\". Available skills: {available}.");
    };

    let body = match read_skill_body(&skill.skill_md()) {
        Ok(body) => body,
        Err(err) => {
            return format!(
                "Error: could not read SKILL.md for \"{name}\" at {}: {err}",
                skill.skill_md().display()
            );
        }
    };

    // Surface the optional metadata so the agent (and user) can sanity-check
    // environment requirements before following the instructions.
    let mut notes = String::new();
    if let Some(compatibility) = &skill.compatibility {
        notes.push_str(&format!("Compatibility: {compatibility}\n"));
    }
    if let Some(license) = &skill.license {
        notes.push_str(&format!("License: {license}\n"));
    }
    if !notes.is_empty() {
        notes.push('\n');
    }

    let dir = skill.dir.display();
    format!(
        "Skill \"{name}\" activated. Its directory is {dir} — resolve any relative \
         file references (scripts/, references/, assets/) against that path. Follow \
         these instructions:\n\n{notes}{body}"
    )
}

/// Discover all valid skills across the known roots. Earlier roots win when two
/// skills share a `name`. The result is sorted by name for stable output.
pub fn discover_skills() -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();

    for root in skill_roots() {
        collect_skills_from_root(&root, &mut skills, &mut seen_names);
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// The skill directories to scan, in priority order.
fn skill_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    // Project-local skills win over user-global ones.
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join(".sigit").join("skills"));
        roots.push(cwd.join(".claude").join("skills"));
    }

    // User-global siGit config dir (honours SIGIT_CONFIG_DIR).
    if let Some(config_dir) = sigit_config_dir() {
        roots.push(config_dir.join("skills"));
    }

    // Shared with the broader Agent Skills ecosystem.
    if let Some(home) = home_dir() {
        roots.push(home.join(".claude").join("skills"));
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

/// Scan a single root for skill subdirectories, appending newly-seen skills.
fn collect_skills_from_root(root: &Path, skills: &mut Vec<Skill>, seen_names: &mut Vec<String>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // Most roots won't exist; that's expected, not an error.
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }

        let skill_md = dir.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }

        let contents = match std::fs::read_to_string(&skill_md) {
            Ok(contents) => contents,
            Err(error) => {
                log::warn!("skipping skill at {}: {error}", skill_md.display());
                continue;
            }
        };

        let skill = match parse_skill(&contents, &dir) {
            Ok(skill) => skill,
            Err(error) => {
                log::warn!("skipping invalid skill at {}: {error}", skill_md.display());
                continue;
            }
        };

        // First root to define a name wins; later duplicates are ignored.
        if seen_names.iter().any(|name| name == &skill.name) {
            log::debug!(
                "skill \"{}\" at {} shadowed by an earlier definition",
                skill.name,
                dir.display()
            );
            continue;
        }

        seen_names.push(skill.name.clone());
        skills.push(skill);
    }
}

/// Parse a `SKILL.md` into a [`Skill`], validating the required fields against
/// the Agent Skills spec. `dir` is the skill's root directory.
fn parse_skill(contents: &str, dir: &Path) -> Result<Skill, String> {
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
    if description.chars().count() > 1024 {
        return Err("`description` exceeds the 1024-character limit".to_string());
    }

    // The spec requires `name` to match the parent directory name. Warn but stay
    // lenient — the frontmatter name is the identity used for activation.
    if let Some(dir_name) = dir.file_name().and_then(|n| n.to_str())
        && dir_name != name
    {
        log::warn!(
            "skill name \"{name}\" does not match its directory \"{dir_name}\" at {}",
            dir.display()
        );
    }

    let license = fields
        .iter()
        .find(|(k, _)| k == "license")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty());
    let compatibility = fields
        .iter()
        .find(|(k, _)| k == "compatibility")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty());

    Ok(Skill {
        name,
        description,
        license,
        compatibility,
        dir: dir.to_path_buf(),
    })
}

/// Validate the `name` field per the Agent Skills spec: 1-64 chars, lowercase
/// alphanumeric and hyphens only, no leading/trailing or consecutive hyphens.
fn validate_name(name: &str) -> Result<(), String> {
    let len = name.chars().count();
    if len == 0 {
        return Err("`name` must not be empty".to_string());
    }
    if len > 64 {
        return Err("`name` exceeds the 64-character limit".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("`name` may only contain lowercase letters, digits, and hyphens".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("`name` must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        return Err("`name` must not contain consecutive hyphens".to_string());
    }
    Ok(())
}

/// Read the Markdown body of a `SKILL.md` (everything after the frontmatter),
/// falling back to the whole file if no frontmatter delimiter is found.
fn read_skill_body(skill_md: &Path) -> Result<String, String> {
    let contents = std::fs::read_to_string(skill_md).map_err(|e| e.to_string())?;
    Ok(strip_frontmatter(&contents).trim().to_string())
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
        std::env::temp_dir().join(format!("sigit-skills-test-{name}-{nanos}"))
    }

    #[test]
    fn validate_name_accepts_valid_names() {
        assert!(validate_name("pdf-processing").is_ok());
        assert!(validate_name("data-analysis").is_ok());
        assert!(validate_name("code-review").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("skill1").is_ok());
    }

    #[test]
    fn validate_name_rejects_invalid_names() {
        assert!(validate_name("").is_err());
        assert!(validate_name("PDF-Processing").is_err());
        assert!(validate_name("-pdf").is_err());
        assert!(validate_name("pdf-").is_err());
        assert!(validate_name("pdf--processing").is_err());
        assert!(validate_name("pdf_processing").is_err());
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn parse_skill_requires_name_and_description() {
        let dir = Path::new("/tmp/example-skill");
        assert!(parse_skill("---\ndescription: x\n---\n", dir).is_err());
        assert!(parse_skill("---\nname: x\n---\n", dir).is_err());
        let ok = parse_skill(
            "---\nname: example-skill\ndescription: does things\n---\nbody",
            dir,
        );
        assert!(ok.is_ok());
        let skill = ok.unwrap();
        assert_eq!(skill.name, "example-skill");
        assert_eq!(skill.description, "does things");
    }

    #[test]
    fn discover_and_activate_roundtrip() {
        let root = unique_dir("roundtrip");
        let skills_root = root.join(".sigit").join("skills");
        let skill_dir = skills_root.join("hello-world");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: hello-world\ndescription: Say hello. Use when greeting.\n---\n\nGreet the user warmly.\n",
        )
        .unwrap();

        // discover_skills() reads the current directory, so run from `root`.
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();

        let skills = discover_skills();
        let found = skills.iter().find(|s| s.name == "hello-world");
        assert!(found.is_some(), "expected to discover hello-world");
        assert_eq!(found.unwrap().description, "Say hello. Use when greeting.");

        let activated = activate_skill(r#"{"name": "hello-world"}"#);
        assert!(activated.contains("Greet the user warmly."));
        assert!(activated.contains("activated"));

        let missing = activate_skill(r#"{"name": "nope"}"#);
        assert!(missing.contains("no skill named"));

        std::env::set_current_dir(prev).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn skill_tool_description_lists_skills() {
        let skills = vec![Skill {
            name: "pdf-processing".to_string(),
            description: "Extract PDF text".to_string(),
            license: None,
            compatibility: None,
            dir: PathBuf::from("/x/pdf-processing"),
        }];
        let desc = skill_tool_description(&skills);
        assert!(desc.contains("Available skills:"));
        assert!(desc.contains("- pdf-processing: Extract PDF text"));
    }

    #[test]
    fn skill_tool_schema_requires_name() {
        let schema = skill_tool_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"][0], "name");
    }
}
