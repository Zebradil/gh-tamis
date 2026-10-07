mod github;
mod owners;
mod rules;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use clap::Parser;
use serde::Deserialize;

use github::{Client, Notification, Pull, Team, User};
use rules::{Facts, RuleSpec, Rules, Tier};

/// Rule-based triage for GitHub notifications. Prints what it would do unless `--apply` is given.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/gh-tamis/config.toml]
    #[arg(long)]
    config: Option<PathBuf>,
    /// Mark threads read or done on GitHub according to their tier.
    #[arg(long)]
    apply: bool,
}

#[derive(Deserialize)]
struct Config {
    /// Author login suffixes that mark a bot, on top of GitHub's own `Bot` user type.
    #[serde(default = "default_bot_suffixes")]
    bot_suffixes: Vec<String>,
    /// Repos always owned (`owner/repo` or `owner/*`), checked after `not_owned`.
    #[serde(default)]
    owned: Vec<String>,
    /// Repos never owned, whatever CODEOWNERS says.
    #[serde(default)]
    not_owned: Vec<String>,
    rule: Vec<RuleSpec>,
}

fn default_bot_suffixes() -> Vec<String> {
    vec!["[bot]".into()]
}

const WORKERS: usize = 8;
const CODEOWNERS_PATHS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(default_config_path);
    let mut config: Config = toml::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    let rules = Rules::compile(std::mem::take(&mut config.rule))?;

    let gh = Client::new()?;
    let me = gh.get_json::<User>("/user")?.context("GET /user")?.login;
    let my_teams: HashSet<String> = gh
        .get_all::<Team>("/user/teams?per_page=100")?
        .into_iter()
        .map(|t| format!("{}/{}", t.organization.login, t.slug).to_lowercase())
        .collect();
    let threads: Vec<Notification> = gh.get_all("/notifications?all=true&per_page=50")?;

    let pr_urls: Vec<&str> = unique(
        threads
            .iter()
            .filter(|n| n.subject.kind == "PullRequest")
            .filter_map(|n| n.subject.url.as_deref()),
    );
    let mut pulls: HashMap<&str, Pull> = HashMap::new();
    for (url, pull) in pr_urls
        .iter()
        .zip(par_map(&pr_urls, |url| gh.get_json::<Pull>(url)))
    {
        if let Some(pull) = pull? {
            pulls.insert(url, pull);
        }
    }

    let repos: Vec<&str> = unique(threads.iter().map(|n| n.repository.full_name.as_str()));
    let mut owned: HashMap<&str, bool> = HashMap::new();
    for (repo, o) in repos.iter().zip(par_map(&repos, |repo| {
        is_owned(&gh, &config, &my_teams, repo)
    })) {
        owned.insert(repo, o?);
    }

    let mut counts: BTreeMap<Tier, usize> = BTreeMap::new();
    for n in &threads {
        let pull = n.subject.url.as_deref().and_then(|u| pulls.get(u));
        let facts = facts(
            n,
            pull,
            &me,
            owned[n.repository.full_name.as_str()],
            &config.bot_suffixes,
        );
        let unread = if n.unread { "* " } else { "" };
        let Some(rule) = rules.classify(&facts)? else {
            println!(
                "{:10} {:20} {:45} {unread}{}",
                "-", "-", facts.repo, facts.title
            );
            continue;
        };
        *counts.entry(rule.tier).or_default() += 1;
        println!(
            "{:10} {:20} {:45} {unread}{}",
            rule.tier.as_str(),
            rule.name,
            facts.repo,
            facts.title
        );
        if cli.apply {
            apply(&gh, n, rule.tier)?;
        }
    }
    let summary: Vec<String> = counts
        .iter()
        .map(|(t, c)| format!("{} {c}", t.as_str()))
        .collect();
    eprintln!("{} threads: {}", threads.len(), summary.join(", "));
    Ok(())
}

fn apply(gh: &Client, n: &Notification, tier: Tier) -> Result<()> {
    match tier {
        // ponytail: digest threads stay untouched until the digest queue exists (phase 3)
        Tier::Notify | Tier::Digest => Ok(()),
        Tier::List if n.unread => gh.mark_read(&n.id),
        Tier::List => Ok(()),
        Tier::OnDemand | Tier::Clear => gh.mark_done(&n.id),
    }
}

fn facts(
    n: &Notification,
    pull: Option<&Pull>,
    me: &str,
    owned: bool,
    bot_suffixes: &[String],
) -> Facts {
    let repo = &n.repository.full_name;
    let mut f = Facts {
        reason: n.reason.clone(),
        kind: n.subject.kind.clone(),
        repo: repo.clone(),
        owner: repo
            .split_once('/')
            .map_or(repo.as_str(), |(o, _)| o)
            .to_owned(),
        title: n.subject.title.clone(),
        unread: n.unread,
        owned,
        ..Default::default()
    };
    if let Some(p) = pull {
        f.author = p.user.login.clone();
        f.bot = p.user.kind == "Bot"
            || bot_suffixes
                .iter()
                .any(|s| p.user.login.ends_with(s.as_str()));
        f.state = if p.merged {
            "merged".into()
        } else {
            p.state.clone()
        };
        f.draft = p.draft;
        f.direct = p
            .requested_reviewers
            .iter()
            .any(|u| u.login.eq_ignore_ascii_case(me));
        f.requested_teams = p.requested_teams.iter().map(|t| t.slug.clone()).collect();
    }
    f
}

fn is_owned(gh: &Client, config: &Config, my_teams: &HashSet<String>, repo: &str) -> Result<bool> {
    if config.not_owned.iter().any(|p| owners::matches(p, repo)) {
        return Ok(false);
    }
    if config.owned.iter().any(|p| owners::matches(p, repo)) {
        return Ok(true);
    }
    for path in CODEOWNERS_PATHS {
        if let Some(text) = gh.get_raw(&format!("/repos/{repo}/contents/{path}"))? {
            return Ok(
                owners::primary_team(&text).is_some_and(|t| my_teams.contains(&t.to_lowercase()))
            );
        }
    }
    Ok(false)
}

fn unique<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = HashSet::new();
    items.filter(|i| seen.insert(*i)).collect()
}

/// `f` over `items` on a few threads, results in input order. API calls are latency-bound.
fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let chunk = items.len().div_ceil(WORKERS).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker panicked"))
            .collect()
    })
}

fn default_config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("gh-tamis/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_compiles() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let rules = Rules::compile(config.rule).unwrap();
        let release = Facts {
            owned: true,
            bot: true,
            title: "chore(main): release 1.2.0".into(),
            ..Default::default()
        };
        assert_eq!(
            rules.classify(&release).unwrap().unwrap().name,
            "own-release"
        );
        let helm = Facts {
            title: "Update Helm release app to v8".into(),
            ..release
        };
        assert_eq!(rules.classify(&helm).unwrap().unwrap().name, "bot-owned");
    }
}
