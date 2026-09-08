use std::{collections::HashMap, path::PathBuf, sync::Arc};

use agent_launcher_core::{BackendKind, RunState, RunSummary};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    fs,
    process::Child,
    sync::{Mutex, RwLock},
};
use uuid::Uuid;

use crate::{Error, Result};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub(crate) enum BackendSession {
    Superset {
        workspace_id: String,
        session_id: String,
        session_kind: String,
        host: Option<String>,
    },
    Native {
        base_url: String,
        remote: bool,
        #[serde(default)]
        target_id: Option<String>,
        #[serde(default)]
        remote_workspace_path: Option<String>,
        #[serde(default)]
        remote_port: Option<u16>,
        #[serde(default)]
        server_password: Option<String>,
        #[serde(default)]
        process_id: Option<u32>,
        #[serde(default)]
        initial_prompt: Option<Value>,
        #[serde(default)]
        pending_permission_id: Option<String>,
        #[serde(default)]
        pending_question_id: Option<String>,
        #[serde(default)]
        pending_question_count: usize,
        #[serde(default)]
        pending_question_prompt: Option<String>,
        #[serde(default)]
        last_message_id: Option<String>,
    },
    Herdr {
        workspace_id: String,
        pane_id: String,
        agent_name: String,
    },
    Conductor {
        workspace_id: String,
        session_id: String,
        deep_link: String,
        project_id: String,
        dispatch_key: String,
        #[serde(default)]
        message_offset: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RunRecord {
    pub summary: RunSummary,
    pub session: BackendSession,
    #[serde(default)]
    pub deletion: Option<DeletionState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DeletionState {
    pub force: bool,
    pub completed: bool,
    #[serde(default)]
    pub expected_inspection: Option<agent_launcher_core::WorktreeInspection>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingDeletion {
    pub run_id: String,
    pub backend: Option<BackendKind>,
    pub force: bool,
    pub completed: bool,
    pub expected_inspection: Option<agent_launcher_core::WorktreeInspection>,
}

pub(crate) struct ManagedChild {
    pub child: Child,
}

pub struct SessionRegistry {
    path: PathBuf,
    records: RwLock<HashMap<String, RunRecord>>,
    persist_lock: Mutex<()>,
    pub(crate) children: Mutex<HashMap<String, ManagedChild>>,
    pub(crate) provision_lock: Mutex<()>,
    pub(crate) conductor_dispatch_lock: Mutex<()>,
}

impl SessionRegistry {
    pub async fn load(path: Option<PathBuf>) -> Result<Arc<Self>> {
        let path = match path {
            Some(path) => path,
            None => dirs::data_local_dir()
                .ok_or(Error::DataDirectoryUnavailable)?
                .join("agent-launcher")
                .join("runner-sessions.json"),
        };
        let records = match fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<Vec<RunRecord>>(&bytes)?
                .into_iter()
                .map(|record| (record.summary.id.clone(), record))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Arc::new(Self {
            path,
            records: RwLock::new(records),
            persist_lock: Mutex::new(()),
            children: Mutex::new(HashMap::new()),
            provision_lock: Mutex::new(()),
            conductor_dispatch_lock: Mutex::new(()),
        }))
    }

    pub async fn summaries(&self) -> Vec<RunSummary> {
        let mut summaries = self
            .records
            .read()
            .await
            .values()
            .filter(|record| record.deletion.is_none())
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| right.started_at.cmp(&left.started_at));
        summaries
    }

    pub async fn summary(&self, run_id: &str) -> Result<RunSummary> {
        Ok(self.get(run_id).await?.summary)
    }

    pub async fn pending_deletions(&self) -> Vec<PendingDeletion> {
        let mut deletions = self
            .records
            .read()
            .await
            .iter()
            .filter_map(|(run_id, record)| {
                record.deletion.as_ref().map(|deletion| PendingDeletion {
                    run_id: run_id.clone(),
                    backend: record
                        .summary
                        .workspace
                        .as_ref()
                        .map(|workspace| workspace.backend),
                    force: deletion.force,
                    completed: deletion.completed,
                    expected_inspection: deletion.expected_inspection.clone(),
                })
            })
            .collect::<Vec<_>>();
        deletions.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        deletions
    }

    pub async fn has_other_workspace_owner(&self, run_id: &str) -> bool {
        let records = self.records.read().await;
        let Some(workspace) = records
            .get(run_id)
            .and_then(|record| record.summary.workspace.as_ref())
        else {
            return false;
        };
        records.iter().any(|(other_id, record)| {
            other_id != run_id
                && record.deletion.is_none()
                && matches!(
                    record.summary.state,
                    RunState::Provisioning
                        | RunState::Starting
                        | RunState::Running
                        | RunState::NeedsInput
                        | RunState::Idle
                        | RunState::Failed
                        | RunState::Disconnected
                )
                && record.summary.workspace.as_ref().is_some_and(|other| {
                    workspace.backend == other.backend
                        && workspace.host == other.host
                        && (workspace.id == other.id || workspace.path == other.path)
                })
        })
    }

    pub async fn finalize_deletion(&self, run_id: &str) -> Result<()> {
        self.remove(run_id).await.map(|_| ())
    }

    pub(crate) async fn insert(&self, record: RunRecord) -> Result<()> {
        let run_id = record.summary.id.clone();
        let previous = self.records.write().await.insert(run_id.clone(), record);
        if let Err(error) = self.persist().await {
            let mut records = self.records.write().await;
            if let Some(previous) = previous {
                records.insert(run_id, previous);
            } else {
                records.remove(&run_id);
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn get(&self, run_id: &str) -> Result<RunRecord> {
        self.records
            .read()
            .await
            .get(run_id)
            .cloned()
            .ok_or_else(|| Error::RunNotFound(run_id.to_string()))
    }

    pub(crate) async fn conductor_dispatch(&self, dispatch_key: &str) -> Option<RunRecord> {
        self.records.read().await.values().find_map(|record| {
            (record.summary.state.is_active()
                && matches!(
                    &record.session,
                    BackendSession::Conductor {
                        dispatch_key: existing,
                        ..
                    } if existing == dispatch_key
                ))
            .then(|| record.clone())
        })
    }

    pub(crate) async fn native_target_active_runs(
        &self,
        target_id: &str,
        destination: &str,
    ) -> usize {
        self.records
            .read()
            .await
            .values()
            .filter(|record| {
                let BackendSession::Native {
                    remote: true,
                    target_id: persisted_target,
                    ..
                } = &record.session
                else {
                    return false;
                };
                let belongs_to_target = persisted_target.as_deref() == Some(target_id)
                    || (persisted_target.is_none()
                        && record
                            .summary
                            .workspace
                            .as_ref()
                            .and_then(|workspace| workspace.host.as_deref())
                            == Some(destination));
                belongs_to_target
                    && (record.deletion.is_some()
                        || matches!(
                            record.summary.state,
                            RunState::Provisioning
                                | RunState::Starting
                                | RunState::Running
                                | RunState::NeedsInput
                                | RunState::Idle
                                | RunState::Failed
                                | RunState::Disconnected
                        ))
            })
            .count()
    }

    pub(crate) async fn update_summary(&self, summary: RunSummary) -> Result<()> {
        let mut records = self.records.write().await;
        let record = records
            .get_mut(&summary.id)
            .ok_or_else(|| Error::RunNotFound(summary.id.clone()))?;
        if record.deletion.is_some() {
            return Err(Error::RunNotFound(summary.id));
        }
        let previous = record.summary.clone();
        record.summary = summary;
        drop(records);
        if let Err(error) = self.persist().await {
            if let Some(record) = self.records.write().await.get_mut(&previous.id) {
                record.summary = previous;
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn update(&self, record: RunRecord) -> Result<()> {
        let mut records = self.records.write().await;
        let Some(existing) = records.get(&record.summary.id) else {
            return Err(Error::RunNotFound(record.summary.id));
        };
        if existing.deletion.is_some() {
            return Err(Error::RunNotFound(record.summary.id));
        }
        let run_id = record.summary.id.clone();
        let previous = records.insert(run_id.clone(), record);
        drop(records);
        if let Err(error) = self.persist().await {
            if let Some(previous) = previous {
                self.records.write().await.insert(run_id, previous);
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn begin_deletion(
        &self,
        run_id: &str,
        force: bool,
        expected_inspection: Option<agent_launcher_core::WorktreeInspection>,
    ) -> Result<()> {
        let mut records = self.records.write().await;
        let record = records
            .get_mut(run_id)
            .ok_or_else(|| Error::RunNotFound(run_id.to_owned()))?;
        let previous = record.deletion.clone();
        record.deletion = Some(DeletionState {
            force,
            completed: false,
            expected_inspection,
        });
        drop(records);
        if let Err(error) = self.persist().await {
            if let Some(record) = self.records.write().await.get_mut(run_id) {
                record.deletion = previous;
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) async fn complete_deletion(&self, run_id: &str) -> Result<()> {
        let mut records = self.records.write().await;
        let record = records
            .get_mut(run_id)
            .ok_or_else(|| Error::RunNotFound(run_id.to_owned()))?;
        let previous = record.deletion.clone();
        let deletion = record
            .deletion
            .as_mut()
            .ok_or_else(|| Error::InvalidRequest(format!("run {run_id} is not being deleted")))?;
        deletion.completed = true;
        drop(records);
        if let Err(error) = self.persist().await {
            if let Some(record) = self.records.write().await.get_mut(run_id) {
                record.deletion = previous;
            }
            return Err(error);
        }
        Ok(())
    }

    pub async fn cancel_deletion(&self, run_id: &str) -> Result<()> {
        let mut records = self.records.write().await;
        let record = records
            .get_mut(run_id)
            .ok_or_else(|| Error::RunNotFound(run_id.to_owned()))?;
        let previous = record.deletion.take();
        drop(records);
        if let Err(error) = self.persist().await {
            if let Some(record) = self.records.write().await.get_mut(run_id) {
                record.deletion = previous;
            }
            return Err(error);
        }
        Ok(())
    }

    pub async fn fail_deletion(&self, run_id: &str, message: String) -> Result<RunSummary> {
        let mut records = self.records.write().await;
        let record = records
            .get_mut(run_id)
            .ok_or_else(|| Error::RunNotFound(run_id.to_owned()))?;
        let previous = record.clone();
        record.deletion = None;
        record.summary.state = RunState::Failed;
        record.summary.message = Some(message);
        record.summary.updated_at = chrono::Utc::now();
        let summary = record.summary.clone();
        drop(records);
        if let Err(error) = self.persist().await {
            self.records
                .write()
                .await
                .insert(run_id.to_owned(), previous);
            return Err(error);
        }
        Ok(summary)
    }

    pub(crate) async fn deletion_pending(&self, run_id: &str) -> bool {
        self.records.read().await.get(run_id).is_some_and(|record| {
            record
                .deletion
                .as_ref()
                .is_some_and(|deletion| deletion.completed)
        })
    }

    pub(crate) async fn deletion_in_progress(&self, run_id: &str) -> bool {
        self.records
            .read()
            .await
            .get(run_id)
            .is_some_and(|record| record.deletion.is_some())
    }

    pub(crate) async fn remove(&self, run_id: &str) -> Result<RunRecord> {
        let record = self
            .records
            .write()
            .await
            .remove(run_id)
            .ok_or_else(|| Error::RunNotFound(run_id.to_owned()))?;
        if let Err(error) = self.persist().await {
            self.records
                .write()
                .await
                .insert(run_id.to_owned(), record.clone());
            return Err(error);
        }
        Ok(record)
    }

    pub(crate) async fn set_state(
        &self,
        run_id: &str,
        state: RunState,
        message: Option<String>,
    ) -> Result<RunSummary> {
        let mut record = self.get(run_id).await?;
        record.summary.state = state;
        record.summary.message = message;
        record.summary.updated_at = chrono::Utc::now();
        let summary = record.summary.clone();
        self.update(record).await?;
        Ok(summary)
    }

    async fn persist(&self) -> Result<()> {
        let _guard = self.persist_lock.lock().await;
        let mut records = self
            .records
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.summary.id.cmp(&right.summary.id));
        let bytes = serde_json::to_vec_pretty(&records)?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::InvalidRequest("registry path has no parent".into()))?;
        fs::create_dir_all(parent).await?;
        let temporary = self.path.with_extension(format!("tmp-{}", Uuid::new_v4()));
        fs::write(&temporary, bytes).await?;
        #[cfg(unix)]
        fs::set_permissions(&temporary, {
            use std::os::unix::fs::PermissionsExt;
            std::fs::Permissions::from_mode(0o600)
        })
        .await?;
        fs::rename(&temporary, &self.path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::{BackendKind, RunState, WorkspaceRef, WorktreeInspection};
    use chrono::Utc;

    use super::*;

    fn conductor_record(id: &str, state: RunState, dispatch_key: &str) -> RunRecord {
        let now = Utc::now();
        RunRecord {
            summary: RunSummary {
                id: id.into(),
                issue_key: format!("issue-{id}"),
                workspace: Some(WorkspaceRef {
                    backend: BackendKind::Conductor,
                    id: format!("workspace-{id}"),
                    host: None,
                    path: None,
                    branch: format!("agent/{id}"),
                }),
                agent: "codex".into(),
                state,
                message: None,
                session_id: Some(format!("session-{id}")),
                started_at: now,
                updated_at: now,
            },
            session: BackendSession::Conductor {
                workspace_id: format!("workspace-{id}"),
                session_id: format!("session-{id}"),
                deep_link: format!("conductor://workspace/{id}"),
                project_id: "project-1".into(),
                dispatch_key: dispatch_key.into(),
                message_offset: 0,
            },
            deletion: None,
        }
    }

    fn native_record(
        id: &str,
        state: RunState,
        target_id: Option<&str>,
        destination: &str,
    ) -> RunRecord {
        let mut record = conductor_record(id, state, "unused");
        record.summary.workspace = Some(WorkspaceRef {
            backend: BackendKind::Native,
            id: format!("workspace-{id}"),
            host: Some(destination.into()),
            path: Some(format!("/srv/workspaces/{id}").into()),
            branch: format!("agent/{id}"),
        });
        record.session = BackendSession::Native {
            base_url: "http://127.0.0.1:31234/".into(),
            remote: true,
            target_id: target_id.map(str::to_owned),
            remote_workspace_path: Some(format!("/srv/workspaces/{id}")),
            remote_port: Some(38123),
            server_password: Some("secret".into()),
            process_id: None,
            initial_prompt: None,
            pending_permission_id: None,
            pending_question_id: None,
            pending_question_count: 0,
            pending_question_prompt: None,
            last_message_id: None,
        };
        record
    }

    #[tokio::test]
    async fn conductor_dedupe_only_returns_active_runs() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should load");
        registry
            .insert(conductor_record("completed", RunState::Completed, "same"))
            .await
            .expect("completed record should persist");
        assert!(registry.conductor_dispatch("same").await.is_none());
        registry
            .insert(conductor_record("running", RunState::Running, "same"))
            .await
            .expect("running record should persist");
        assert_eq!(
            registry
                .conductor_dispatch("same")
                .await
                .expect("active record should dedupe")
                .summary
                .id,
            "running"
        );
        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn removed_records_stay_removed_and_cannot_be_updated() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should load");
        let record = conductor_record("deleted", RunState::Running, "dispatch");
        registry
            .insert(record.clone())
            .await
            .expect("record should persist");

        registry
            .remove("deleted")
            .await
            .expect("record should be removed");

        assert!(matches!(
            registry.update(record).await,
            Err(Error::RunNotFound(id)) if id == "deleted"
        ));
        let reloaded = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should reload");
        assert!(reloaded.summaries().await.is_empty());
        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn deletion_tombstones_survive_restart_until_finalized() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should load");
        registry
            .insert(conductor_record("deleted", RunState::Running, "dispatch"))
            .await
            .expect("record should persist");
        let expected = WorktreeInspection {
            has_uncommitted_changes: true,
            fingerprint: Some("confirmed-state".into()),
            ..WorktreeInspection::default()
        };
        registry
            .begin_deletion("deleted", true, Some(expected.clone()))
            .await
            .expect("deletion intent should persist");
        drop(registry);

        let reloaded = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should reload");
        assert!(reloaded.summaries().await.is_empty());
        assert_eq!(reloaded.pending_deletions().await, [PendingDeletion {
            run_id: "deleted".into(),
            backend: Some(BackendKind::Conductor),
            force: true,
            completed: false,
            expected_inspection: Some(expected),
        }]);
        assert!(reloaded.deletion_in_progress("deleted").await);
        assert!(!reloaded.deletion_pending("deleted").await);
        reloaded
            .cancel_deletion("deleted")
            .await
            .expect("incomplete deletion should be recoverable");
        assert_eq!(reloaded.summaries().await.len(), 1);
        reloaded
            .begin_deletion("deleted", true, None)
            .await
            .expect("deletion should restart");
        reloaded
            .complete_deletion("deleted")
            .await
            .expect("completed tombstone should persist");
        drop(reloaded);

        let reloaded = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should reload again");
        assert_eq!(reloaded.pending_deletions().await, [PendingDeletion {
            run_id: "deleted".into(),
            backend: Some(BackendKind::Conductor),
            force: true,
            completed: true,
            expected_inspection: None,
        }]);
        assert!(reloaded.deletion_in_progress("deleted").await);
        assert!(reloaded.deletion_pending("deleted").await);
        reloaded
            .finalize_deletion("deleted")
            .await
            .expect("tombstone should be removable");
        assert!(reloaded.pending_deletions().await.is_empty());
        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn deletion_recovery_detects_another_live_workspace_owner() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone())).await.unwrap();
        let mut target = conductor_record("target", RunState::Failed, "target");
        let mut other = conductor_record("other", RunState::Disconnected, "other");
        let workspace = WorkspaceRef {
            backend: BackendKind::Native,
            id: "shared-workspace".into(),
            host: Some("buildbox".into()),
            path: Some("/srv/shared-workspace".into()),
            branch: "agent/target".into(),
        };
        target.summary.workspace = Some(workspace.clone());
        other.summary.workspace = Some(WorkspaceRef {
            branch: "agent/other".into(),
            ..workspace
        });
        registry.insert(target).await.unwrap();
        registry.insert(other).await.unwrap();
        registry.begin_deletion("target", true, None).await.unwrap();

        assert!(registry.has_other_workspace_owner("target").await);
        registry.remove("other").await.unwrap();
        assert!(!registry.has_other_workspace_owner("target").await);
        let _ = fs::remove_file(path).await;
    }

    #[test]
    fn loads_sessions_persisted_before_cursor_fields_existed() {
        let native: BackendSession = serde_json::from_value(serde_json::json!({
            "backend": "native",
            "base_url": "http://127.0.0.1:1234/",
            "remote": false
        }))
        .expect("legacy native session should load");
        assert!(matches!(native, BackendSession::Native {
            target_id: None,
            remote_workspace_path: None,
            remote_port: None,
            server_password: None,
            process_id: None,
            initial_prompt: None,
            pending_permission_id: None,
            pending_question_id: None,
            pending_question_count: 0,
            pending_question_prompt: None,
            last_message_id: None,
            ..
        }));

        let native: BackendSession = serde_json::from_value(serde_json::json!({
            "backend": "native",
            "base_url": "http://127.0.0.1:1234/",
            "remote": false,
            "process_id": 1234,
            "pending_question_id": "que_1",
            "pending_question_count": 2,
            "pending_question_prompt": "Choose a target"
        }))
        .expect("native question state should load");
        let persisted = serde_json::to_value(native).expect("native question state should persist");
        assert_eq!(persisted["pending_question_id"], "que_1");
        assert_eq!(persisted["pending_question_count"], 2);
        assert_eq!(persisted["pending_question_prompt"], "Choose a target");
        assert_eq!(persisted["process_id"], 1234);

        let conductor: BackendSession = serde_json::from_value(serde_json::json!({
            "backend": "conductor",
            "workspace_id": "workspace-1",
            "session_id": "session-1",
            "deep_link": "conductor://workspace/1",
            "project_id": "project-1",
            "dispatch_key": "dispatch-1"
        }))
        .expect("legacy Conductor session should load");
        assert!(matches!(conductor, BackendSession::Conductor {
            message_offset: 0,
            ..
        }));
    }

    #[tokio::test]
    async fn native_process_id_survives_registry_reload() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should load");
        let mut record = conductor_record("native", RunState::Running, "unused");
        record.session = BackendSession::Native {
            base_url: "http://127.0.0.1:31234/".into(),
            remote: true,
            target_id: Some("builder".into()),
            remote_workspace_path: Some("~/.local/share/agent-launcher/workspaces/issue".into()),
            remote_port: Some(38123),
            server_password: Some("secret".into()),
            process_id: Some(4321),
            initial_prompt: Some(serde_json::json!({
                "messageID": "msg_initial",
                "parts": [{"type": "text", "text": "Fix it"}]
            })),
            pending_permission_id: None,
            pending_question_id: None,
            pending_question_count: 0,
            pending_question_prompt: None,
            last_message_id: None,
        };
        registry
            .insert(record)
            .await
            .expect("native record should persist");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).await.unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(registry);

        let reloaded = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should reload");
        assert!(matches!(
            reloaded
                .get("native")
                .await
                .expect("record should exist")
                .session,
            BackendSession::Native {
                target_id: Some(ref target_id),
                process_id: Some(4321),
                initial_prompt: Some(ref initial_prompt),
                remote_workspace_path: Some(ref path),
                remote_port: Some(38123),
                server_password: Some(ref password),
                ..
            } if target_id == "builder" && initial_prompt["messageID"] == "msg_initial" && path == "~/.local/share/agent-launcher/workspaces/issue" && password == "secret"
        ));
        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn native_capacity_counts_resumable_and_deleting_runs() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone())).await.unwrap();
        registry
            .insert(native_record(
                "running",
                RunState::Running,
                Some("builder"),
                "new-host",
            ))
            .await
            .unwrap();
        registry
            .insert(native_record(
                "failed",
                RunState::Failed,
                Some("builder"),
                "new-host",
            ))
            .await
            .unwrap();
        registry
            .insert(native_record(
                "legacy",
                RunState::Disconnected,
                None,
                "new-host",
            ))
            .await
            .unwrap();
        registry
            .insert(native_record(
                "cancelled",
                RunState::Cancelled,
                Some("builder"),
                "new-host",
            ))
            .await
            .unwrap();
        registry
            .begin_deletion("cancelled", true, None)
            .await
            .unwrap();
        registry
            .insert(native_record(
                "other",
                RunState::Running,
                Some("other"),
                "other-host",
            ))
            .await
            .unwrap();

        assert_eq!(
            registry
                .native_target_active_runs("builder", "new-host")
                .await,
            4
        );
        assert_eq!(
            registry
                .native_target_active_runs("other", "other-host")
                .await,
            1
        );
        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn failed_deletion_becomes_visible_and_retains_capacity() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone())).await.unwrap();
        registry
            .insert(native_record(
                "failed-delete",
                RunState::Cancelled,
                Some("builder"),
                "builder",
            ))
            .await
            .unwrap();
        registry
            .begin_deletion("failed-delete", true, None)
            .await
            .unwrap();

        let summary = registry
            .fail_deletion("failed-delete", "host unavailable".into())
            .await
            .unwrap();
        assert_eq!(summary.state, RunState::Failed);
        assert_eq!(registry.summaries().await.len(), 1);
        assert_eq!(
            registry
                .native_target_active_runs("builder", "builder")
                .await,
            1
        );
        let _ = fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn concurrent_persistence_keeps_the_latest_complete_snapshot() {
        let path = std::env::temp_dir().join(format!("runner-registry-{}.json", Uuid::new_v4()));
        let registry = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should load");
        let inserts = (0..32).map(|index| {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move {
                registry
                    .insert(conductor_record(
                        &format!("run-{index}"),
                        RunState::Running,
                        &format!("dispatch-{index}"),
                    ))
                    .await
            })
        });
        for insert in inserts {
            insert
                .await
                .expect("insert task should complete")
                .expect("record should persist");
        }
        let reloaded = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should reload");
        assert_eq!(reloaded.summaries().await.len(), 32);
        let _ = fs::remove_file(path).await;
    }
}
