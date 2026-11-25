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
    pub backup_budget_bytes: u64,
}

impl Default for ClusterSettings {
    fn default() -> Self {
        let policy = BackupPolicy::default();
        ClusterSettings {
            backup_schedule: policy.schedule,
            backup_budget_bytes: policy
                .budget_bytes
                .unwrap_or(crate::backup::points::DEFAULT_BUDGET),
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
                .setting::<u64>(BACKUP_BUDGET)?
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
        let new_schedule = now.backup_schedule != self.backup_schedule;
        let new_budget = now.backup_budget_bytes != self.backup_budget_bytes;
        let schedule = if new_schedule {
            store.set_setting(BACKUP_SCHEDULE, &self.backup_schedule)
        } else {
            Ok(())
        };
        schedule
            .and_then(|()| {
                if new_budget {
                    store.set_setting(BACKUP_BUDGET, &self.backup_budget_bytes)
                } else {
                    Ok(())
                }
            })
            .map(|()| new_schedule || new_budget)
    }

    pub fn new_service_backup(&self) -> BackupPolicy {
        BackupPolicy {
            enabled: true,
            schedule: self.backup_schedule.clone(),
            budget_bytes: Some(self.backup_budget_bytes),
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

    const GB: u64 = 1_000_000_000;

    fn monthly_10gb() -> ClusterSettings {
        ClusterSettings {
            backup_schedule: "0 3 1 * *".into(),
            backup_budget_bytes: 10 * GB,
        }
    }

    #[test]
    fn a_cluster_nobody_configured_backs_new_services_up_monthly_within_10_gb() {
        let settings = ClusterSettings::read(&Store::new("node1")).unwrap();
        assert_eq!(settings.new_service_backup(), BackupPolicy::default());
        assert_eq!(settings.backup_schedule, "0 3 1 * *");
        assert_eq!(settings.backup_budget_bytes, 10_000_000_000);
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
    fn saving_the_defaults_on_a_cluster_nobody_configured_writes_nothing() {
        let mut store = Store::new("node1");
        assert!(!monthly_10gb().write_changes(&mut store).unwrap());
    }

    #[test]
    fn saving_what_is_already_there_writes_nothing() {
        let mut store = Store::new("node1");
        let weekly_20gb = ClusterSettings {
            backup_schedule: "0 3 * * 0".into(),
            backup_budget_bytes: 20 * GB,
        };
        assert!(weekly_20gb.write_changes(&mut store).unwrap());
        assert!(!weekly_20gb.write_changes(&mut store).unwrap());
        assert!(!ClusterSettings::read(&store)
            .unwrap()
            .write_changes(&mut store)
            .unwrap());
    }

    #[test]
    fn only_the_setting_that_changed_is_written() {
        let mut store = Store::new("node1");
        ClusterSettings {
            backup_budget_bytes: 20 * GB,
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
            backup_budget_bytes: 0,
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

    #[test]
    fn an_unreadable_schedule_or_space_is_an_error_not_the_default() {
        let mut bad_schedule = Store::new("node1");
        bad_schedule.set_setting(BACKUP_SCHEDULE, &5u64).unwrap();
        assert!(ClusterSettings::read(&bad_schedule).is_err());
        assert!(monthly_10gb().write_changes(&mut bad_schedule).is_err());

        let mut bad_budget = Store::new("node1");
        bad_budget.set_setting(BACKUP_BUDGET, &"lots").unwrap();
        assert!(ClusterSettings::read(&bad_budget).is_err());
    }

    async fn answer(res: Response) -> (StatusCode, serde_json::Value) {
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn the_settings_page_shows_the_built_in_settings_on_a_fresh_cluster() {
        assert_eq!(
            answer(get().await).await,
            (
                StatusCode::OK,
                serde_json::to_value(ClusterSettings::default()).unwrap()
            )
        );
    }

    #[tokio::test]
    async fn a_store_that_cannot_be_read_fails_the_settings_page_rather_than_showing_defaults() {
        crate::store::locked()
            .set_setting(BACKUP_SCHEDULE, &5u64)
            .unwrap();
        assert!(current().is_err());
        assert_eq!(
            answer(get().await).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            answer(put(Json(monthly_10gb())).await).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn an_invalid_setting_is_refused_with_the_reason() {
        let (status, body) = answer(
            put(Json(ClusterSettings {
                backup_schedule: "every month".into(),
                ..monthly_10gb()
            }))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap().contains("schedule"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn saved_settings_are_kept_on_disk_and_saving_them_again_changes_nothing() {
        let weekly = ClusterSettings {
            backup_schedule: "0 3 * * 0".into(),
            ..monthly_10gb()
        };
        let expected = (StatusCode::OK, serde_json::to_value(&weekly).unwrap());
        assert_eq!(answer(put(Json(weekly.clone())).await).await, expected);
        let on_disk = Store::open("node1", &crate::store::default_path()).unwrap();
        assert_eq!(ClusterSettings::read(&on_disk).unwrap(), weekly);
        assert_eq!(answer(put(Json(weekly.clone())).await).await, expected);
        assert_eq!(current().unwrap(), weekly);
    }

    #[tokio::test]
    async fn a_change_that_cannot_be_saved_to_disk_is_an_error() {
        let path = crate::store::default_path();
        let blocker = path.with_file_name("store-debug.json");
        std::fs::create_dir_all(blocker.join("inside")).unwrap();
        let status = answer(
            put(Json(ClusterSettings {
                backup_budget_bytes: 20 * GB,
                ..monthly_10gb()
            }))
            .await,
        )
        .await
        .0;
        std::fs::remove_dir_all(&blocker).unwrap();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
