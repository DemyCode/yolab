use kube::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const LABEL_GROUP: &str = "yolab.io/group";
const LABEL_MAIN: &str = "yolab.io/group-main";
const ANN_TITLE: &str = "yolab.io/group-title";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Membership {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub main: bool,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Joining {
    pub title: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub main: bool,
}

impl Joining {
    pub(crate) fn membership(&self) -> Result<Membership, String> {
        let title = self.title.trim();
        let name = match &self.name {
            Some(name) if crate::folders::is_name(name) => name.clone(),
            Some(name) => return Err(format!("{name:?} is not a group name")),
            None => crate::folders::name_from_title(title)
                .ok_or("a group needs a name with at least one letter or number")?,
        };
        let title = match title {
            "" => name.clone(),
            title => title.to_string(),
        };
        Ok(Membership {
            name,
            title,
            main: self.main,
        })
    }
}

pub(crate) fn group_of(namespace: &Value) -> Option<Membership> {
    let labels = &namespace["metadata"]["labels"];
    let name = labels[LABEL_GROUP].as_str().filter(|n| !n.is_empty())?;
    Some(Membership {
        name: name.to_string(),
        title: namespace["metadata"]["annotations"][ANN_TITLE]
            .as_str()
            .unwrap_or(name)
            .to_string(),
        main: labels[LABEL_MAIN].as_str() == Some("true"),
    })
}

pub(crate) fn membership_patch(namespace: &str, membership: Option<&Membership>) -> Value {
    let (group, main, title) = match membership {
        Some(m) => (
            json!(m.name),
            if m.main { json!("true") } else { Value::Null },
            json!(m.title),
        ),
        None => (Value::Null, Value::Null, Value::Null),
    };
    json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": namespace,
            "labels": { LABEL_GROUP: group, LABEL_MAIN: main },
            "annotations": { ANN_TITLE: title },
        },
    })
}

pub(crate) async fn set(
    client: &Client,
    namespace: &str,
    membership: Option<&Membership>,
) -> anyhow::Result<()> {
    crate::k8s::merge_patch(client, &membership_patch(namespace, membership)).await
}

pub(crate) fn members<'a>(namespaces: &'a [Value], group: &str) -> Vec<&'a Value> {
    namespaces
        .iter()
        .filter(|ns| group_of(ns).is_some_and(|m| m.name == group))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespace(labels: Value, annotations: Value) -> Value {
        json!({
            "metadata": {
                "name": "yolab-sonarr",
                "labels": labels,
                "annotations": annotations,
            }
        })
    }

    #[test]
    fn an_app_outside_any_group_has_no_membership() {
        assert_eq!(group_of(&namespace(json!({}), json!({}))), None);
        assert_eq!(
            group_of(&namespace(json!({ LABEL_GROUP: "" }), json!({}))),
            None
        );
    }

    #[test]
    fn a_member_carries_the_groups_title_and_whether_it_is_the_main_app() {
        let ns = namespace(
            json!({ LABEL_GROUP: "movies-tv", LABEL_MAIN: "true" }),
            json!({ ANN_TITLE: "Movies & TV" }),
        );
        assert_eq!(
            group_of(&ns),
            Some(Membership {
                name: "movies-tv".into(),
                title: "Movies & TV".into(),
                main: true,
            })
        );
    }

    #[test]
    fn leaving_a_group_removes_every_mark_of_it() {
        let patch = membership_patch("yolab-sonarr", None);
        assert_eq!(patch["metadata"]["labels"][LABEL_GROUP], Value::Null);
        assert_eq!(patch["metadata"]["labels"][LABEL_MAIN], Value::Null);
        assert_eq!(patch["metadata"]["annotations"][ANN_TITLE], Value::Null);
    }

    #[test]
    fn a_member_that_is_not_the_main_app_loses_an_old_main_mark() {
        let m = Membership {
            name: "movies-tv".into(),
            title: "Movies & TV".into(),
            main: false,
        };
        let patch = membership_patch("yolab-sonarr", Some(&m));
        assert_eq!(patch["metadata"]["labels"][LABEL_GROUP], "movies-tv");
        assert_eq!(patch["metadata"]["labels"][LABEL_MAIN], Value::Null);
        assert_eq!(patch["metadata"]["annotations"][ANN_TITLE], "Movies & TV");
    }

    #[test]
    fn joining_by_title_names_the_group_after_it() {
        let joining = Joining {
            title: " Movies & TV ".into(),
            name: None,
            main: true,
        };
        assert_eq!(
            joining.membership().unwrap(),
            Membership {
                name: "movies-tv".into(),
                title: "Movies & TV".into(),
                main: true,
            }
        );
        let bad = Joining {
            title: "x".into(),
            name: Some("Not A Name".into()),
            main: false,
        };
        assert!(bad.membership().is_err());
        let empty = Joining {
            title: "&&".into(),
            name: None,
            main: false,
        };
        assert!(empty.membership().is_err());
    }

    #[test]
    fn a_group_lists_only_its_own_members() {
        let a = namespace(json!({ LABEL_GROUP: "movies-tv" }), json!({}));
        let b = namespace(json!({ LABEL_GROUP: "photos" }), json!({}));
        let c = namespace(json!({}), json!({}));
        let all = vec![a.clone(), b, c];
        assert_eq!(members(&all, "movies-tv"), vec![&a]);
    }
}
