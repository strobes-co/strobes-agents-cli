use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub workspace: Option<WorkspaceDef>,
    #[serde(default)]
    pub variables: HashMap<String, String>,
    pub phases: Vec<PhaseDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceDef {
    pub name: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PhaseDef {
    pub name: String,
    pub tasks: Vec<TaskDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskDef {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub model: Option<i64>,
    #[serde(default)]
    pub vars: HashMap<String, String>,
}

pub fn load(path: &str) -> Result<WorkflowDef> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("cannot read {path}: {e}"))?;
    let def: WorkflowDef = serde_yaml::from_str(&content)
        .map_err(|e| anyhow!("parse error in {path}: {e}"))?;
    validate(&def)?;
    Ok(def)
}

fn validate(def: &WorkflowDef) -> Result<()> {
    if def.name.is_empty() {
        return Err(anyhow!("workflow must have a 'name' field"));
    }
    if def.phases.is_empty() {
        return Err(anyhow!("workflow must have at least one phase"));
    }
    let mut all: HashSet<String> = HashSet::new();
    for phase in &def.phases {
        if phase.tasks.is_empty() {
            return Err(anyhow!("phase '{}' has no tasks", phase.name));
        }
        for task in &phase.tasks {
            if !all.insert(task.name.clone()) {
                return Err(anyhow!("duplicate task name: '{}'", task.name));
            }
        }
    }
    for phase in &def.phases {
        for task in &phase.tasks {
            for dep in &task.depends_on {
                if !all.contains(dep) {
                    return Err(anyhow!(
                        "task '{}' depends_on unknown task '{dep}'",
                        task.name
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Replace `${VAR}` and `$VAR` placeholders with values from `vars`.
///
/// Replacement order: longest key first, so `$PROGRAM_NAME` is replaced
/// before `$PROGRAM` and `$PROGRAM` before `$PRO` — preventing a shorter
/// key from clobbering the start of a longer one during bare `$VAR` passes.
/// The bare `$VAR` form also requires a non-identifier character (or end of
/// string) immediately after the name, so `$TARGET` never matches inside
/// `$TARGET_EXTRA`.
pub fn interpolate(s: &str, vars: &HashMap<String, String>) -> String {
    // Sort keys longest-first to avoid prefix collisions.
    let mut keys: Vec<&str> = vars.keys().map(|k| k.as_str()).collect();
    keys.sort_by(|a, b| b.len().cmp(&a.len()));

    let mut out = s.to_string();
    for k in keys {
        let v = &vars[k];
        // ${VAR} — always safe (braces make it unambiguous).
        out = out.replace(&format!("${{{k}}}"), v);
        // $VAR — only replace when not immediately followed by [A-Za-z0-9_].
        let needle = format!("${k}");
        let mut result = String::with_capacity(out.len());
        let mut remaining = out.as_str();
        while let Some(pos) = remaining.find(needle.as_str()) {
            let after = &remaining[pos + needle.len()..];
            let next_is_ident = after
                .chars()
                .next()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
            if next_is_ident {
                // Not a clean boundary — skip over this occurrence.
                result.push_str(&remaining[..pos + needle.len()]);
            } else {
                result.push_str(&remaining[..pos]);
                result.push_str(v);
            }
            remaining = &remaining[pos + needle.len()..];
        }
        result.push_str(remaining);
        out = result;
    }
    out
}

/// Find `.yaml` / `.yml` files in `dir` that contain a `phases:` key.
pub fn list_workflows(dir: &str) -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "yaml" || ext == "yml" {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    if content.contains("phases:") {
                        found.push(p.to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    found
}

pub fn starter_template() -> &'static str {
    r#"name: "my-workflow"
description: "A sample Strobes workflow"

# Workspace to use (created automatically if 'id' is omitted).
workspace:
  name: "Workflow Workspace"

# Variables interpolated into prompts as ${VAR} or $VAR.
variables:
  TARGET: "https://example.com"

phases:
  - name: "discovery"
    tasks:
      - name: "recon"
        prompt: "Perform reconnaissance on ${TARGET} and summarise findings."

  - name: "analysis"
    tasks:
      # These two tasks run in PARALLEL (both depend only on 'recon').
      - name: "deep-analysis"
        prompt: "Perform a deep analysis of the recon results for ${TARGET}."
        depends_on: ["recon"]
      - name: "quick-scan"
        prompt: "Run a quick vulnerability scan on ${TARGET}."
        depends_on: ["recon"]

  - name: "report"
    tasks:
      - name: "final-report"
        prompt: "Generate a comprehensive security report for ${TARGET}."
        depends_on: ["deep-analysis", "quick-scan"]
"#
}
