# What's All This, Then?
Elaborate API proxy for **FlipMap, the Map App for Flip-phones!**

Relies on public external API's via OpenRouteService and Photon (hosted by Komoot).

Prevents us from having to ship those keys to untrustworthy client devices, and provides convenient input/output that simplifies the phone app.

# Requirements and Compiling
You may or may not need a suitable 'TLS backend' to build the project (TODO: check this), but you *will* need one to run it. Most systems should already have this, but lightweight containers may require explicit installation. Ensure OpenSSL (often `libssl`) and the standard CA certificates for the distribution are installed. **Not having these will result in an early runtime panic upon attempting to construct the `ExternalRequester`.**

Ensure you have a functional Rust toolchain installed. See the [official installation guide](https://www.rust-lang.org/tools/install) for more.

Afterwards, Cargo takes the wheel: `cargo build`.

# Running

It is required to set an openrouteservice API key as an environmental variable: `ORS_API_KEY`. Not doing so will cause an early runtime panic, with a slightly more terse message telling you to do this.

Finally, running can be as simple as `<program> 127.0.0.1 1337` for testing on loopback with a very cool port. Use the `--help` flag in the application for up-to-date information on other options. It is possible to do other cool things like point the external API sources to arbitrary addresses.


# Endpoints
For now, all API endpoints are placed in `main.rs`. These take and return normal JSON, and not GeoJSON. This is intentional to simplify the app. 

## /route
HTTP POST

Speaks JSON exclusively

Proof of concept route. Poorly named, in hindsight. Requires an 'anchor position' as the lat/lon (most likely the current position), and a query to search for a location to be routed to from that anchor point. Returns an array of floats representing flattened coordinates of the form `[lat,lon,lat,lon...]` which are the waypoints.

#### Input Dict Items:

`lat: <number>` Additional Constraint: float where -90 <= n <= 90

`lon: <number>` Additional Constraint: float where -180 <= n <= 180

`query: <string>`

#### Output Dict Items:

HTTP 200:

`route: <array[number]>` 

Where route is a flattened LineString of 2-element Positions, representing a contiguous array of waypoints for the route. 'LineString' and 'Position' are used as defined in [RFC 7946](https://datatracker.ietf.org/doc/html/rfc7946).

HTTP 422|500:

`msg: <string>` 

Where msg is an error that is purposely vague on details for internal matters.

# Troubleshooting
Tracing is enabled by default, but filters out some detail for brevity. Set the environment variables `RUST_BACKTRACE=1` and `RUST_LOG=trace` to maximize detail.

The error messages returned to the client will purposely not describe the specifics of internal failures. The error messages raised internally also may currently not log enough useful information. See the documentation `cargo doc --bins --document-private-items --open` 
and refer to the `error.rs` enum `RouteError` for the most-up-to-date information on possible errors.

# Deployment Consideration
This application expects to be able to make HTTPS requests to API endpoints. Errors will naturally result if firewalls or other configurations get in the way.

TLS for connecting clients, and rate-limiting anywhere are not implemented in the application. **It's strongly recommended to put the application behind a rate-limiting reverse proxy such as NGINX or Caddy.** Future functionality for inbuilt rate-limiting to external APIs is planned, but client-facing rate limiting should be accomplished elsewhere.

Containerization is supported as a first-class deployment method. Provided the TLS backend is present (see: Requirements), the effort to have full functionality should be minimal. An example Dockerfile may be provided in this repository.
