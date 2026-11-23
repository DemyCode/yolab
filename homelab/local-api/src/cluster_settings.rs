use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::routers::apps::BackupPolicy;
use crate::store::{Store, StoreError};

const BACKUP_SCHEDULE: &str = "backup.schedule";
const BACKUP_BUDGET: &str = "backup.budget_bytes";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ClusterSettings {
    pub backup_schedule: String,
    pub backup_budget_bytes: Option<u64>,
}

impl Default for ClusterSettings {
    fn default() -> Self {
        let policy = BackupPolicy::default();
        ClusterSettings {
            backup_schedule: policy.schedule,
            backup_budget_bytes: policy.budget_bytes,
        }
    }
}

impl ClusterSettings {
    pub fn read(store: &Store) -> Result<Self, StoreError> {
        let built_in = ClusterSettings::default();
        Ok(ClusterSettings {
            backup_schedule: store
                .setting::<String>(BACKUP_SCHEDULE)?
                .unwrap_or(built_in.backup_schedule),
            backup_budget_bytes: store
                .setting::<Option<u64>>(BACKUP_BUDGET)?
                .unwrap_or(built_in.backup_budget_bytes),
        })
    }

    pub fn checked(self) -> Result<Self, String> {
        crate::cron::Cron::parse(&self.backup_schedule)
            .map_err(|e| format!("That backup schedule is not valid: {e}"))?;
        crate::routers::backups::checked_budget(self.backup_budget_bytes)
            .map_err(|e| format!("That backup space is not valid: {e}"))?;
        Ok(self)
    }

    pub fn write_changes(&self, store: &mut Store) -> Result<bool, StoreError> {
        let now = ClusterSettings::read(store)?;
        let mut changed = false;
        if now.backup_schedule != self.backup_schedule {
            store.set_setting(BACKUP_SCHEDULE, &self.backup_schedule)?;
            changed = true;
        }
        if now.backup_budget_bytes != self.backup_budget_bytes {
            store.set_setting(BACKUP_BUDGET, &self.backup_budget_bytes)?;
            changed = true;
        }
        Ok(changed)
    }

    pub fn new_service_backup(&self) -> BackupPolicy {
        BackupPolicy {
            enabled: true,
            schedule: self.backup_schedule.clone(),
            budget_bytes: self.backup_budget_bytes,
        }
    }
}

pub fn current() -> anyhow::Result<ClusterSettings> {
    Ok(ClusterSettings::read(&crate::store::locked())?)
}

fn refused(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

pub async fn get() -> Response {
    match current() {
        Ok(settings) => Json(settings).into_response(),
        Err(e) => crate::error::AppError(e).into_response(),
    }
}

pub async fn put(Json(body): Json<ClusterSettings>) -> Response {
    let wanted = match body.checked() {
        Ok(s) => s,
        Err(why) => return refused(StatusCode::BAD_REQUEST, why),
    };
    let saved = (|| -> anyhow::Result<()> {
        let mut store = crate::store::locked();
        if wanted.write_changes(&mut store)? {
            store.persist(&crate::store::default_path())?;
            crate::runtime::wake("store-sync");
        }
        Ok(())
    })();
    match saved {
        Ok(()) => Json(wanted).into_response(),
        Err(e) => crate::error::AppError(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn monthly_10gb() -> ClusterSettings {
        ClusterSettings {
            backup_schedule: "0 3 1 * *".into(),
            backup_budget_bytes: Some(10 * GB),
        }
    }

    #[test]
    fn a_cluster_nobody_configured_backs_new_services_up_the_way_it_always_did() {
        let settings = ClusterSettings::read(&Store::new("node1")).unwrap();
        assert_eq!(settings.new_service_backup(), BackupPolicy::default());
    }

    #[test]
    fn a_new_service_backs_up_on_the_cluster_schedule_within_the_cluster_space() {
        let mut store = Store::new("node1");
        monthly_10gb().write_changes(&mut store).unwrap();
        let policy = ClusterSettings::read(&store).unwrap().new_service_backup();
        assert!(policy.enabled);
        assert_eq!(policy.schedule, "0 3 1 * *");
        assert_eq!(policy.budget_bytes, Some(10 * GB));
    }

    #[test]
    fn settings_saved_on_one_machine_apply_on_the_other_once_they_sync() {
        let mut a = Store::new("node1");
        monthly_10gb().write_changes(&mut a).unwrap();
        let mut b = Store::new("node2");
        b.merge(&mut a).unwrap();
        assert_eq!(ClusterSettings::read(&b).unwrap(), monthly_10gb());
    }

    #[test]
    fn going_back_to_letting_yolab_choose_the_space_is_remembered() {
        let mut store = Store::new("node1");
        monthly_10gb().write_changes(&mut store).unwrap();
        let automatic = ClusterSettings {
            backup_budget_bytes: None,
            ..monthly_10gb()
        };
        automatic.write_changes(&mut store).unwrap();
        assert_eq!(
            ClusterSettings::read(&store).unwrap().backup_budget_bytes,
            None
        );
    }

    #[test]
    fn saving_what_is_already_there_writes_nothing() {
        let mut store = Store::new("node1");
        assert!(monthly_10gb().write_changes(&mut store).unwrap());
        assert!(!monthly_10gb().write_changes(&mut store).unwrap());
        assert!(!ClusterSettings::read(&store)
            .unwrap()
            .write_changes(&mut store)
            .unwrap());
    }

    #[test]
    fn only_the_setting_that_changed_is_written() {
        let mut store = Store::new("node1");
        ClusterSettings {
            backup_budget_bytes: Some(10 * GB),
            ..ClusterSettings::default()
        }
        .write_changes(&mut store)
        .unwrap();
        assert_eq!(
            store.settings().unwrap().keys().collect::<Vec<_>>(),
            vec![BACKUP_BUDGET]
        );
    }

    #[test]
    fn a_schedule_that_is_not_a_cron_line_is_refused() {
        let err = ClusterSettings {
            backup_schedule: "every month".into(),
            ..monthly_10gb()
        }
        .checked()
        .unwrap_err();
        assert!(err.contains("schedule"), "{err}");
    }

    #[test]
    fn a_space_of_nothing_is_refused() {
        let err = ClusterSettings {
            backup_budget_bytes: Some(0),
            ..monthly_10gb()
        }
        .checked()
        .unwrap_err();
        assert!(err.contains("space"), "{err}");
    }

    #[test]
    fn monthly_backups_within_10_gb_are_accepted() {
        assert_eq!(monthly_10gb().checked(), Ok(monthly_10gb()));
    }
}
