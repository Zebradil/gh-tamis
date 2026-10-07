/// The responsible team of a repo: the first team on the CODEOWNERS line for `*`. Later teams
/// on that line are backups. Returned as `org/slug`, without the `@`.
///
/// Path-specific lines are ignored, so ownership is per repo, not per file.
pub fn primary_team(codeowners: &str) -> Option<String> {
    // GitHub uses the last matching line, so the last `*` line decides.
    codeowners
        .lines()
        .map(|l| l.split('#').next().unwrap_or_default())
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            (parts.next() == Some("*")).then_some(parts)
        })
        .next_back()?
        .filter_map(|o| o.strip_prefix('@'))
        .find(|o| o.contains('/'))
        .map(str::to_owned)
}

/// Repo pattern from config: `owner/repo` or `owner/*`.
pub fn matches(pattern: &str, repo: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(owner) => repo
            .split_once('/')
            .is_some_and(|(o, _)| o.eq_ignore_ascii_case(owner)),
        None => pattern.eq_ignore_ascii_case(repo),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_team_on_last_star_line() {
        let co = "\
# owners
* @old/team
* @alice @org/primary @org/backup  # comment
/charts/ @org/other
";
        assert_eq!(primary_team(co).as_deref(), Some("org/primary"));
        assert_eq!(primary_team("* @alice @bob"), None);
        assert_eq!(primary_team("/docs/ @org/docs"), None);
    }

    #[test]
    fn repo_patterns() {
        assert!(matches("Zebradil/*", "zebradil/know"));
        assert!(matches("org/repo", "org/repo"));
        assert!(!matches("org/repo", "org/repo2"));
        assert!(!matches("org/*", "organisation/repo"));
    }
}
