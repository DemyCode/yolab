use std::collections::{BTreeMap, BTreeSet};

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
    pub claim: Option<String>,
    pub snapshot: Option<VolumeSnapshot>,
}

impl SavedFolder {
    pub(crate) fn repository(&self) -> String {
        self.claim
            .clone()
            .unwrap_or_else(|| crate::folders::claim_name(&self.name))
    }
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
                claim: f["claim"]
                    .as_str()
                    .filter(|c| crate::folders::is_claim(c))
                    .map(str::to_string),
                snapshot: VolumeSnapshot::recorded_in(f),
            })
        })
        .collect()
}

pub(crate) fn for_wanted(saved: &[SavedFolder], wanted: &BTreeSet<String>) -> Vec<SavedFolder> {
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
                    claim: None,
                    snapshot: None,
                })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Placed {
    pub saved: SavedFolder,
    pub name: String,
    pub title: String,
    pub live_claim: Option<String>,
    pub used_by: Vec<String>,
}

impl Placed {
    pub(crate) fn exists(&self) -> bool {
        self.live_claim.is_some()
    }
}

pub(crate) fn place(saved: &[SavedFolder], live: &[crate::folders::Folder]) -> Vec<Placed> {
    let mut taken: BTreeSet<String> = live.iter().map(|l| l.name.clone()).collect();
    saved
        .iter()
        .map(|s| {
            let same = live.iter().find(|l| match &s.claim {
                Some(claim) => &l.claim == claim,
                None => l.name == s.name,
            });
            match same {
                Some(l) => Placed {
                    saved: s.clone(),
                    name: l.name.clone(),
                    title: l.title.clone(),
                    live_claim: Some(l.claim.clone()),
                    used_by: l.used_by.clone(),
                },
                None => {
                    let name = crate::folders::free_name(&s.name, &taken);
                    taken.insert(name.clone());
                    Placed {
                        saved: s.clone(),
                        name,
                        title: s.title.clone(),
                        live_claim: None,
                        used_by: Vec::new(),
                    }
                }
            }
        })
        .collect()
}

