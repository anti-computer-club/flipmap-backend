//! Wraps [reqwest] to make external API calls to OpenRouteService and Komoot easier.
//! *Not a stable API.*
use crate::{
    error::RouteError,
    ratelimit::{LimitChain, RateLimit},
    retry_after::{self, BackerOff},
    Result,
};
use reqwest::{header, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use tokio::time::Duration;
use tracing::instrument;

// Testing without HTTPS is much easier. Otherwise, no excuse.
#[cfg(test)]
const HTTPS_ONLY: bool = false;
#[cfg(not(test))]
const HTTPS_ONLY: bool = true;

/// Sent over the wire when [ExternalRequester] makes requests.
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"),);

// Hoisted because these are used in test code and normal code
const ORS_DIRECTIONS_PATH: &str = "/v2/directions/driving-car/geojson";
const PHOTON_PATH: &str = "/api/";
const PHOTON_REVERSE_PATH: &str = "/reverse";

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

/// Used to construct [ExternalRequester]. Niche and opinionated defaults are deployed for endpoint
/// URLs and Photon rate-limiting if the setters are not used.
#[derive(Clone, Debug)]
pub struct ExternalRequesterBuilder {
    // The client *could* be configurable here.
    open_route_service_key: SecretString,

    ors_base: Url,
    photon_base: Url,

    // Sue me. It's internal
    photon_limit_params: Vec<(u32, Duration, String)>,
    // BackerOffs are not configurable.
}

impl ExternalRequesterBuilder {
    pub fn new(ors_base: Url, photon_base: Url, open_route_service_key: SecretString) -> Self {
        Self {
            open_route_service_key,
            ors_base,
            photon_base,
            photon_limit_params: vec![],
        }
    }

    pub fn with_photon_ratelimiter(
        mut self,
        requests_allowed: u32,
        reset_time: Duration,
        name: String,
    ) -> Self {
        self.photon_limit_params
            .push((requests_allowed, reset_time, name));
        self
    }

    pub fn build(self) -> ExternalRequester {
        let ratelimit_params = if self.photon_limit_params.is_empty() {
            vec![
                // Parity with OpenRouteService limits (may or may not be a good idea)
                (40, Duration::from_secs(60), "Photon Minutely".to_string()),
                (2000, Duration::from_secs(86400), "Photon Daily".to_string()),
            ]
        } else {
            self.photon_limit_params
        };

        let photon_limits: Vec<RateLimit> = ratelimit_params
            .iter()
            .map(|truple| RateLimit::new(truple.0, truple.1, truple.2.clone()))
            .collect();
        // Not sure if optimal, but making this static here makes life way easier
        let photon_limiter = LimitChain::new_from(Box::leak(photon_limits.into_boxed_slice()));

        ExternalRequester {
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(10))
                .https_only(HTTPS_ONLY)
                .build()
                .unwrap_or_else(|e| panic!("couldn't build reqwest Client: {:?}", e)),
            open_route_service_key: self.open_route_service_key,
            ors_directions: self
                .ors_base
                .join(ORS_DIRECTIONS_PATH)
                .unwrap_or_else(|e| panic!("couldn't assemble ors directions full URL: {:?}", e)),
            photon: self
                .photon_base
                .join(PHOTON_PATH)
                .unwrap_or_else(|e| panic!("couldn't assemble photon geocoding full URL: {:?}", e)),
            photon_reverse: self
                .photon_base
                .join(PHOTON_REVERSE_PATH)
                .unwrap_or_else(|e| {
                    panic!("couldn't assemble photon rev geocoding full URL: {:?}", e)
                }),
            photon_limiter,
            ors_retry_after: BackerOff::new().with_name("OpenRouteService".to_string()),
            photon_retry_after: BackerOff::new().with_name("Photon".to_string()),
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
    /// If present, a time after which the next request is allowed, according to ORS
    ors_retry_after: BackerOff,
    /// If present, a time after which the next request is allowed, according to Komoot
    photon_retry_after: BackerOff,
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
        ExternalRequesterBuilder::new(ors_base, photon_base, open_route_service_key).build()
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
        self.ors_retry_after.can_request()?;
        let res = self
            .client
            .post(self.ors_directions.clone())
            .header("Content-Type", "application/json")
            .header("Authorization", self.open_route_service_key.expose_secret())
            .json(req)
            .send()
            .await?;

        let good_res = Self::check_limiting_status(res, &self.ors_retry_after)?;
        let obj = good_res.json::<geojson::FeatureCollection>().await?;
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
        coord: &PhotonRevGeocodeRequest,
    ) -> Result<geojson::FeatureCollection> {
        self.photon_retry_after.can_request()?; // Checks for backoff period
        self.check_photon_limit(1)?; // Checks our own ratelimiter
        let q = [("lon", coord.lon), ("lat", coord.lat)];
        let res = self
            .client
            .get(self.photon_reverse.clone())
            .query(&q)
            .send()
            .await?;

        // This checks if we need to set a backoff period in response to this call
        let good_res = Self::check_limiting_status(res, &self.photon_retry_after)?;
        let obj = good_res.json::<geojson::FeatureCollection>().await?;
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
        self.photon_retry_after.can_request()?;
        self.check_photon_limit(1)?;
        let res = self
            .client
            .get(self.photon.clone())
            .query(req)
            .send()
            .await?;

        let good_res = Self::check_limiting_status(res, &self.photon_retry_after)?;
        let obj = good_res.json::<geojson::FeatureCollection>().await?;
        Ok(obj)
    }

    // Originally this was intended for pub use in routes where we may know that we want more than
    // 1 request, but that's bad ergonomics and we have no routes which even use that yet
    // Wraps the generic [Instant] error in something usable by the web server directly
    fn check_photon_limit(&self, n: u32) -> Result<()> {
        self.photon_limiter
            .try_consume(n)
            .map_err(RouteError::new_external_api_limit_failure)
    }

    /// Checks if the response indicates a rate limit (429/503) and sets the backoff accordingly.
    /// Returns `Err(RouteError::ExternalAPILimit)` if backoff was triggered, otherwise Ok(response).
    fn check_limiting_status(
        resp: reqwest::Response,
        backer_off: &BackerOff,
    ) -> Result<reqwest::Response> {
        let status = resp.status();
        // We only care about these response types (429|503). Any other !200 response is out of scope
        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
            let maybe_retry_val = resp
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|val| val.to_str().ok());

            // Set backoff based on header or default, and get the resulting Instant
            if let Some(value) = maybe_retry_val {
                match backer_off.parse_maybe_set(value) {
                    Ok(_) => {}
                    Err(retry_after::Error::ParseFail(s)) => {
                        tracing::warn!("using default retry-after due to unparsable header: {s}");
                        backer_off.set_without_header();
                    }
                    Err(retry_after::Error::FromPast) => {
                        tracing::warn!("passing request along because remote returned retry-after from the past");
                        return Ok(resp); // sue me
                    }
                }
            } else {
                tracing::warn!("got {status} from request but no Retry-After value, using default");
                backer_off.set_without_header();
            };

            match backer_off.get_retry_until() {
                Some(inst) => Err(RouteError::ExternalAPILimit(inst)),
                None => {
                    tracing::error!("attempted to set retry-after, but query afterwards found none! passing request...");
                    Ok(resp) // Good luck lil' buddy
                }
            }
        } else {
            // Not a limiting status code, pass the response through.
            Ok(resp)
        }
    }
}

