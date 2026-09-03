use agent_launcher_core::{Issue, IssueKey, RunSummary, RuntimeSnapshot};

use crate::rows::display_rows_matching;

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
    pub status_message: Option<String>,
    pub tick: u32,
}

impl AppState {
    pub fn clamp_selection(&mut self, snapshot: &RuntimeSnapshot) {
        let count = display_rows_matching(snapshot, &self.search_query).len();
        self.selected = self.selected.min(count.saturating_sub(1));
        self.scroll = self.scroll.min(count.saturating_sub(1));
    }

    pub fn selected_issue<'a>(&self, snapshot: &'a RuntimeSnapshot) -> Option<&'a Issue> {
        display_rows_matching(snapshot, &self.search_query)
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
    }
}
