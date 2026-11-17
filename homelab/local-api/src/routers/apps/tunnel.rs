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
