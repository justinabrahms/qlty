use super::{GlobsMatcher, WorkspaceEntryMatcher};
use crate::WorkspaceEntry;
use qlty_config::config::ignore_group::IgnoreGroup;

#[derive(Default, Debug)]
pub struct IgnoreGroupsMatcher {
    matchers: Vec<GlobsMatcher>,
}

impl IgnoreGroupsMatcher {
    pub fn new(ignore_groups: Vec<IgnoreGroup>) -> Self {
        let matchers = ignore_groups
            .into_iter()
            .filter_map(|ignore_group| {
                GlobsMatcher::new_for_globs(&ignore_group.ignores, ignore_group.negate).ok()
            })
            .collect();

        Self { matchers }
    }
}

impl WorkspaceEntryMatcher for IgnoreGroupsMatcher {
    fn matches(&self, workspace_entry: WorkspaceEntry, _tool_name: &str) -> Option<WorkspaceEntry> {
        // By default, include the file
        let path = workspace_entry.path_string();

        for matcher in self.matchers.iter().rev() {
            if matcher.glob_set.is_match(&path) {
                return if matcher.include {
                    Some(workspace_entry) // No need to clone; directly return ownership
                } else {
                    None // Immediately exclude the file if a normal ignore rule matches
                };
            }
        }

        Some(workspace_entry)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{WorkspaceEntry, WorkspaceEntryKind};
    use qlty_config::config::ignore_group::IgnoreGroup;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn create_workspace_entry(path: &str) -> WorkspaceEntry {
        WorkspaceEntry {
            path: PathBuf::from(path),
            kind: WorkspaceEntryKind::File,
            content_modified: SystemTime::now(),
            contents_size: 100,
            language_name: None,
        }
    }

    #[test]
    fn test_ignore_groups_matcher_excludes_ignored_paths() {
        let ignore_groups = vec![IgnoreGroup {
            ignores: vec!["logs/**".to_string(), "target/**".to_string()],
            negate: false,
        }];
        let matcher = IgnoreGroupsMatcher::new(ignore_groups);

        assert!(matcher
            .matches(create_workspace_entry("src/main.rs"), "test")
            .is_some());
        assert!(matcher
            .matches(create_workspace_entry("logs/output.log"), "test")
            .is_none());
        assert!(matcher
            .matches(create_workspace_entry("target/debug/app"), "test")
            .is_none());
    }

    #[test]
    fn test_ignore_groups_matcher_allows_negated_patterns() {
        let ignore_groups = vec![
            IgnoreGroup {
                ignores: vec!["logs/**".to_string()],
                negate: false,
            },
            IgnoreGroup {
                ignores: vec!["logs/important.log".to_string()],
                negate: true,
            },
        ];
        let matcher = IgnoreGroupsMatcher::new(ignore_groups);

        assert!(matcher
            .matches(create_workspace_entry("logs/error.log"), "test")
            .is_none());
        assert!(matcher
            .matches(create_workspace_entry("logs/important.log"), "test")
            .is_some());
    }

    #[test]
    fn test_ignore_groups_matcher_empty_list() {
        let matcher = IgnoreGroupsMatcher::new(vec![]);
        assert!(matcher
            .matches(create_workspace_entry("src/main.rs"), "test")
            .is_some());
    }
}