pub(crate) fn renames(placed: &[Placed]) -> BTreeMap<String, String> {
    placed
        .iter()
        .filter(|p| p.name != p.saved.name)
        .map(|p| (p.saved.name.clone(), p.name.clone()))
        .collect()
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct FolderRow {
    pub name: String,
    pub title: String,
    pub capacity: String,
    pub backed_up: bool,
    pub exists: bool,
    pub used_by: Vec<String>,
    pub becomes: Option<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct DataRow {
    pub claim: String,
    pub capacity: String,
    pub backed_up: bool,
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

pub(crate) fn plan(placed: &[Placed], data: &[SavedData<'_>]) -> RestorePlan {
    RestorePlan {
        folders: placed
            .iter()
            .map(|p| FolderRow {
                name: p.saved.name.clone(),
                title: p.title.clone(),
                capacity: p.saved.capacity.clone(),
                backed_up: p.saved.snapshot.is_some(),
                exists: p.exists(),
                used_by: p.used_by.clone(),
                becomes: (p.name != p.saved.name).then(|| p.name.clone()),
            })
            .collect(),
        data: data
            .iter()
            .map(|d| DataRow {
                claim: d.claim.to_string(),
                capacity: d.capacity.to_string(),
                backed_up: d.backed_up,
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
                { "name": "movies", "title": "Movies", "capacity": "500Gi", "claim": "space-0a1b2c",
                  "snapshot_id": "abc", "snapshot_time": "2026-10-12T03:00:00Z" },
                { "name": "downloads", "title": "Downloads", "capacity": "?", "claim": "../x" },
                { "name": "../etc", "title": "bad" },
            ],
        }]})
    }

    fn saved(name: &str, claim: Option<&str>) -> SavedFolder {
        SavedFolder {
            name: name.into(),
            title: name.into(),
            capacity: "1Ti".into(),
            claim: claim.map(str::to_string),
            snapshot: Some(VolumeSnapshot {
                id: "abc".into(),
                time: "2026-10-12T03:00:00Z".into(),
            }),
        }
    }

    #[test]
    fn the_folders_an_app_used_are_read_back_from_its_backup_with_their_identity() {
        assert_eq!(
            saved_folders(&catalog(), "yolab-jellyfin"),
            vec![
                SavedFolder {
                    name: "movies".into(),
                    title: "Movies".into(),
                    capacity: "500Gi".into(),
                    claim: Some("space-0a1b2c".into()),
                    snapshot: Some(VolumeSnapshot {
                        id: "abc".into(),
                        time: "2026-10-12T03:00:00Z".into(),
                    }),
                },
                SavedFolder {
                    name: "downloads".into(),
                    title: "Downloads".into(),
                    capacity: EMPTY_FOLDER_SIZE.into(),
                    claim: None,
                    snapshot: None,
                },
            ],
            "a name or identity that is not plain never reaches a volume"
        );
        assert!(saved_folders(&catalog(), "yolab-other").is_empty());
    }

    #[test]
    fn a_backup_from_before_spaces_had_an_identity_reads_from_the_old_repository() {
        assert_eq!(saved("movies", None).repository(), "folder-movies");
        assert_eq!(
            saved("movies", Some("space-0a1b2c")).repository(),
            "space-0a1b2c"
        );
    }

    #[test]
    fn only_the_folders_the_settings_still_name_are_prepared_and_a_new_one_starts_unsaved() {
        let saved = saved_folders(&catalog(), "yolab-jellyfin");
        let wanted: BTreeSet<String> = ["movies".to_string(), "films-2026".to_string()].into();
        let prepared = for_wanted(&saved, &wanted);
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[0].name, "films-2026");
        assert_eq!(prepared[0].snapshot, None);
        assert_eq!(prepared[0].claim, None);
        assert_eq!(prepared[0].capacity, EMPTY_FOLDER_SIZE);
        assert_eq!(prepared[1], saved[0]);
    }

    #[test]
    fn a_backup_taken_before_folders_were_saved_lists_none() {
        let old = json!({ "services": [{ "namespace": "yolab-jellyfin", "pvcs": [] }] });
        assert!(saved_folders(&old, "yolab-jellyfin").is_empty());
    }

    fn live(name: &str, claim: &str, used_by: &[&str]) -> crate::folders::Folder {
        crate::folders::Folder {
            name: name.into(),
            title: name.to_uppercase(),
            size: "1Ti".into(),
            ready: true,
            used_by: used_by.iter().map(|s| s.to_string()).collect(),
            claim: claim.into(),
        }
    }

    #[test]
    fn the_same_space_is_found_by_its_identity_not_its_name() {
        let placed = place(
            &[saved("movies", Some("space-1"))],
            &[live("films", "space-1", &["sonarr"])],
        );
        assert_eq!(placed[0].name, "films");
        assert!(placed[0].exists());
        assert_eq!(placed[0].used_by, vec!["sonarr"]);
        assert_eq!(
            renames(&placed),
            BTreeMap::from([("movies".to_string(), "films".to_string())])
        );
    }

    #[test]
    fn a_different_space_that_took_the_name_is_never_mistaken_for_the_saved_one() {
        let placed = place(
            &[saved("movies", Some("space-1"))],
            &[live("movies", "space-2", &["plex"])],
        );
        assert!(!placed[0].exists());
        assert_eq!(placed[0].name, "movies-2");
        assert!(placed[0].used_by.is_empty());
        assert_eq!(
            renames(&placed),
            BTreeMap::from([("movies".to_string(), "movies-2".to_string())])
        );
    }

    #[test]
    fn a_missing_space_comes_back_under_its_own_name_when_it_is_free() {
        let placed = place(&[saved("movies", Some("space-1"))], &[]);
        assert!(!placed[0].exists());
        assert_eq!(placed[0].name, "movies");
        assert!(renames(&placed).is_empty());
    }

    #[test]
    fn a_space_saved_without_an_identity_is_matched_by_name() {
        let placed = place(
            &[saved("movies", None)],
            &[live("movies", "folder-movies", &[])],
        );
        assert!(placed[0].exists());
        assert_eq!(placed[0].live_claim.as_deref(), Some("folder-movies"));
    }

    #[test]
    fn two_spaces_restored_at_once_never_get_the_same_new_name() {
        let placed = place(
            &[
                saved("movies", Some("space-1")),
                saved("movies-2", Some("space-3")),
            ],
            &[live("movies", "space-2", &[])],
        );
        assert_eq!(placed[0].name, "movies-2");
        assert_eq!(placed[1].name, "movies-3");
    }

    #[test]
    fn each_folder_says_whether_it_is_still_there_who_uses_it_and_its_new_name() {
        let placed = place(
            &[saved("movies", Some("space-1")), saved("tv", Some("space-9"))],
            &[
                live("movies", "space-1", &["sonarr"]),
                live("tv", "space-2", &[]),
            ],
        );
        let rows = plan(&placed, &[]);
        assert_eq!(
            rows.folders,
            vec![
                FolderRow {
                    name: "movies".into(),
                    title: "MOVIES".into(),
                    capacity: "1Ti".into(),
                    backed_up: true,
                    exists: true,
                    used_by: vec!["sonarr".into()],
                    becomes: None,
                },
                FolderRow {
                    name: "tv".into(),
                    title: "tv".into(),
                    capacity: "1Ti".into(),
                    backed_up: true,
                    exists: false,
                    used_by: vec![],
                    becomes: Some("tv-2".into()),
                },
            ]
        );
    }

    #[test]
    fn the_apps_own_data_only_says_whether_it_was_backed_up() {
        let rows = plan(
            &[],
            &[SavedData {
                claim: "jellyfin-data",
                capacity: "50Gi",
                backed_up: true,
            }],
        );
        assert_eq!(
            rows.data,
            vec![DataRow {
                claim: "jellyfin-data".into(),
                capacity: "50Gi".into(),
                backed_up: true,
            }]
        );
    }
}
