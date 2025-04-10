//! Wraps [reqwest] to make external API calls to OpenRouteService and Komoot easier.
//! *Not a stable API.*
use crate::{
    error::RouteError,
    ratelimit::{LimitChain, RateLimit},
    Result,
};
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use std::time::Duration;
use tracing::instrument;

/// Sent over the wire when [ExternalRequester] makes requests.
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"),);

/// Serializable payload for OpenRouteService routing v2 requests.
///
/// **Very unstable.** Implements a tiny subset of options that are immediately useful to the program.
/// See the [Open Route Service API documentation](https://openrouteservice.org/dev/#/api-docs/v2/directions/{profile}/geojson/post) for more.
#[derive(Serialize, Debug)]
pub struct OpenRouteRequest {
    pub coordinates: Vec<geojson::Position>,
    pub instructions: bool,
}

/// Serializable payload for Photon geocoding requests (hosted by Komoot)
///
/// **Unstable.** Has a particularly dumb implementation of sending the anchor point that'll change.
/// See the [Komoot documentation](https://photon.komoot.io/) for more.
#[derive(Serialize, Debug)]
pub struct PhotonGeocodeRequest {
    pub limit: u8, // Probably just 1 for "where am I" and ~10 for a search
    #[serde(rename(serialize = "q"))]
    pub query: String, // Might be possible to use str here
    lat: Option<f64>,
    lon: Option<f64>,
}

impl PhotonGeocodeRequest {
    // Not actually sure what this does perf-wise, doesn't really matter
    /// Not necessarily an 'anchor' in strong terms. Influences results, though.
    pub fn with_location_bias(self, lat: f64, lon: f64) -> Self {
        PhotonGeocodeRequest {
            limit: self.limit,
            query: self.query,
            lat: Some(lat),
            lon: Some(lon),
        }
    }

    /// Creates a basic query struct *without* a location bias
    pub fn new(limit: u8, query: String) -> Self {
        PhotonGeocodeRequest {
            limit,
            query,
            lat: None,
            lon: None,
        }
    }
}

/// Serializable payload for Photon reverse-geocoding requests (hosted by Komoot)
///
/// See the [Komoot documentation](https://photon.komoot.io/) for more.
#[derive(Serialize, Debug)]
pub struct PhotonRevGeocodeRequest {
    pub lat: f64,
    pub lon: f64,
}

impl PhotonRevGeocodeRequest {
    // This could be a trait, but I don't think it's intuitive enough to be desirable
    /// Convenience/safety method for direct conversion
    pub fn from_position(pos: geojson::Position) -> Self {
        PhotonRevGeocodeRequest {
            lon: pos[0],
            lat: pos[1],
        }
    }
}

/// Wraps [reqwest::Client] to provide opinionated execution and parsing of external API endpoints.
#[derive(Debug)]
pub struct ExternalRequester {
    /// Wrapped client. Will be created for you, against your will. You're welcome.
    client: reqwest::Client,
    // Shouldn't leak to logs unless Reqwest traces headers? Won't get sent over wire in response either way
    open_route_service_key: SecretString,

    // client.post() won't take &Url but .clone() is no worse than passing &str and front-loads error checking
    ors_directions: Url,
    photon: Url,
    photon_reverse: Url,

    /// They don't enforce limits so we do this to be polite
    photon_limiter: LimitChain<'static>,
}

