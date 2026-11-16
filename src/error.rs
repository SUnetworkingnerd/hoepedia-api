use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// One error type for both the Action-style and REST-style APIs.
/// Wire format: `{"error": {"code": "...", "info": "..."}, "httpCode": 404}`
#[derive(Clone, Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub info: String,
    /// Extra machine-readable details, merged into the `error` object (e.g. `conflict`).
    pub data: Option<Box<serde_json::Value>>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, info: impl Into<String>) -> Self {
        Self { status, code, info: info.into(), data: None }
    }
    pub fn bad(code: &'static str, info: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, info)
    }
    pub fn not_found(code: &'static str, info: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, info)
    }
    pub fn conflict(code: &'static str, info: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, info)
    }
    pub fn forbidden(code: &'static str, info: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, info)
    }
    pub fn unauthorized(info: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", info)
    }
    pub fn internal(info: impl std::fmt::Display) -> Self {
        tracing::error!(error = %info, "internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_api_error", "internal error")
    }
    pub fn with(mut self, key: &str, value: serde_json::Value) -> Self {
        let mut m = self.data.take().map(|b| *b).unwrap_or_else(|| json!({}));
        m[key] = value;
        self.data = Some(Box::new(m));
        self
    }
    pub fn bad_title(reason: impl Into<String>) -> Self {
        Self::bad("invalidtitle", reason)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut err = json!({ "code": self.code, "info": self.info });
        if let Some(serde_json::Value::Object(extra)) = self.data.map(|b| *b) {
            for (k, v) in extra {
                err[k] = v;
            }
        }
        let body = Json(json!({ "error": err, "httpCode": self.status.as_u16() }));
        let mut resp = (self.status, body).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        }
        resp
    }
}

pub type Res<T> = Result<T, ApiError>;
