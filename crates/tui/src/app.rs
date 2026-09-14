use agent_launcher_core::{
    BackendKind, DispatchOptions, Issue, IssueKey, ModelSelection, RunSummary, RuntimeSnapshot,
    WorktreeDeletePreview,
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
    Security,
}

impl InboxTab {
    pub const ALL: [Self; 3] = [Self::Issues, Self::PullRequests, Self::Security];

    pub const fn index(self) -> usize {
        match self {
            Self::Issues => 0,
            Self::PullRequests => 1,
            Self::Security => 2,
        }
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Issues => "Issues",
            Self::PullRequests => "PRs",
            Self::Security => "Security",
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
    pub settings: LaunchSettings,
    pub prompt: PromptView,
    pub issue_key: IssueKey,
    pub cursor: usize,
    pub stage: DispatchStage,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LaunchSettings {
    pub backend: Option<BackendKind>,
    pub default_harness: String,
    pub default_model: Option<String>,
    pub options: DispatchOptions,
    pub model_editor: Option<crate::widgets::editor::Editor>,
    pub target_cursor: usize,
    // Map the previous cursor to a stable target ID when refreshes reorder hosts.
    pub target_choices: Vec<String>,
    pub had_targets: bool,
    pub review: bool,
    pub security: bool,
    pub privacy_confirmation: bool,
    pub scroll: u16,
    pub scroll_max: u16,
}

impl LaunchSettings {
    pub fn harness_choices(&self) -> &'static [&'static str] {
        match self.backend {
            Some(BackendKind::Herdr) => &["opencode", "claude", "pi"],
            Some(BackendKind::Native) => &["opencode"],
            Some(BackendKind::Conductor) => &["claude", "codex", "cursor", "acp"],
            Some(BackendKind::Superset) | None => &[],
        }
    }

    pub fn model_label(&self) -> String {
        match &self.options.model {
            ModelSelection::Inherit => format!(
                "Configured default: {}",
                // Native's configured name is an OpenCode subagent, not another harness.
                if self.backend == Some(BackendKind::Native)
                    || self
                        .options
                        .harness
                        .as_ref()
                        .is_none_or(|h| *h == self.default_harness)
                {
                    self.default_model
                        .as_deref()
                        .unwrap_or("uses harness default")
                } else {
                    "uses harness default (different harness)"
                }
            ),
            ModelSelection::HarnessDefault => "Harness default".into(),
            ModelSelection::Explicit(model) => format!("Custom model: {model}"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PromptView {
    pub name: String,
    pub request: Option<u64>,
    pub loading_source: bool,
    /// Unrendered template source, including literal MiniJinja placeholders.
    pub preview: Option<Result<String, String>>,
    pub scroll: u16,
    pub scroll_max: u16,
    pub editor: Option<PromptEditor>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct PromptEditor {
    pub name: String,
    pub naming: bool,
    pub buffer: crate::widgets::editor::Editor,
    pub original: Option<String>,
    pub discard: bool,
    pub busy: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DispatchStage {
    Prompt,
    Target {
        profile: Option<String>,
    },
    Settings {
        profile: Option<String>,
        target: Option<String>,
    },
}

#[derive(Default)]
pub(crate) struct AppState {
    pub layout: crate::LayoutMode,
    pub mouse: crate::mouse::MouseGeometry,
    pub route: Route,
    pub tab: InboxTab,
    pub inactive_lists: [InactiveList; 3],
    pub selected: usize,
    pub detail_issue_key: Option<IssueKey>,
    pub scroll: usize,
    pub visible_rows: usize,
    pub search_query: String,
    pub detail_scroll: u16,
    pub detail_scroll_max: u16,
    pub(crate) markdown_cache: crate::widgets::markdown::MarkdownCache,
    pub input_overlay: Option<InputOverlay>,
    pub delete_overlay: Option<DeleteOverlay>,
    pub delete_confirmation_visible: bool,
    pub issue_delete_overlay: Option<IssueDeleteOverlay>,
    pub issue_delete_confirmation_visible: bool,
    pub dispatch_overlay: Option<DispatchOverlay>,
    pub security_confirmation_visible: bool,
    pub security_launch_pending: Option<u64>,
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
        if self.tab == InboxTab::Security {
            return self.status_message.clone().or_else(|| {
                snapshot.sources.iter().any(|source| source.name.starts_with("security:github:") && !source.connected)
                    .then(|| "Private advisory source unavailable or unauthorized; check GitHub advisory access (Ctrl+G, r retries).".into())
            });
        }
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
        self.set_tab(self.tab.next());
    }

    fn swap_list(&mut self) {
        let list = &mut self.inactive_lists[self.tab.index()];
        std::mem::swap(&mut self.selected, &mut list.selected);
        std::mem::swap(&mut self.scroll, &mut list.scroll);
        std::mem::swap(&mut self.search_query, &mut list.search_query);
        std::mem::swap(&mut self.issue_sort, &mut list.issue_sort);
    }

    pub fn set_tab(&mut self, tab: InboxTab) {
        if self.tab == tab {
            return;
        }
        self.swap_list();
        self.tab = tab;
        self.swap_list();
    }

    pub fn rows(&self, snapshot: &RuntimeSnapshot) -> Vec<crate::rows::DisplayRow> {
        display_rows_matching(snapshot, &self.search_query, self.issue_sort, self.tab)
    }

    pub fn reconcile_lists(&mut self, previous: &RuntimeSnapshot, next: &RuntimeSnapshot) {
        // Reconcile the hidden tab too, before its old snapshot is discarded.
        for _ in InboxTab::ALL {
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
        // Confirmations and prompt drafts survive a refresh removing their issue.
        if self.issue_delete_overlay.is_some()
            || self
                .dispatch_overlay
                .as_ref()
                .is_some_and(|o| o.prompt.editor.is_some())
        {
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
        if overlay.prompt.editor.is_none()
            && !snapshot
                .issues
                .iter()
                .any(|issue| issue.key == overlay.issue_key)
        {
            self.dispatch_overlay = None;
            self.security_confirmation_visible = false;
            self.status_message =
                Some("dispatch chooser closed because its issue is unavailable".into());
            return true;
        }
        let (count, unavailable) = match &overlay.stage {
            DispatchStage::Prompt => (snapshot.prompt_profiles.len().max(1), None),
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
            DispatchStage::Settings { .. } => (1, None),
        };
        if let Some(message) = unavailable {
            self.dispatch_overlay = None;
            self.status_message = Some(message.into());
            return true;
        }
        let overlay = self.dispatch_overlay.as_mut().expect("overlay exists");
        if matches!(overlay.stage, DispatchStage::Target { .. }) {
            if overlay.cursor > 0 {
                let id = overlay.settings.target_choices.get(overlay.cursor - 1);
                let position = snapshot
                    .compute_targets
                    .iter()
                    .position(|t| Some(&t.id) == id);
                let Some(position) = position else {
                    self.dispatch_overlay = None;
                    self.status_message = Some(
                        "selected compute target is no longer available; reopen dispatch".into(),
                    );
                    return true;
                };
                overlay.cursor = position + 1;
            }
            overlay.settings.target_choices = snapshot
                .compute_targets
                .iter()
                .map(|t| t.id.clone())
                .collect();
            overlay.settings.target_cursor = overlay.cursor;
        }
        if overlay.stage == DispatchStage::Prompt
            && let Some(index) = snapshot
                .prompt_profiles
                .iter()
                .position(|name| *name == overlay.prompt.name)
        {
            overlay.cursor = index;
        }
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
        self.markdown_cache = Default::default();
        self.detail_scroll = 0;
        self.detail_scroll_max = 0;
        self.input_overlay = None;
        self.delete_overlay = None;
        self.delete_confirmation_visible = false;
        self.delete_preview_request = None;
        self.dispatch_overlay = None;
        self.security_confirmation_visible = false;
    }
}
