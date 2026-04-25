//! `.synclineignore` parser and matcher.
//!
//! A reduced subset of `.gitignore` semantics, sufficient for letting the
//! user opt out of syncing device-specific files (e.g. Obsidian's
//! `workspace.json`) while still syncing useful hidden config.
//!
//! Supported syntax:
//!
//! - `# comment` and blank lines are ignored.
//! - `name`             matches any path component named `name`.
//! - `name/`            matches a directory called `name` (and its contents).
//! - `dir/file`         anchored path match relative to the vault root.
//! - `dir/sub/`         anchored directory match (including everything inside).
//! - `*` and `?`        glob wildcards within a single path component.
//!
//! Not supported: `**` recursive globs, character classes (`[abc]`),
//! negation (`!pattern`). The grammar is deliberately minimal — patterns
//! that can't be expressed here belong in code.
//!
//! `.syncline/` is **always** ignored, regardless of pattern file content.
//! That rule is enforced by [`IgnoreList::is_ignored`] and cannot be
//! overridden — Syncline's own metadata directory must never round-trip
//! through the sync layer.

use std::path::Path;

/// Built-in patterns layered under any user `.synclineignore`. Skips
/// VCS metadata and device-specific Obsidian state that is rebuilt on
/// every machine (window layout, cache, graph node positions).
pub const DEFAULT_PATTERNS: &str = "\
.git/
.obsidian/workspace.json
.obsidian/workspace-mobile.json
.obsidian/cache/
.obsidian/graph.json
";

#[derive(Debug, Clone, Default)]
pub struct IgnoreList {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    parts: Vec<String>,
    dir_only: bool,
    anchored: bool,
}

impl IgnoreList {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_defaults() -> Self {
        Self::from_text(DEFAULT_PATTERNS)
    }

    pub fn from_text(text: &str) -> Self {
        let mut list = Self::empty();
        list.extend_from_text(text);
        list
    }

    pub fn extend_from_text(&mut self, text: &str) {
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (body, dir_only) = match line.strip_suffix('/') {
                Some(stripped) => (stripped, true),
                None => (line, false),
            };
            let (body, leading_slash) = match body.strip_prefix('/') {
                Some(stripped) => (stripped, true),
                None => (body, false),
            };
            if body.is_empty() {
                continue;
            }
            let parts: Vec<String> = body.split('/').map(str::to_owned).collect();
            let anchored = leading_slash || parts.len() > 1;
            self.rules.push(Rule { parts, dir_only, anchored });
        }
    }

    /// Loads `.synclineignore` from `root` if it exists, layered on top
    /// of [`DEFAULT_PATTERNS`]. Missing/unreadable files yield the
    /// defaults-only list.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load(root: &Path) -> Self {
        let mut list = Self::with_defaults();
        if let Ok(text) = std::fs::read_to_string(root.join(".synclineignore")) {
            list.extend_from_text(&text);
        }
        list
    }

    /// Returns true if the given vault-relative path should be excluded
    /// from sync. Path components are separated by `/` regardless of OS.
    /// `.syncline/` is hardcoded-ignored at any depth.
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        let comps: Vec<&str> = rel_path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            return false;
        }
        if comps.contains(&".syncline") {
            return true;
        }
        self.rules.iter().any(|r| r.matches(&comps, is_dir))
    }
}

impl Rule {
    fn matches(&self, comps: &[&str], is_dir: bool) -> bool {
        if self.anchored {
            if self.parts.len() > comps.len() {
                return false;
            }
            for (pat, c) in self.parts.iter().zip(comps.iter()) {
                if !glob_match(pat, c) {
                    return false;
                }
            }
            if self.parts.len() < comps.len() {
                // Path extends beyond pattern → only a directory pattern
                // matches descendants.
                return self.dir_only;
            }
            // Exact-length match: a dir-only pattern still matches the
            // directory itself, but not a file with the same path.
            !self.dir_only || is_dir
        } else {
            let pat = &self.parts[0];
            let last = comps.len() - 1;
            for (i, c) in comps.iter().enumerate() {
                if !glob_match(pat, c) {
                    continue;
                }
                if !self.dir_only || i < last || is_dir {
                    return true;
                }
            }
            false
        }
    }
}

