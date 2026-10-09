use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const FILE: &str = "setups.json";
const MAX_APPS: usize = 20;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub folders: BTreeMap<String, SetupFolder>,
    pub apps: BTreeMap<String, SetupApp>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SetupFolder {
    pub title: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SetupApp {
    pub chart: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub settings: Map<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub folders: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CatalogSetup {
    pub id: String,
    #[serde(flatten)]
    pub setup: Setup,
}

fn is_chart_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl Setup {
    pub(crate) fn problems(&self) -> Vec<String> {
        let mut found = Vec::new();
        if self.title.trim().is_empty() {
            found.push("the setup has no title".to_string());
        }
        if self.apps.is_empty() {
            found.push("the setup has no apps".to_string());
        }
        if self.apps.len() > MAX_APPS {
            found.push(format!("a setup holds at most {MAX_APPS} apps"));
        }
        if let Some(main) = &self.main {
            if !self.apps.contains_key(main) {
                found.push(format!("main names {main}, which is not one of its apps"));
            }
        }
        for (key, folder) in &self.folders {
            if !crate::folders::is_name(key) {
                found.push(format!("folder {key:?} is not a plain name"));
            }
            if folder.title.trim().is_empty() {
                found.push(format!("folder {key} has no title"));
            }
        }
        for (key, app) in &self.apps {
            if !crate::folders::is_name(key) {
                found.push(format!("app {key:?} is not a plain name"));
            }
            if !is_chart_name(&app.chart) {
                found.push(format!("app {key}: {:?} is not a chart name", app.chart));
            }
            for (field, folder) in &app.folders {
                if !self.folders.contains_key(folder) {
                    found.push(format!(
                        "app {key}: {field} uses folder {folder}, which the setup does not list"
                    ));
                }
            }
        }
        found
    }

    pub(crate) fn parse(text: &str) -> Result<Setup, String> {
        let setup: Setup =
            serde_norway::from_str(text).map_err(|e| format!("not a setup file: {e}"))?;
        match setup.problems().as_slice() {
            [] => Ok(setup),
            problems => Err(problems.join("; ")),
        }
    }

    pub(crate) fn to_yaml(&self) -> anyhow::Result<String> {
        Ok(serde_norway::to_string(self)?)
    }
}

pub(crate) struct Member {
    pub instance: String,
    pub app_id: String,
    pub main: bool,
    pub settings: Map<String, Value>,
    pub folder_fields: BTreeSet<String>,
    pub credentials: HashSet<String>,
}

pub(crate) fn export(
    title: &str,
    members: &[Member],
    folder_titles: &BTreeMap<String, String>,
) -> Setup {
    let mut folders = BTreeMap::new();
    let mut apps = BTreeMap::new();
    for m in members {
        let mut settings = Map::new();
        let mut uses = BTreeMap::new();
        for (key, value) in &m.settings {
            if m.credentials.contains(key) || key == crate::routers::apps::YOLAB_TOKEN_FIELD {
                continue;
            }
            if m.folder_fields.contains(key) {
                if let Some(folder) = value.as_str().filter(|f| !f.is_empty()) {
                    uses.insert(key.clone(), folder.to_string());
                    folders.insert(
                        folder.to_string(),
                        SetupFolder {
                            title: folder_titles
                                .get(folder)
                                .cloned()
                                .unwrap_or_else(|| folder.to_string()),
                        },
                    );
                }
                continue;
            }
            settings.insert(key.clone(), value.clone());
        }
        apps.insert(
            m.instance.clone(),
            SetupApp {
                chart: m.app_id.clone(),
                settings,
                folders: uses,
            },
        );
    }
    Setup {
        title: title.to_string(),
        tagline: None,
        main: members.iter().find(|m| m.main).map(|m| m.instance.clone()),
        folders,
        apps,
    }
}

pub(crate) fn keep(dir: &Path, setups: &[CatalogSetup]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec(setups)?)?;
    std::fs::rename(tmp, dir.join(FILE))?;
    Ok(())
}

pub(crate) fn kept(dir: &Path) -> Vec<CatalogSetup> {
    std::fs::read(dir.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub(crate) fn usable(repo: &str, offered: Vec<CatalogSetup>) -> Vec<CatalogSetup> {
    offered
        .into_iter()
        .filter(|s| {
            let problems = s.setup.problems();
            let id_ok = crate::folders::is_name(&s.id);
            if !problems.is_empty() || !id_ok {
                tracing::warn!(
                    "{repo}: setup {:?} is skipped: {}",
                    s.id,
                    if id_ok { problems.join("; ") } else { "its id is not a plain name".into() }
                );
            }
            problems.is_empty() && id_ok
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MOVIES: &str = "\
title: Movies & TV
tagline: Watch what you own on every screen
main: jellyfin
folders:
  media: { title: Movies & TV }
apps:
  qbittorrent: { chart: qbittorrent, folders: { media_folder: media } }
  sonarr: { chart: sonarr, folders: { media_folder: media } }
  jellyfin:
    chart: jellyfin
    settings: { hardware_transcoding: true }
    folders: { media_folder: media }
";

    #[test]
    fn a_setup_file_reads_as_its_apps_folders_and_main_app() {
        let setup = Setup::parse(MOVIES).unwrap();
        assert_eq!(setup.title, "Movies & TV");
        assert_eq!(setup.main.as_deref(), Some("jellyfin"));
        assert_eq!(setup.folders["media"].title, "Movies & TV");
        assert_eq!(setup.apps.len(), 3);
        assert_eq!(setup.apps["jellyfin"].settings["hardware_transcoding"], json!(true));
        assert_eq!(setup.apps["sonarr"].folders["media_folder"], "media");
    }

    #[test]
    fn a_setup_written_out_reads_back_the_same() {
        let setup = Setup::parse(MOVIES).unwrap();
        assert_eq!(Setup::parse(&setup.to_yaml().unwrap()).unwrap(), setup);
    }

    #[test]
    fn a_misspelt_key_is_refused_rather_than_silently_ignored() {
        let e = Setup::parse("title: x\napps: { a: { chart: a, setings: {} } }\n").unwrap_err();
        assert!(e.contains("setings"), "{e}");
    }

    #[test]
    fn a_setup_must_name_only_what_it_lists() {
        let e = Setup::parse(
            "title: x\nmain: plex\napps:\n  a: { chart: a, folders: { media_folder: films } }\n",
        )
        .unwrap_err();
        assert!(e.contains("main names plex"), "{e}");
        assert!(e.contains("uses folder films"), "{e}");
    }

    #[test]
    fn names_that_would_reach_kubernetes_must_be_plain() {
        let e = Setup::parse(
            "title: x\nfolders: { ../etc: { title: t } }\napps: { A: { chart: Not/Chart } }\n",
        )
        .unwrap_err();
        assert!(e.contains("folder \"../etc\""), "{e}");
        assert!(e.contains("app \"A\""), "{e}");
        assert!(e.contains("not a chart name"), "{e}");
    }

    #[test]
    fn an_empty_setup_is_refused() {
        assert!(Setup::parse("title: x\napps: {}\n").is_err());
        assert!(Setup::parse("title: ' '\napps: { a: { chart: a } }\n").is_err());
    }

    fn member(instance: &str, app_id: &str, main: bool, settings: Value) -> Member {
        Member {
            instance: instance.into(),
            app_id: app_id.into(),
            main,
            settings: settings.as_object().cloned().unwrap(),
            folder_fields: ["media_folder".to_string()].into_iter().collect(),
            credentials: ["admin_password".to_string()].into_iter().collect(),
        }
    }

    #[test]
    fn exporting_a_group_keeps_its_choices_but_never_its_secrets() {
        let titles: BTreeMap<String, String> =
            [("movies-tv".to_string(), "Movies & TV".to_string())].into_iter().collect();
        let setup = export(
            "Movies & TV",
            &[
                member(
                    "jellyfin",
                    "jellyfin",
                    true,
                    json!({ "media_folder": "movies-tv", "hardware_transcoding": true }),
                ),
                member(
                    "sonarr-2",
                    "sonarr",
                    false,
                    json!({
                        "media_folder": "movies-tv",
                        "admin_password": "hunter2",
                        "yolab_token": "tok",
                    }),
                ),
            ],
            &titles,
        );
        assert_eq!(setup.main.as_deref(), Some("jellyfin"));
        assert_eq!(setup.folders["movies-tv"].title, "Movies & TV");
        assert_eq!(setup.apps["sonarr-2"].chart, "sonarr");
        assert_eq!(setup.apps["sonarr-2"].folders["media_folder"], "movies-tv");
        assert!(setup.apps["sonarr-2"].settings.is_empty());
        assert_eq!(
            setup.apps["jellyfin"].settings,
            json!({ "hardware_transcoding": true }).as_object().cloned().unwrap()
        );
        let text = setup.to_yaml().unwrap();
        assert!(!text.contains("hunter2") && !text.contains("tok"), "{text}");
        assert!(Setup::parse(&text).is_ok(), "{text}");
    }

    #[test]
    fn an_app_keeping_its_files_inside_uses_no_folder() {
        let setup = export(
            "Solo",
            &[member("sonarr", "sonarr", false, json!({ "media_folder": "" }))],
            &BTreeMap::new(),
        );
        assert!(setup.folders.is_empty());
        assert!(setup.apps["sonarr"].folders.is_empty());
        assert_eq!(setup.main, None);
    }

    #[test]
    fn setups_kept_after_a_sync_are_read_back_and_a_broken_one_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let good = CatalogSetup {
            id: "movies-tv".into(),
            setup: Setup::parse(MOVIES).unwrap(),
        };
        let mut broken = good.clone();
        broken.id = "Bad Id".into();
        let offered = usable("official", vec![good.clone(), broken]);
        keep(dir.path(), &offered).unwrap();
        assert_eq!(kept(dir.path()), vec![good]);
        assert!(kept(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn a_catalog_entry_carries_its_id_next_to_the_setup() {
        let entry: CatalogSetup = serde_norway::from_str(&format!("id: movies-tv\n{MOVIES}")).unwrap();
        assert_eq!(entry.id, "movies-tv");
        assert_eq!(entry.setup.apps.len(), 3);
    }
}
