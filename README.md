# gh-tamis

Rule-based triage for GitHub notifications. A tamis is a fine sieve.

Each notification thread is matched against an ordered list of [CEL](https://cel.dev) rules. The first rule that
matches puts the thread into a tier, and the tier decides what happens to it on GitHub:

| Tier | `--apply` does |
| --- | --- |
| `notify` | keeps it unread |
| `digest` | keeps it as is (a daily digest is planned) |
| `list` | marks it read; it stays in the inbox |
| `on-demand` | marks it done |
| `clear` | marks it done |

Rules see the notification (reason, subject type, repo, title) plus details of the pull request behind it: author,
whether the author is a bot, state, pending team reviews, and whether you were asked for review by name. They also see
whether you own the repo. A repo is owned when the first team on its CODEOWNERS `*` line is one of your teams. Config
lists can override this.

## Usage

```sh
cp config.example.toml ~/.config/gh-tamis/config.toml   # then edit
cargo run              # dry run: print the tier and rule for every thread
cargo run -- --apply   # mark threads read or done on GitHub
```

The token comes from `GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth token`, in that order.
