use std::collections::{HashMap, HashSet};

use agent_launcher_core::{Issue, RuntimeSnapshot};
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum IssueSort {
    #[default]
    Newest,
    Oldest,
    RecentlyUpdated,
    Priority,
    Title,
}

impl IssueSort {
    pub const ALL: [Self; 5] = [
        Self::Newest,
        Self::Oldest,
        Self::RecentlyUpdated,
        Self::Priority,
        Self::Title,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Newest => "newest first",
            Self::Oldest => "oldest first",
            Self::RecentlyUpdated => "recently updated",
            Self::Priority => "priority",
            Self::Title => "title A-Z",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DisplayRow {
    pub issue_idx: usize,
    pub depth: u8,
    pub last_child: bool,
    pub context_only: bool,
}

pub(crate) fn display_rows(snapshot: &RuntimeSnapshot, sort: IssueSort) -> Vec<DisplayRow> {
    let mut ordered = (0..snapshot.issues.len()).collect::<Vec<_>>();
    ordered.sort_by(|&left, &right| {
        let left = &snapshot.issues[left];
        let right = &snapshot.issues[right];
        compare_issues(left, right, sort)
    });

    let parents = parent_indices(snapshot);
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for &idx in &ordered {
        if let Some(parent) = parents[idx] {
            children.entry(parent).or_default().push(idx);
        }
    }

    let mut rows = Vec::with_capacity(ordered.len());
    let mut visited = HashSet::new();
    for &idx in &ordered {
        if parents[idx].is_none() {
            append_tree(idx, 0, false, &children, &mut visited, &mut rows);
        }
    }
    // Broken and cyclic parent links stay visible as top-level rows.
    for &idx in &ordered {
        if !visited.contains(&idx) {
            append_tree(idx, 0, false, &children, &mut visited, &mut rows);
        }
    }
    rows
}

fn append_tree(
    idx: usize,
    depth: u8,
    last_child: bool,
    children: &HashMap<usize, Vec<usize>>,
    visited: &mut HashSet<usize>,
    rows: &mut Vec<DisplayRow>,
) {
    if !visited.insert(idx) {
        return;
    }
    rows.push(DisplayRow {
        issue_idx: idx,
        depth,
        last_child,
        context_only: false,
    });
    if let Some(child_indices) = children.get(&idx) {
        for (position, child) in child_indices.iter().enumerate() {
            append_tree(
                *child,
                depth.saturating_add(1),
                position + 1 == child_indices.len(),
                children,
                visited,
                rows,
            );
        }
    }
}

pub(crate) fn display_rows_matching(
    snapshot: &RuntimeSnapshot,
    query: &str,
    sort: IssueSort,
) -> Vec<DisplayRow> {
    let rows = display_rows(snapshot, sort);
    let query = query.trim();
    if query.is_empty() {
        return rows;
    }

    let matcher = SkimMatcherV2::default().smart_case();
    let matched = rows
        .iter()
        .filter_map(|row| {
            snapshot.issues.get(row.issue_idx)?;
            matcher
                .fuzzy_match(&searchable_text(snapshot, row.issue_idx), query)
                .map(|_| row.issue_idx)
        })
        .collect::<HashSet<_>>();
    let mut visible = matched.clone();
    let parents = parent_indices(snapshot);
    for &matched_idx in &matched {
        let mut current_idx = matched_idx;
        let mut guard = HashSet::new();
        while let Some(parent_idx) = parents[current_idx] {
            if !guard.insert(parent_idx) {
                break;
            }
            visible.insert(parent_idx);
            current_idx = parent_idx;
        }
    }

    rows.into_iter()
        .filter_map(|mut row| {
            snapshot.issues.get(row.issue_idx)?;
            visible.contains(&row.issue_idx).then(|| {
                row.context_only = !matched.contains(&row.issue_idx);
                row
            })
        })
        .collect()
}

fn compare_issues(left: &Issue, right: &Issue, sort: IssueSort) -> std::cmp::Ordering {
    let order = match sort {
        IssueSort::Newest => compare_optional_dates(left.created_at, right.created_at, true),
        IssueSort::Oldest => compare_optional_dates(left.created_at, right.created_at, false),
        IssueSort::RecentlyUpdated => compare_optional_dates(
            left.updated_at.or(left.created_at),
            right.updated_at.or(right.created_at),
            true,
        ),
        IssueSort::Priority => match (left.priority, right.priority) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        },
        IssueSort::Title => left
            .title
            .to_ascii_lowercase()
            .cmp(&right.title.to_ascii_lowercase()),
    };
    order
        .then_with(|| right.created_at.cmp(&left.created_at))
        .then_with(|| left.key.canonical().cmp(&right.key.canonical()))
}

fn compare_optional_dates(
    left: Option<chrono::DateTime<chrono::Utc>>,
    right: Option<chrono::DateTime<chrono::Utc>>,
    newest: bool,
) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) if newest => right.cmp(&left),
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn parent_indices(snapshot: &RuntimeSnapshot) -> Vec<Option<usize>> {
    let mut native_ids = HashMap::new();
    let mut identifiers = HashMap::new();
    for (idx, issue) in snapshot.issues.iter().enumerate() {
        native_ids.insert(issue_identity(issue, &issue.key.native_id), idx);
        identifiers.insert(issue_identity(issue, &issue.identifier), idx);
    }
    snapshot
        .issues
        .iter()
        .enumerate()
        .map(|(idx, issue)| {
            issue.parent_id.as_deref().and_then(|id| {
                let identity = issue_identity(issue, id);
                native_ids
                    .get(&identity)
                    .or_else(|| identifiers.get(&identity))
                    .copied()
                    .filter(|parent| *parent != idx)
            })
        })
        .collect()
}

