use std::{collections::HashMap, path::PathBuf, sync::Arc};

use agent_launcher_core::{RunState, RunSummary};
use serde::{Deserialize, Serialize};
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
        process_id: Option<u32>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingDeletion {
    pub run_id: String,
    pub completed: bool,
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
                    completed: deletion.completed,
                })
            })
            .collect::<Vec<_>>();
        deletions.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        deletions
    }

    pub async fn finalize_deletion(&self, run_id: &str) -> Result<()> {
        self.remove(run_id).await.map(|_| ())
    }

    pub(crate) async fn insert(&self, record: RunRecord) -> Result<()> {
        self.records
            .write()
            .await
            .insert(record.summary.id.clone(), record);
        self.persist().await
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

    pub(crate) async fn begin_deletion(&self, run_id: &str, force: bool) -> Result<()> {
        let mut records = self.records.write().await;
        let record = records
            .get_mut(run_id)
            .ok_or_else(|| Error::RunNotFound(run_id.to_owned()))?;
        let previous = record.deletion.clone();
        record.deletion = Some(DeletionState {
            force,
            completed: false,
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

    pub(crate) async fn deletion_pending(&self, run_id: &str) -> bool {
        self.records.read().await.get(run_id).is_some_and(|record| {
            record
                .deletion
                .as_ref()
                .is_some_and(|state| state.completed)
        })
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
        fs::rename(&temporary, &self.path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agent_launcher_core::{BackendKind, RunState, WorkspaceRef};
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
        registry
            .begin_deletion("deleted", true)
            .await
            .expect("deletion intent should persist");
        drop(registry);

        let reloaded = SessionRegistry::load(Some(path.clone()))
            .await
            .expect("registry should reload");
        assert!(reloaded.summaries().await.is_empty());
        assert_eq!(reloaded.pending_deletions().await, [PendingDeletion {
            run_id: "deleted".into(),
            completed: false,
        }]);
        reloaded
            .cancel_deletion("deleted")
            .await
            .expect("incomplete deletion should be recoverable");
        assert_eq!(reloaded.summaries().await.len(), 1);
        reloaded
            .begin_deletion("deleted", true)
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
            completed: true,
        }]);
        reloaded
            .finalize_deletion("deleted")
            .await
            .expect("tombstone should be removable");
        assert!(reloaded.pending_deletions().await.is_empty());
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
            process_id: None,
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
            remote: false,
            process_id: Some(4321),
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
                process_id: Some(4321),
                ..
            }
        ));
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
