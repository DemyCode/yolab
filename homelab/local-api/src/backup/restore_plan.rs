use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct DataRow {
    pub claim: String,
    pub capacity: String,
    pub backed_up: bool,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    pub data: Vec<DataRow>,
}

pub(crate) struct SavedData<'a> {
    pub claim: &'a str,
    pub capacity: &'a str,
    pub backed_up: bool,
}

pub(crate) fn plan(data: &[SavedData<'_>]) -> RestorePlan {
    RestorePlan {
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
    fn data_that_is_still_there_is_used_as_it_is_unless_asked_to_replace_it() {
        assert_eq!(step(true, true, None, Choice::Use), Step::Keep);
        assert_eq!(step(true, true, Some(Choice::Use), Choice::Use), Step::Keep);
        assert_eq!(
            step(true, true, Some(Choice::Replace), Choice::Use),
            Step::Replace
        );
    }

    #[test]
    fn data_that_is_still_there_is_never_emptied_or_recreated() {
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
    fn data_with_nothing_saved_cannot_be_replaced_so_it_is_kept() {
        assert_eq!(
            step(true, false, Some(Choice::Replace), Choice::Use),
            Step::Keep
        );
    }

    #[test]
    fn missing_data_comes_back_from_the_backup_unless_asked_to_start_empty() {
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
    fn missing_data_with_nothing_saved_starts_empty() {
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
            "data": { "jellyfin-data": "use" },
        }))
        .unwrap();
        assert_eq!(choices.data["jellyfin-data"], Choice::Use);
        assert_eq!(
            serde_json::from_value::<Choices>(json!({})).unwrap(),
            Choices::default()
        );
    }

    #[test]
    fn the_apps_own_data_only_says_whether_it_was_backed_up() {
        let rows = plan(&[SavedData {
            claim: "jellyfin-data",
            capacity: "50Gi",
            backed_up: true,
        }]);
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