impl ExternalRequester {
    /// Makes the requester with the settings you probably need.
    ///
    ///
    /// # Panics
    /// May be caused by problem in the TLS backend. See [reqwest::ClientBuilder::build].
    /// Caused if a proper 'base' [Url] was parsed into [Opt](crate::Opt), but somehow can't be extended
    /// with the exact endpoints hardcoded here
    pub fn new(ors_base: Url, photon_base: Url, open_route_service_key: SecretString) -> Self {
        // These might not actually be constant among all deployments. Works for now.
        // Could shift defaults into here and use a builder pattern if needed?
        const ORS_DIRECTIONS_PATH: &str = "/v2/directions/driving-car/geojson";
        const PHOTON_PATH: &str = "/api/";
        const PHOTON_REVERSE_PATH: &str = "/reverse";

        // Parity with OpenRouteService limits (may or may not be a good idea)
        let photon_limits = [
            RateLimit::new(40, Duration::from_secs(60)),
            RateLimit::new(2000, Duration::from_secs(86400)),
        ];
        // Not sure if optimal, but point is to make it static
        let photon_limiter = LimitChain::new_from(Box::leak(Box::new(photon_limits)));

        ExternalRequester {
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(10))
                .https_only(true)
                .build()
                .unwrap_or_else(|e| panic!("couldn't build reqwest Client: {:?}", e)),
            open_route_service_key,
            ors_directions: ors_base
                .join(ORS_DIRECTIONS_PATH)
                .unwrap_or_else(|e| panic!("couldn't assemble ors directions full URL: {:?}", e)),
            photon: photon_base
                .join(PHOTON_PATH)
                .unwrap_or_else(|e| panic!("couldn't assemble photon geocoding full URL: {:?}", e)),
            photon_reverse: photon_base.join(PHOTON_REVERSE_PATH).unwrap_or_else(|e| {
                panic!("couldn't assemble photon rev geocoding full URL: {:?}", e)
            }),
            photon_limiter,
        }
    }

    /// Prepare *and execute* a request to OpenRouteService v2 directions endpoint.
    ///
    /// # Errors
    /// [ExternalAPIRequest][crate::error::RouteError::ExternalAPIRequest]: if [reqwest] fails for network reasons
    ///
    /// [ExternalAPIJson][crate::error::RouteError::ExternalAPIJson]: if [reqwest] tries to use [serde] to deserialize into
    /// [geojson::FeatureCollection] and fails
    #[instrument(skip(self))]
    pub async fn ors_send(&self, req: &OpenRouteRequest) -> Result<geojson::FeatureCollection> {
        let res = self
            .client
            .post(self.ors_directions.clone())
            .header("Content-Type", "application/json")
            .header("Authorization", self.open_route_service_key.expose_secret())
            .json(req)
            .send()
            .await?;
        let obj = res.json::<geojson::FeatureCollection>().await?;
        Ok(obj)
    }

    /// Prepare *and execute* a request to Photon's reverse geocoding endpoint.
    ///
    /// # Errors
    /// [ExternalAPIRequest][crate::error::RouteError::ExternalAPIRequest]: if [reqwest] fails for network reasons
    ///
    /// [ExternalAPIJson][crate::error::RouteError::ExternalAPIJson]: if [reqwest] tries to use [serde] to deserialize into
    /// [geojson::FeatureCollection] and fails
    #[instrument(skip(self))]
    pub async fn photon_reverse_send(
        &self,
        coord: PhotonRevGeocodeRequest,
    ) -> Result<geojson::FeatureCollection> {
        let q = [("lon", coord.lon), ("lat", coord.lat)];
        let res = self
            .client
            .get(self.photon_reverse.clone())
            .query(&q)
            .send()
            .await?;
        let obj = res.json::<geojson::FeatureCollection>().await?;
        Ok(obj)
    }

    /// Prepare *and execute* a request to Photon's geocoding endpoint.
    ///
    /// # Errors
    /// [ExternalAPIRequest][crate::error::RouteError::ExternalAPIRequest]: if [reqwest] fails for network reasons
    ///
    /// [ExternalAPIJson][crate::error::RouteError::ExternalAPIJson]: if [reqwest] tries to use [serde] to deserialize into
    /// [geojson::FeatureCollection] and fails
    #[instrument(skip(self))]
    pub async fn photon_send(
        &self,
        req: &PhotonGeocodeRequest,
    ) -> Result<geojson::FeatureCollection> {
        let res = self
            .client
            .get(self.photon.clone())
            .query(req)
            .send()
            .await?;
        let obj = res.json::<geojson::FeatureCollection>().await?;
        Ok(obj)
    }

    //FIXME: This interface may not be viable for use inside a route
    /// Not used internally. Should be used at the start of the route to fail faster given
    /// knowledge of how many calls in total we'll want to make
    pub fn try_photon_limit(&self, n: u32) -> Option<RouteError> {
        if self.photon_limiter.try_consume(n) {
            None
        } else {
            //TODO: Need to pass this more context
            Some(RouteError::new_external_api_limit_failure())
        }
    }
}