// These are more like janky partial-integration tests rather than real unit tests.
// Problem is, using external unit testing requires making more internals changable at runtime
// AND syncing internal test state + external mock state (because these mocks must be stateful)
// TODO: Wiremock (real one, not the Rust crate), Yaak, Postman, who knows?
#[cfg(test)]
mod tests {
    use super::*;
    use crate::retry_after;

    use httpdate::fmt_http_date;
    use httpmock::prelude::*;
    use serde_json::Value;
    use std::time::SystemTime;
    use tokio::{task, time};

    // We have to convert these into json at runtime because serde_json is !const
    const ORS_DIRECTIONS_EXAMPLE: &str = "{\"type\":\"FeatureCollection\",\"bbox\":[-123.280691,44.567643,-123.277631,44.569025],\"features\":[{\"bbox\":[-123.280691,44.567643,-123.277631,44.569025],\"type\":\"Feature\",\"properties\":{\"segments\":[{\"distance\":493.8,\"duration\":94.6,\"steps\":[{\"distance\":89.8,\"duration\":21.5,\"type\":11,\"instruction\":\"Head west\",\"name\":\"-\",\"way_points\":[0,4]},{\"distance\":176.5,\"duration\":42.4,\"type\":1,\"instruction\":\"Turn right onto Northwest Orchard Avenue\",\"name\":\"Northwest Orchard Avenue\",\"way_points\":[4,6]},{\"distance\":198.9,\"duration\":23.9,\"type\":3,\"instruction\":\"Turn sharp right onto Monroe Avenue\",\"name\":\"Monroe Avenue\",\"way_points\":[6,10]},{\"distance\":28.6,\"duration\":6.9,\"type\":2,\"instruction\":\"Turn sharp left onto Northwest 23rd Street\",\"name\":\"Northwest 23rd Street\",\"way_points\":[10,11]},{\"distance\":0.0,\"duration\":0.0,\"type\":10,\"instruction\":\"Arrive at Northwest 23rd Street, on the left\",\"name\":\"-\",\"way_points\":[11,11]}]}],\"way_points\":[0,11],\"summary\":{\"distance\":493.8,\"duration\":94.6}},\"geometry\":{\"coordinates\":[[-123.279959,44.567648],[-123.280643,44.567643],[-123.280691,44.567669],[-123.28069,44.567765],[-123.280687,44.567946],[-123.279971,44.567948],[-123.280034,44.569025],[-123.27941,44.568886],[-123.278941,44.568796],[-123.278441,44.568689],[-123.277631,44.568506],[-123.277635,44.568763]],\"type\":\"LineString\"}}],\"metadata\":{\"attribution\":\"openrouteservice.org | OpenStreetMap contributors\",\"service\":\"routing\",\"timestamp\":1746670734315,\"query\":{\"coordinates\":[[-123.27963174780633,44.56720205],[-123.27788489405276,44.5687606]],\"profile\":\"driving-car\",\"profileName\":\"driving-car\",\"format\":\"geojson\",\"instructions\":true},\"engine\":{\"version\":\"9.1.2\",\"build_date\":\"2025-04-10T21:25:30Z\",\"graph_date\":\"2025-05-04T17:44:45Z\"}}}";
    const PHOTON_EXAMPLE: &str = "{\"features\":[{\"geometry\":{\"coordinates\":[-123.27788489405276,44.5687606],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":384119068,\"extent\":[-123.2780056,44.5688366,-123.277764,44.5686895],\"country\":\"United States\",\"city\":\"Corvallis\",\"countrycode\":\"US\",\"postcode\":\"97331\",\"county\":\"Benton\",\"type\":\"house\",\"osm_type\":\"W\",\"osm_key\":\"amenity\",\"street\":\"Northwest Monroe Avenue\",\"osm_value\":\"restaurant\",\"name\":\"Downward Dog\",\"state\":\"OR\"}},{\"geometry\":{\"coordinates\":[-116.617571,48.2630081],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":1069025747,\"extent\":[-116.6195304,48.2642298,-116.6166758,48.2622937],\"country\":\"United States\",\"city\":\"Dover\",\"countrycode\":\"US\",\"postcode\":\"83825\",\"county\":\"Bonner\",\"type\":\"street\",\"osm_type\":\"W\",\"osm_key\":\"highway\",\"osm_value\":\"path\",\"name\":\"Downward Dog\",\"state\":\"Idaho\"}},{\"geometry\":{\"coordinates\":[-114.2002596,51.0727856],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":932224045,\"extent\":[-114.2003584,51.0732352,-114.1999291,51.0722682],\"country\":\"Canada\",\"city\":\"Calgary\",\"countrycode\":\"CA\",\"postcode\":\"T3H 4X5\",\"type\":\"street\",\"osm_type\":\"W\",\"osm_key\":\"highway\",\"district\":\"Cougar Ridge\",\"osm_value\":\"path\",\"name\":\"Downward Facing Duck\",\"state\":\"Alberta\"}},{\"geometry\":{\"coordinates\":[-111.9946922,40.3417988],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":1118748795,\"extent\":[-111.997409,40.3445907,-111.9918981,40.3388893],\"country\":\"United States\",\"city\":\"Eagle Mountain\",\"countrycode\":\"US\",\"postcode\":\"84005\",\"county\":\"Utah County\",\"type\":\"street\",\"osm_type\":\"W\",\"osm_key\":\"highway\",\"osm_value\":\"cycleway\",\"name\":\"The Downward Spiral\",\"state\":\"Utah\"}},{\"geometry\":{\"coordinates\":[-111.4847386,40.6889075],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":667244116,\"extent\":[-111.4874303,40.692321,-111.4815622,40.6841203],\"country\":\"United States\",\"city\":\"Park City\",\"countrycode\":\"US\",\"postcode\":\"84068\",\"county\":\"Summit\",\"type\":\"street\",\"osm_type\":\"W\",\"osm_key\":\"highway\",\"osm_value\":\"path\",\"name\":\"Downward Dog\",\"state\":\"Utah\"}},{\"geometry\":{\"coordinates\":[-1.2341656982784492,51.01181699999999],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":368200709,\"extent\":[-1.2368335,51.0145445,-1.2311141,51.0091299],\"country\":\"United Kingdom\",\"city\":\"Winchester\",\"countrycode\":\"GB\",\"county\":\"Hampshire\",\"type\":\"other\",\"osm_type\":\"W\",\"osm_key\":\"natural\",\"district\":\"Owslebury\",\"osm_value\":\"wood\",\"name\":\"Downwards Plantation\",\"state\":\"England\"}},{\"geometry\":{\"coordinates\":[-1.2357489,51.0110353],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":12696053772,\"country\":\"United Kingdom\",\"city\":\"Winchester\",\"countrycode\":\"GB\",\"postcode\":\"SO21 1JP\",\"county\":\"Hampshire\",\"type\":\"locality\",\"osm_type\":\"N\",\"osm_key\":\"place\",\"district\":\"Owslebury\",\"osm_value\":\"locality\",\"name\":\"Downwards Copse\",\"state\":\"England\"}},{\"geometry\":{\"coordinates\":[-3.0450202,53.4331984],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":2618779466,\"country\":\"United Kingdom\",\"city\":\"Wallasey\",\"countrycode\":\"GB\",\"postcode\":\"CH45 5BG\",\"county\":\"Liverpool City Region\",\"type\":\"house\",\"osm_type\":\"N\",\"osm_key\":\"amenity\",\"street\":\"Field Road\",\"district\":\"New Brighton\",\"osm_value\":\"doctors\",\"name\":\"Field Road Health Centre - Dc Downward\",\"state\":\"England\"}},{\"geometry\":{\"coordinates\":[-91.2526733,46.168124],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_type\":\"W\",\"osm_id\":992209374,\"extent\":[-91.2539443,46.1682781,-91.2510665,46.1675571],\"country\":\"United States\",\"osm_key\":\"highway\",\"city\":\"Cable\",\"countrycode\":\"US\",\"osm_value\":\"cycleway\",\"name\":\"Downward Spiral\",\"county\":\"Bayfield\",\"state\":\"Wisconsin\",\"type\":\"street\"}},{\"geometry\":{\"coordinates\":[-85.7417642,38.1860092],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":531319755,\"extent\":[-85.7417642,38.1860092,-85.7416771,38.1858811],\"country\":\"United States\",\"city\":\"Louisville\",\"countrycode\":\"US\",\"postcode\":\"40221\",\"county\":\"Jefferson\",\"type\":\"street\",\"osm_type\":\"W\",\"osm_key\":\"highway\",\"osm_value\":\"steps\",\"name\":\"Main Downward Escalator\",\"state\":\"Kentucky\"}},{\"geometry\":{\"coordinates\":[-79.901113,40.4327109],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":342659442,\"extent\":[-79.9021076,40.4327594,-79.9002589,40.4323901],\"country\":\"United States\",\"city\":\"Pittsburgh\",\"countrycode\":\"US\",\"postcode\":\"15218\",\"locality\":\"Squirrel Hill South\",\"county\":\"Allegheny\",\"type\":\"street\",\"osm_type\":\"W\",\"osm_key\":\"highway\",\"osm_value\":\"path\",\"name\":\"Downward Dog Trail\",\"state\":\"Pennsylvania\"}},{\"geometry\":{\"coordinates\":[121.7392837,25.1372142],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":896829126,\"extent\":[121.7391349,25.1373835,121.7392837,25.1372142],\"country\":\"臺灣\",\"city\":\"基隆市\",\"countrycode\":\"TW\",\"postcode\":\"20343\",\"locality\":\"中興里\",\"type\":\"street\",\"osm_type\":\"W\",\"osm_key\":\"highway\",\"district\":\"中山區\",\"osm_value\":\"service\",\"name\":\"虎仔山迴車塔(下行)\"}},{\"geometry\":{\"coordinates\":[115.8901352,38.4483478],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":388418518,\"extent\":[115.8873444,38.4529023,115.8933623,38.4455405],\"country\":\"中国\",\"city\":\"沧州市\",\"countrycode\":\"CN\",\"postcode\":\"062300\",\"type\":\"house\",\"osm_type\":\"W\",\"osm_key\":\"railway\",\"street\":\"黄榆线\",\"district\":\"肃宁县\",\"osm_value\":\"rail\",\"name\":\"王佐下联线\",\"state\":\"河北省\"}},{\"geometry\":{\"coordinates\":[115.8678597,38.4415208],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":388418516,\"extent\":[115.8678597,38.4415208,115.8681994,38.4412515],\"country\":\"中国\",\"city\":\"沧州市\",\"countrycode\":\"CN\",\"postcode\":\"062300\",\"type\":\"house\",\"osm_type\":\"W\",\"osm_key\":\"railway\",\"street\":\"德善街\",\"district\":\"肃宁县\",\"osm_value\":\"rail\",\"name\":\"肃宁下联线\",\"state\":\"河北省\"}},{\"geometry\":{\"coordinates\":[115.8665264,38.4338899],\"type\":\"Point\"},\"type\":\"Feature\",\"properties\":{\"osm_id\":388418517,\"extent\":[115.8611995,38.4412515,115.869729,38.4284719],\"country\":\"中国\",\"city\":\"沧州市\",\"countrycode\":\"CN\",\"postcode\":\"062300\",\"type\":\"house\",\"osm_type\":\"W\",\"osm_key\":\"railway\",\"street\":\"德善街\",\"district\":\"肃宁县\",\"osm_value\":\"rail\",\"name\":\"肃宁下联线\",\"state\":\"河北省\"}}],\"type\":\"FeatureCollection\"}";
    // Nothing's really added by testing reverse geocoding

