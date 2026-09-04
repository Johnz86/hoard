use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

use hoard_index::IndexError;
use hoard_mirror::MirrorError;

/// An error on a HuggingFace-compatible route.
///
/// The Hub contract is specific: the status, the lower-case `x-error-code` header and the
/// body are what `huggingface_hub` reads to decide between a missing repo, a missing file
/// and a bad revision, so this type carries all three together rather than deriving them.
#[derive(Debug, Clone)]
pub struct HubError {
    status: StatusCode,
    code: &'static str,
    body: Option<serde_json::Value>,
}

impl HubError {
    /// 401 with `RepoNotFound`: deliberate, because `huggingface_hub` maps it to
    /// `RepositoryNotFoundError`. A proxy denial answers the same way as a missing repo.
    pub fn repo_not_found() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "RepoNotFound",
            body: Some(json!({ "error": "Repository not found" })),
        }
    }

    /// 404 for an unknown route or an unknown repo type.
    pub fn page_not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "RepoNotFound",
            body: Some(json!({ "error": "Sorry, we can't find the page you are looking for." })),
        }
    }

    /// 404 with an empty body when the path is not in the revision.
    pub fn entry_not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "EntryNotFound",
            body: None,
        }
    }

    pub fn revision_not_found(revision: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "RevisionNotFound",
            body: Some(json!({ "error": format!("Invalid rev id: {revision}") })),
        }
    }

    pub fn proxy_timeout() -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            code: "ProxyTimeout",
            body: None,
        }
    }

    pub fn proxy_invalid_data() -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            code: "ProxyInvalidData",
            body: None,
        }
    }

    /// 416 carrying `content-range: bytes */<size>` when no requested range is satisfiable.
    pub fn range_not_satisfiable(size: u64) -> Self {
        Self {
            status: StatusCode::RANGE_NOT_SATISFIABLE,
            code: "RangeNotSatisfiable",
            body: Some(json!({ "size": size })),
        }
    }

    pub fn unprocessable(message: &'static str) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "BadRequest",
            body: Some(json!({ "error": message })),
        }
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl From<MirrorError> for HubError {
    fn from(error: MirrorError) -> Self {
        match error {
            MirrorError::RepoNotFound(_) => HubError::repo_not_found(),
            MirrorError::RevisionNotFound(revision) => HubError::revision_not_found(&revision),
            MirrorError::EntryNotFound(_) => HubError::entry_not_found(),
            MirrorError::RangeNotSatisfiable { size } => HubError::range_not_satisfiable(size),
            MirrorError::Offline(_) => HubError::entry_not_found(),
            MirrorError::ProxyTimeout | MirrorError::Upstream(_) => HubError::proxy_timeout(),
            MirrorError::ProxyInvalidData(_) | MirrorError::CacheCorrupt(_) => {
                HubError::proxy_invalid_data()
            }
            other => {
                tracing::error!(error = %other, "hub route failed");
                HubError::proxy_invalid_data()
            }
        }
    }
}

impl From<IndexError> for HubError {
    fn from(error: IndexError) -> Self {
        match error {
            IndexError::NotFound { .. } => HubError::repo_not_found(),
            other => {
                tracing::error!(error = %other, "hub route failed against the index");
                HubError::proxy_invalid_data()
            }
        }
    }
}

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        let mut response = match self.body {
            Some(body) => (self.status, Json(body)).into_response(),
            None => self.status.into_response(),
        };
        response
            .headers_mut()
            .insert("x-error-code", HeaderValue::from_static(self.code));
        if self.status == StatusCode::RANGE_NOT_SATISFIABLE {
            response.headers_mut().remove(header::CONTENT_TYPE);
        }
        response
    }
}

/// An error on an `/api/v1` route, rendered as
/// `{ "error": { "code": "<PascalCase>", "message": "<text>" } }`.
#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
}

#[derive(Serialize)]
struct ApiErrorBody<'a> {
    error: ApiErrorDetail<'a>,
}

#[derive(Serialize)]
struct ApiErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

impl ApiError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NotFound", message)
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BadRequest", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "a node write token is required",
        )
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "Conflict", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal", message)
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}

impl From<IndexError> for ApiError {
    fn from(error: IndexError) -> Self {
        match error {
            IndexError::NotFound { kind, id } => ApiError::not_found(format!("{kind} {id}")),
            IndexError::Query(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, "InvalidQuery", message)
            }
            IndexError::Config(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, "InvalidConfig", message)
            }
            other => {
                tracing::error!(error = %other, "api route failed");
                ApiError::internal(other.to_string())
            }
        }
    }
}

impl From<MirrorError> for ApiError {
    fn from(error: MirrorError) -> Self {
        match error {
            MirrorError::RepoNotFound(id) => ApiError::not_found(format!("repo {id}")),
            MirrorError::RevisionNotFound(rev) => ApiError::not_found(format!("revision {rev}")),
            MirrorError::EntryNotFound(path) => ApiError::not_found(format!("path {path}")),
            MirrorError::Offline(subject) => {
                ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Offline", subject)
            }
            other => {
                tracing::error!(error = %other, "api route failed against the mirror");
                ApiError::internal(other.to_string())
            }
        }
    }
}

impl From<hoard_core::HoardError> for ApiError {
    fn from(error: hoard_core::HoardError) -> Self {
        ApiError::bad_request(error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ApiErrorBody {
            error: ApiErrorDetail {
                code: &self.code,
                message: &self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
pub type HubResult<T> = Result<T, HubError>;
