use std::process::Command;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use ureq::Agent;
use ureq::http::Response;

const API: &str = "https://api.github.com";

#[derive(Debug, Deserialize)]
pub struct Notification {
    pub id: String,
    pub unread: bool,
    pub reason: String,
    pub subject: Subject,
    pub repository: Repository,
}

#[derive(Debug, Deserialize)]
pub struct Subject {
    pub title: String,
    /// API URL of the PR, issue or release; absent for check suites.
    pub url: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub full_name: String,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub login: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct Pull {
    pub user: User,
    pub state: String,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub requested_teams: Vec<TeamRef>,
    #[serde(default)]
    pub requested_reviewers: Vec<User>,
}

#[derive(Debug, Deserialize)]
pub struct TeamRef {
    pub slug: String,
}

#[derive(Debug, Deserialize)]
pub struct Team {
    pub slug: String,
    pub organization: User,
}

pub struct Client {
    agent: Agent,
    auth: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .user_agent("gh-tamis")
            .build()
            .into();
        Ok(Self {
            agent,
            auth: format!("Bearer {}", token()?),
        })
    }

    fn get(&self, url: &str, accept: &str) -> Result<Option<Response<ureq::Body>>> {
        let url = if url.starts_with("https://") {
            url.to_owned()
        } else {
            format!("{API}{url}")
        };
        let res = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .with_context(|| format!("GET {url}"))?;
        match res.status().as_u16() {
            200..=299 => Ok(Some(res)),
            404 => Ok(None),
            s => bail!("GET {url}: HTTP {s}"),
        }
    }

    pub fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<Option<T>> {
        let Some(mut res) = self.get(url, "application/vnd.github+json")? else {
            return Ok(None);
        };
        Ok(Some(
            res.body_mut()
                .read_json()
                .with_context(|| format!("decode {url}"))?,
        ))
    }

    /// File contents from the contents API, or `None` if the file doesn't exist.
    pub fn get_raw(&self, url: &str) -> Result<Option<String>> {
        let Some(mut res) = self.get(url, "application/vnd.github.raw+json")? else {
            return Ok(None);
        };
        Ok(Some(res.body_mut().read_to_string()?))
    }

    /// Every page of a list endpoint, following `Link: rel="next"`.
    pub fn get_all<T: DeserializeOwned>(&self, url: &str) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(url.to_owned());
        while let Some(url) = next {
            let mut res = self
                .get(&url, "application/vnd.github+json")?
                .with_context(|| format!("GET {url}: not found"))?;
            next = res
                .headers()
                .get("link")
                .and_then(|v| v.to_str().ok())
                .and_then(next_link);
            out.extend(
                res.body_mut()
                    .read_json::<Vec<T>>()
                    .with_context(|| format!("decode {url}"))?,
            );
        }
        Ok(out)
    }

    pub fn mark_read(&self, thread: &str) -> Result<()> {
        self.send(
            self.agent
                .patch(format!("{API}/notifications/threads/{thread}"))
                .header("Authorization", &self.auth)
                .send_empty(),
            thread,
        )
    }

    pub fn mark_done(&self, thread: &str) -> Result<()> {
        self.send(
            self.agent
                .delete(format!("{API}/notifications/threads/{thread}"))
                .header("Authorization", &self.auth)
                .call(),
            thread,
        )
    }

    fn send(&self, res: Result<Response<ureq::Body>, ureq::Error>, thread: &str) -> Result<()> {
        let status = res.with_context(|| format!("thread {thread}"))?.status();
        if !status.is_success() {
            bail!("thread {thread}: HTTP {status}");
        }
        Ok(())
    }
}

fn next_link(header: &str) -> Option<String> {
    header
        .split(',')
        .find(|part| part.contains(r#"rel="next""#))
        .and_then(|part| Some(part.split_once('<')?.1.split_once('>')?.0.to_owned()))
}

/// Same lookup order as `gh`: environment first, then the `gh` login.
fn token() -> Result<String> {
    for var in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(var)
            && !t.is_empty()
        {
            return Ok(t);
        }
    }
    let out = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("run `gh auth token`")?;
    if !out.status.success() {
        bail!(
            "no GH_TOKEN and `gh auth token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_next_link() {
        let h = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#;
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        assert_eq!(next_link(r#"<https://a/x?page=1>; rel="prev""#), None);
    }
}
