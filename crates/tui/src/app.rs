use agent_launcher_core::{
    BackendKind, DispatchOptions, Issue, IssueKey, LogEntry, LogLevel, ModelSelection, RunSummary,
    RuntimeSnapshot, WorktreeDeletePreview,
};
use fuzzy_matcher::{FuzzyMatcher, skim::SkimMatcherV2};

use crate::{
    catalog::{HarnessCatalog, Models},
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
    Logs,
}

impl InboxTab {
    pub const ALL: [Self; 4] = [Self::Issues, Self::PullRequests, Self::Security, Self::Logs];

    pub const fn index(self) -> usize {
        match self {
            Self::Issues => 0,
            Self::PullRequests => 1,
            Self::Security => 2,
            Self::Logs => 3,
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
            Self::Logs => "Logs",
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
    pub instructions_editor: Option<crate::widgets::editor::Editor>,
    pub instructions_focused: bool,
    pub target_cursor: usize,
    // Map the previous cursor to a stable target ID when refreshes reorder hosts.
    pub target_choices: Vec<String>,
    pub had_targets: bool,
    pub review: bool,
    pub security: bool,
    pub privacy_confirmation: bool,
    pub scroll: u16,
    pub scroll_max: u16,
    pub picker: Option<Picker>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PickerKind {
    Harness,
    Model,
}

/// The dropdown opened from a launch settings chip.
#[derive(Clone, Debug)]
pub(crate) struct Picker {
    pub kind: PickerKind,
    pub cursor: usize,
    pub filter: String,
    pub scroll: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PickerAction {
    Harness(Option<String>),
    Model(ModelSelection),
    /// Opens the custom model field.
    CustomModel,
}

#[derive(Clone, Debug)]
pub(crate) struct PickerItem {
    pub label: String,
    pub detail: String,
    pub current: bool,
    /// False for a harness not found on this machine; still selectable.
    pub available: bool,
    pub action: PickerAction,
}

impl LaunchSettings {
    // Empty name is the runtime's built-in prompt, not a saved profile.
    pub fn prompt_choices<'a>(&self, snapshot: &'a RuntimeSnapshot) -> Vec<&'a str> {
        let mut names = Vec::new();
        if self.review || self.security || snapshot.prompt_profiles.is_empty() {
            names.push("");
        }
        names.extend(snapshot.prompt_profiles.iter().map(String::as_str));
        names
    }

    pub fn builtin_prompt_label(&self) -> &'static str {
        if self.security {
            "Built-in private security review"
        } else if self.review {
            "Built-in PR review"
        } else {
            "Built-in default"
        }
    }

    pub fn harness_choices(&self) -> &'static [&'static str] {
        if self.security && self.backend == Some(BackendKind::Herdr) {
            return &["opencode", "claude", "codex"];
        }
        match self.backend {
            Some(BackendKind::Herdr) => &["opencode", "claude", "pi"],
            Some(BackendKind::Native) => &["opencode"],
            Some(BackendKind::Conductor) => &["claude", "codex", "cursor", "acp"],
            Some(BackendKind::Superset) | None => &[],
        }
    }

    /// The harness this launch runs, or empty when the backend picks it.
    pub fn effective_harness(&self) -> &str {
        if let Some(harness) = &self.options.harness {
            harness
        } else if self.backend == Some(BackendKind::Native) {
            // Native's configured name is an OpenCode subagent.
            "opencode"
        } else {
            &self.default_harness
        }
    }

