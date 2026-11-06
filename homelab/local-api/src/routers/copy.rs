use std::collections::BTreeSet;

use kube::Client;

use crate::tree::{self, Claimed, Op, Planned};

pub(crate) fn copies(
    source: &[Claimed],
    source_instance: &str,
    dest: &[Planned],
    copy_files: bool,
) -> Vec<Op> {
    let mut pairs: Vec<(&str, &str)> = dest
        .iter()
        .filter_map(|d| {
            let s = source
                .iter()
                .find(|s| tree::same_role(&s.claim, &d.claimed.claim))?;
            let moved = s.folder != d.claimed.folder;
            let wanted = tree::is_own(&s.folder, source_instance) || copy_files;
            (moved && wanted).then_some((s.folder.as_str(), d.claimed.folder.as_str()))
        })
        .collect();
    pairs.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then(a.cmp(b)));
    pairs.dedup();
    pairs
        .into_iter()
        .map(|(from, to)| Op::Copy {
            from: from.to_string(),
            to: to.to_string(),
            leave_out: source
                .iter()
                .filter_map(|s| s.folder.strip_prefix(from)?.strip_prefix('/'))
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        })
        .collect()
}

pub(crate) async fn copy_live_folders(
    client: &Client,
    source_namespace: &str,
    dest: &[Planned],
    copy_files: bool,
) -> anyhow::Result<()> {
    let source_instance = source_namespace
        .strip_prefix("yolab-")
        .ok_or_else(|| anyhow::anyhow!("{source_namespace} is not a service"))?;
    let source = tree::claimed_in(&tree::volumes(client).await?, source_namespace);
    for op in copies(&source, source_instance, dest, copy_files) {
        tree::run(client, &op).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn claimed(claim: &str, folder: &str) -> Claimed {
        Claimed {
            namespace: "yolab-jellyfin".into(),
            claim: claim.into(),
            folder: folder.into(),
        }
    }

    fn planned(claim: &str, folder: &str) -> Planned {
        Planned {
            claimed: Claimed {
                namespace: "yolab-jellyfin2".into(),
                claim: claim.into(),
                folder: folder.into(),
            },
            volume: Value::Null,
            request: Value::Null,
        }
    }

    fn copy(from: &str, to: &str, leave_out: &[&str]) -> Op {
        Op::Copy {
            from: from.into(),
            to: to.into(),
            leave_out: leave_out.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_copy_gets_its_own_folders_filled_from_the_originals() {
        let source = [
            claimed("jellyfin-data", "jellyfin"),
            claimed("content-1a2b3c4d", "jellyfin.files"),
        ];
        let dest = [
            planned("jellyfin-data", "jellyfin2"),
            planned("content-99999999", "jellyfin2.files"),
        ];
        assert_eq!(
            copies(&source, "jellyfin", &dest, false),
            vec![
                copy("jellyfin", "jellyfin2", &[]),
                copy("jellyfin.files", "jellyfin2.files", &[]),
            ]
        );
    }

    #[test]
    fn a_folder_the_copy_still_points_at_is_shared_not_copied() {
        let source = [
            claimed("jellyfin-data", "jellyfin"),
            claimed("content-1a2b3c4d", "media"),
        ];
        let dest = [
            planned("jellyfin-data", "jellyfin2"),
            planned("content-1a2b3c4d", "media"),
        ];
        assert_eq!(
            copies(&source, "jellyfin", &dest, true),
            vec![copy("jellyfin", "jellyfin2", &[])]
        );
    }

    #[test]
    fn a_pointed_folder_moved_elsewhere_is_filled_only_when_asked() {
        let source = [
            claimed("jellyfin-data", "jellyfin"),
            claimed("content-1a2b3c4d", "media"),
        ];
        let dest = [
            planned("jellyfin-data", "jellyfin2"),
            planned("content-99999999", "media2"),
        ];
        assert_eq!(
            copies(&source, "jellyfin", &dest, false),
            vec![copy("jellyfin", "jellyfin2", &[])]
        );
        assert_eq!(
            copies(&source, "jellyfin", &dest, true),
            vec![
                copy("media", "media2", &[]),
                copy("jellyfin", "jellyfin2", &[]),
            ]
        );
    }

    #[test]
    fn a_folder_inside_another_is_copied_on_its_own_and_left_out_of_the_outer_one() {
        let source = [
            claimed("jellyfin-data", "jellyfin"),
            claimed("content-1a2b3c4d", "jellyfin/media"),
        ];
        let dest = [
            planned("jellyfin-data", "jellyfin2"),
            planned("content-99999999", "jellyfin2/media"),
        ];
        assert_eq!(
            copies(&source, "jellyfin", &dest, false),
            vec![
                copy("jellyfin", "jellyfin2", &["media"]),
                copy("jellyfin/media", "jellyfin2/media", &[]),
            ]
        );
    }

    #[test]
    fn a_shared_folder_inside_an_own_one_is_not_copied_with_it() {
        let source = [
            claimed("jellyfin-data", "jellyfin"),
            claimed("content-1a2b3c4d", "jellyfin/media"),
        ];
        let dest = [
            planned("jellyfin-data", "jellyfin2"),
            planned("content-1a2b3c4d", "jellyfin/media"),
        ];
        assert_eq!(
            copies(&source, "jellyfin", &dest, false),
            vec![copy("jellyfin", "jellyfin2", &["media"])]
        );
    }

    #[test]
    fn a_pack_copies_each_apps_folder_and_its_files() {
        let source = [
            claimed("movies-tv-radarr-data", "movies-tv.radarr"),
            claimed("movies-tv-sonarr-data", "movies-tv.sonarr"),
            claimed("content-1a2b3c4d", "movies-tv.files"),
        ];
        let dest = [
            planned("movies-tv-radarr-data", "films.radarr"),
            planned("movies-tv-sonarr-data", "films.sonarr"),
            planned("content-99999999", "films.files"),
        ];
        assert_eq!(
            copies(&source, "movies-tv", &dest, false),
            vec![
                copy("movies-tv.files", "films.files", &[]),
                copy("movies-tv.radarr", "films.radarr", &[]),
                copy("movies-tv.sonarr", "films.sonarr", &[]),
            ]
        );
    }

    #[test]
    fn a_folder_the_copy_does_not_render_is_not_copied() {
        let source = [
            claimed("jellyfin-data", "jellyfin"),
            claimed("jellyfin-cache", "jellyfin-cache"),
        ];
        let dest = [planned("jellyfin-data", "jellyfin2")];
        assert_eq!(
            copies(&source, "jellyfin", &dest, true),
            vec![copy("jellyfin", "jellyfin2", &[])]
        );
    }

    #[test]
    fn a_folder_named_like_the_original_but_not_its_own_is_not_its_own() {
        let source = [claimed("content-1a2b3c4d", "jellyfin-media")];
        let dest = [planned("content-99999999", "jellyfin2-media")];
        assert!(copies(&source, "jellyfin", &dest, false).is_empty());
    }

    #[tokio::test]
    async fn a_copy_from_something_that_is_not_a_service_is_refused() {
        let e = copy_live_folders(
            &crate::k8s::testing::unreachable(),
            "kube-system",
            &[],
            false,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("not a service"), "{e}");
    }
}
