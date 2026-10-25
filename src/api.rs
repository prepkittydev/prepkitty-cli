//! HTTP client for the PrepKitty web API.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::config::{self, Credentials};

pub const CLIENT_ID: &str = "prepkitty-cli";

pub struct Api {
    pub base: String,
    token: Option<String>,
    http: Client,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KittySummary {
    pub name: String,
    pub mood: String,
    pub stage: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub name: String,
    pub username: Option<String>,
    pub level: u32,
    pub xp_to_next: u32,
    pub treats: u32,
    pub streak: u32,
    pub xp_today: u32,
    pub daily_goal: u32,
    pub kitty: Option<KittySummary>,
    pub onboarded: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub pack_hash: String,
    pub pack_url: String,
    pub web_url: String,
    pub completed: bool,
    pub current_stage: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextItem {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub xp: u32,
    pub web_url: String,
}

#[derive(Debug, Deserialize)]
pub struct NextItems {
    pub items: Vec<NextItem>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Submission<'a> {
    pub item_id: &'a str,
    pub stage: u32,
    pub pack_hash: &'a str,
    pub commit_sha: &'a str,
    pub cli_version: &'a str,
    pub passed: bool,
    pub results: &'a [TestResult],
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reward {
    pub xp: u32,
    pub treats: u32,
    pub level_before: u32,
    pub level_after: u32,
    pub streak: u32,
    pub streak_extended: bool,
    pub daily_goal_reached: bool,
}

#[derive(Debug, Deserialize)]
pub struct Badge {
    pub name: String,
    pub emoji: String,
}

#[derive(Debug, Deserialize)]
pub struct UnlockedItem {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct NextStage {
    pub stage: u32,
    pub title: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub passed: bool,
    pub first_completion: bool,
    pub reward: Option<Reward>,
    pub badges: Vec<Badge>,
    pub items: Vec<UnlockedItem>,
    pub project_complete: Option<bool>,
    pub next_stage: Option<NextStage>,
}

#[derive(Debug, Deserialize)]
pub struct SubmitResponse {
    pub outcome: Outcome,
    pub summary: Summary,
}

#[derive(Debug, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub interval: Option<u64>,
    pub expires_in: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TokenResponse {
    Token {
        access_token: String,
    },
    Error {
        error: String,
        error_description: Option<String>,
    },
}

impl Api {
    pub fn new(creds: Option<&Credentials>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("prepkitty-cli/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: config::api_base(creds),
            token: creds.map(|c| c.token.clone()),
            http,
        })
    }

    /// Client for commands that need a signed-in user.
    pub fn authed() -> Result<Self> {
        match config::load_credentials()? {
            Some(c) => Self::new(Some(&c)),
            None => bail!("You're not signed in. Run `prepkitty login` first."),
        }
    }

    pub fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}{}", self.base, path)
        }
    }

    fn auth(&self, rb: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    fn send(&self, rb: RequestBuilder) -> Result<Response> {
        rb.send().map_err(|e| {
            if e.is_connect() {
                anyhow!(
                    "Couldn't reach PrepKitty at {}. Check your connection (or PREPKITTY_API).",
                    self.base
                )
            } else {
                anyhow!(e)
            }
        })
    }

    fn json<T: DeserializeOwned>(&self, resp: Response) -> Result<T> {
        let status = resp.status();
        let text = resp.text()?;
        if status.is_success() {
            return serde_json::from_str(&text)
                .with_context(|| format!("unexpected response: {text}"));
        }
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {status}"));
        if status.as_u16() == 401 {
            bail!("{msg}");
        }
        bail!("{msg}")
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self.send(self.auth(self.http.get(self.url(path))))?;
        self.json(resp)
    }

    pub fn post<T: DeserializeOwned, B: Serialize>(&self, path: &str, body: &B) -> Result<T> {
        let resp = self.send(self.auth(self.http.post(self.url(path)).json(body)))?;
        self.json(resp)
    }

    pub fn me(&self) -> Result<Summary> {
        self.get("/api/cli/me")
    }

    pub fn item(&self, id: &str, start: bool) -> Result<Item> {
        self.get(&format!(
            "/api/cli/items/{id}{}",
            if start { "?start=1" } else { "" }
        ))
    }

    pub fn next(&self) -> Result<NextItems> {
        self.get("/api/cli/next")
    }

    pub fn submit(&self, s: &Submission) -> Result<SubmitResponse> {
        self.post("/api/cli/submissions", s)
    }

    pub fn download(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.send(self.http.get(self.url(url)))?;
        if !resp.status().is_success() {
            bail!("downloading the test pack failed (HTTP {})", resp.status());
        }
        Ok(resp.bytes()?.to_vec())
    }

    // ---- Device authorization (RFC 8628) via Better Auth ----

    pub fn device_code(&self) -> Result<DeviceCode> {
        self.post(
            "/api/auth/device/code",
            &serde_json::json!({ "client_id": CLIENT_ID }),
        )
    }

    pub fn device_token(&self, device_code: &str) -> Result<TokenResponse> {
        let resp = self.send(self.http.post(self.url("/api/auth/device/token")).json(
            &serde_json::json!({
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                "device_code": device_code,
                "client_id": CLIENT_ID,
            }),
        ))?;
        let text = resp.text()?;
        serde_json::from_str(&text).with_context(|| format!("unexpected token response: {text}"))
    }
}
