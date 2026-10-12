//! Which commit the workspace says it is on, without asking git.
//!
//! A status line wants a branch name, and the cheapest honest way to get one is
//! to read the file git itself writes: `.git/HEAD`. Spawning git for a label
//! would put a process between the screen and a keystroke, and reading the
//! object database would make this crate a git implementation, which is
//! `agent-tools`' job for the tools that need one.
//!
//! What this cannot see is stated rather than hidden: a linked worktree keeps its
//! real HEAD outside the workspace, in the directory its `.git` file points at, so
//! it reports no branch here.

use std::path::Path;

/// The branch checked out in `root`, or `None` when there is nothing true to say.
///
/// Detached HEAD is a state a person wants to see before they let an agent write,
/// so it is reported as `detached <short sha>` rather than as no branch.
pub fn head_branch(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let text = text.trim();
    // The marker is matched without its space, because the text above has already
    // lost any trailing one: a HEAD that declares a symref and names no target must
    // report nothing rather than fall through and claim to be a commit.
    if let Some(reference) = text.strip_prefix("ref:") {
        let name = reference.trim().rsplit('/').next()?;
        if name.is_empty() {
            return None;
        }
        return Some(name.to_owned());
    }
    if text.is_empty() {
        return None;
    }
    let sha: String = text.chars().take(7).collect();
    Some(format!("detached {sha}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(dir: &Path, text: &str) -> Option<String> {
        std::fs::create_dir_all(dir.join(".git")).expect("a .git dir");
        std::fs::write(dir.join(".git").join("HEAD"), text).expect("a HEAD file");
        head_branch(dir)
    }

    #[test]
    fn a_named_branch_is_the_last_part_of_its_reference() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            head(dir.path(), "ref: refs/heads/master\n"),
            Some("master".to_owned())
        );
        assert_eq!(
            head(dir.path(), "ref: refs/heads/feature/tui\n"),
            Some("tui".to_owned()),
            "a slash in a branch name must not be mistaken for the ref prefix"
        );
    }

    #[test]
    fn detached_head_says_so_with_the_commit_it_is_on() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            head(dir.path(), "01c0a1e9b7c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6\n"),
            Some("detached 01c0a1e".to_owned())
        );
    }

    #[test]
    fn a_workspace_that_is_not_a_repository_reports_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(head_branch(dir.path()), None);
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(head(dir.path(), ""), None, "an empty HEAD is not a branch");
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(head(dir.path(), "ref: \n"), None);
    }
}
