//! Defines an app-specific error.
//!
//! On creation, it should trace all information that's safe and relevant
//! It can also be serialized into a response that won't give too much information to the client
use axum::{
    extract::rejection::JsonRejection,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

pub struct RouteError {
    kind: Kind,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}
enum Kind {
    BadRequestJson,
    // TODO: See if we need to actually capture a string here or we can trace state outside
    ExternalAPIParse(String),
    ExternalAPIRequest,
}

impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorResponse {
            message: String,
        }
        let (status, message) = match self.kind {
            //TODO: Probably pass through whatever message and statsucode for BadReqJson
            // AKA internal parse failure, because it should be handled by JSON extractor
            Kind::BadRequestJson => (StatusCode::BAD_REQUEST, "OOPS!".to_owned()),
            Kind::ExternalAPIParse(_) => (
                // Purposely vague. Pretty sure it should be 500 because we're not a gateway?
                StatusCode::INTERNAL_SERVER_ERROR,
                "problem parsing external API response".to_owned(),
            ),
            Kind::ExternalAPIRequest => (StatusCode::INTERNAL_SERVER_ERROR, "OOPS!".to_owned()),
        };
        (status, Json(ErrorResponse { message })).into_response()
    }
}

impl RouteError {
    pub fn new_external_parse_failure(msg: String) -> Self {
        RouteError {
            kind: Kind::ExternalAPIParse(msg),
            source: None,
        }
    }
}

// TODO: Decide where to tracepoint, probably here. Also, distinguish parse problems
impl From<reqwest::Error> for RouteError {
    fn from(value: reqwest::Error) -> Self {
        RouteError {
            kind: Kind::ExternalAPIRequest,
            source: Some(Box::new(value)),
        }
    }
}

impl From<axum::extract::rejection::JsonRejection> for RouteError {
    fn from(_: JsonRejection) -> Self {
        RouteError {
            kind: Kind::BadRequestJson,
            //TODO: Need details and/or tracept here
            source: None, //Not a proper error?
        }
    }
}

/* I suspect using Reqwest JSONization directly emits a reqwest error
impl From<serde_json::Error> for RouteError {
    fn from(value: serde_json::Error) -> Self {
        RouteError {
            kind: Kind::ExternalAPIParse,
            source: Some(Box::new(value)),
        }
    }
}
*/
