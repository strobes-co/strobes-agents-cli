//! Direct containerized-analyzer scans for `strobes cicd <type>`.
//!
//! Mints short-lived ECR pull credentials from the analyzer-registry ("maze")
//! service, pulls the right scanner image on the fly, runs it, and reads back
//! its native JSON output. No LLM call anywhere in this path — that's what
//! distinguishes it from the agent-driven `strobes ci <type>` family. (An
//! opt-in `--ai-triage` step, implemented in main.rs, can still hand the
//! deterministic findings to an agent afterward.)
//!
//! Each analyzer's CLI interface genuinely differs (confirmed by hand this
//! session) — flag names, whether the target needs read-write access, extra
//! required flags — so callers in main.rs build the exact `docker run` args
//! per scan type; this module only handles credentials, the registry, and
//! running/reading back the container.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Profile;

/// Which scan type the user asked for, and which analyzer image serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanType {
    Sast,
    Sca,
    Container,
    Iac,
    Dast,
}

impl ScanType {
    /// ECR repository path under the tenant's analyzer registry.
    pub fn repo(&self) -> &'static str {
        match self {
            ScanType::Sast => "analyzers/strobessastanalyzer",
            ScanType::Sca => "analyzers/dependency",
            ScanType::Container => "analyzers/trivyanalyzer",
            ScanType::Iac => "analyzers/checkovanalyzer",
            ScanType::Dast => "analyzers/zapanalyzer",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ScanType::Sast => "sast",
            ScanType::Sca => "sca",
            ScanType::Container => "container",
            ScanType::Iac => "iac",
            ScanType::Dast => "dast",
        }
    }
}

/// Short-lived ECR pull credentials minted by the analyzer-registry service.
#[derive(Debug, Clone, Deserialize)]
pub struct AnalyzerCreds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
    #[serde(default)]
    pub expiration: String,
    pub region: String,
    pub registry_url: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub repository_filter: String,
}

/// `POST {analyzer_registry_url}/organizations/{org}/analyzer-registry/credentials/`.
/// Same Authorization/JSON conventions `ApiClient` uses against `base_url`,
/// but against the separate analyzer-registry host — see
/// `Profile::analyzer_registry_base` for why the two aren't derived from
/// each other.
pub async fn mint_credentials(profile: &Profile, duration_secs: u64) -> Result<AnalyzerCreds> {
    let base = profile.analyzer_registry_base()?;
    let url = format!(
        "{base}{}/organizations/{}/analyzer-registry/credentials/",
        profile.api_prefix(),
        profile.org_id,
    );
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(!profile.verify_tls)
        .user_agent("strobes-cli/0.1")
        .build()?;
    let resp = http
        .post(&url)
        .header("Authorization", format!("token {}", profile.master_key))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "duration_seconds": duration_secs }))
        .send()
        .await
        .context("requesting analyzer-registry credentials")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "analyzer-registry credentials request failed: HTTP {} {}",
            status.as_u16(),
            text.chars().take(300).collect::<String>()
        ));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("parsing analyzer-registry credentials response: {text}"))
}

