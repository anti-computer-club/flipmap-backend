use axum::{
    extract::{rejection::JsonRejection, FromRequest, State},
    response::{IntoResponse, Response},
    routing::{post, get},
    Router,
};
use clap::Parser;
use core::net;
use geojson::Position;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::env;
use std::io::Read; // Needed for read_to_string
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
/// deserialization. Rejection at either stage sends a response back before hitting routes.
struct ValidatedJson<T>(T);
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
    type Rejection = RouteError;
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
    #[arg(env = "HELLO_OSM_IP", value_parser = clap::value_parser!(net::IpAddr))]
    ip: net::IpAddr,
    #[arg(env = "HELLO_OSM_PORT", value_parser = clap::value_parser!(u16).range(1..=65535))]
    port: u16,
    #[arg(short, long, value_parser = clap::value_parser!(reqwest::Url), default_value = "https://api.openrouteservice.org")]
    ors_base: reqwest::Url,
    #[arg(short, long, value_parser = clap::value_parser!(reqwest::Url), default_value = "https://photon.komoot.io")]
    photon_base: reqwest::Url,
}

#[tokio::main]
async fn main() {
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

    // Add both the original "/route" endpoint and our new "/search/:query" endpoint.
    let app: Router = Router::new()
        .route("/route", post(route))
        .route("/search/:query", get(search_handler))
        .with_state(client)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(format!("{}:{}", opts.ip, opts.port))
        .await
        .unwrap();
    tracing::info!("starting server on {}:{}", opts.ip, opts.port);
    axum::serve(listener, app).await.unwrap();
}

/// Extracted by [ValidatedJson] after successful deserialization & validation.
#[derive(Deserialize, Debug, Validate)]
pub struct RouteRequest {
    #[validate(range(min = -90.0, max = 90.0))]
    pub lat: f64,
    #[validate(range(min = -180.0, max = 180.0))]
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
    // First request to know where to ask for the route's end waypoint.
    let req = PhotonGeocodeRequest::new(1, params.query).with_location_bias(params.lat, params.lon);
    let features = client.photon_send(&req).await?;
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

    // Second request to actually get the route.
    let start_coord: Position = vec![params.lon, params.lat];
    let req = OpenRouteRequest {
        instructions: false,
        coordinates: vec![start_coord, end_coord],
    };
    let features = client.ors_send(&req).await?;
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

// =================================================================
// Additional code appended below (do not change the above logic)
// =================================================================

/// Utility: maps a value from one range to another.
fn map_range(value: f64, from_low: f64, from_high: f64, to_low: f64, to_high: f64) -> f64 {
    to_low + (value - from_low) * (to_high - to_low) / (from_high - from_low)
}

/// Define user location constants used by your new functions.
const USER_LAT: &str = "37.7749";
const USER_LON: &str = "-122.4194";

/// Struct to store multiple search results.
#[derive(Serialize, Deserialize)]
struct Location {
    name: String,    // Place name
    lat: f64,        // Latitude
    lon: f64,        // Longitude
    address: Option<String>, // Address 
}

/// Function to fetch multiple places from the Photon API.
fn fetch_places(query: &str) -> Vec<Location> {
    let mut url = reqwest::Url::parse("https://photon.komoot.io/api/").expect("Invalid URL");
    let params = [
        ("q", query.to_string()),
        ("lat", USER_LAT.to_string()),
        ("lon", USER_LON.to_string()),
    ];
    for (key, val) in params {
        url.query_pairs_mut().append_pair(&key, &val);
    }
    let mut search_results_str = String::new();
    reqwest::blocking::get(url)
        .expect("API request failed")
        .read_to_string(&mut search_results_str)
        .expect("Failed to read response");

    let search_json: geojson::FeatureCollection = serde_json::from_str(&search_results_str)
        .expect("Failed to parse JSON");

    let mut locations = Vec::new();
    // Extract multiple destinations.
    for feature in search_json.features {
        if let Some(geojson::Value::Point(coords)) = feature.geometry.map(|g| g.value) {
            // Use serde_json::Map::new() instead of a HashMap.
            let props = feature.properties.unwrap_or_else(|| serde_json::Map::new());
            let name = props.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let address = props.get("address")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            locations.push(Location {
                name,
                lat: coords[1],
                lon: coords[0],
                address,
            });
        }
    }
    locations
}

/// Function to find the route using OpenRouteService.
fn fetch_route(start_lat: &str, start_lon: &str, dest_lat: &str, dest_lon: &str) -> geojson::FeatureCollection {
    let openroute_key = env::var("OPENROUTE_API_KEY").expect("Missing API Key");
    let mut url = reqwest::Url::parse("https://api.openrouteservice.org/v2/directions/driving-car")
        .expect("Broken URL");

    let params = [
        ("api_key", openroute_key.to_string()),
        ("start", format!("{},{}", start_lon, start_lat)),
        ("end", format!("{},{}", dest_lon, dest_lat))
    ];
    for (key, val) in params {
        url.query_pairs_mut().append_pair(&key, &val);
    }

    let mut route_str = String::new();
    reqwest::blocking::get(url)
        .unwrap()
        .read_to_string(&mut route_str)
        .expect("Failed to fetch route");

    serde_json::from_str(&route_str).expect("Failed to parse route JSON")
}

/// New Axum search API handler which returns JSON results.
/// Since fetch_places uses blocking calls, we wrap it in spawn_blocking.
async fn search_handler(axum::extract::Path(query): axum::extract::Path<String>) -> axum::Json<Vec<Location>> {
    let locations = tokio::task::spawn_blocking(move || fetch_places(&query))
        .await
        .expect("spawn_blocking failed");
    axum::Json(locations)
}

