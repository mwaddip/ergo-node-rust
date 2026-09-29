//! Request extractors that refuse with the `ApiError` body.
//!
//! axum's own `Query`, `Path`, `Json` and `Bytes` answer a request they
//! can't extract with a plain-text body. The contract promises the
//! `ApiError` JSON for every error on the main listener, a request refused
//! before any handler logic runs included (`facts/api.md` § Error Model).
//! These wrap axum's and convert the refusal. A client error answers 400,
//! except two that HTTP has a more precise status for: 413 for a body over
//! the limit, and 415 for one not sent as `application/json`. A server
//! error, a route that disagrees with its handler, keeps its status.

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use serde::de::DeserializeOwned;

use crate::handlers::api_error;
use crate::response::Json;
use crate::types::ApiError;

type Refusal = (StatusCode, Json<ApiError>);

/// The `ApiError` answer for an axum rejection of `status` and text
/// `detail`. `reason` names what the request got wrong, for a 400.
fn refusal(status: StatusCode, reason: &str, detail: String) -> Refusal {
    let (status, reason) = match status {
        StatusCode::PAYLOAD_TOO_LARGE => (status, "request body too large"),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => (status, "request body must be application/json"),
        status if status.is_server_error() => (status, "request extraction failed"),
        // 400, and 422: axum's JSON that parses but doesn't fit the endpoint.
        _ => (StatusCode::BAD_REQUEST, reason),
    };
    api_error(status, reason, Some(detail))
}

/// [`Query`], refusing with the `ApiError` body.
pub struct ApiQuery<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Refusal;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Refusal> {
        match Query::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(r) => Err(refusal(r.status(), "malformed query string", r.body_text())),
        }
    }
}

/// [`Path`], refusing with the `ApiError` body.
pub struct ApiPath<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Refusal;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Refusal> {
        match Path::from_request_parts(parts, state).await {
            Ok(Path(value)) => Ok(Self(value)),
            Err(r) => Err(refusal(r.status(), "malformed path", r.body_text())),
        }
    }
}

/// [`axum::Json`], refusing with the `ApiError` body.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Refusal;

    async fn from_request(req: Request, state: &S) -> Result<Self, Refusal> {
        match axum::Json::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(r) => Err(refusal(r.status(), "malformed JSON body", r.body_text())),
        }
    }
}

/// [`Bytes`], refusing with the `ApiError` body.
pub struct ApiBytes(pub Bytes);

impl<S> FromRequest<S> for ApiBytes
where
    S: Send + Sync,
{
    type Rejection = Refusal;

    async fn from_request(req: Request, state: &S) -> Result<Self, Refusal> {
        match Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(Self(bytes)),
            Err(r) => Err(refusal(
                r.status(),
                "unreadable request body",
                r.body_text(),
            )),
        }
    }
}
