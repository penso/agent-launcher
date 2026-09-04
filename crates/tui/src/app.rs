use agent_launcher_core::{Issue, IssueKey, RunSummary, RuntimeSnapshot, WorktreeDeletePreview};

use crate::{
    activity::AgentActivity,
    metrics::HostMetrics,
    rows::{IssueSort, display_rows_matching},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Route {
    #[default]
    Inbox,
    Detail,
}

#[derive(Clone, Debug)]
pub(crate) struct InputOverlay {
    pub run_id: String,
    pub prompt: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub(crate) struct DeleteOverlay {
    pub preview: WorktreeDeletePreview,
}

#[derive(Clone, Debug)]
pub(crate) struct DispatchOverlay {
    pub issue_key: IssueKey,
    pub cursor: usize,
}

#[derive(Default)]
pub(crate) struct AppState {
    pub route: Route,
    pub selected: usize,
    pub detail_issue_key: Option<IssueKey>,
    pub scroll: usize,
    pub visible_rows: usize,
    pub search_query: String,
    pub detail_scroll: u16,
    pub detail_scroll_max: u16,
    pub input_overlay: Option<InputOverlay>,
    pub delete_overlay: Option<DeleteOverlay>,
    pub delete_confirmation_visible: bool,
    pub dispatch_overlay: Option<DispatchOverlay>,
    pub delete_preview_request: Option<u64>,
    pub next_request_id: u64,
    pub command_overlay: bool,
    pub sort_overlay: bool,
    pub sort_cursor: usize,
    pub issue_sort: IssueSort,
    pub status_message: Option<String>,
    pub host_metrics: HostMetrics,
    pub agent_activity: AgentActivity,
    pub tick: u32,
}

impl AppState {
    pub fn reconcile_selection(
        &mut self,
        snapshot: &RuntimeSnapshot,
        selected_key: Option<&IssueKey>,
    ) {
        let rows = display_rows_matching(snapshot, &self.search_query, self.issue_sort);
        if let Some(position) = selected_key.and_then(|key| {
            rows.iter()
                .position(|row| snapshot.issues[row.issue_idx].key == *key)
        }) {
            self.selected = position;
        } else {
            self.selected = self.selected.min(rows.len().saturating_sub(1));
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(1));
    }

    pub fn selected_issue<'a>(&self, snapshot: &'a RuntimeSnapshot) -> Option<&'a Issue> {
        display_rows_matching(snapshot, &self.search_query, self.issue_sort)
            .get(self.selected)
            .and_then(|row| snapshot.issues.get(row.issue_idx))
    }

    pub fn detail_issue<'a>(&self, snapshot: &'a RuntimeSnapshot) -> Option<&'a Issue> {
        let key = self.detail_issue_key.as_ref()?;
        snapshot.issues.iter().find(|issue| issue.key == *key)
    }

    pub fn open_detail(&mut self, snapshot: &RuntimeSnapshot) -> bool {
        let Some(key) = self.selected_issue(snapshot).map(|issue| issue.key.clone()) else {
            return false;
        };
        self.route = Route::Detail;
        self.detail_issue_key = Some(key);
        self.detail_scroll = 0;
        self.status_message = None;
        true
    }

    pub fn reconcile_detail(&mut self, snapshot: &RuntimeSnapshot) -> bool {
        if self.route != Route::Detail || self.detail_issue(snapshot).is_some() {
            return false;
        }
        self.reset_detail();
        self.status_message =
            Some("selected issue is no longer available; returned to inbox".to_owned());
        true
    }

    pub fn reconcile_dispatch(&mut self, snapshot: &RuntimeSnapshot) -> bool {
        let Some(overlay) = self.dispatch_overlay.as_mut() else {
            return false;
        };
        if !snapshot
            .issues
            .iter()
            .any(|issue| issue.key == overlay.issue_key)
            || snapshot.prompt_profiles.is_empty()
        {
            self.dispatch_overlay = None;
            self.status_message =
                Some("prompt chooser closed because its issue or profiles are unavailable".into());
            return true;
        }
        overlay.cursor = overlay.cursor.min(snapshot.prompt_profiles.len() - 1);
        false
    }

    pub fn latest_run<'a>(
        &self,
        snapshot: &'a RuntimeSnapshot,
        issue: &Issue,
    ) -> Option<&'a RunSummary> {
        let key = issue.key.canonical();
        snapshot
            .runs
            .iter()
            .filter(|run| run.issue_key == key)
            .max_by_key(|run| (run.updated_at, run.started_at))
    }

    pub fn reset_detail(&mut self) {
        self.route = Route::Inbox;
        self.detail_issue_key = None;
        self.detail_scroll = 0;
        self.detail_scroll_max = 0;
        self.input_overlay = None;
        self.delete_overlay = None;
        self.delete_confirmation_visible = false;
        self.delete_preview_request = None;
        self.dispatch_overlay = None;
    }
}
