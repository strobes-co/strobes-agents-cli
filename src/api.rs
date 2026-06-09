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

pub struct ApiClient {
    profile: Profile,
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new(profile: Profile) -> Result<Self> {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(!profile.verify_tls)
            .user_agent("strobes-cli/0.1")
            .build()?;
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
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
