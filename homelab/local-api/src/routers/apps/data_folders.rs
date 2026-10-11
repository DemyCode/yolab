use super::*;

pub(crate) fn chart_members(chart_dir: &std::path::Path) -> Vec<String> {
    read_chart(chart_dir)
        .map(|meta| meta.members())
        .unwrap_or_default()
}

pub(crate) async fn keep_data_as_folders(
    client: &Client,
    namespace: &str,
    app_id: &str,
    release: &str,
    members: &[String],
) -> anyhow::Result<Vec<String>> {
    crate::folders::adopt(
        client,
        &crate::folders::AppVolumes {
            namespace,
            app_id,
            release,
            members,
        },
    )
    .await
}

pub(crate) async fn keep_all_data_as_folders(
    client: &Client,
    catalog: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let mut failed = Vec::new();
    for app in installed_apps(client).await? {
        if namespace_is_terminating(client, &app.namespace).await {
            continue;
        }
        let release = read_definition_opt(client, &app.namespace)
            .await
            .map(|d| d.release().to_string())
            .unwrap_or_else(|| {
                app.namespace
                    .strip_prefix("yolab-")
                    .unwrap_or(&app.namespace)
                    .to_string()
            });
        let members = chart_members(&catalog.join(&app.app_id));
        if let Err(e) =
            keep_data_as_folders(client, &app.namespace, &app.app_id, &release, &members).await
        {
            tracing::warn!(
                "{}: its data is not kept as a folder yet ({e:#})",
                app.namespace
            );
            failed.push(app.namespace);
        }
    }
    Ok(failed)
}

pub struct DataFoldersController;

impl crate::runtime::Controller for DataFoldersController {
    fn name(&self) -> &'static str {
        "data-folders"
    }
    fn scope(&self) -> crate::runtime::Scope {
        crate::runtime::Scope::Cluster
    }
    fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(300)
    }
    fn requires(&self) -> &'static [crate::runtime::Requirement] {
        &[crate::runtime::Requirement::KubeApi]
    }
    async fn reconcile(&self, _ctx: &crate::runtime::Ctx) -> anyhow::Result<crate::runtime::Tick> {
        let catalog = crate::config::Config::from_env().catalog_dir();
        let failed = keep_all_data_as_folders(&crate::k8s::client().await?, &catalog).await?;
        if failed.is_empty() {
            Ok(crate::runtime::Tick::Done)
        } else {
            Ok(crate::runtime::Tick::NotYet(format!(
                "the data of {} is not kept as a folder yet",
                failed.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pack_names_the_apps_whose_data_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Chart.yaml"),
            "apiVersion: v2\nname: movies-tv\nversion: 0.2.0\nannotations:\n  yolab.io/members: \"jellyfin,sonarr\"\n",
        )
        .unwrap();
        assert_eq!(chart_members(dir.path()), vec!["jellyfin", "sonarr"]);
        assert!(chart_members(std::path::Path::new("/nonexistent")).is_empty());
    }
}