    pub fn open_picker(&mut self, kind: PickerKind, catalog: &HarnessCatalog) {
        let cursor = self
            .picker_items(kind, catalog, "")
            .iter()
            .position(|item| item.current)
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind,
            cursor,
            filter: String::new(),
            scroll: 0,
        });
        self.model_editor = None;
        self.instructions_focused = false;
    }

    /// Choices for a picker, fuzzy-filtered by `filter` and best match first.
    pub fn picker_items(
        &self,
        kind: PickerKind,
        catalog: &HarnessCatalog,
        filter: &str,
    ) -> Vec<PickerItem> {
        let mut items = match kind {
            PickerKind::Harness => self.harness_items(catalog),
            PickerKind::Model => self.model_items(catalog),
        };
        let filter = filter.trim();
        if !filter.is_empty() {
            let matcher = SkimMatcherV2::default().smart_case();
            let mut scored: Vec<_> = items
                .into_iter()
                .filter_map(|item| Some((matcher.fuzzy_match(&item.label, filter)?, item)))
                .collect();
            scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
            items = scored.into_iter().map(|(_, item)| item).collect();
            if kind == PickerKind::Model && !items.iter().any(|item| item.label == filter) {
                items.push(PickerItem {
                    label: format!("Use \"{filter}\""),
                    detail: "custom model".into(),
                    current: false,
                    available: true,
                    action: PickerAction::Model(ModelSelection::Explicit(filter.into())),
                });
            }
        } else if kind == PickerKind::Model {
            items.push(PickerItem {
                label: "Custom model…".into(),
                detail: "type a model ID".into(),
                current: false,
                available: true,
                action: PickerAction::CustomModel,
            });
        }
        items
    }

    fn harness_items(&self, catalog: &HarnessCatalog) -> Vec<PickerItem> {
        let default = if self.security && self.backend == Some(BackendKind::Native) {
            "opencode"
        } else if self.default_harness.is_empty() {
            "backend default"
        } else {
            &self.default_harness
        };
        let mut items = vec![PickerItem {
            label: "Configured default".into(),
            detail: default.into(),
            current: self.options.harness.is_none(),
            available: true,
            action: PickerAction::Harness(None),
        }];
        items.extend(self.harness_choices().iter().map(|harness| {
            let found = catalog.installed(harness);
            PickerItem {
                label: (*harness).into(),
                detail: match found {
                    Some(true) => "installed",
                    Some(false) => "not found",
                    None => "",
                }
                .into(),
                current: self.options.harness.as_deref() == Some(*harness),
                available: found != Some(false),
                action: PickerAction::Harness(Some((*harness).into())),
            }
        }));
        items
    }

    fn model_items(&self, catalog: &HarnessCatalog) -> Vec<PickerItem> {
        let configured = self.model_label();
        let mut items = vec![
            PickerItem {
                label: "Configured default".into(),
                detail: configured
                    .strip_prefix("Configured default: ")
                    .unwrap_or(&configured)
                    .into(),
                current: self.options.model == ModelSelection::Inherit,
                available: true,
                action: PickerAction::Model(ModelSelection::Inherit),
            },
            PickerItem {
                label: "Harness default".into(),
                detail: self.effective_harness().into(),
                current: self.options.model == ModelSelection::HarnessDefault,
                available: true,
                action: PickerAction::Model(ModelSelection::HarnessDefault),
            },
        ];
        let explicit = match &self.options.model {
            ModelSelection::Explicit(model) => Some(model.as_str()),
            _ => None,
        };
        let detected = match catalog.models.get(self.effective_harness()) {
            Some(Models::Ready(models)) => models.as_slice(),
            _ => &[],
        };
        if let Some(model) = explicit.filter(|model| !detected.iter().any(|m| m == model)) {
            items.push(PickerItem {
                label: model.into(),
                detail: "custom".into(),
                current: true,
                available: true,
                action: PickerAction::Model(ModelSelection::Explicit(model.into())),
            });
        }
        items.extend(detected.iter().map(|model| PickerItem {
            label: model.clone(),
            detail: String::new(),
            current: explicit == Some(model.as_str()),
            available: true,
            action: PickerAction::Model(ModelSelection::Explicit(model.clone())),
        }));
        items
    }

    /// Why the model list is short, shown under the picker's items.
    pub fn picker_status(&self, kind: PickerKind, catalog: &HarnessCatalog) -> Option<String> {
        if kind != PickerKind::Model {
            return None;
        }
        let harness = self.effective_harness();
        if harness.is_empty() {
            return Some("The backend picks the harness; type a custom model ID".into());
        }
        match catalog.models.get(harness) {
            None | Some(Models::Loading) => Some(format!("Detecting {harness} models…")),
            Some(Models::Failed(error)) => Some(error.clone()),
            Some(Models::Ready(models)) if models.is_empty() => Some(format!(
                "{harness} has no model list; type a custom model ID"
            )),
            Some(Models::Ready(_)) => None,
        }
    }

    /// Applies a picker choice and closes the picker.
    pub fn choose(&mut self, action: PickerAction) {
        self.picker = None;
        match action {
            PickerAction::Harness(harness) => {
                if harness != self.options.harness {
                    self.options.harness = harness;
                    self.options.model = ModelSelection::HarnessDefault;
                }
            },
            PickerAction::Model(model) => self.options.model = model,
            PickerAction::CustomModel => {
                let text = match &self.options.model {
                    ModelSelection::Explicit(model) => model.clone(),
                    _ => String::new(),
                };
                let cursor = text.len();
                self.model_editor = Some(crate::widgets::editor::Editor { text, cursor });
            },
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
    /// Issue template source or a contextual PR/private preview; never editor input.
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
    pub away_overlay: Option<crate::away::AwayOverlay>,
    pub away_pending: Option<u64>,
    pub away_quit: bool,
    pub away_quit_visible: bool,
    pub layout: crate::LayoutMode,
    pub mouse: crate::mouse::MouseGeometry,
    pub route: Route,
    pub tab: InboxTab,
    pub inactive_lists: [InactiveList; 4],
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
    /// A one-line dialog any key dismisses, e.g. no agent to jump to.
    pub notice: Option<String>,
    /// Harnesses and models detected on this machine, cached for the session.
    pub harness_catalog: HarnessCatalog,
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
    /// When the TUI started; drives the launch logo fade over the activity graph.
    pub launched: Option<std::time::Instant>,
    pub activity_history_origin: Option<chrono::DateTime<chrono::Utc>>,
    pub toast: Option<Toast>,
    /// Items whose dispatch was submitted but has not returned yet.
    pub launching: std::collections::HashSet<String>,
    /// Highest runtime log sequence already considered for a toast.
    pub last_log_seq: u64,
}

pub(crate) struct Toast {
    pub level: LogLevel,
    pub message: String,
    pub expires: std::time::Instant,
}

impl Toast {
    pub fn new(level: LogLevel, message: impl Into<String>) -> Self {
        // Failures stay up longer: they usually need reading, not just noticing.
        let seconds = if level == LogLevel::Error {
            15
        } else {
            8
        };
        Self {
            level,
            message: message.into(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(seconds),
        }
    }
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

    /// Raises a toast for the newest unseen runtime log entry flagged for one.
    pub fn ingest_log(&mut self, snapshot: &RuntimeSnapshot) {
        if let Some(entry) = snapshot
            .log
            .iter()
            .rev()
            .take_while(|entry| entry.seq > self.last_log_seq)
            .find(|entry| entry.toast)
        {
            self.toast = Some(Toast::new(entry.level, entry.message.clone()));
        }
        if let Some(last) = snapshot.log.last() {
            self.last_log_seq = self.last_log_seq.max(last.seq);
        }
    }

    /// Drops an expired toast; returns whether the screen needs a redraw.
    pub fn expire_toast(&mut self) -> bool {
        let expired = self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.expires <= std::time::Instant::now());
        if expired {
            self.toast = None;
        }
        expired
    }

    /// Log entries for the Logs tab, newest first, filtered by the search query.
    pub fn log_entries<'a>(&self, snapshot: &'a RuntimeSnapshot) -> Vec<&'a LogEntry> {
        let query = self.search_query.to_lowercase();
        snapshot
            .log
            .iter()
            .rev()
            .filter(|entry| query.is_empty() || entry.message.to_lowercase().contains(&query))
            .collect()
    }

    /// Number of selectable rows in the current tab.
    pub fn list_len(&self, snapshot: &RuntimeSnapshot) -> usize {
        if self.tab == InboxTab::Logs {
            self.log_entries(snapshot).len()
        } else {
            self.rows(snapshot).len()
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
        if self.tab == InboxTab::Logs {
            // Log rows are not issues; only keep the cursor in range.
            self.selected = self.selected.min(self.list_len(snapshot).saturating_sub(1));
            return;
        }
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
            DispatchStage::Prompt => (overlay.settings.prompt_choices(snapshot).len(), None),
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
        if overlay.stage == DispatchStage::Prompt {
            let choices = overlay.settings.prompt_choices(snapshot);
            if let Some(index) = choices.iter().position(|name| *name == overlay.prompt.name) {
                overlay.cursor = index;
            }
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
