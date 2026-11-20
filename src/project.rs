//! What the CLI tells the agent about the project it runs in.
//!
//! * `STROBES.md` — notes for the agent (scope, rules of engagement, test
//!   accounts), read from the user's config dir and from the project root down
//!   to the current directory, like `CLAUDE.md`.
//! * Local skills — `.strobes/skills/<slug>/SKILL.md` in the project (or the
//!   user's `skills/` config dir). Only a manifest goes with each message; the
//!   agent loads one with `load_skill("local:<slug>")` and the server asks the
//!   CLI for it, so nothing is uploaded.
//!
//! Both are read fresh for every message, so edits apply on the next turn.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Total size of the instructions sent (the server caps it again).
const MAX_INSTRUCTIONS: usize = 20_000;
const MAX_SKILLS: usize = 100;

/// The project root: the nearest ancestor holding `.git`, `.strobes` or a
/// `STROBES.md`; the current directory when there is none.
pub fn root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for dir in cwd.ancestors() {
        if dir.join(".git").exists() || dir.join(".strobes").is_dir() || dir.join("STROBES.md").is_file() {
            return dir.to_path_buf();
        }
    }
    cwd
}

/// Directories from the project root down to the current directory.
fn project_dirs() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = root();
    let mut dirs: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|d| d.starts_with(&root))
        .map(Path::to_path_buf)
        .collect();
    dirs.reverse();
    dirs
}

/// The `STROBES.md` files that apply here, general first.
pub fn instruction_files() -> Vec<PathBuf> {
    let mut files = vec![crate::config::config_dir().join("STROBES.md")];
    for d in project_dirs() {
        files.push(d.join("STROBES.md"));
        files.push(d.join(".strobes").join("STROBES.md"));
    }
    files.into_iter().filter(|f| f.is_file()).collect()
}

pub fn instructions() -> Option<String> {
    let mut out = String::new();
    for f in instruction_files() {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("<!-- {} -->\n{text}", f.display()));
    }
    if out.is_empty() {
        return None;
    }
    if out.len() > MAX_INSTRUCTIONS {
        let mut cut = MAX_INSTRUCTIONS;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    Some(out)
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalSkill {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// Where local skills live, most specific last (so it wins on a clash).
pub fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![crate::config::config_dir().join("skills")];
    for d in project_dirs() {
        dirs.push(d.join(".strobes").join("skills"));
    }
    dirs
}

fn slugify(name: &str) -> Option<String> {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    (!s.is_empty() && s.len() <= 64 && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())).then_some(s)
}

/// `name` / `description` from a SKILL.md's YAML front matter.
fn front_matter(text: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = text.strip_prefix("---") else { return (None, None) };
    let Some(end) = rest.find("\n---") else { return (None, None) };
    let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&rest[..end]) else { return (None, None) };
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    (get("name"), get("description"))
}

/// The first prose line, when a skill has no description.
fn first_line(text: &str) -> String {
    let body = match text.strip_prefix("---").and_then(|r| r.find("\n---").map(|e| &r[e + 4..])) {
        Some(b) => b,
        None => text,
    };
    body.lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

pub fn local_skills() -> Vec<LocalSkill> {
    let mut by_slug: std::collections::BTreeMap<String, LocalSkill> = Default::default();
    for dir in skill_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let md = path.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&md) else { continue };
            let Some(slug) = path.file_name().and_then(|n| n.to_str()).and_then(slugify) else { continue };
            let (name, desc) = front_matter(&text);
            by_slug.insert(
                slug.clone(),
                LocalSkill {
                    name: name.unwrap_or_else(|| slug.clone()),
                    description: desc.unwrap_or_else(|| first_line(&text)),
                    slug,
                    dir: path,
                },
            );
        }
    }
    by_slug.into_values().take(MAX_SKILLS).collect()
}

