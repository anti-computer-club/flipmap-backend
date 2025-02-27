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
mod requester;
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

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                //TODO: tune later after seeing what's interesting on not-happy path
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
    pub lat: f64,
    #[validate(range(min=-180.0, max=180.0))]
    pub lon: f64,
    pub query: String,
}

#[derive(Serialize)]
pub struct RouteResponse {
    /// This is just a flattened LineString. Requested for easier processing on app.
    pub route: Vec<f64>,
}

/// Proof-of-concept route that turns anchor locations + query into routes.
#[instrument(level = "debug", skip(client))]
async fn route(
    State(client): State<Arc<ExternalRequester>>,
    ValidatedJson(params): ValidatedJson<RouteRequest>,
) -> Result<ValidatedJson<RouteResponse>> {
    // First request to know where to ask for the route's end waypoint
    let req = PhotonGeocodeRequest::new(1, params.query).with_location_bias(params.lat, params.lon);
    let features = client.photon_send(&req).await?;
    // All we want is the coordinates of the point. FeatureCollection -> Feature -> Point
    // Failing to find a geometry, or a point in the geometry is an error
    // ASSUMPTION: geojson will fail to parse if the FeatureCollection has no Feature
    let geometry = features.features[0].geometry.as_ref().ok_or_else(|| {
        RouteError::new_external_parse_failure(
            "failed to find geometry in Photon response".to_owned(),
        )
    })?;
    let end_coord: Position = match &geometry.value {
        geojson::Value::Point(x) => x.clone(),
        v => {
            return Err(RouteError::new_external_parse_failure(format!(
                "found {} geojson datatype instead of Point in Photon response geometry",
                v.type_name()
            )))
        }
    };

    // Second request to actually get the route
    let start_coord: Position = vec![params.lon, params.lat];
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
