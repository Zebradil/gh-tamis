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
cargo run              # dry run: print the tier and rule for every new or updated thread
cargo run -- --apply   # also mark threads read or done on GitHub
cargo run -- --full    # handle the whole inbox, not only what changed since the last run
```

The token comes from `GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth token`, in that order.

Each run asks GitHub only for threads updated since the previous run and skips updates it has already handled. An idle
poll gets `304 Not Modified`, which costs no rate limit, and runs closer together than GitHub's `X-Poll-Interval`
exit at once. What a run learned is kept in `$XDG_STATE_HOME/gh-tamis/state.json` (default
`~/.local/state/gh-tamis/state.json`). Without that file, the first run records the inbox and notifies about none of
it.

For each new update of an unread `notify` thread, `notify_command` runs, with or without `--apply`. The example config
uses [terminal-notifier](https://github.com/julienXX/terminal-notifier); a click opens the thread.

## Running every minute on macOS

Save as `~/Library/LaunchAgents/gh-tamis.plist`, with your paths. `PATH` must reach `gh` (unless `GH_TOKEN` is set) and
the notify command.

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>gh-tamis</string>
  <key>ProgramArguments</key>
  <array><string>/Users/me/.cargo/bin/gh-tamis</string></array>
  <key>StartInterval</key><integer>60</integer>
  <key>RunAtLoad</key><true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>/Users/me/.nix-profile/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
  </dict>
  <key>StandardOutPath</key><string>/Users/me/Library/Logs/gh-tamis.log</string>
  <key>StandardErrorPath</key><string>/Users/me/Library/Logs/gh-tamis.log</string>
</dict>
</plist>
```

```sh
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/gh-tamis.plist   # start
launchctl bootout gui/$(id -u)/gh-tamis                                   # stop
```

The log gets one line per handled thread and a summary per run that handled any, which makes it a record of decisions
to check while running without `--apply`.