/// Per-component glob: `?` matches one byte, `*` matches any run of
/// bytes. No path-separator semantics — patterns are split on `/` first.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();

    // Iterative algorithm with a single fallback point — handles `*`
    // without recursion blow-up and is plenty for our pattern shape.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut tstar) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti] || p[pi] == b'?') {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            tstar = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            tstar += 1;
            ti = tstar;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("foo", "foo"));
        assert!(!glob_match("foo", "bar"));
        assert!(glob_match("*.json", "workspace.json"));
        assert!(!glob_match("*.json", "workspace.txt"));
        assert!(glob_match("file?.md", "file1.md"));
        assert!(!glob_match("file?.md", "file12.md"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "azzzbzzc"));
    }

    #[test]
    fn comments_and_blanks_skipped() {
        let l = IgnoreList::from_text("# comment\n\n  \nfoo\n");
        assert!(l.is_ignored("foo", false));
        assert!(!l.is_ignored("comment", false));
    }

    #[test]
    fn syncline_dir_always_ignored() {
        let l = IgnoreList::empty();
        assert!(l.is_ignored(".syncline", true));
        assert!(l.is_ignored(".syncline/manifest.bin", false));
        assert!(l.is_ignored("nested/.syncline/foo", false));
    }

    #[test]
    fn dir_pattern_matches_descendants() {
        let l = IgnoreList::from_text(".obsidian/cache/\n");
        assert!(l.is_ignored(".obsidian/cache", true));
        assert!(l.is_ignored(".obsidian/cache/index.bin", false));
        assert!(!l.is_ignored(".obsidian/cache.json", false));
        assert!(!l.is_ignored(".obsidian/plugins/foo/main.js", false));
    }

    #[test]
    fn anchored_file_pattern_is_exact() {
        let l = IgnoreList::from_text(".obsidian/workspace.json\n");
        assert!(l.is_ignored(".obsidian/workspace.json", false));
        assert!(!l.is_ignored(".obsidian/workspace.json.bak", false));
        assert!(!l.is_ignored("nested/.obsidian/workspace.json", false));
    }

    #[test]
    fn unanchored_dir_pattern_matches_anywhere() {
        let l = IgnoreList::from_text(".git/\n");
        assert!(l.is_ignored(".git", true));
        assert!(l.is_ignored(".git/HEAD", false));
        assert!(l.is_ignored("submodule/.git/refs/heads/main", false));
        // a *file* called .git is not the same as the directory.
        assert!(!l.is_ignored(".git", false));
    }

    #[test]
    fn defaults_cover_obsidian_device_files() {
        let l = IgnoreList::with_defaults();
        assert!(l.is_ignored(".obsidian/workspace.json", false));
        assert!(l.is_ignored(".obsidian/workspace-mobile.json", false));
        assert!(l.is_ignored(".obsidian/cache/index.bin", false));
        assert!(l.is_ignored(".obsidian/graph.json", false));
        assert!(l.is_ignored(".git/HEAD", false));
        // sync targets — must NOT be ignored
        assert!(!l.is_ignored(".obsidian/app.json", false));
        assert!(!l.is_ignored(".obsidian/community-plugins.json", false));
        assert!(!l.is_ignored(".obsidian/plugins/dataview/main.js", false));
        assert!(!l.is_ignored(".obsidian/snippets/custom.css", false));
        assert!(!l.is_ignored(".obsidian/themes/things/theme.css", false));
    }

    #[test]
    fn glob_pattern_in_path() {
        let l = IgnoreList::from_text(".obsidian/*.tmp\n");
        assert!(l.is_ignored(".obsidian/foo.tmp", false));
        assert!(!l.is_ignored(".obsidian/foo.json", false));
        assert!(!l.is_ignored(".obsidian/sub/foo.tmp", false));
    }

    #[test]
    fn leading_slash_treated_as_anchor() {
        let l = IgnoreList::from_text("/secret.txt\n");
        assert!(l.is_ignored("secret.txt", false));
        assert!(!l.is_ignored("nested/secret.txt", false));
    }

    #[test]
    fn syncline_rule_cannot_be_overridden_via_unsupported_negation() {
        // We don't support `!` negation; lines starting with `!` parse
        // as patterns whose first component begins with `!`. Either way,
        // the hardcoded `.syncline` rule is unreachable.
        let l = IgnoreList::from_text("!.syncline/\n");
        assert!(l.is_ignored(".syncline/manifest.bin", false));
    }
}
