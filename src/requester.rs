//! Wraps [reqwest] to make external API calls to OpenRouteService and Komoot easier.
//! *Not a stable API.*
use std::env;
use std::time::Duration;
use serde::Serialize;

use crate::consts;

// TODO: Constructor for both these maybe
/// Serializable payload for OpenRouteService routing v2 requests.
///
/// **Very unstable.** Implements a tiny subset of options that are immediately useful to the program.
/// See the [Open Route Service API documentation](https://openrouteservice.org/dev/#/api-docs/v2/directions/{profile}/geojson/post) for more.
#[derive(Serialize,Debug)]
pub struct OpenRouteRequest {
    pub coordinates: Vec<geojson::Position>,
    pub instructions: bool,
}

/// Serializable payload for Photon geocoding requests (hosted by Komoot)
///
/// **Unstable.** Has a particularly dumb implementation of sending the anchor point that'll change.
/// See the [Komoot documentation](https://photon.komoot.io/) for more.
#[derive(Serialize,Debug)]
pub struct PhotonGeocodeRequest { 
    pub limit: u8, // Probably just 1 for "where am I" and ~10 for a search
    #[serde(rename(serialize="q"))]
    pub query: String, // Might be possible to use str here
    // TODO: Quick and dirty optional 'anchor' here 
    // in the future we'll use a geojson type with proper deserialization
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

/// Serializable payload for Photon reverse-geocoding requests (hosted by Komoot)
///
/// See the [Komoot documentation](https://photon.komoot.io/) for more.
#[derive(Serialize)]
pub struct PhotonRevGeocodeRequest {
    pub lat: f64,
    pub lon: f64,
}

/// Wraps [reqwest::Client] to provide opinionated initialization and [reqwest:RequestBuilder] for
/// API calls.
///
/// Nothing in this struct actually makes web requests. The yielded [reqwest::RequestBuilder](s) must
/// still be sent, awaited, and checked for errors elsewhere.
pub struct ExternalRequester {
    /// Wrapped client. Will be created for you, against your will. You're welcome.
    client: reqwest::Client,
    /// Required to make ORS calls.
    open_route_service_key: String,
    // We also use Photon (via Komoot) but it has an unauthenticated API
}

impl ExternalRequester {
    /// Makes the requester with the settings you probably need.
    ///
    /// # Errors 
    /// May be caused by problem in the TLS backend. See [reqwest::ClientBuilder::build].
    ///
    /// # Panics
    /// Caused if environment variable `ORS_API_KEY` is unset. This is not ideal.
    pub fn new() -> Self {
    ExternalRequester {
            client: 
                reqwest::Client::builder()
                    .user_agent(consts::USER_AGENT)
                    .timeout(Duration::from_secs(10))
                    .https_only(true)
                    .build()
                    .expect("req client construction failed"),
            open_route_service_key:
                // TODO: Allow reading from a file too and other such logic
                env::var("ORS_API_KEY")
                    .expect("Place an Open Route Service API key in ORS_API_KEY env variable!")
                    .to_string(),
    }
}
    // TODO: Re-evaluate if these are useful here. They just make futures
    /// Hard-coded request to OpenRouteService v2 directions endpoint. Will yield geojson.
    pub fn ors(&self, req: &OpenRouteRequest) -> reqwest::RequestBuilder {
        self.client.post("https://api.openrouteservice.org/v2/directions/driving-car/geojson")
            .header("Content-Type", "application/json")
            .header("Authorization", &self.open_route_service_key)
            .json(req)
    }

    /// Hard-coded request to Komoot's reverse geocoding endpoint. Will yield geojson.
    pub fn photon_reverse(&self, coord: PhotonRevGeocodeRequest) -> reqwest::RequestBuilder {
        let q = [("lon",coord.lon),("lat",coord.lat)];
        self.client.get("https://photon.komoot.io/reverse").query(&q)
    }

    /// Hard-coded request to Komoot's main geocoding endpoint. Will yield geojson.
    pub fn photon(&self, req: &PhotonGeocodeRequest) -> reqwest::RequestBuilder {
        self.client.get("https://photon.komoot.io/api/").query(req)
    }
}
