use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::routers::backup::VolumeSnapshot;

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    Use,
    Replace,
    Restore,
    Empty,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Choices {
    #[serde(default)]
    pub folders: BTreeMap<String, Choice>,
    #[serde(default)]
    pub data: BTreeMap<String, Choice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Keep,
    Replace,
    Restore,
    CreateEmpty,
}

pub(crate) fn step(exists: bool, backed_up: bool, chosen: Option<Choice>, unsaid: Choice) -> Step {
    match (exists, chosen.unwrap_or(unsaid)) {
        (true, Choice::Replace) if backed_up => Step::Replace,
        (true, _) => Step::Keep,
        (false, Choice::Empty) => Step::CreateEmpty,
        (false, _) if backed_up => Step::Restore,
        (false, _) => Step::CreateEmpty,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedFolder {
    pub name: String,
    pub title: String,
    pub capacity: String,
    pub snapshot: Option<VolumeSnapshot>,
}

pub(crate) const EMPTY_FOLDER_SIZE: &str = "1024Gi";

fn service<'a>(catalog: &'a Value, namespace: &str) -> Option<&'a Value> {
    catalog["services"]
        .as_array()?
        .iter()
        .find(|s| s["namespace"].as_str() == Some(namespace))
}

pub(crate) fn saved_folders(catalog: &Value, namespace: &str) -> Vec<SavedFolder> {
    service(catalog, namespace)
        .and_then(|s| s["folders"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let name = f["name"].as_str().filter(|n| crate::folders::is_name(n))?;
            Some(SavedFolder {
                name: name.to_string(),
                title: f["title"].as_str().unwrap_or(name).to_string(),
                capacity: f["capacity"]
                    .as_str()
                    .filter(|c| !c.is_empty() && *c != "?")
                    .unwrap_or(EMPTY_FOLDER_SIZE)
                    .to_string(),
                snapshot: VolumeSnapshot::recorded_in(f),
            })
        })
        .collect()
}

pub(crate) fn for_wanted(
    saved: &[SavedFolder],
    wanted: &std::collections::BTreeSet<String>,
) -> Vec<SavedFolder> {
    wanted
        .iter()
        .map(|name| {
            saved
                .iter()
                .find(|f| &f.name == name)
                .cloned()
                .unwrap_or_else(|| SavedFolder {
                    name: name.clone(),
                    title: name.clone(),
                    capacity: EMPTY_FOLDER_SIZE.to_string(),
                    snapshot: None,
                })
        })
        .collect()
}

pub(crate) fn saved_release(catalog: &Value, namespace: &str) -> Option<String> {
    let s = service(catalog, namespace)?;
    let def = &s["definition"];
    def["release"]
        .as_str()
        .filter(|r| !r.is_empty())
        .or_else(|| def["instance_name"].as_str().filter(|r| !r.is_empty()))
        .or_else(|| s["instance_name"].as_str().filter(|r| !r.is_empty()))
        .map(str::to_string)
}

