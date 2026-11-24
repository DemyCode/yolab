use super::*;

pub(super) fn tunnel_config(cfg: &Config) -> anyhow::Result<toml::Table> {
    cfg.tunnel_table()
        .ok_or_else(|| anyhow::anyhow!("missing [tunnel] in config"))
}

pub(crate) const YOLAB_SWITCH: &str = "yolab_enabled";
pub(crate) const YOLAB_TOKEN_FIELD: &str = "yolab_token";
pub(super) const TUNNEL_SECRET: &str = "yolab-tunnel-credentials";
pub(super) const LABEL_TOKEN_SOURCE: &str = "yolab.io/token-source";
pub(super) const TOKEN_FROM_FORM: &str = "form";

pub(crate) fn take_yolab_token(config: &mut serde_json::Map<String, Value>) -> Option<String> {
    let token = config.remove(YOLAB_TOKEN_FIELD)?;
    let token = token.as_str()?.trim();
    (!token.is_empty() && token != REDACTED).then(|| token.to_string())
}

pub(super) fn box_token(tunnel_cfg: &toml::Table) -> &str {
    tunnel_cfg
        .get("account_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

pub(super) fn tunnel_secret(ns: &str, token: &str, source: &str) -> Value {
    crate::k8s::secret_manifest(
        TUNNEL_SECRET,
        ns,
        &[("account-token", token)],
        &[
            ("app.kubernetes.io/managed-by", "yolab"),
            (LABEL_TOKEN_SOURCE, source),
        ],
    )
}

pub(crate) async fn store_form_token(client: &Client, ns: &str, token: &str) -> anyhow::Result<()> {
    crate::k8s::apply(client, &tunnel_secret(ns, token, TOKEN_FROM_FORM)).await
}

pub(super) fn keeps_form_token(existing: Option<&Value>) -> bool {
    existing.is_some_and(|s| {
        s["metadata"]["labels"][LABEL_TOKEN_SOURCE].as_str() == Some(TOKEN_FROM_FORM)
    })
}

pub(super) async fn ensure_tunnel_credentials(
    client: &Client,
    ns: &str,
    tunnel_cfg: &toml::Table,
) -> anyhow::Result<()> {
    let existing = crate::k8s::get(
        client,
        &crate::k8s::reference("v1", "Secret", ns, TUNNEL_SECRET),
    )
    .await?;
    if keeps_form_token(existing.as_ref()) {
        return Ok(());
    }
    crate::k8s::apply(client, &tunnel_secret(ns, box_token(tunnel_cfg), "box")).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::testing::{accept_patches, api_server, patched, serve, status};
    use serde_json::json;

    const SECRET_PATH: &str = "/api/v1/namespaces/yolab-notes/secrets/yolab-tunnel-credentials";

    fn config(body: &str) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, body).unwrap();
        let cfg = Config::for_test(&file);
        (dir, cfg)
    }

    #[test]
    fn the_tunnel_section_is_required_and_its_token_may_be_blank() {
        let (_d, without) = config("[homelab]\nhostname = \"x\"\n");
        assert_eq!(
            tunnel_config(&without).unwrap_err().to_string(),
            "missing [tunnel] in config"
        );
        let (_d, with) = config("[tunnel]\naccount_token = \"acct\"\n");
        let table = tunnel_config(&with).unwrap();
        assert_eq!(box_token(&table), "acct");
        assert_eq!(box_token(&toml::Table::new()), "");
    }

    #[test]
    fn a_typed_token_is_taken_out_of_the_form_unless_blank_or_the_placeholder() {
        let form = |v: Value| {
            let mut m = serde_json::Map::new();
            m.insert(YOLAB_TOKEN_FIELD.into(), v);
            m
        };
        let mut typed = form(json!("  tok  "));
        assert_eq!(take_yolab_token(&mut typed).as_deref(), Some("tok"));
        assert!(typed.is_empty());
        assert_eq!(take_yolab_token(&mut form(json!(""))), None);
        assert_eq!(take_yolab_token(&mut form(json!(REDACTED))), None);
        assert_eq!(take_yolab_token(&mut form(json!(5))), None);
        assert_eq!(take_yolab_token(&mut serde_json::Map::new()), None);
    }

    #[tokio::test]
    async fn a_typed_token_is_stored_as_coming_from_the_form() {
        let (server, kube) = api_server().await;
        accept_patches(&server).await;
        store_form_token(&kube, "yolab-notes", "tok").await.unwrap();
        let sent = patched(&server).await;
        assert_eq!(sent[0]["stringData"]["account-token"], "tok");
        assert_eq!(
            sent[0]["metadata"]["labels"][LABEL_TOKEN_SOURCE],
            TOKEN_FROM_FORM
        );
    }

    #[tokio::test]
    async fn a_service_without_a_typed_token_gets_this_server_s_token() {
        let (server, kube) = api_server().await;
        serve(&server, SECRET_PATH, 404, status(404, "NotFound")).await;
        accept_patches(&server).await;
        let mut tunnel = toml::Table::new();
        tunnel.insert("account_token".into(), "acct".into());
        ensure_tunnel_credentials(&kube, "yolab-notes", &tunnel)
            .await
            .unwrap();
        let sent = patched(&server).await;
        assert_eq!(sent[0]["stringData"]["account-token"], "acct");
        assert_eq!(sent[0]["metadata"]["labels"][LABEL_TOKEN_SOURCE], "box");
    }

    #[tokio::test]
    async fn a_token_typed_in_the_form_is_never_replaced_by_the_server_s() {
        let (server, kube) = api_server().await;
        serve(
            &server,
            SECRET_PATH,
            200,
            tunnel_secret("yolab-notes", "typed", TOKEN_FROM_FORM),
        )
        .await;
        accept_patches(&server).await;
        ensure_tunnel_credentials(&kube, "yolab-notes", &toml::Table::new())
            .await
            .unwrap();
        assert!(patched(&server).await.is_empty());
        assert!(ensure_tunnel_credentials(
            &crate::k8s::testing::unreachable(),
            "yolab-notes",
            &toml::Table::new()
        )
        .await
        .is_err());
    }
}
