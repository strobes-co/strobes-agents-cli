//! REST client for the Strobes backend (MasterKey auth).
//!
//! `Authorization: token <key>` — matches strobes/app/authentication.py.

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::config::Profile;

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// AI credit usage totals (and optional per-workspace breakdown).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreditsSummary {
    #[serde(default)]
    pub credits: f64,
    #[serde(default)]
    pub tokens: i64,
    #[serde(default)]
    pub runs: i64,
    #[serde(default)]
    pub by_workspace: Vec<WorkspaceCredits>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WorkspaceCredits {
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub credits: f64,
    #[serde(default)]
    pub tokens: i64,
    #[serde(default)]
    pub runs: i64,
}


#[derive(Debug, Clone, Deserialize)]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub last_message: String,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkspaceFile_ {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default, rename = "isFolder")]
    pub is_folder: bool,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HistMsg {
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ActiveRun {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ThreadHistory {
    #[serde(default)]
    pub messages: Vec<HistMsg>,
    #[serde(default)]
    pub active_run: Option<ActiveRun>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Finding {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub severity_label: String,
    #[serde(default)]
    pub state_label: String,
    #[serde(default)]
    pub cvss: Option<f64>,
    #[serde(default)]
    pub asset: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mitigation: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Approval {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub thread_id: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub action_type: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub target_ids: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SlashCmd {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub argument_hint: String,
}

/// Built-in model IDs from the backend's `llm_model_choices` constant.
/// ID `0` is the sentinel that means "use the org's configured default"
/// (no `llm_model` key is sent in the context).
pub const BUILTIN_MODELS: &[(i64, &str)] = &[
    (0,  "Default (org setting)"),
    (4,  "Claude Haiku 4.5"),
    (10, "Claude Sonnet 4.5"),
    (18, "Claude Sonnet 4.6"),
    (11, "Claude Opus 4.5"),
    (14, "Claude Opus 4.6"),
    (21, "Claude Opus 4.7"),
    (24, "Claude Opus 4.8"),
    (12, "Nova 2 Lite"),
    (15, "DeepSeek-R1"),
    (16, "MiniMax M2"),
    (19, "MiniMax M2.5"),
    (17, "Kimi K2 Thinking"),
    (22, "Kimi K2.5"),
    (20, "GLM 5"),
    (25, "GPT-5.4"),
    (26, "GPT-5.5"),
    (27, "Gemma 4 31B"),
    (28, "Gemma 4 26B-A4B"),
];

/// Display name for a model id, or "Default" when None/0.
pub fn model_name(id: Option<i64>) -> String {
    match id {
        None | Some(0) => "Default".to_string(),
        Some(id) => BUILTIN_MODELS
            .iter()
            .find(|(mid, _)| *mid == id)
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| format!("model:{id}")),
    }
}

pub struct ApiClient {
    profile: Profile,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(profile: Profile) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .danger_accept_invalid_certs(!profile.verify_tls)
            .user_agent("strobes-cli/0.1");
        // Dev fast path: pin the host to a static IP to skip slow (.local/mDNS) DNS.
        if let (Some(ip), Some(host), Ok(base)) =
            (profile.resolve_override(), profile.host(), profile.http_base())
        {
            let port = url::Url::parse(&base)
                .ok()
                .and_then(|u| u.port_or_known_default())
                .unwrap_or(80);
            builder = builder.resolve(&host, std::net::SocketAddr::new(ip, port));
        }
        let http = builder.build()?;
        Ok(Self { profile, http })
    }

    fn url(&self, path: &str) -> Result<String> {
        Ok(format!("{}{}", self.profile.http_base()?, path))
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value> {
        let url = self.url(path)?;
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("token {}", self.profile.master_key))
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("GET {path} -> HTTP {}: {}", status.as_u16(), trunc(&body, 300)));
        }
        Ok(serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
    }

    fn org_path(&self, suffix: &str) -> String {
        format!(
            "{}/organizations/{}{}",
            self.profile.api_prefix(),
            self.profile.org_id,
            suffix
        )
    }

    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        let v = self.get_json(&self.org_path("/cli/workspaces/")).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    pub async fn list_threads(&self, workspace_id: Option<&str>) -> Result<Vec<Thread>> {
        let mut path = self.org_path("/cli/threads/");
        if let Some(ws) = workspace_id {
            path.push_str(&format!("?workspace_id={ws}"));
        }
        let v = self.get_json(&path).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }


    /// Existing conversation + active-run state for a thread (for chat startup).
    pub async fn get_thread_history(&self, thread_id: &str, limit: u32) -> Result<ThreadHistory> {
        let path = self.org_path(&format!("/cli/threads/{thread_id}/messages/?limit={limit}"));
        let v = self.get_json(&path).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// Full-fidelity event history (messages, tools, tasks) ordered by seq.
    pub async fn get_thread_events(&self, thread_id: &str, after_seq: i64, limit: u32) -> Result<Vec<serde_json::Value>> {
        let path = self.org_path(&format!(
            "/cli/threads/{thread_id}/events/?after_seq={after_seq}&limit={limit}"
        ));
        let v = self.get_json(&path).await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub async fn list_workspace_files(&self, workspace_id: &str, recursive: bool) -> Result<Vec<WorkspaceFile_>> {
        let path = self.org_path(&format!(
            "/cli/workspaces/{workspace_id}/files/?recursive={recursive}"
        ));
        let v = self.get_json(&path).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    pub async fn list_workspace_findings(&self, workspace_id: &str) -> Result<Vec<Finding>> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/findings/?limit=500"));
        let v = self.get_json(&path).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    pub async fn list_workspace_approvals(&self, workspace_id: &str) -> Result<Vec<Approval>> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/approvals/?limit=500"));
        let v = self.get_json(&path).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// Upload one file's bytes to the workspace at `dest_path` (relative).
    pub async fn upload_workspace_file(
        &self,
        workspace_id: &str,
        dest_path: &str,
        content: Vec<u8>,
    ) -> Result<()> {
        let url = self.url(&self.org_path(&format!("/cli/workspaces/{workspace_id}/upload/")))?;
        let resp = self
            .http
            .post(&url)
            .query(&[("path", dest_path)])
            .header("Authorization", format!("token {}", self.profile.master_key))
            .header("Content-Type", "application/octet-stream")
            .body(content)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("upload failed: HTTP {}: {}", status.as_u16(), trunc(&body, 200)));
        }
        Ok(())
    }

    /// Download the workspace zip bytes (presigned URL → GET).
    pub async fn download_workspace_bytes(&self, workspace_id: &str) -> Result<Vec<u8>> {
        let url = self.workspace_download_url(workspace_id).await?;
        let bytes = reqwest::Client::new().get(&url).send().await?.bytes().await?;
        Ok(bytes.to_vec())
    }

    /// Presigned S3 URL to a zip of the workspace (for local download).
    pub async fn workspace_download_url(&self, workspace_id: &str) -> Result<String> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/download/"));
        let v = self.get_json(&path).await?;
        v.get("url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("no download url returned: {v}"))
    }

    /// AI credit usage totals, optionally scoped to a workspace and/or thread.
    pub async fn get_credits(
        &self,
        workspace_id: Option<&str>,
        thread_id: Option<&str>,
    ) -> Result<CreditsSummary> {
        let mut qs: Vec<String> = Vec::new();
        if let Some(w) = workspace_id {
            qs.push(format!("workspace_id={w}"));
        }
        if let Some(t) = thread_id {
            qs.push(format!("thread_id={t}"));
        }
        let mut path = self.org_path("/cli/credits/");
        if !qs.is_empty() {
            path.push('?');
            path.push_str(&qs.join("&"));
        }
        let v = self.get_json(&path).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    pub async fn create_thread(&self, title: &str, workspace_id: Option<&str>) -> Result<String> {
        let mut body = serde_json::json!({ "title": title });
        if let Some(w) = workspace_id {
            body["workspace_id"] = serde_json::json!(w);
        }
        let v = self.post_json(&self.org_path("/cli/threads/create/"), body).await?;
        v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string())
            .ok_or_else(|| anyhow!("create thread failed: {v}"))
    }

    pub async fn create_workspace(&self, name: &str) -> Result<(String, Option<String>)> {
        let body = serde_json::json!({ "name": name });
        let v = self.post_json(&self.org_path("/cli/workspaces/create/"), body).await?;
        let id = v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string())
            .ok_or_else(|| anyhow!("create workspace failed: {v}"))?;
        let setup = v.get("setup_thread_id").and_then(|x| x.as_str()).map(|s| s.to_string());
        Ok((id, setup))
    }

    async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let url = self.url(path)?;
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("token {}", self.profile.master_key))
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("POST {path} -> HTTP {}: {}", status.as_u16(), trunc(&text, 300)));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// Available slash commands for the org (native + skill-backed).
    pub async fn list_slash_commands(&self) -> Result<Vec<SlashCmd>> {
        let v = self.get_json(&self.org_path("/slash-commands/")).await?;
        let arr = v.get("commands").cloned().unwrap_or(serde_json::Value::Null);
        Ok(serde_json::from_value(arr).unwrap_or_default())
    }


    /// Cheap auth/connectivity check.
    pub async fn ping(&self) -> Result<()> {
        self.list_workspaces().await.map(|_| ())
    }

    // ── GraphQL (workflow API) ────────────────────────────────────────────────

    /// PATCH JSON to a REST path (MasterKey token auth). Returns the parsed body.
    async fn patch_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let url = self.url(path)?;
        let resp = self
            .http
            .patch(&url)
            .header("Authorization", format!("token {}", self.profile.master_key))
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("PATCH {path} -> HTTP {}: {}", status.as_u16(), trunc(&text, 300)));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    /// DELETE a REST path (MasterKey token auth). Returns the parsed body.
    async fn delete_req(&self, path: &str) -> Result<serde_json::Value> {
        let url = self.url(path)?;
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", format!("token {}", self.profile.master_key))
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("DELETE {path} -> HTTP {}: {}", status.as_u16(), trunc(&text, 300)));
        }
        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub async fn workflow_templates(&self) -> Result<Vec<WorkflowTemplate>> {
        let v = self.get_json(&self.org_path("/cli/workflow-templates/")).await?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    pub async fn workspace_workflow(
        &self,
        workspace_id: &str,
    ) -> Result<Option<WorkflowState>> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/workflow/"));
        let v = self.get_json(&path).await?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(v).ok())
    }

    // ── Template mutations ────────────────────────────────────────────────────

    pub async fn attach_workflow_template(
        &self,
        workspace_id: &str,
        template_slug: &str,
        variables: &serde_json::Value,
    ) -> Result<WorkflowState> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/workflow/"));
        let body = serde_json::json!({
            "template_slug": template_slug,
            "variables": variables,
        });
        let data = self.post_json(&path, body).await?;
        Ok(serde_json::from_value(data).unwrap_or_default())
    }

    pub async fn create_custom_workflow(
        &self,
        workspace_id: &str,
        name: &str,
        phases: &serde_json::Value,
        variables: &serde_json::Value,
    ) -> Result<WorkflowState> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/workflow/"));
        let body = serde_json::json!({
            "name": name,
            "phases": phases,
            "variables": variables,
        });
        let data = self.post_json(&path, body).await?;
        Ok(serde_json::from_value(data).unwrap_or_default())
    }

    pub async fn edit_custom_workflow(
        &self,
        workspace_id: &str,
        name: &str,
        phases: &serde_json::Value,
    ) -> Result<WorkflowState> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/workflow/"));
        let body = serde_json::json!({
            "name": name,
            "phases": phases,
        });
        let data = self.patch_json(&path, body).await?;
        Ok(serde_json::from_value(data).unwrap_or_default())
    }

    pub async fn save_workflow_as_template(
        &self,
        workspace_id: &str,
        name: &str,
        description: &str,
        icon: &str,
    ) -> Result<String> {
        let path = self.org_path(&format!(
            "/cli/workspaces/{workspace_id}/workflow/save-as-template/"
        ));
        let body = serde_json::json!({
            "name": name,
            "description": description,
            "icon": icon,
        });
        let data = self.post_json(&path, body).await?;
        Ok(data
            .get("templateSlug")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn delete_custom_workflow_template(&self, template_slug: &str) -> Result<bool> {
        let path = self.org_path(&format!("/cli/workflow-templates/{template_slug}/"));
        let data = self.delete_req(&path).await?;
        Ok(data
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    // ── Execution control mutations ───────────────────────────────────────────

    /// POST a workflow control action (pause/resume/cancel/restart/advance/...).
    async fn workflow_action(
        &self,
        workspace_id: &str,
        action: &str,
        body: serde_json::Value,
    ) -> Result<()> {
        let path = self.org_path(&format!(
            "/cli/workspaces/{workspace_id}/workflow/{action}/"
        ));
        self.post_json(&path, body).await?;
        Ok(())
    }

    pub async fn pause_workflow(&self, workspace_id: &str) -> Result<()> {
        self.workflow_action(workspace_id, "pause", serde_json::json!({})).await
    }

    pub async fn resume_workflow(&self, workspace_id: &str) -> Result<()> {
        self.workflow_action(workspace_id, "resume", serde_json::json!({})).await
    }

    pub async fn cancel_workflow(&self, workspace_id: &str) -> Result<()> {
        self.workflow_action(workspace_id, "cancel", serde_json::json!({})).await
    }

    pub async fn restart_workflow(&self, workspace_id: &str) -> Result<()> {
        self.workflow_action(workspace_id, "restart", serde_json::json!({})).await
    }

    pub async fn restart_workflow_from_phase(
        &self,
        workspace_id: &str,
        phase_key: &str,
    ) -> Result<()> {
        self.workflow_action(
            workspace_id,
            "restart-from-phase",
            serde_json::json!({ "phase_key": phase_key }),
        )
        .await
    }

    pub async fn advance_workflow_phase(&self, workspace_id: &str) -> Result<()> {
        self.workflow_action(workspace_id, "advance", serde_json::json!({})).await
    }

    pub async fn detach_workflow(&self, workspace_id: &str) -> Result<()> {
        let path = self.org_path(&format!("/cli/workspaces/{workspace_id}/workflow/"));
        self.delete_req(&path).await?;
        Ok(())
    }
}

// ── GraphQL type structs ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkflowTemplate {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, rename = "phaseCount")]
    pub phase_count: i64,
    #[serde(default, rename = "requiredVariables")]
    pub required_variables: Vec<String>,
    #[serde(default)]
    pub phases: Vec<WorkflowTemplatePhase>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkflowTemplatePhase {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub order: i64,
    #[serde(default, rename = "taskCount")]
    pub task_count: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkflowState {
    #[serde(default, rename = "workflowId")]
    pub workflow_id: String,
    #[serde(default, rename = "templateSlug")]
    pub template_slug: Option<String>,
    #[serde(default, rename = "templateVersion")]
    pub template_version: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default, rename = "currentPhaseKey")]
    pub current_phase_key: Option<String>,
    #[serde(default, rename = "totalTasks")]
    pub total_tasks: i64,
    #[serde(default, rename = "completedTasks")]
    pub completed_tasks: i64,
    #[serde(default, rename = "startedAt")]
    pub started_at: Option<String>,
    #[serde(default, rename = "completedAt")]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub phases: Vec<WorkflowStatePhase>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkflowStatePhase {
    #[serde(default, rename = "phaseKey")]
    pub phase_key: String,
    #[serde(default, rename = "phaseName")]
    pub phase_name: String,
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub status: String,
    #[serde(default, rename = "startedAt")]
    pub started_at: Option<String>,
    #[serde(default, rename = "completedAt")]
    pub completed_at: Option<String>,
}

// ── GraphQL input literal serialisation ──────────────────────────────────────

/// Recursively convert a `serde_json::Value` to an inline GraphQL input literal.
/// Strings are serialized with proper escaping (block strings for multi-line content).
pub fn json_to_gql_input(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) if map.is_empty() => "{}".to_string(),
        serde_json::Value::Object(map) => {
            let pairs: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k}: {}", json_to_gql_input(v)))
                .collect();
            format!("{{ {} }}", pairs.join(", "))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(json_to_gql_input).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::String(s) => gql_string(s),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
    }
}

/// Encode a string as a GraphQL string literal.
/// Uses block strings (`"""..."""`) for multi-line or special-char content
/// to avoid character-by-character escaping; falls back to regular escaping
/// when the content itself contains `"""`.
pub fn gql_string(s: &str) -> String {
    if s.contains("\"\"\"") {
        // Block strings can't contain """; fall back to regular escaped string.
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t");
        format!("\"{escaped}\"")
    } else {
        format!("\"\"\"{}\"\"\"", s)
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
