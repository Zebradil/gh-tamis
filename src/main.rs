mod github;
mod owners;
mod rules;
mod state;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};
use clap::Parser;
use serde::Deserialize;

use github::{Client, Notification, Pull, Team, User};
use rules::{Facts, RuleSpec, Rules, Tier};
use state::{Owned, State, Thread};

/// Rule-based triage for GitHub notifications. Handles threads updated since the last run and
/// prints what it would do unless `--apply` is given.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/gh-tamis/config.toml]
    #[arg(long)]
    config: Option<PathBuf>,
    /// State file [default: $XDG_STATE_HOME/gh-tamis/state.json]
    #[arg(long)]
    state: Option<PathBuf>,
    /// Mark threads read or done on GitHub according to their tier.
    #[arg(long)]
    apply: bool,
    /// Handle every thread in the inbox, not only those updated since the last run.
    #[arg(long)]
    full: bool,
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
    /// Run for each new update of an unread `notify` thread. Placeholders: {title}, {repo},
    /// {url}, {rule}, {reason}.
    #[serde(default)]
    notify_command: Vec<String>,
    rule: Vec<RuleSpec>,
}

fn default_bot_suffixes() -> Vec<String> {
    vec!["[bot]".into()]
}

const WORKERS: usize = 8;
/// Re-asks for threads this far before the last run, in case GitHub files a notification late.
/// Already seen updates are skipped by `updated_at`.
const SINCE_OVERLAP: u64 = 10 * 60;
/// launchd's interval and `X-Poll-Interval` are equal in practice; without slack, jitter would
/// skip every other run.
const POLL_SLACK: u64 = 5;
const OWNED_TTL: u64 = 24 * 60 * 60;
const KEEP_THREADS: u64 = 30 * 24 * 60 * 60;
const CODEOWNERS_PATHS: [&str; 3] = [".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli
        .config
        .unwrap_or_else(|| xdg_dir("XDG_CONFIG_HOME", ".config").join("gh-tamis/config.toml"));
    let mut config: Config = toml::from_str(
        &std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    let rules = Rules::compile(std::mem::take(&mut config.rule))?;

    let state_path = cli
        .state
        .unwrap_or_else(|| xdg_dir("XDG_STATE_HOME", ".local/state").join("gh-tamis/state.json"));
    let loaded = State::load(&state_path)?;
    // The first run only records the inbox; notifying about all of it would be a flood.
    let seed = loaded.is_none();
    let mut state = loaded.unwrap_or_default();
    let start = state::now();
    if !cli.full && start + POLL_SLACK < state.next_poll_at {
        return Ok(());
    }

    let gh = Client::new()?;
    let incremental = !cli.full && !seed;
    let poll = gh.poll_notifications(
        state.since.as_deref().filter(|_| incremental),
        state.last_modified.as_deref().filter(|_| incremental),
    )?;
    state.next_poll_at = start + poll.interval;
    let Some(threads) = poll.threads else {
        return state.save(&state_path);
    };
    state.last_modified = poll.last_modified;
    state.since = Some(state::iso(start.saturating_sub(SINCE_OVERLAP)));

    let updated = |n: &Notification| {
        state
            .threads
            .get(&n.id)
            .is_none_or(|t| t.updated_at != n.updated_at)
    };
    let threads: Vec<(Notification, bool)> = threads
        .into_iter()
        .map(|n| {
            let u = updated(&n);
            (n, u)
        })
        .filter(|(_, u)| *u || cli.full)
        .collect();
    if threads.is_empty() {
        return state.save(&state_path);
    }

    let me = gh.get_json::<User>("/user")?.context("GET /user")?.login;
    let pr_urls: Vec<&str> = unique(
        threads
            .iter()
            .filter(|(n, _)| n.subject.kind == "PullRequest")
            .filter_map(|(n, _)| n.subject.url.as_deref()),
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

    let mut owned: HashMap<&str, bool> = HashMap::new();
    let mut lookup = Vec::new();
    for repo in unique(threads.iter().map(|(n, _)| n.repository.full_name.as_str())) {
        if let Some(o) = configured_owned(&config, repo) {
            owned.insert(repo, o);
        } else if let Some(c) = state
            .owned
            .get(repo)
            .filter(|c| start < c.checked_at + OWNED_TTL)
        {
            owned.insert(repo, c.owned);
        } else {
            lookup.push(repo);
        }
    }
    if !lookup.is_empty() {
        let my_teams: HashSet<String> = gh
            .get_all::<Team>("/user/teams?per_page=100")?
            .into_iter()
            .map(|t| format!("{}/{}", t.organization.login, t.slug).to_lowercase())
            .collect();
        for (repo, o) in lookup.iter().zip(par_map(&lookup, |repo| {
            codeowners_owned(&gh, &my_teams, repo)
        })) {
            let o = o?;
            owned.insert(repo, o);
            state.owned.insert(
                repo.to_string(),
                Owned {
                    owned: o,
                    checked_at: start,
                },
            );
        }
    }

    let mut counts: BTreeMap<Tier, usize> = BTreeMap::new();
    for (n, updated) in &threads {
        let pull = n.subject.url.as_deref().and_then(|u| pulls.get(u));
        let facts = facts(
            n,
            pull,
            &me,
            owned[n.repository.full_name.as_str()],
            &config.bot_suffixes,
        );
        let rule = rules.classify(&facts)?;
        let unread = if n.unread { "* " } else { "" };
        println!(
            "{:10} {:20} {:45} {unread}{}",
            rule.map_or("-", |r| r.tier.as_str()),
            rule.map_or("-", |r| r.name.as_str()),
            facts.repo,
            facts.title
        );
        state.threads.insert(
            n.id.clone(),
            Thread {
                updated_at: n.updated_at.clone(),
                tier: rule.map(|r| r.tier),
                rule: rule.map(|r| r.name.clone()),
            },
        );
        let Some(rule) = rule else { continue };
        *counts.entry(rule.tier).or_default() += 1;
        if cli.apply {
            apply(&gh, n, rule.tier)?;
        }
        if rule.tier == Tier::Notify && n.unread && *updated && !seed {
            // A failed notification must not lose the run's state; the next update retries.
            if let Err(e) = notify(&config.notify_command, n, pull, &rule.name) {
                eprintln!("notify {}: {e:#}", n.id);
            }
        }
    }

    let cutoff = state::iso(start.saturating_sub(KEEP_THREADS));
    state.threads.retain(|_, t| t.updated_at >= cutoff);
    state.save(&state_path)?;

    let summary: Vec<String> = counts
        .iter()
        .map(|(t, c)| format!("{} {c}", t.as_str()))
        .collect();
    eprintln!(
        "{} {} threads: {}",
        state::iso(start),
        threads.len(),
        summary.join(", ")
    );
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

fn notify(command: &[String], n: &Notification, pull: Option<&Pull>, rule: &str) -> Result<()> {
    let Some((program, args)) = command.split_first() else {
        return Ok(());
    };
    let repo = &n.repository.full_name;
    let url = web_url(n, pull);
    // {title} goes last: it is the only value that might contain a placeholder itself.
    let args = args.iter().map(|a| {
        a.replace("{repo}", repo)
            .replace("{url}", &url)
            .replace("{rule}", rule)
            .replace("{reason}", &n.reason)
            .replace("{title}", &n.subject.title)
    });
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("run {program}"))?;
    anyhow::ensure!(status.success(), "{program}: {status}");
    Ok(())
}

/// Where a click on the notification should land.
fn web_url(n: &Notification, pull: Option<&Pull>) -> String {
    let repo = &n.repository.full_name;
    if let Some(p) = pull {
        return p.html_url.clone();
    }
    match (n.subject.kind.as_str(), n.subject.url.as_deref()) {
        ("Issue", Some(url)) => {
            url.replacen("https://api.github.com/repos/", "https://github.com/", 1)
        }
        ("CheckSuite", _) => format!("https://github.com/{repo}/actions"),
        _ => format!("https://github.com/{repo}"),
    }
}

/// Ownership forced by config, checked before CODEOWNERS.
fn configured_owned(config: &Config, repo: &str) -> Option<bool> {
    if config.not_owned.iter().any(|p| owners::matches(p, repo)) {
        return Some(false);
    }
    if config.owned.iter().any(|p| owners::matches(p, repo)) {
        return Some(true);
    }
    None
}

fn codeowners_owned(gh: &Client, my_teams: &HashSet<String>, repo: &str) -> Result<bool> {
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

/// `$var`, or `$HOME/fallback` when unset.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback)
    })
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