fn issue_identity<'a>(
    issue: &'a Issue,
    id: &'a str,
) -> (
    agent_launcher_core::IssueProvider,
    &'a str,
    &'a str,
    &'a str,
) {
    (
        issue.key.provider,
        &issue.key.host,
        &issue.key.repository,
        id,
    )
}

fn searchable_text(snapshot: &RuntimeSnapshot, issue_idx: usize) -> String {
    let issue = &snapshot.issues[issue_idx];
    let mut fields = vec![
        issue.key.provider.to_string(),
        issue.key.host.clone(),
        issue.key.repository.clone(),
        issue.key.native_id.clone(),
        issue.identifier.clone(),
        issue.title.clone(),
        issue.state.clone(),
    ];
    fields.extend(issue.description.iter().cloned());
    fields.extend(issue.url.iter().cloned());
    fields.extend(issue.author.iter().cloned());
    fields.extend(issue.labels.iter().cloned());
    fields.extend(issue.blocked_by.iter().cloned());
    let issue_key = issue.key.canonical();
    for run in snapshot
        .runs
        .iter()
        .filter(|run| run.issue_key == issue_key)
    {
        fields.push(run.agent.clone());
        fields.push(crate::status::run_label(run.state).to_owned());
        fields.extend(run.message.iter().cloned());
        fields.extend(run.session_id.iter().cloned());
        if let Some(workspace) = &run.workspace {
            fields.push(workspace.id.clone());
            fields.push(workspace.branch.clone());
        }
    }
    fields.join(" ")
}

