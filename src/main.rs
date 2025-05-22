use axum::{
    extract::{rejection::JsonRejection, FromRequest, State},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use clap::Parser;
use core::net;
use geojson::Position;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::env;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::instrument;
use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt};
use validator::Validate;

mod error;
mod ratelimit;
mod retry_after;
//TODO: Reverse geocoding is ready but no route exists here & app FE is not ready
#[allow(dead_code)]
mod requester;
#[cfg(test)]
mod test_utils;
use crate::error::RouteError;
use crate::requester::{ExternalRequester, OpenRouteRequest, PhotonGeocodeRequest};

pub(crate) type Result<T> = std::result::Result<T, RouteError>;

/// Wraps [axum::Json] so that we can validate requests with [validator::Validate] after
/// deserialization. Rejection at either stage sends a response back before hitting routes
struct ValidatedJson<T>(T);
// Pass-through. There's no derive macro so we have to impl. Response formatting is via error
impl<T> IntoResponse for ValidatedJson<T>
where
    axum::Json<T>: IntoResponse,
{
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}
impl<T, S> FromRequest<S> for ValidatedJson<T>
where
    T: DeserializeOwned + Validate,
    S: Send + Sync,
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = RouteError; // Why is this required? Compiler made me. 'ate generics.
    async fn from_request(
        req: axum::extract::Request,
        state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        let axum::Json(data) = axum::Json::<T>::from_request(req, state).await?;
        data.validate()?;
        Ok(ValidatedJson(data))
    }
}

/// Arguments as parsed by [clap]. Not used outside [main].
#[derive(clap::Parser, Debug)]
struct Opt {
    // Tried to make these compile-time dynamic to crate name. Seems impossible w/ stdlib
    #[arg(env = "HELLO_OSM_IP", value_parser = clap::value_parser!(net::IpAddr))]
    ip: net::IpAddr,
    #[arg(env = "HELLO_OSM_PORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    port: u16,
    #[arg(short,long, value_parser = clap::value_parser!(reqwest::Url), default_value = "https://api.openrouteservice.org")]
    ors_base: reqwest::Url,
    #[arg(short, long, value_parser = clap::value_parser!(reqwest::Url), default_value = "https://photon.komoot.io")]
    photon_base: reqwest::Url,
    // I'd put the API key here but clap purposely seems to deny the ability to ONLY allow w/ env
}

fn tracing_subscribe() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!(
                    "{}=debug,tower_http=debug,axum=trace,hyper_util=warn",
                    env!("CARGO_CRATE_NAME")
                )
                .into()
            }),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_thread_ids(true),
        )
        .init();
}

#[tokio::main]
async fn main() {
    tracing_subscribe();

    let ors_key: secrecy::SecretString = env::var("ORS_API_KEY")
        .expect("Place an Open Route Service API key in ORS_API_KEY env variable!")
        .to_string()
        .into();

    let opts = Opt::parse();
    tracing::trace!("parsed args: {:?}", &opts);

    // Re-used Reqwest client for external API calls
    let client = Arc::new(ExternalRequester::new(
        opts.ors_base,
        opts.photon_base,
        ors_key,
    ));
    tracing::trace!("created reqwest client: {:?}", &client);

    let app: Router = Router::new()
        .route("/route", post(route))
        .route("/get_locations", post(get_locations))
        .with_state(client)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", opts.ip, opts.port))
        .await
        .unwrap();
    tracing::info!("starting server on {}:{}", opts.ip, opts.port);
    axum::serve(listener, app).await.unwrap();
}

/// Extracted by [ValidatedJson] after succesful deserialization & validation
#[derive(Deserialize, Debug, Validate)]
pub struct RouteRequest {
    #[validate(range(min=-90.0, max=90.0))]
    pub src_lat: f64,
    #[validate(range(min=-180.0, max=180.0))]
    pub src_lon: f64,
    #[validate(range(min=-90.0, max=90.0))]
    pub dst_lat: f64,
    #[validate(range(min=-180.0, max=180.0))]
    pub dst_lon: f64,
}

#[derive(Serialize)]
pub struct RouteResponse {
    /// This is just a flattened LineString. Requested for easier processing on app.
    pub route: Vec<f64>,
}

/// Simple point-to-point route that takes a single starting and ending position.
#[instrument(level = "debug", skip(client))]
async fn route(
    State(client): State<Arc<ExternalRequester>>,
    ValidatedJson(params): ValidatedJson<RouteRequest>,
) -> Result<ValidatedJson<RouteResponse>> {
    let start_coord: Position = vec![params.src_lon, params.src_lat];
    let end_coord: Position = vec![params.dst_lon, params.dst_lat];
    let req = OpenRouteRequest {
        instructions: false,
        coordinates: vec![start_coord, end_coord],
    };
    let features = client.ors_send(&req).await?;
    // Grab the LineString from the ORS route, then remove interior arrays to make app processing easier
    let geometry = features.features[0].geometry.as_ref().ok_or_else(|| {
        RouteError::new_external_parse_failure(
            "failed to find geometry in Photon response".to_owned(),
        )
    })?;
    let route: Vec<f64> = match &geometry.value {
        geojson::Value::LineString(x) => x.clone(),
        v => {
            return Err(RouteError::new_external_parse_failure(format!(
                "found {} geojson datatype instead of LineString in ORS response geometry",
                v.type_name()
            )))
        }
    }
    .into_iter()
    .flatten()
    .collect();
    Ok(ValidatedJson(RouteResponse { route }))
}

#[derive(Deserialize, Debug, Validate)]
pub struct GetLocationsRequest {
    #[validate(range(min=-90.0, max=90.0))]
    pub lat: f64,
    #[validate(range(min=-180.0, max=180.0))]
    pub lon: f64,
    pub query: String,
    #[validate(range(min = 1, max = 20))]
    pub amount: u8,
}

#[derive(Serialize)]
pub struct GetLocationsResponse {
    pub results: Vec<PlaceResult>,
}

#[derive(Serialize)]
pub struct PlaceResult {
    pub lat: f64,
    pub lon: f64,
    pub name: String,
}

/// Used by the app to search out locations from a given position
#[instrument(level = "debug", skip(client))]
async fn get_locations(
    State(client): State<Arc<ExternalRequester>>,
    ValidatedJson(params): ValidatedJson<GetLocationsRequest>,
) -> Result<ValidatedJson<GetLocationsResponse>> {
    let req = PhotonGeocodeRequest::new(params.amount, params.query)
        .with_location_bias(params.lat, params.lon);
    let features = client.photon_send(&req).await?;

    let results = features
        .features
        .iter()
        .map(|feature| {
            let geometry = feature.geometry.as_ref().ok_or_else(|| {
                RouteError::new_external_parse_failure(
                    "failed to find geometry in Photon response".to_owned(),
                )
            })?;
            let coords: Position = match &geometry.value {
                geojson::Value::Point(x) => x.clone(),
                v => {
                    return Err(RouteError::new_external_parse_failure(format!(
                        "found {} geojson datatype instead of Point in Photon response geometry",
                        v.type_name()
                    )))
                }
            };

            let name = feature
                .properties
                .as_ref() // Ensure properties is not None
                .and_then(|properties| properties.get("name")) // Try to get "name" from properties
                .and_then(|value| value.as_str()) // Convert the Value to &str (if it is a string)
                .unwrap_or("Unknown") // If "name" doesn't exist or is not a string, use "Unknown"
                .to_string(); // Convert the &str to String

            Ok(PlaceResult {
                lat: coords[1],
                lon: coords[0],
                name,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(ValidatedJson(GetLocationsResponse { results }))
}
