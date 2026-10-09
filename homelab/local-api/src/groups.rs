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
}
