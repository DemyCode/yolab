use std::fmt::Display;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::exec::{AsCmdError, CmdError, Failure};

#[derive(Debug)]
pub struct AppError(pub anyhow::Error);

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

/// The HTTP status a failure deserves. A missing object is a 404 and a server
/// that did not answer is a 503/504 — the UI shows those differently ("not
/// there" vs "try again"), and before this every failure was a 500.
fn status_for(e: &anyhow::Error) -> StatusCode {
    match e.as_cmd_error() {
        Some(CmdError::Timeout { .. }) => StatusCode::GATEWAY_TIMEOUT,
        Some(CmdError::Spawn { .. }) | Some(CmdError::Busy { .. }) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        Some(CmdError::Forbidden { .. }) => StatusCode::FORBIDDEN,
        Some(c) => match c.failure() {
            Some(Failure::NotFound) => StatusCode::NOT_FOUND,
            Some(Failure::AlreadyExists) | Some(Failure::Conflict) => StatusCode::CONFLICT,
            Some(Failure::Unreachable) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        None => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = status_for(&self.0);
        tracing::error!("{:#}", self.0);
        let body = serde_json::json!({ "error": self.0.to_string() });
        (status, Json(body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

/// Handling for a result whose failure must not stop the caller.
///
/// `let _ = fallible().await;` is banned crate-wide (clippy
/// `let_underscore_must_use`), because it was indistinguishable from forgetting,
/// and several incidents were a discarded error that turned out to matter: an
/// `osd unset noout` that failed silently, a ConfigMap write that never landed
/// and left a restore "running" forever. These make the decision visible and
/// leave a line in the journal when it goes wrong.
pub trait Outcome<T> {
    /// Best effort: log a failure at WARN with `what` for context, then continue.
    fn warn_on_err(self, what: impl Display);
    /// Best effort, and a failure is expected often enough that WARN would be
    /// noise (cleanup of something that may already be gone).
    fn debug_on_err(self, what: impl Display);
    /// Keep the value, log the failure.
    fn ok_or_warn(self, what: impl Display) -> Option<T>;
}

impl<T, E: Display> Outcome<T> for std::result::Result<T, E> {
    fn warn_on_err(self, what: impl Display) {
        if let Err(e) = self {
            tracing::warn!("{what}: {e}");
        }
    }

    fn debug_on_err(self, what: impl Display) {
        if let Err(e) = self {
            tracing::debug!("{what}: {e}");
        }
    }

    fn ok_or_warn(self, what: impl Display) -> Option<T> {
        match self {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("{what}: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_object_is_a_404_and_an_unreachable_server_is_not() {
        let nf = anyhow::Error::new(CmdError::failed(
            "kubectl get secret x",
            "Error from server (NotFound): secrets \"x\" not found",
        ));
        assert_eq!(status_for(&nf), StatusCode::NOT_FOUND);

        let down = anyhow::Error::new(CmdError::failed(
            "kubectl get secret x",
            "The connection to the server localhost:6443 was refused",
        ));
        assert_eq!(status_for(&down), StatusCode::SERVICE_UNAVAILABLE);

        let slow = anyhow::Error::new(CmdError::Timeout {
            cmd: "ceph -s".into(),
            after: std::time::Duration::from_secs(30),
        });
        assert_eq!(status_for(&slow), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn best_effort_results_keep_the_value_and_drop_only_the_error() {
        let ok: std::result::Result<u8, String> = Ok(7);
        let err: std::result::Result<u8, String> = Err("boom".into());
        assert_eq!(ok.clone().ok_or_warn("x"), Some(7));
        assert_eq!(err.clone().ok_or_warn("x"), None);
        // These only log; what matters is that neither panics on either arm.
        ok.clone().warn_on_err("x");
        err.clone().warn_on_err("x");
        ok.debug_on_err("x");
        err.debug_on_err("x");
    }

    #[test]
    fn an_app_error_displays_its_whole_chain() {
        let e = AppError(anyhow::anyhow!("root cause").context("while reading the disk"));
        let shown = e.to_string();
        assert!(shown.contains("while reading the disk") && shown.contains("root cause"));
    }

    #[test]
    fn an_error_response_carries_its_status_and_message() {
        let r = AppError(anyhow::anyhow!("nope")).into_response();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn a_plain_error_is_a_500() {
        assert_eq!(
            status_for(&anyhow::anyhow!("boom")),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn a_conflict_is_a_409() {
        let c = anyhow::Error::new(CmdError::failed(
            "kubectl replace -f -",
            "Error from server (Conflict): the object has been modified",
        ));
        assert_eq!(status_for(&c), StatusCode::CONFLICT);
    }
}
