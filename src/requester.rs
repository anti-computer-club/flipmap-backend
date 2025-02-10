//! Wraps [reqwest] to make external API calls to OpenRouteService and Komoot easier.
//! *Not a stable API.*
use std::env;
use std::time::Duration;
use serde::Serialize;
use secrecy::{ExposeSecret, SecretString};
use tracing::instrument;
use crate::consts;
use crate::Result;
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
#[derive(Serialize,Debug)]
pub struct PhotonRevGeocodeRequest {
    pub lat: f64,
    pub lon: f64,
}

/// Wraps [reqwest::Client] to provide opinionated [reqwest::RequestBuilder] preparation (probably
/// don't) or execution and parsing of external API endpoints.
#[derive(Debug)]
pub struct ExternalRequester {
    /// Wrapped client. Will be created for you, against your will. You're welcome.
    client: reqwest::Client,
    // Shouldn't leak to logs unless Reqwest traces headers? Won't get sent over wire in response either way
    open_route_service_key: SecretString,
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
                    .to_string().into(),
    }
}
    /// Prepare a request (builder) to OpenRouteService v2 directions endpoint. Will yield geojson.
    #[deprecated(note="use _send methods for error handling and simpler logic")]
    #[instrument(skip(self))]
    pub fn ors(&self, req: &OpenRouteRequest) -> reqwest::RequestBuilder {
        self.client.post("https://api.openrouteservice.org/v2/directions/driving-car/geojson")
            .header("Content-Type", "application/json")
            .header("Authorization", self.open_route_service_key.expose_secret())
            .json(req)
    }

    /// Prepare a request (builder) to Komoot's main geocoding endpoint. Will yield geojson.
    #[deprecated(note="use _send methods for error handling and simpler logic")]
    #[instrument(skip(self))]
    pub fn photon(&self, req: &PhotonGeocodeRequest) -> reqwest::RequestBuilder {
        self.client.get("https://photon.komoot.io/api/").query(req)
    }

    /// Prepare a request (builder) to Komoot's reverse geocoding endpoint. Will yield geojson.
    #[deprecated(note="use _send methods for error handling and simpler logic")]
    #[instrument(skip(self))]
    pub fn photon_reverse(&self, coord: PhotonRevGeocodeRequest) -> reqwest::RequestBuilder {
        let q = [("lon",coord.lon),("lat",coord.lat)];
        self.client.get("https://photon.komoot.io/reverse").query(&q)
    }

    /// Prepare *and execute* a request to OpenRouteService v2 directions endpoint.
    ///
    /// # Errors 
    /// [ExternalAPIRequest][crate::error::RouteError::ExternalAPIRequest]: if [reqwest] fails for network reasons
    /// 
    /// [ExternalAPIParse][crate::error::RouteError::ExternalAPIParse]: if [reqwest] tries to use [serde] to deserialize into
    /// [geojson::FeatureCollection] and fails
    #[instrument(skip(self))]
    pub async fn ors_send(&self, req: &OpenRouteRequest) -> Result<geojson::FeatureCollection> {
        let res = self.client.post("https://api.openrouteservice.org/v2/directions/driving-car/geojson")
                    .header("Content-Type", "application/json")
                    .header("Authorization", self.open_route_service_key.expose_secret())
                    .json(req).send().await?;
        let obj = res.json::<geojson::FeatureCollection>().await?;
        Ok(obj)
    }

    /// Prepare *and execute* a request to Photon's reverse geocoding endpoint.
    ///
    /// # Errors 
    /// [ExternalAPIRequest][crate::error::RouteError::ExternalAPIRequest]: if [reqwest] fails for network reasons
    /// 
    /// [ExternalAPIParse][crate::error::RouteError::ExternalAPIParse]: if [reqwest] tries to use [serde] to deserialize into
    /// [geojson::FeatureCollection] and fails
    #[instrument(skip(self))]
    pub async fn photon_reverse_send(&self, coord: PhotonRevGeocodeRequest) -> Result<geojson::FeatureCollection> {
        let q = [("lon",coord.lon),("lat",coord.lat)];
        let res = self.client.get("https://photon.komoot.io/reverse").query(&q).send().await?;
        let obj = res.json::<geojson::FeatureCollection>().await?;
        Ok(obj)
    }

    /// Prepare *and execute* a request to Photon's geocoding endpoint.
    ///
    /// # Errors 
    /// [ExternalAPIRequest][crate::error::RouteError::ExternalAPIRequest]: if [reqwest] fails for network reasons
    /// 
    /// [ExternalAPIParse][crate::error::RouteError::ExternalAPIParse]: if [reqwest] tries to use [serde] to deserialize into
    /// [geojson::FeatureCollection] and fails
    #[instrument(skip(self))]
    pub async fn photon_send(&self, req: &PhotonGeocodeRequest) -> Result<geojson::FeatureCollection> {
        let res = self.client.get("https://photon.komoot.io/api/").query(req).send().await?;
        let obj = res.json::<geojson::FeatureCollection>().await?;
        Ok(obj)
    }
}
