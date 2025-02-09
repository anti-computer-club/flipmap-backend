use axum::{response::IntoResponse, routing::post, Router};
use geojson::Position;
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Arc;
mod consts;
mod error;
mod requester;

use crate::error::RouteError;
use crate::requester::{ExternalRequester, OpenRouteRequest, PhotonGeocodeRequest};
use axum::extract::{FromRequest, State};
use axum::response::Response;

type Result<T> = std::result::Result<T, RouteError>;

// Create our own JSON extractor by wrapping `axum::Json`. This makes it easy to override the
// rejection and provide our own which formats errors to match our application.
//
// `axum::Json` responds with plain text if the input is invalid.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(RouteError))]
struct AppJson<T>(T);
impl<T> IntoResponse for AppJson<T>
where
    axum::Json<T>: IntoResponse,
{
    fn into_response(self) -> Response {
        //TODO: Customize as needed for errors
        axum::Json(self.0).into_response()
    }
}

#[tokio::main]
async fn main() {
    let client = Arc::new(ExternalRequester::new());
    let app: Router = Router::new()
        .route("/route", post(route))
        .with_state(client);

    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} <IP> <PORT>", args[0]);
        std::process::exit(1);
    }
    let ip = &args[1];
    let port = &args[2];

    println!("Starting server on {}:{}", ip, port);
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", ip, port))
        .await
        .unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[derive(Deserialize)]
pub struct RouteRequest {
    pub lat: f64,
    pub lon: f64,
    pub query: String,
}

#[derive(Serialize)]
pub struct RouteResponse {
    /// This is just a flattened LineString. Requested for easier processing on app.
    pub route: Vec<f64>,
}

async fn route(
    State(client): State<Arc<ExternalRequester>>,
    AppJson(params): AppJson<RouteRequest>,
) -> Result<AppJson<RouteResponse>> {
    /*
    // Photon will also do this (and identify the wrong param) but let's fail fast
    // TODO: May or may not be preferable to do this during deserialization??
    if (params.lat < -90.0 || params.lat > 90.0) || (params.lon < -180.0 && params.lon > 180.0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(RouteResponse {
                route: None,
                errmsg: Some("AHHH!".to_owned()),
            }),
        );
    }
    */

    // First request to know where to ask for the route's end waypointj
    let req = PhotonGeocodeRequest {
        lat: Some(params.lat),
        lon: Some(params.lon),
        limit: 1,
        query: params.query,
    };
    dbg!(&req); //TODO: Replace all this with a proper trace layer
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
    dbg!(&req);
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
    Ok(AppJson(RouteResponse { route }))
}
