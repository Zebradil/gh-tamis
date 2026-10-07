use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use cel::{Context, Env, Program, Value};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    Notify,
    Digest,
    List,
    OnDemand,
    Clear,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Notify => "notify",
            Tier::Digest => "digest",
            Tier::List => "list",
            Tier::OnDemand => "on-demand",
            Tier::Clear => "clear",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RuleSpec {
    pub name: String,
    pub when: String,
    pub tier: Tier,
}

/// What a rule sees about one notification thread. Field names are the CEL variable names.
#[derive(Debug, Default, Serialize)]
pub struct Facts {
    pub reason: String,
    /// Subject type: `PullRequest`, `Issue`, `Release`, `CheckSuite`, …
    pub kind: String,
    pub repo: String,
    pub owner: String,
    pub title: String,
    pub unread: bool,
    /// PR author login; empty for subjects without one.
    pub author: String,
    pub bot: bool,
    /// `open`, `merged` or `closed` for PRs; empty otherwise.
    pub state: String,
    pub draft: bool,
    /// I am requested as a reviewer by name, not through a team.
    pub direct: bool,
    pub owned: bool,
    pub requested_teams: Vec<String>,
}

pub struct Rule {
    pub name: String,
    pub tier: Tier,
    program: Program,
}

pub struct Rules {
    env: Arc<Env>,
    rules: Vec<Rule>,
}

impl Rules {
    pub fn compile(specs: Vec<RuleSpec>) -> Result<Self> {
        if specs.is_empty() {
            bail!("no rules configured");
        }
        let env = Arc::new(Env::stdlib());
        let rules = specs
            .into_iter()
            .map(|s| {
                let program = env
                    .compile(&s.when)
                    .map_err(|e| anyhow!("rule {:?}: {e}", s.name))?;
                Ok(Rule {
                    name: s.name,
                    tier: s.tier,
                    program,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Self { env, rules })
    }

    /// First matching rule wins. A thread no rule matches is left alone.
    pub fn classify(&self, facts: &Facts) -> Result<Option<&Rule>> {
        let mut ctx = Context::with_env(Arc::clone(&self.env));
        let serde_json::Value::Object(fields) = serde_json::to_value(facts)? else {
            unreachable!("Facts serializes to an object");
        };
        for (name, value) in fields {
            ctx.add_variable_from_value(name, cel::to_value(value)?);
        }
        for rule in &self.rules {
            let matched = rule
                .program
                .execute(&ctx)
                .with_context(|| format!("rule {:?}", rule.name))?;
            match matched {
                Value::Bool(true) => return Ok(Some(rule)),
                Value::Bool(false) => {}
                other => bail!("rule {:?} returned {other:?}, not a bool", rule.name),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(src: &str) -> Rules {
        #[derive(Deserialize)]
        struct F {
            rule: Vec<RuleSpec>,
        }
        Rules::compile(toml::from_str::<F>(src).unwrap().rule).unwrap()
    }

    #[test]
    fn first_match_wins_and_sees_all_facts() {
        let r = rules(
            r#"
            [[rule]]
            name = "mention"
            when = 'reason == "mention"'
            tier = "notify"
            [[rule]]
            name = "bot-foreign"
            when = 'bot && !owned && "backup" in requested_teams'
            tier = "on-demand"
            [[rule]]
            name = "rest"
            when = 'true'
            tier = "digest"
            "#,
        );
        let mut f = Facts {
            reason: "review_requested".into(),
            bot: true,
            requested_teams: vec!["backup".into()],
            ..Default::default()
        };
        assert_eq!(r.classify(&f).unwrap().unwrap().name, "bot-foreign");
        f.reason = "mention".into();
        assert_eq!(r.classify(&f).unwrap().unwrap().tier, Tier::Notify);
        f = Facts::default();
        assert_eq!(r.classify(&f).unwrap().unwrap().name, "rest");
    }

    #[test]
    fn non_bool_rule_is_an_error() {
        let r = rules("[[rule]]\nname = \"x\"\nwhen = 'title'\ntier = \"list\"\n");
        assert!(r.classify(&Facts::default()).is_err());
    }
}
