//! The JSON response every handler answers with.
//!
//! `axum::Json` answers a body that fails to serialize with a plain-text
//! 500. The contract promises the `ApiError` body for that error too
//! (`facts/api.md` § Error Model), and the failure is reachable: a
//! transaction or a box re-serializes its scripts and constants on the way
//! out. [`Json`] is `axum::Json` with that one difference.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::types::ApiError;

/// `axum::Json`, except that a body that fails to serialize answers 500
/// with the `ApiError` body.
#[derive(Debug)]
pub struct Json<T>(pub T);

impl<T> std::ops::Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        match serde_json::to_vec(&self.0) {
            Ok(body) => json_body(body),
            Err(e) => {
                let error = ApiError {
                    error: 500,
                    reason: "response serialization failed".into(),
                    detail: Some(e.to_string()),
                };
                let body = serde_json::to_vec(&error).expect("an ApiError serializes");
                (StatusCode::INTERNAL_SERVER_ERROR, json_body(body)).into_response()
            }
        }
    }
}

/// A 200 answer of `body`, already serialized as JSON.
pub(crate) fn json_body(body: Vec<u8>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value that fails to serialize, as a script or a constant that
    /// won't re-serialize does.
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("constant does not re-serialize"))
        }
    }

    fn read(response: Response) -> (StatusCode, Option<String>, serde_json::Value) {
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().expect("ASCII content type").to_string());
        let body = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .expect("in-memory body");
        let json = serde_json::from_slice(&body).expect("a JSON body");
        (status, content_type, json)
    }

    #[test]
    fn a_body_that_fails_to_serialize_answers_the_api_error() {
        let (status, content_type, body) = read(Json(Unserializable).into_response());
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(
            body,
            serde_json::json!({
                "error": 500,
                "reason": "response serialization failed",
                "detail": "constant does not re-serialize",
            })
        );
    }

    #[test]
    fn a_body_that_serializes_answers_200_json() {
        let (status, content_type, body) = read(Json(vec![8, 4]).into_response());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("application/json"));
        assert_eq!(body, serde_json::json!([8, 4]));
    }
}
