pub struct SpacesController {
    pub kube: crate::k8s::Kube,
}

impl SpacesController {
    pub fn real() -> Self {
        Self {
            kube: crate::k8s::Kube::from_environment(),
        }
    }
}

impl crate::runtime::Controller for SpacesController {
    fn name(&self) -> &'static str {
        "spaces"
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
        let client = self.kube.client().await?;
        let deleted = crate::folders::sweep_unlinked(&client).await?;
        if deleted.is_empty() {
            Ok(crate::runtime::Tick::Idle(
                "every space is used by an app or still in its hour of grace".into(),
            ))
        } else {
            Ok(crate::runtime::Tick::Done)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Controller;

    #[test]
    fn the_sweep_runs_once_for_the_whole_cluster_and_is_listed() {
        let controller = SpacesController::real();
        assert_eq!(controller.name(), "spaces");
        assert!(matches!(controller.scope(), crate::runtime::Scope::Cluster));
        assert!(crate::controllers::NAMES.contains(&"spaces"));
        assert!(
            controller.interval().as_secs() * 4 <= crate::folders::GRACE_SECS as u64,
            "a space is looked at several times within its hour of grace"
        );
    }
}