pub(crate) fn hierarchy_prefix(depth: u8, last_child: bool) -> String {
    if depth == 0 {
        String::new()
    } else {
        let indent = "  ".repeat(usize::from(depth.saturating_sub(1)));
        format!(
            "{indent}{}",
            if last_child {
                "└─ "
            } else {
                "├─ "
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent_launcher_core::{Issue, IssueKey, IssueProvider, Repository};

    use super::*;

    fn issue(id: &str, title: &str, parent_id: Option<&str>) -> Issue {
        Issue {
            key: IssueKey {
                provider: IssueProvider::Beads,
                host: "local".to_owned(),
                repository: "/repo".to_owned(),
                native_id: id.to_owned(),
            },
            identifier: id.to_owned(),
            title: title.to_owned(),
            description: None,
            state: "open".to_owned(),
            url: None,
            author: None,
            labels: Vec::new(),
            parent_id: parent_id.map(str::to_owned),
            blocked_by: Vec::new(),
            priority: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn fuzzy_child_match_keeps_parent_context_and_hierarchy() {
        let snapshot = RuntimeSnapshot {
            repository: Some(Repository {
                root: PathBuf::from("/repo"),
                git_dir: PathBuf::from("/repo/.git"),
                remote: None,
                has_beads: true,
            }),
            issues: vec![
                issue("epic", "Release epic", None),
                issue("child", "Fix authentication", Some("epic")),
                issue("other", "Write docs", None),
            ],
            ..RuntimeSnapshot::default()
        };

        let rows = display_rows_matching(&snapshot, "authn", IssueSort::Newest);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].issue_idx, 0);
        assert!(rows[0].context_only);
        assert_eq!(rows[1].issue_idx, 1);
        assert_eq!(rows[1].depth, 1);
        assert!(rows[1].last_child);
    }

    #[test]
    fn identical_parent_ids_do_not_cross_source_boundaries() {
        let mut other_parent = issue("epic", "Other source epic", None);
        other_parent.key.repository = "/other".to_owned();
        let mut other_child = issue("other-child", "Other child", Some("epic"));
        other_child.key.repository = "/other".to_owned();
        let snapshot = RuntimeSnapshot {
            issues: vec![
                issue("epic", "Repository epic", None),
                other_parent,
                issue("child", "Repository child", Some("epic")),
                other_child,
            ],
            ..RuntimeSnapshot::default()
        };

        let rows = display_rows(&snapshot, IssueSort::Newest);
        let repository_child = rows.iter().find(|row| row.issue_idx == 2).unwrap();
        let other_child = rows.iter().find(|row| row.issue_idx == 3).unwrap();
        assert_eq!(repository_child.depth, 1);
        assert_eq!(other_child.depth, 1);
        assert_eq!(
            rows.iter().position(|row| row.issue_idx == 2).unwrap(),
            rows.iter().position(|row| row.issue_idx == 0).unwrap() + 1
        );
        assert_eq!(
            rows.iter().position(|row| row.issue_idx == 3).unwrap(),
            rows.iter().position(|row| row.issue_idx == 1).unwrap() + 1
        );
    }

    #[test]
    fn defaults_to_newest_and_supports_alternate_sorting() {
        let now = chrono::Utc::now();
        let mut older = issue("1", "Zulu", None);
        older.created_at = Some(now - chrono::Duration::days(2));
        older.updated_at = Some(now);
        older.priority = Some(3);
        let mut newer = issue("2", "Alpha", None);
        newer.created_at = Some(now - chrono::Duration::days(1));
        newer.updated_at = Some(now - chrono::Duration::days(1));
        newer.priority = Some(1);
        let snapshot = RuntimeSnapshot {
            issues: vec![older, newer],
            ..RuntimeSnapshot::default()
        };
        let ids = |sort| {
            display_rows(&snapshot, sort)
                .into_iter()
                .map(|row| snapshot.issues[row.issue_idx].key.native_id.as_str())
                .collect::<Vec<_>>()
        };

        assert_eq!(ids(IssueSort::default()), ["2", "1"]);
        assert_eq!(ids(IssueSort::Oldest), ["1", "2"]);
        assert_eq!(ids(IssueSort::RecentlyUpdated), ["1", "2"]);
        assert_eq!(ids(IssueSort::Priority), ["2", "1"]);
        assert_eq!(ids(IssueSort::Title), ["2", "1"]);
    }
}