/// The SKILL.md of a local skill, plus where its other files are, for the
/// agent (served over `load_local_skill`).
pub fn load_skill(slug: &str) -> Result<String, String> {
    let slug = slug.trim().trim_start_matches("local:").to_lowercase();
    let skill = local_skills()
        .into_iter()
        .find(|s| s.slug == slug)
        .ok_or_else(|| format!("no local skill named '{slug}' (looked in {})", skill_dirs().iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")))?;
    let text = std::fs::read_to_string(skill.dir.join("SKILL.md")).map_err(|e| e.to_string())?;
    let mut files = Vec::new();
    collect_files(&skill.dir, &skill.dir, &mut files);
    files.retain(|f| f != "SKILL.md");
    let mut out = text;
    out.push_str(&format!("\n\n---\nSkill directory (on this machine): {}\n", skill.dir.display()));
    if !files.is_empty() {
        out.push_str("Files:\n");
        for f in files.iter().take(200) {
            out.push_str(&format!("- {f}\n"));
        }
    }
    Ok(out)
}

fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.path());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "__pycache__" {
            continue;
        }
        if p.is_dir() {
            collect_files(base, &p, out);
        } else if let Ok(rel) = p.strip_prefix(base) {
            out.push(rel.display().to_string());
        }
        if out.len() >= 500 {
            return;
        }
    }
}

/// The extra `send_message` context: instructions, the local-skill manifest
/// and the MCP tools. Keys are left out when empty, so a plain project sends
/// exactly what it did before.
pub fn client_context() -> Value {
    let mut ctx = json!({});
    if let Some(text) = instructions() {
        ctx["project_instructions"] = json!(text);
    }
    let skills: Vec<Value> = local_skills()
        .into_iter()
        .map(|s| json!({"slug": s.slug, "name": s.name, "description": s.description}))
        .collect();
    if !skills.is_empty() {
        ctx["local_skills"] = json!(skills);
    }
    let tools = crate::mcp::client_tools();
    if !tools.is_empty() {
        ctx["client_tools"] = json!(tools);
    }
    ctx
}

/// A starter STROBES.md for `/init`.
pub const STARTER: &str = "# STROBES.md

Notes the Strobes agent reads at the start of every turn in this project.

## Scope
- In scope:
- Out of scope:

## Rules of engagement
- No destructive actions (no DoS, no data deletion) unless asked.
- Rate limit:

## Test accounts
<!-- Prefer /creds for secrets; don't paste passwords here. -->

## Notes
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_fallback_description() {
        let (n, d) = front_matter("---\nname: JWT attacks\ndescription: Forge tokens\n---\n# Body");
        assert_eq!(n.as_deref(), Some("JWT attacks"));
        assert_eq!(d.as_deref(), Some("Forge tokens"));
        assert_eq!(front_matter("# no front matter"), (None, None));
        assert_eq!(first_line("---\nname: x\n---\n\n# Recon steps\nmore"), "Recon steps");
    }

    #[test]
    fn slugs_are_safe() {
        assert_eq!(slugify("My Skill").as_deref(), Some("my-skill"));
        assert!(slugify("../etc").is_none());
        assert!(slugify("").is_none());
        assert!(slugify("---").is_none());
    }

    #[test]
    fn skills_and_instructions_are_found_in_the_project() {
        let tmp = std::env::temp_dir().join(format!("strobes-proj-{}", std::process::id()));
        let skill = tmp.join(".strobes/skills/recon");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: Recon\ndescription: Map the target\n---\nDo recon.").unwrap();
        std::fs::write(skill.join("scripts/run.py"), "print(1)").unwrap();
        std::fs::write(tmp.join("STROBES.md"), "Only test staging.").unwrap();
        let prev = std::env::current_dir().unwrap();
        let home = std::env::var("STROBES_AI_HOME").ok();
        std::env::set_var("STROBES_AI_HOME", tmp.join("cfg"));
        std::env::set_current_dir(&tmp).unwrap();

        let skills = local_skills();
        let text = load_skill("local:recon");
        let notes = instructions();

        std::env::set_current_dir(prev).unwrap();
        match home {
            Some(h) => std::env::set_var("STROBES_AI_HOME", h),
            None => std::env::remove_var("STROBES_AI_HOME"),
        }
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].slug, "recon");
        assert_eq!(skills[0].description, "Map the target");
        let text = text.unwrap();
        assert!(text.contains("Do recon.") && text.contains("- scripts/run.py"), "{text}");
        assert!(notes.unwrap().contains("Only test staging."));
        assert!(load_skill("nope").is_err());
    }
}