pub(crate) fn saved_app_id(catalog: &Value, namespace: &str) -> Option<String> {
    service(catalog, namespace)?["app_id"]
        .as_str()
        .filter(|a| !a.is_empty())
        .map(str::to_string)
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct FolderRow {
    pub name: String,
    pub title: String,
    pub capacity: String,
    pub backed_up: bool,
    pub exists: bool,
    pub used_by: Vec<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct DataRow {
    pub claim: String,
    pub capacity: String,
    pub backed_up: bool,
    pub kept: Option<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    pub folders: Vec<FolderRow>,
    pub data: Vec<DataRow>,
}

pub(crate) struct SavedData<'a> {
    pub claim: &'a str,
    pub capacity: &'a str,
    pub backed_up: bool,
}

pub(crate) fn plan(
    saved: &[SavedFolder],
    data: &[SavedData<'_>],
    live: &[crate::folders::Folder],
    kept: &BTreeMap<String, String>,
) -> RestorePlan {
    RestorePlan {
        folders: saved
            .iter()
            .map(|f| {
                let found = live.iter().find(|l| l.name == f.name);
                FolderRow {
                    name: f.name.clone(),
                    title: found.map_or_else(|| f.title.clone(), |l| l.title.clone()),
                    capacity: f.capacity.clone(),
                    backed_up: f.snapshot.is_some(),
                    exists: found.is_some(),
                    used_by: found.map(|l| l.used_by.clone()).unwrap_or_default(),
                }
            })
            .collect(),
        data: data
            .iter()
            .map(|d| DataRow {
                claim: d.claim.to_string(),
                capacity: d.capacity.to_string(),
                backed_up: d.backed_up,
                kept: kept.get(d.claim).cloned(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_folder_that_is_still_there_is_used_as_it_is_unless_asked_to_replace_it() {
        assert_eq!(step(true, true, None, Choice::Use), Step::Keep);
        assert_eq!(step(true, true, Some(Choice::Use), Choice::Use), Step::Keep);
        assert_eq!(
            step(true, true, Some(Choice::Replace), Choice::Use),
            Step::Replace
        );
    }

    #[test]
    fn a_folder_that_is_still_there_is_never_emptied_or_recreated() {
        assert_eq!(
            step(true, true, Some(Choice::Empty), Choice::Use),
            Step::Keep
        );
        assert_eq!(
            step(true, true, Some(Choice::Restore), Choice::Use),
            Step::Keep
        );
    }

    #[test]
    fn a_folder_with_nothing_saved_cannot_be_replaced_so_it_is_kept() {
        assert_eq!(
            step(true, false, Some(Choice::Replace), Choice::Use),
            Step::Keep
        );
    }

    #[test]
    fn a_missing_folder_comes_back_from_the_backup_unless_asked_to_start_empty() {
        assert_eq!(step(false, true, None, Choice::Use), Step::Restore);
        assert_eq!(
            step(false, true, Some(Choice::Restore), Choice::Use),
            Step::Restore
        );
        assert_eq!(
            step(false, true, Some(Choice::Empty), Choice::Use),
            Step::CreateEmpty
        );
    }

    #[test]
    fn a_missing_folder_with_nothing_saved_starts_empty() {
        assert_eq!(step(false, false, None, Choice::Use), Step::CreateEmpty);
        assert_eq!(
            step(false, false, Some(Choice::Restore), Choice::Use),
            Step::CreateEmpty
        );
    }

    #[test]
    fn an_in_place_restore_replaces_the_apps_own_data_unless_told_to_keep_it() {
        assert_eq!(step(true, true, None, Choice::Replace), Step::Replace);
        assert_eq!(
            step(true, true, Some(Choice::Use), Choice::Replace),
            Step::Keep
        );
    }

    #[test]
    fn choices_are_read_from_plain_words() {
        let choices: Choices = serde_json::from_value(json!({
            "folders": { "movies": "replace", "photos": "empty" },
            "data": { "jellyfin-data": "use" },
        }))
        .unwrap();
        assert_eq!(choices.folders["movies"], Choice::Replace);
        assert_eq!(choices.folders["photos"], Choice::Empty);
        assert_eq!(choices.data["jellyfin-data"], Choice::Use);
        assert_eq!(
            serde_json::from_value::<Choices>(json!({})).unwrap(),
            Choices::default()
        );
    }

    fn catalog() -> Value {
        json!({ "services": [{
            "namespace": "yolab-jellyfin",
            "app_id": "jellyfin",
            "instance_name": "jellyfin",
            "definition": { "release": "jellyfin", "instance_name": "jellyfin" },
            "folders": [
                { "name": "movies", "title": "Movies", "capacity": "500Gi",
                  "snapshot_id": "abc", "snapshot_time": "2026-10-12T03:00:00Z" },
                { "name": "downloads", "title": "Downloads", "capacity": "?" },
                { "name": "../etc", "title": "bad" },
            ],
        }]})
    }

    #[test]
    fn the_folders_an_app_used_are_read_back_from_its_backup() {
        assert_eq!(
            saved_folders(&catalog(), "yolab-jellyfin"),
            vec![
                SavedFolder {
                    name: "movies".into(),
                    title: "Movies".into(),
                    capacity: "500Gi".into(),
                    snapshot: Some(VolumeSnapshot {
                        id: "abc".into(),
                        time: "2026-10-12T03:00:00Z".into(),
                    }),
                },
                SavedFolder {
                    name: "downloads".into(),
                    title: "Downloads".into(),
                    capacity: EMPTY_FOLDER_SIZE.into(),
                    snapshot: None,
                },
            ],
            "a name that is not a folder name never reaches a volume"
        );
        assert!(saved_folders(&catalog(), "yolab-other").is_empty());
    }

    #[test]
    fn only_the_folders_the_settings_still_name_are_prepared_and_a_new_one_starts_unsaved() {
        let saved = saved_folders(&catalog(), "yolab-jellyfin");
        let wanted: std::collections::BTreeSet<String> =
            ["movies".to_string(), "films-2026".to_string()].into();
        let prepared = for_wanted(&saved, &wanted);
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[0].name, "films-2026");
        assert_eq!(prepared[0].snapshot, None);
        assert_eq!(prepared[0].capacity, EMPTY_FOLDER_SIZE);
        assert_eq!(prepared[1], saved[0]);
    }

    #[test]
    fn a_backup_taken_before_folders_were_saved_lists_none() {
        let old = json!({ "services": [{ "namespace": "yolab-jellyfin", "pvcs": [] }] });
        assert!(saved_folders(&old, "yolab-jellyfin").is_empty());
    }

    #[test]
    fn the_release_comes_from_the_saved_definition() {
        assert_eq!(
            saved_release(&catalog(), "yolab-jellyfin").as_deref(),
            Some("jellyfin")
        );
        let old = json!({ "services": [{ "namespace": "yolab-a", "instance_name": "a-1234" }] });
        assert_eq!(saved_release(&old, "yolab-a").as_deref(), Some("a-1234"));
        assert_eq!(
            saved_app_id(&catalog(), "yolab-jellyfin").as_deref(),
            Some("jellyfin")
        );
    }

    fn live(name: &str, title: &str, used_by: &[&str]) -> crate::folders::Folder {
        crate::folders::Folder {
            name: name.into(),
            title: title.into(),
            size: "1Ti".into(),
            ready: true,
            used_by: used_by.iter().map(|s| s.to_string()).collect(),
            kind: crate::folders::KIND_FILES.into(),
            app: None,
            origin: None,
        }
    }

    #[test]
    fn each_folder_says_whether_it_is_still_there_who_uses_it_and_whether_it_was_saved() {
        let saved = saved_folders(&catalog(), "yolab-jellyfin");
        let rows = plan(
            &saved,
            &[],
            &[live("movies", "Films", &["sonarr"])],
            &BTreeMap::new(),
        );
        assert_eq!(
            rows.folders,
            vec![
                FolderRow {
                    name: "movies".into(),
                    title: "Films".into(),
                    capacity: "500Gi".into(),
                    backed_up: true,
                    exists: true,
                    used_by: vec!["sonarr".into()],
                },
                FolderRow {
                    name: "downloads".into(),
                    title: "Downloads".into(),
                    capacity: EMPTY_FOLDER_SIZE.into(),
                    backed_up: false,
                    exists: false,
                    used_by: vec![],
                },
            ]
        );
    }

    #[test]
    fn the_apps_own_data_says_whether_a_copy_was_kept_after_uninstall() {
        let kept: BTreeMap<String, String> =
            [("jellyfin-data".to_string(), "jellyfin-data".to_string())].into();
        let rows = plan(
            &[],
            &[
                SavedData {
                    claim: "jellyfin-data",
                    capacity: "50Gi",
                    backed_up: true,
                },
                SavedData {
                    claim: "jellyfin-cache",
                    capacity: "5Gi",
                    backed_up: false,
                },
            ],
            &[],
            &kept,
        );
        assert_eq!(
            rows.data,
            vec![
                DataRow {
                    claim: "jellyfin-data".into(),
                    capacity: "50Gi".into(),
                    backed_up: true,
                    kept: Some("jellyfin-data".into()),
                },
                DataRow {
                    claim: "jellyfin-cache".into(),
                    capacity: "5Gi".into(),
                    backed_up: false,
                    kept: None,
                },
            ]
        );
    }
}