/// Exchange the maze-minted STS credentials for an ECR docker-login password
/// (via `ecr:GetAuthorizationToken`) and log `docker` into the registry.
/// Uses the AWS SDK directly rather than shelling out to an `aws` CLI the
/// user may not have installed — only `docker` itself is a hard requirement,
/// matching `strobes ci container`'s existing precedent.
pub async fn ecr_login(creds: &AnalyzerCreds) -> Result<()> {
    let aws_creds = aws_credential_types::Credentials::new(
        creds.access_key_id.clone(),
        creds.secret_access_key.clone(),
        Some(creds.session_token.clone()),
        None,
        "strobes-analyzer-registry",
    );
    let conf = aws_sdk_ecr::config::Config::builder()
        .behavior_version(aws_sdk_ecr::config::BehaviorVersion::latest())
        .region(aws_sdk_ecr::config::Region::new(creds.region.clone()))
        .credentials_provider(aws_creds)
        .build();
    let client = aws_sdk_ecr::Client::from_conf(conf);
    let resp = client
        .get_authorization_token()
        .send()
        .await
        .context("ECR GetAuthorizationToken failed")?;
    let auth = resp
        .authorization_data()
        .first()
        .ok_or_else(|| anyhow!("ECR returned no authorization data"))?;
    let token = auth
        .authorization_token()
        .ok_or_else(|| anyhow!("ECR authorization data missing token"))?;
    let decoded_bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, token)
        .context("decoding ECR authorization token")?;
    let decoded = String::from_utf8(decoded_bytes).context("ECR authorization token is not UTF-8")?;
    let (_user, password) = decoded
        .split_once(':')
        .ok_or_else(|| anyhow!("unexpected ECR authorization token format"))?;

    let mut child = Command::new("docker")
        .args(["login", "--username", "AWS", "--password-stdin", &creds.registry_url])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `docker login` — is Docker installed?")?;
    {
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin.write_all(password.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        return Err(anyhow!("docker login failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}

/// `docker pull {registry_url}/{repo}:{tag}` — returns the full image ref.
pub async fn pull_image(creds: &AnalyzerCreds, scan_type: ScanType, tag: &str) -> Result<String> {
    let image_ref = format!("{}/{}:{tag}", creds.registry_url, scan_type.repo());
    let out = Command::new("docker")
        .args(["pull", &image_ref])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run `docker pull` — is Docker installed and running?")?;
    if !out.status.success() {
        return Err(anyhow!(
            "docker pull {image_ref} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(image_ref)
}

/// Ensure the target git repo has an `origin` remote configured. The
/// dependency (SCA) analyzer reads `repo.remote().url` to build asset
/// metadata and hard-fails with `ValueError: Remote named 'origin' didn't
/// exist` without one — confirmed by hand this session against a freshly
/// `git init`'d directory. Adds a clearly-fake placeholder rather than
/// failing the whole scan over metadata the analyzer doesn't actually need
/// for finding vulnerabilities.
pub fn ensure_origin_remote(dir: &Path) -> Result<()> {
    let has_origin = std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if has_origin {
        return Ok(());
    }
    let status = std::process::Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "remote",
            "add",
            "origin",
            "https://example.invalid/strobes-cicd/local-scan.git",
        ])
        .status()
        .context("failed to add a placeholder git origin remote")?;
    if !status.success() {
        return Err(anyhow!("`git remote add origin` failed in {}", dir.display()));
    }
    Ok(())
}

/// Result of running an analyzer container: whatever it wrote to stderr/stdout
/// (for the text-mode summary and for detecting known-bug signatures — see
/// the checkov IaC handling in main.rs) plus its parsed output JSON, if any.
pub struct AnalyzerRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_ok: bool,
    pub output_json: Option<serde_json::Value>,
}

/// Run `docker run --rm <docker_args>`, then read back `{out_dir}/{out_file}`.
/// `docker_args` is everything AFTER `run --rm` — mounts, the image ref, and
/// the analyzer's own CLI args — built by the caller in main.rs, since each
/// analyzer's interface differs too much to generalize here (see the plan's
/// per-analyzer spec table).
pub async fn run_container(docker_args: &[String], out_dir: &Path, out_file: &str) -> Result<AnalyzerRun> {
    let mut args: Vec<String> = vec!["run".into(), "--rm".into()];
    args.extend(docker_args.iter().cloned());
    let out = Command::new("docker")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run `docker run` — is Docker installed and running?")?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let output_path = out_dir.join(out_file);
    let output_json = std::fs::read_to_string(&output_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    Ok(AnalyzerRun {
        stdout,
        stderr,
        exit_ok: out.status.success(),
        output_json,
    })
}
