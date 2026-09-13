use agent_launcher_core::{
    BackendKind, Issue, IssueKey, RunSummary, RuntimeSnapshot, WorktreeDeletePreview,
};

use crate::{
    metrics::HostMetrics,
    rows::{IssueSort, display_rows_matching},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Route {
    #[default]
    Inbox,
    Detail,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum InboxTab {
    #[default]
    Issues,
    PullRequests,
}

impl InboxTab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Issues => "Issues",
            Self::PullRequests => "PRs",
        }
    }
}

#[derive(Default)]
pub(crate) struct InactiveList {
    selected: usize,
    scroll: usize,
    search_query: String,
    issue_sort: IssueSort,
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
pub(crate) struct IssueDeleteOverlay {
    pub issue_key: IssueKey,
    pub identifier: String,
    pub title: String,
    pub pending: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct DispatchOverlay {
    pub issue_key: IssueKey,
    pub cursor: usize,
    pub stage: DispatchStage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DispatchStage {
    Prompt,
    Target { profile: Option<String> },
}

#[derive(Default)]
pub(crate) struct AppState {
    pub layout: crate::LayoutMode,
    pub mouse: crate::mouse::MouseGeometry,
    pub route: Route,
    pub tab: InboxTab,
    pub inactive_list: InactiveList,
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
    pub issue_delete_overlay: Option<IssueDeleteOverlay>,
    pub issue_delete_confirmation_visible: bool,
    pub dispatch_overlay: Option<DispatchOverlay>,
    pub delete_preview_request: Option<u64>,
    pub next_request_id: u64,
    pub command_overlay: bool,
    pub debug_overlay: bool,
    pub debug_scroll: u16,
    pub debug_scroll_max: u16,
    pub debug_page_size: u16,
    pub sort_overlay: bool,
    pub sort_cursor: usize,
    pub issue_sort: IssueSort,
    pub status_message: Option<String>,
    pub host_metrics: HostMetrics,
    pub tick: u32,
    pub demo_started: Option<std::time::Instant>,
    pub activity_history_origin: Option<chrono::DateTime<chrono::Utc>>,
}

impl AppState {
    pub(crate) fn visible_status(&self, snapshot: &RuntimeSnapshot) -> Option<String> {
        let error = snapshot
            .error
            .as_ref()
            .or(snapshot.diagnostic_log_error.as_ref());
        match (self.status_message.as_ref(), error) {
            (Some(status), Some(error)) => Some(format!("{status} | {error}")),
            (status, error) => status.or(error).cloned(),
        }
    }

    pub fn switch_tab(&mut self) {
        self.tab = match self.tab {
            InboxTab::Issues => InboxTab::PullRequests,
            InboxTab::PullRequests => InboxTab::Issues,
        };
        std::mem::swap(&mut self.selected, &mut self.inactive_list.selected);
        std::mem::swap(&mut self.scroll, &mut self.inactive_list.scroll);
        std::mem::swap(&mut self.search_query, &mut self.inactive_list.search_query);
        std::mem::swap(&mut self.issue_sort, &mut self.inactive_list.issue_sort);
    }

    pub fn rows(&self, snapshot: &RuntimeSnapshot) -> Vec<crate::rows::DisplayRow> {
        display_rows_matching(
            snapshot,
            &self.search_query,
            self.issue_sort,
            self.tab == InboxTab::PullRequests,
        )
    }

    pub fn reconcile_lists(&mut self, previous: &RuntimeSnapshot, next: &RuntimeSnapshot) {
        // Reconcile the hidden tab too, before its old snapshot is discarded.
        for _ in 0..2 {
            let key = self.selected_issue(previous).map(|issue| issue.key.clone());
            self.reconcile_selection(next, key.as_ref());
            self.switch_tab();
        }
    }

    pub fn reconcile_selection(
        &mut self,
        snapshot: &RuntimeSnapshot,
        selected_key: Option<&IssueKey>,
    ) {
        let rows = self.rows(snapshot);
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
        self.rows(snapshot)
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
        // The confirmation owns its target, even if a refresh removes it from the list.
        if self.issue_delete_overlay.is_some() {
            return false;
        }
        if self.route != Route::Detail || self.detail_issue(snapshot).is_some() {
            return false;
        }
        self.reset_detail();
        self.status_message =
            Some("selected issue is no longer available; returned to inbox".to_owned());
        true
    }

    pub fn reconcile_dispatch(&mut self, snapshot: &RuntimeSnapshot) -> bool {
        let Some(overlay) = self.dispatch_overlay.as_ref() else {
            return false;
        };
        if !snapshot
            .issues
            .iter()
            .any(|issue| issue.key == overlay.issue_key)
        {
            self.dispatch_overlay = None;
            self.status_message =
                Some("dispatch chooser closed because its issue is unavailable".into());
            return true;
        }
        let (count, unavailable) = match &overlay.stage {
            DispatchStage::Prompt => (
                snapshot.prompt_profiles.len(),
                snapshot
                    .prompt_profiles
                    .is_empty()
                    .then_some("prompt chooser closed because its profiles are unavailable"),
            ),
            DispatchStage::Target { .. }
                if snapshot.selected_backend != Some(BackendKind::Native)
                    || snapshot.compute_targets.is_empty() =>
            {
                (
                    0,
                    Some("compute target chooser closed because targets are unavailable"),
                )
            },
            DispatchStage::Target { .. } => (snapshot.compute_targets.len() + 1, None),
        };
        if let Some(message) = unavailable {
            self.dispatch_overlay = None;
            self.status_message = Some(message.into());
            return true;
        }
        let overlay = self.dispatch_overlay.as_mut().expect("overlay exists");
        overlay.cursor = overlay.cursor.min(count - 1);
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
