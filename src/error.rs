//! Defines an app-specific error.
//!
//! On creation, it should trace all information that's safe and relevant
//! It can also be serialized into a response that won't give too much information to the client
use tokio::time::Instant;

use axum::{
    extract::rejection::JsonRejection,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use validator::ValidationErrors;

/// All expectable errors. Internal tuple values represent information that's safe to send in
/// response. Most relevant information should be traced rather than placed inside.
#[derive(Debug)]
pub enum RouteError {
    /// HTTP 422(always?): Produced by [axum::Json] when it doesn't like the request. Includes error.
    RequestJson(Box<JsonRejection>),
    /// HTTP 422: Produced by [validator::Validate] when the response can be deserialized, but isn't O.K
    /// semantically (example: lat/lon is a float, but out of bounds)
    RequestConstraint(Box<ValidationErrors>),
    /// HTTP 500: Produced when [serde] (via [reqwest::Response::json]) fails to deserialize an external API response body
    ExternalAPIJson,
    /// HTTP 500: Produced when the external API is deserialized, but lacks content or has unexpected
    /// content that disrupts processing afterwards.
    ExternalAPIContent,
    /// HTTP 500: Produced when a Photon or ORS request fails entirely in [crate::ExternalRequester]
    ExternalAPIRequest,
    /// HTTP 503: Produced when we (maybe this client, maybe another) makes too many calls with [crate::ExternalRequester]
    ///
    /// Contains an instant that gets seralized into a Retry-After header. Not guaranteed it'll be
    /// available 'after', but it is a good-faith estimate.
    ExternalAPILimit(Instant),
}

impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorResponse {
            message: String,
        }
        match self {
            RouteError::RequestJson(err) => {
                let status = err.status();
                let message = err.body_text();
                (status, Json(ErrorResponse { message })).into_response()
            }
            RouteError::RequestConstraint(err) => {
                let status = StatusCode::UNPROCESSABLE_ENTITY;
                let message = format!("good json, bad request semantics: {}", err);
                (status, Json(ErrorResponse { message })).into_response()
            }
            RouteError::ExternalAPIJson => {
                let status = StatusCode::INTERNAL_SERVER_ERROR;
                let message = "problem deserializing external API response".to_owned();
                (status, Json(ErrorResponse { message })).into_response()
            }
            RouteError::ExternalAPIContent => {
                let status = StatusCode::INTERNAL_SERVER_ERROR;
                let message = "problem with content of external API response".to_owned();
                (status, Json(ErrorResponse { message })).into_response()
            }
            RouteError::ExternalAPIRequest => {
                let status = StatusCode::INTERNAL_SERVER_ERROR;
                let message = "problem making call to external API".to_owned();
                (status, Json(ErrorResponse { message })).into_response()
            }
            RouteError::ExternalAPILimit(retry_instant) => {
                let status = StatusCode::SERVICE_UNAVAILABLE;
                let message = "server is overusing external API".to_owned();

                // Create the basic response first
                let mut response = (status, Json(ErrorResponse { message })).into_response();

                // Seconds are preferable to return in retry-after header
                let delay_duration = retry_instant.saturating_duration_since(Instant::now());
                let delay_seconds = delay_duration.as_secs();
                //TODO: Does this work reasonably with improper past instances?

                // Using expect as the conversion from u64 string to HeaderValue should never fail.
                let header_value = HeaderValue::from_str(&delay_seconds.to_string())
                    .expect("Seconds value should always be representable as HeaderValue");

                response
                    .headers_mut()
                    .insert(header::RETRY_AFTER, header_value);

                response // Return the modified response
            }
        }
    }
}

impl RouteError {
    pub fn new_external_parse_failure(msg: String) -> Self {
        tracing::error!("external API content error: {}", msg);
        RouteError::ExternalAPIContent
    }

    // Ensure this constructor receives the Instant
    pub fn new_external_api_limit_failure(retry_after: Instant) -> Self {
        // Kind of silly we do this twice
        let duration = retry_after.saturating_duration_since(Instant::now());
        tracing::error!(
            "external API ratelimit reached, retry suggested after {:?}",
            duration
        );
        RouteError::ExternalAPILimit(retry_after)
    }
}

impl From<reqwest::Error> for RouteError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_decode() {
            //TODO: Can't test rn. Make sure bad JSON responses actually hit this path
            tracing::error!("external API call JSON deserializing error: {}", err);
            RouteError::ExternalAPIJson
        } else {
            tracing::error!("external API call error: {}", err);
            RouteError::ExternalAPIRequest
        }
    }
}

impl From<axum::extract::rejection::JsonRejection> for RouteError {
    fn from(rejection: JsonRejection) -> Self {
        // Not necessarily that important
        tracing::warn!("rejected route JSON: {}", rejection);
        RouteError::RequestJson(Box::new(rejection))
    }
}

impl From<validator::ValidationErrors> for RouteError {
    fn from(rejections: ValidationErrors) -> Self {
        //Validator fails slow and may return /many/ errors in this wacky struct
        //hopefully just printing it is enough info
        tracing::warn!("rejected route JSON after deserializing: {}", rejections);
        RouteError::RequestConstraint(Box::new(rejections))
    }
}