    fn gen_tester_requester(stringly_base: String) -> ExternalRequester {
        let stringly_base = format!("http://{}", stringly_base);
        let base = reqwest::Url::parse(&stringly_base)
            .unwrap_or_else(|_| panic!("couldn't unwrap {stringly_base}")); // it's giving golang
        ExternalRequesterBuilder::new(base.clone(), base, SecretString::from("foo"))
            .with_photon_ratelimiter(2, Duration::from_secs(1), "short boy".to_string())
            .with_photon_ratelimiter(4, Duration::from_secs(3), "long boy".to_string())
            .build()
    }

    fn geocode_request() -> PhotonGeocodeRequest {
        PhotonGeocodeRequest {
            limit: 10,
            query: "downward".to_string(),
            lat: Some(-123.279166),
            lon: Some(44.567189),
        }
    }

    fn route_request() -> OpenRouteRequest {
        OpenRouteRequest {
            coordinates: vec![
                vec![-123.27963174780633, 44.56720205],
                vec![-123.27788489405276, 44.5687606],
            ],
            instructions: true,
        }
    }

    // Make requests within Photon limit bounds. Should work until it doesn't. Doesn't need mock
    // state because the limit is self-imposed
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn photon_ratelimit_test() {
        let server = MockServer::start();
        let resp_body: Value = serde_json::from_str(PHOTON_EXAMPLE).unwrap();
        server.mock(|when, then| {
            when.method(GET)
                .path(PHOTON_PATH)
                .query_param_exists("limit")
                .query_param_exists("q")
                .query_param_exists("lat")
                .query_param_exists("lon");
            then.status(200)
                // There's other headers that could be interesting, but aren't immediately relevant
                .header("Content-Type", "application/json;charset=utf-8")
                .json_body(resp_body);
        });

        let reqr = gen_tester_requester(server.address().to_string());
        let gr = geocode_request();
        assert!(reqr.photon_send(&gr).await.is_ok());
        assert!(reqr.photon_send(&gr).await.is_ok());
        assert!(reqr
            .photon_send(&gr)
            .await
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));
        // Yes boss the unit tests are so heavy they take time to run
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(reqr.photon_send(&gr).await.is_ok());
        assert!(reqr.photon_send(&gr).await.is_ok());
        assert!(reqr
            .photon_send(&gr)
            .await
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));
    }

    // Get a 429 with valid retry-after. Ensure a request made within the time fails, and one after
    // doesn't. In reality we have Access-Control-Expose-Headers we could use, but we don't
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn overloaded_ors() {
        // We're going to fake a stateful mock by swapping in different mocks on the same port
        let server = MockServer::start();
        let resp_body: Value = serde_json::from_str(ORS_DIRECTIONS_EXAMPLE).unwrap();
        // In truth, I don't know what the real server will exactly respond.
        let mut tired_server = server.mock(|when, then| {
            when.method(POST).path(ORS_DIRECTIONS_PATH);
            then.status(429).header(
                "Retry-After",
                fmt_http_date(SystemTime::now() + Duration::from_secs(1)),
            );
        });

        let mut reqr = gen_tester_requester(tired_server.server_address().to_string());
        let or = route_request();

        // Get backer-offed
        assert!(reqr
            .ors_send(&or)
            .await
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));

        // Pretend this is a stateful mock and not just two mocks in a trenchcoat
        tired_server.delete();
        let wired_server = server.mock(|when, then| {
            when.method(POST).path(ORS_DIRECTIONS_PATH);
            then.status(200)
                .header("Content-Type", "application/geo+json;charset=UTF-8")
                .json_body(resp_body);
        });
        // This wouldn't work in a real intergration test, and it wouldn't be needed either
        reqr.ors_directions =
            Url::parse(format!("http://{}", wired_server.server_address()).as_str())
                .unwrap_or_else(|e| panic!("couldn't parse mock base address: {e:?}"))
                .join(ORS_DIRECTIONS_PATH)
                .unwrap_or_else(|e| panic!("couldn't merge mock base address with path: {e:?}"));

        // More of a test of whether it takes more than 1 seconds to make a mock and request
        assert!(reqr
            .ors_send(&or)
            .await
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(reqr.ors_send(&or).await.is_ok());
    }

    // Get a 503 with no retry-after. Ensure a request made within the time fails, and one after
    // doesn't. Paused so we don't have to wait out the hard-coded production delay
    #[tokio::test()]
    async fn headerless_overload() {
        let server = MockServer::start();
        let resp_body: Value = serde_json::from_str(ORS_DIRECTIONS_EXAMPLE).unwrap();
        // In truth, I don't know what the real server will exactly respond.
        let mut tired_server = server.mock(|when, then| {
            when.method(POST).path(ORS_DIRECTIONS_PATH);
            then.status(503);
        });

        let mut reqr = gen_tester_requester(tired_server.server_address().to_string());
        let or = route_request();

        // Get backer-offed
        assert!(reqr
            .ors_send(&or)
            .await
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));

        // Pretend this is a stateful mock and not just two mocks in a trenchcoat
        tired_server.delete();
        let wired_server = server.mock(|when, then| {
            when.method(POST).path(ORS_DIRECTIONS_PATH);
            then.status(200)
                .header("Content-Type", "application/geo+json;charset=UTF-8")
                .json_body(resp_body);
        });
        // This wouldn't work in a real intergration test, and it wouldn't be needed either
        reqr.ors_directions =
            Url::parse(format!("http://{}", wired_server.server_address()).as_str())
                .unwrap_or_else(|e| panic!("couldn't parse mock base address: {e:?}"))
                .join(ORS_DIRECTIONS_PATH)
                .unwrap_or_else(|e| panic!("couldn't merge mock base address with path: {e:?}"));

        // More of a test of whether it takes more than 1 seconds to make a mock and request
        assert!(reqr
            .ors_send(&or)
            .await
            .is_err_and(|x| matches!(x, RouteError::ExternalAPILimit(_))));
        time::pause();
        time::advance(retry_after::HEADERLESS_BACKOFF_TIME).await;
        task::yield_now().await; // httpmock doesn't like this buffoonery
        assert!(reqr.ors_send(&or).await.is_ok());
    }
}
