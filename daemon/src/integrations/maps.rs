//! Maps client for agent "voyager" (Travel & Logistics).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests), and holds the user's own Maps
//! Platform API key, which it attaches per request at the moment of the send.
//!
//! KEY HANDLING — the security crux of this client. Google Maps web services
//! classically accept the API key as a `?key=...` QUERY PARAMETER, but a key in
//! the URL would land in every logged/recorded request line. So this client
//! attaches the key ONLY in the `X-Goog-Api-Key` HEADER (a Google Maps Platform
//! auth method) and NEVER puts it in the URL. The key value is never logged, never
//! stored on the transport, never put in an error or a `Debug` field — only its
//! presence (a bool) is ever recorded. A test (`api_key_never_appears_in_a_logged_url`)
//! pins that no recorded URL — and no produced output — ever contains the key.
//!
//! READ-ONLY by construction. This client reads ROUTES, PLACES, and TRAVEL TIMES;
//! it does NOT book or pay for anything. There is deliberately NO reservation or
//! payment method on it — not even a gated one — so it holds NO [`super::ActionMode`]
//! surface and never touches the foundation's `gate()`. Booking a flight/hotel/ride
//! would need many provider APIs plus payment, which is out of scope. A test
//! (`no_booking_or_payment_method_exists`) pins that the whole module names no
//! reservation/payment endpoint.
//!
//! Three READ methods, each a plain GET that fetches and reports:
//!   * [`MapsClient::directions`] — GET /maps/api/directions/json -> a route
//!     summary (start/end address, distance, duration) for the chosen travel mode.
//!   * [`MapsClient::places_search`] — GET /maps/api/place/textsearch/json -> the
//!     top matching places (name + address), optionally biased near a location.
//!   * [`MapsClient::eta`] — GET /maps/api/distancematrix/json -> the distance and
//!     duration between an origin and destination for the chosen travel mode.
//!
//! Each method returns a concise human-facing `String` — what voyager would say —
//! while parsing only the typed fields it needs. Map-level errors are surfaced
//! honestly: a Google `status` of `REQUEST_DENIED` maps to a key/config hint,
//! `ZERO_RESULTS` maps to a friendly "no results", and a non-2xx HTTP code maps via
//! [`map_status`]. The provider body is never echoed in an error.

use serde::Deserialize;
use tracing::info;

use super::{
    resolve_secret, status_outcome, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    ReqwestTransport, StatusOutcome,
};

/// The Keychain account holding the user's Maps Platform API key (from their own
/// Google Maps Platform project). Pasted in Settings. Rides ONLY the
/// `X-Goog-Api-Key` request header at call time — never the URL, never a log.
pub const ACCOUNT_API_KEY: &str = "maps_api_key";

/// Default Maps base URL — Google Maps Platform web services. No trailing slash so
/// `{base}/maps/api/...` is clean.
pub const DEFAULT_BASE: &str = "https://maps.googleapis.com";

/// The header Google Maps Platform reads the API key from. Using the header (not a
/// `?key=` query param) keeps the key out of every logged/recorded URL.
const API_KEY_HEADER: &str = "X-Goog-Api-Key";

/// Default travel mode when the caller does not specify one.
const DEFAULT_MODE: &str = "driving";
/// Travel modes Google Maps accepts; anything else is normalized to the default so
/// a stray value never reaches the provider.
const VALID_MODES: &[&str] = &["driving", "walking", "bicycling", "transit"];
/// How many places to name in a search summary before collapsing to "and N more".
const LIST_PREVIEW: usize = 5;

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields voyager actually needs are decoded.
// `#[serde(default)]` keeps parsing resilient to the many extra keys the Maps
// APIs return (geometry, place_id, types, bounds, … we don't read).
// ---------------------------------------------------------------------------

/// A `{ "text": "...", "value": N }` measure (distance/duration), as the
/// Directions and Distance Matrix APIs return. voyager shows the human `text`.
#[derive(Debug, Clone, Deserialize, Default)]
struct Measure {
    #[serde(default)]
    text: String,
}

/// One leg of a Directions route — the part voyager summarizes.
#[derive(Debug, Clone, Deserialize, Default)]
struct Leg {
    #[serde(default)]
    start_address: String,
    #[serde(default)]
    end_address: String,
    #[serde(default)]
    distance: Measure,
    #[serde(default)]
    duration: Measure,
}

/// One Directions route (voyager reads its first leg).
#[derive(Debug, Clone, Deserialize, Default)]
struct Route {
    #[serde(default)]
    legs: Vec<Leg>,
}

/// The `/directions/json` response shape voyager reads.
#[derive(Debug, Clone, Deserialize, Default)]
struct DirectionsResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    routes: Vec<Route>,
}

/// One place result from `/place/textsearch/json`.
#[derive(Debug, Clone, Deserialize)]
struct Place {
    #[serde(default)]
    name: String,
    #[serde(default)]
    formatted_address: String,
}

impl Place {
    /// The label to show: "<name> (<address>)", or whichever is present.
    fn label(&self) -> String {
        match (self.name.is_empty(), self.formatted_address.is_empty()) {
            (false, false) => format!("{} ({})", self.name, self.formatted_address),
            (false, true) => self.name.clone(),
            (true, false) => self.formatted_address.clone(),
            (true, true) => "a place".to_string(),
        }
    }
}

/// The `/place/textsearch/json` response shape voyager reads.
#[derive(Debug, Clone, Deserialize, Default)]
struct PlacesResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    results: Vec<Place>,
}

/// One Distance Matrix element (the origin->destination cell voyager reads).
#[derive(Debug, Clone, Deserialize, Default)]
struct MatrixElement {
    #[serde(default)]
    status: String,
    #[serde(default)]
    distance: Measure,
    #[serde(default)]
    duration: Measure,
}

/// One Distance Matrix row (one origin's cells).
#[derive(Debug, Clone, Deserialize, Default)]
struct MatrixRow {
    #[serde(default)]
    elements: Vec<MatrixElement>,
}

/// The `/distancematrix/json` response shape voyager reads.
#[derive(Debug, Clone, Deserialize, Default)]
struct DistanceMatrixResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    rows: Vec<MatrixRow>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Maps READ client bound to a transport, a base URL, and the user's API key.
///
/// Construct with [`MapsClient::connect`] (resolves the key from the Keychain,
/// wires the real transport) or, in tests, [`MapsClient::with_key`] (an explicit
/// base URL + fake key + a `MockTransport`). The key is held only to compose the
/// per-request `X-Goog-Api-Key` header; it is never logged, never put in the URL,
/// and the `Debug` impl below redacts it.
///
/// READ-ONLY by construction: the only methods are the three reads. There is no
/// reservation/payment method — not even a gated one — so this struct has no
/// `ActionMode` surface and never touches the foundation gate. Voyager finds the
/// way; it never books the trip.
pub struct MapsClient<T: HttpTransport> {
    transport: T,
    /// Maps base URL with any trailing slash trimmed.
    base: String,
    api_key: String,
}

/// Custom `Debug` that NEVER prints the API key — only that one is present, plus
/// the base URL (a public Maps host, not a secret). So a `{:?}` of a client (in a
/// log line, a panic message, a test) can't leak the key.
impl<T: HttpTransport> std::fmt::Debug for MapsClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MapsClient")
            .field("base", &self.base)
            .field("api_key_present", &!self.api_key.is_empty())
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> MapsClient<T> {
    /// Build a client with an explicitly supplied base URL + API key. Used by tests
    /// (paired with `MockTransport`) and by any caller that has already resolved the
    /// secret. The key is consumed into the client and never logged; the base URL
    /// has its trailing slash trimmed so path joins are clean.
    pub fn with_key(transport: T, base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        Self {
            transport,
            base,
            api_key: api_key.into(),
        }
    }

    /// Compose a GET to a Maps endpoint. The API key is attached HERE — at the
    /// moment of the call — ONLY in the `X-Goog-Api-Key` header, NEVER in the URL,
    /// so it can never land in a logged/recorded request line. `query` is the
    /// already-URL-encoded query string (key=value pairs, no leading `?`); it
    /// carries the request params but NEVER the API key.
    fn get(&self, path: &str, query: &str) -> HttpRequest {
        let url = if query.is_empty() {
            format!("{}{path}", self.base)
        } else {
            format!("{}{path}?{query}", self.base)
        };
        HttpRequest::new(HttpMethod::Get, url).header(API_KEY_HEADER, &self.api_key)
    }

    // -- READ (the only surface — no gate, no booking) -----------------------

    /// Get a route between two places (`GET /maps/api/directions/json`). Read-only.
    /// `mode` is an optional travel mode (driving/walking/bicycling/transit,
    /// defaulting to driving). Returns "<start> to <end>: <distance>, about
    /// <duration> by <mode>." Map status of ZERO_RESULTS -> a friendly "no route".
    pub async fn directions(
        &self,
        origin: &str,
        destination: &str,
        mode: Option<&str>,
    ) -> IntegrationResult<String> {
        let mode = normalize_mode(mode);
        let query = format!(
            "origin={}&destination={}&mode={mode}",
            encode(origin),
            encode(destination),
        );
        let req = self.get("/maps/api/directions/json", &query);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "getting directions")?;

        let parsed: DirectionsResponse = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("getting directions returned an unexpected response"))?;
        map_provider_status(&parsed.status, "directions")?;
        info!(mode, "maps: read directions");

        let leg = parsed
            .routes
            .first()
            .and_then(|r| r.legs.first())
            .ok_or_else(|| anyhow::anyhow!("no route came back for that trip"))?;
        let from = if leg.start_address.is_empty() { origin } else { &leg.start_address };
        let to = if leg.end_address.is_empty() { destination } else { &leg.end_address };
        Ok(format!(
            "{from} to {to}: {}, about {} by {mode}.",
            measure_or(&leg.distance, "distance unknown"),
            measure_or(&leg.duration, "time unknown"),
        ))
    }

    /// Search for places by text (`GET /maps/api/place/textsearch/json`).
    /// Read-only. `near` is an optional "lat,lng" location bias passed through to
    /// the API. Returns a count plus the first few "<name> (<address>)". Map status
    /// of ZERO_RESULTS -> a friendly "no places".
    pub async fn places_search(&self, query_text: &str, near: Option<&str>) -> IntegrationResult<String> {
        let mut query = format!("query={}", encode(query_text));
        if let Some(loc) = near {
            if !loc.trim().is_empty() {
                query.push_str(&format!("&location={}", encode(loc)));
            }
        }
        let req = self.get("/maps/api/place/textsearch/json", &query);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "searching for places")?;

        let parsed: PlacesResponse = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("searching for places returned an unexpected response"))?;
        map_provider_status(&parsed.status, "places")?;
        info!(count = parsed.results.len(), "maps: searched places");

        if parsed.results.is_empty() {
            return Ok(format!("No places found for \"{query_text}\"."));
        }
        let lines: Vec<String> = parsed
            .results
            .iter()
            .take(LIST_PREVIEW)
            .map(Place::label)
            .collect();
        let more = parsed.results.len().saturating_sub(lines.len());
        let mut out = format!(
            "Found {} place{} for \"{query_text}\": {}",
            parsed.results.len(),
            if parsed.results.len() == 1 { "" } else { "s" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    /// Get the travel time + distance between two places
    /// (`GET /maps/api/distancematrix/json`). Read-only. `mode` is an optional
    /// travel mode (defaulting to driving). Returns "<origin> to <destination>:
    /// <distance>, about <duration> by <mode>." A per-cell ZERO_RESULTS / NOT_FOUND
    /// -> a friendly "no route".
    pub async fn eta(
        &self,
        origin: &str,
        destination: &str,
        mode: Option<&str>,
    ) -> IntegrationResult<String> {
        let mode = normalize_mode(mode);
        let query = format!(
            "origins={}&destinations={}&mode={mode}",
            encode(origin),
            encode(destination),
        );
        let req = self.get("/maps/api/distancematrix/json", &query);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "getting the travel time")?;

        let parsed: DistanceMatrixResponse = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("getting the travel time returned an unexpected response"))?;
        map_provider_status(&parsed.status, "eta")?;
        info!(mode, "maps: read eta");

        let element = parsed
            .rows
            .first()
            .and_then(|r| r.elements.first())
            .ok_or_else(|| anyhow::anyhow!("no travel time came back for that trip"))?;
        // The per-cell status is its own field (OK / ZERO_RESULTS / NOT_FOUND).
        if element.status != "OK" {
            return Err(anyhow::anyhow!(
                "no route between those two places — check the origin and destination"
            ));
        }
        Ok(format!(
            "{origin} to {destination}: {}, about {} by {mode}.",
            measure_or(&element.distance, "distance unknown"),
            measure_or(&element.duration, "time unknown"),
        ))
    }
}

impl MapsClient<ReqwestTransport> {
    /// Production constructor: resolve the Maps Platform API key from the macOS
    /// Keychain via the foundation's allowlisted resolver, and wire the real reqwest
    /// transport. Returns the friendly, secret-free "maps isn't configured" error
    /// when the key is missing — voyager relays that to the user without ever
    /// surfacing the key.
    pub async fn connect() -> IntegrationResult<Self> {
        let api_key = resolve_secret(ACCOUNT_API_KEY).await.ok_or_else(not_configured)?;
        Ok(Self::with_key(ReqwestTransport::new(), DEFAULT_BASE, api_key))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// The friendly, secret-free "not configured" error the missing-key path returns —
/// points the user at Settings and names what to add. Booking is out of scope, so
/// the copy says routes/places only.
fn not_configured() -> anyhow::Error {
    anyhow::anyhow!(
        "maps isn't configured — add your Maps Platform API key in Settings (Voyager reads routes, places, and travel times; it does not book or pay for anything)"
    )
}

/// Normalize a caller-supplied travel mode to one Google Maps accepts, defaulting
/// (and falling back for anything unrecognized) to driving. Pure.
fn normalize_mode(mode: Option<&str>) -> &'static str {
    match mode.map(str::trim).map(str::to_ascii_lowercase) {
        Some(m) => VALID_MODES
            .iter()
            .copied()
            .find(|valid| *valid == m)
            .unwrap_or(DEFAULT_MODE),
        None => DEFAULT_MODE,
    }
}

/// A measure's human text, or a fallback when the provider gave none. Pure.
fn measure_or(m: &Measure, fallback: &'static str) -> String {
    if m.text.is_empty() {
        fallback.to_string()
    } else {
        m.text.clone()
    }
}

/// Minimal percent-encoding for the query VALUES voyager sends (addresses, place
/// queries, lat/lng). Encodes everything that is not an unreserved character so a
/// space or `&` in an address cannot break the query or smuggle a parameter. This
/// only ever touches user-supplied request params — NEVER the API key, which rides
/// the header. Pure.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Map a Google Maps top-level `status` string to a friendly, secret-free error.
/// `OK` (and the empty string, for responses that carry results without a status)
/// pass. `REQUEST_DENIED` means the API key was rejected or the API is not enabled;
/// `ZERO_RESULTS` / `NOT_FOUND` mean nothing matched; `OVER_QUERY_LIMIT` is a quota
/// hit; anything else is a generic provider error. The provider body / status is
/// never echoed beyond choosing the wording. Pure.
fn map_provider_status(status: &str, _what: &str) -> IntegrationResult<()> {
    match status {
        "OK" | "" => Ok(()),
        "ZERO_RESULTS" | "NOT_FOUND" => {
            Err(anyhow::anyhow!("no results for that — try a more specific place"))
        }
        "REQUEST_DENIED" => Err(anyhow::anyhow!(
            "the Maps request was denied — check your Maps Platform API key (and that the API is enabled) in Settings"
        )),
        "OVER_QUERY_LIMIT" => {
            Err(anyhow::anyhow!("your Maps Platform quota is exhausted; try again later"))
        }
        "INVALID_REQUEST" => {
            Err(anyhow::anyhow!("that request was missing or malformed — check the origin and destination"))
        }
        _ => Err(anyhow::anyhow!("the maps lookup failed; this is usually transient")),
    }
}

/// Map a Maps HTTP status to a friendly, secret-free error. 2xx is `Ok` (the
/// provider `status` field is checked separately by [`map_provider_status`]). The
/// provider body is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::Unauthorized => Err(anyhow::anyhow!(
            "{what} failed — your Maps Platform API key was rejected; check it in Settings"
        )),
        StatusOutcome::RateLimited => {
            Err(anyhow::anyhow!("{what} was rate limited by Maps; try again shortly"))
        }
        StatusOutcome::ServerError => {
            Err(anyhow::anyhow!("{what} failed on the maps provider's side; this is usually transient"))
        }
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned Maps JSON (realistic API SHAPE, never fetched). No network,
// no real Google Maps, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;

    /// A throwaway base + API key used only to prove the request is shaped and
    /// authed. The key VALUE is never asserted to APPEAR — only its ABSENCE, in the
    /// URL and in every produced string.
    const FAKE_BASE: &str = "https://maps.googleapis.com";
    const FAKE_KEY: &str = "MAPS-FAKE-API-KEY-NEVER-LEAK-AIzaSyTEST";

    fn client(mock: MockTransport) -> MapsClient<MockTransport> {
        MapsClient::with_key(mock, FAKE_BASE, FAKE_KEY)
    }

    // -- realistic canned payloads (hand-written from the Maps API shapes) ----

    fn directions_json() -> &'static str {
        r#"{
          "status": "OK",
          "routes": [
            {"legs": [
              {"start_address": "1 Infinite Loop, Cupertino, CA",
               "end_address": "San Francisco International Airport, CA",
               "distance": {"text": "42.1 mi", "value": 67752},
               "duration": {"text": "52 mins", "value": 3120}}
            ]}
          ]
        }"#
    }

    fn places_json() -> &'static str {
        r#"{
          "status": "OK",
          "results": [
            {"name": "Blue Bottle Coffee", "formatted_address": "66 Mint St, San Francisco, CA"},
            {"name": "Sightglass Coffee", "formatted_address": "270 7th St, San Francisco, CA"},
            {"name": "Ritual Coffee Roasters", "formatted_address": "1026 Valencia St, San Francisco, CA"}
          ]
        }"#
    }

    fn distance_matrix_json() -> &'static str {
        r#"{
          "status": "OK",
          "rows": [
            {"elements": [
              {"status": "OK",
               "distance": {"text": "12.4 mi", "value": 19956},
               "duration": {"text": "23 mins", "value": 1380}}
            ]}
          ]
        }"#
    }

    // -- READ: directions parsing --------------------------------------------

    #[tokio::test]
    async fn directions_parses_and_summarizes() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/directions/json", 200, directions_json());
        let out = client(mock).directions("Cupertino", "SFO", None).await.unwrap();
        assert!(out.contains("1 Infinite Loop"), "got: {out}");
        assert!(out.contains("San Francisco International Airport"), "got: {out}");
        assert!(out.contains("42.1 mi"), "distance missing: {out}");
        assert!(out.contains("52 mins"), "duration missing: {out}");
        assert!(out.contains("by driving"), "default mode missing: {out}");
    }

    #[tokio::test]
    async fn directions_zero_results_is_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/maps/api/directions/json",
            200,
            r#"{"status":"ZERO_RESULTS","routes":[]}"#,
        );
        let err = client(mock).directions("nowhere", "nowhere else", None).await.unwrap_err().to_string();
        assert!(err.contains("no results"), "ZERO_RESULTS -> friendly: {err}");
    }

    // -- READ: places parsing ------------------------------------------------

    #[tokio::test]
    async fn places_parses_and_summarizes() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/place/textsearch/json", 200, places_json());
        let out = client(mock).places_search("coffee near me", None).await.unwrap();
        assert!(out.contains("3 places"), "got: {out}");
        assert!(out.contains("Blue Bottle Coffee (66 Mint St, San Francisco, CA)"), "got: {out}");
        assert!(out.contains("Sightglass Coffee"), "got: {out}");
    }

    #[tokio::test]
    async fn places_empty_is_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/maps/api/place/textsearch/json",
            200,
            r#"{"status":"ZERO_RESULTS","results":[]}"#,
        );
        // ZERO_RESULTS surfaces as the provider-status "no results" hint.
        let err = client(mock).places_search("asdfqwer", None).await.unwrap_err().to_string();
        assert!(err.contains("no results"), "got: {err}");
    }

    // -- READ: eta parsing ---------------------------------------------------

    #[tokio::test]
    async fn eta_parses_distance_and_duration() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/distancematrix/json", 200, distance_matrix_json());
        let out = client(mock).eta("the office", "the venue", Some("walking")).await.unwrap();
        assert!(out.contains("12.4 mi"), "distance missing: {out}");
        assert!(out.contains("23 mins"), "duration missing: {out}");
        assert!(out.contains("by walking"), "mode missing: {out}");
    }

    #[tokio::test]
    async fn eta_per_cell_not_found_is_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/maps/api/distancematrix/json",
            200,
            r#"{"status":"OK","rows":[{"elements":[{"status":"NOT_FOUND"}]}]}"#,
        );
        let err = client(mock).eta("nowhere", "elsewhere", None).await.unwrap_err().to_string();
        assert!(err.contains("no route between those two places"), "got: {err}");
    }

    // -- the API KEY rides the HEADER, never the URL -------------------------

    #[tokio::test]
    async fn request_carries_key_in_header_not_in_url() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/directions/json", 200, directions_json());
        let c = client(mock);
        c.directions("A", "B", None).await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        // The key is in the X-Goog-Api-Key header (presence asserted, not value).
        assert!(req.has_header("x-goog-api-key"), "api key header attached");
        // The URL carries the request params but NOT the key.
        assert!(req.url.starts_with("https://maps.googleapis.com/maps/api/directions/json?"), "got: {}", req.url);
        assert!(req.url.contains("origin=A"), "params in url: {}", req.url);
        assert!(!req.url.to_lowercase().contains("key="), "the API key must NOT be a URL query param: {}", req.url);
    }

    /// THE security pin: the API key value must never appear in any RECORDED URL,
    /// nor in any produced outcome/error/Debug string. This is what guarantees a
    /// logged request line can never leak the key (the key rides the header only).
    #[tokio::test]
    async fn api_key_never_appears_in_a_logged_url() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/maps/api/directions/json", 200, directions_json())
            .on(HttpMethod::Get, "/maps/api/place/textsearch/json", 200, places_json())
            .on(HttpMethod::Get, "/maps/api/distancematrix/json", 200, distance_matrix_json());
        let c = client(mock);
        let ok1 = c.directions("Cupertino", "SFO", None).await.unwrap();
        let ok2 = c.places_search("coffee near me", Some("37.77,-122.41")).await.unwrap();
        let ok3 = c.eta("the office", "the venue", Some("driving")).await.unwrap();

        // No RECORDED request URL contains the key — this is the "logged URL" surface.
        for req in c.transport.requests() {
            assert!(
                !req.url.contains(FAKE_KEY),
                "the API key leaked into a recorded URL: {}",
                req.url
            );
        }
        // No success string contains the key.
        for s in [&ok1, &ok2, &ok3] {
            assert!(!s.contains(FAKE_KEY), "outcome leaked the API key: {s}");
        }
        // Debug of the client redacts the key.
        let dbg = format!("{:?}", MapsClient::with_key(MockTransport::new(), FAKE_BASE, FAKE_KEY));
        assert!(!dbg.contains(FAKE_KEY), "Debug leaked the API key: {dbg}");
        assert!(dbg.contains("api_key_present"), "Debug should note presence");
        assert!(dbg.contains("maps.googleapis.com"), "Debug may show the base URL (not a secret)");

        // An error path must not leak the key either.
        let err_mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/directions/json", 401, "{}");
        let err = client(err_mock).directions("A", "B", None).await.unwrap_err().to_string();
        assert!(!err.contains(FAKE_KEY), "error leaked the API key: {err}");
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_trimmed() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/directions/json", 200, directions_json());
        let c = MapsClient::with_key(mock, "https://maps.googleapis.com/", FAKE_KEY);
        c.directions("A", "B", None).await.unwrap();
        assert!(
            c.transport.last_request().url.starts_with("https://maps.googleapis.com/maps/api/directions/json?"),
            "no double slash before the path: {}",
            c.transport.last_request().url
        );
    }

    // -- error mapping: provider status first, then HTTP status ---------------

    #[tokio::test]
    async fn request_denied_maps_to_key_hint() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/maps/api/directions/json",
            200,
            r#"{"status":"REQUEST_DENIED","error_message":"The provided API key is invalid.","routes":[]}"#,
        );
        let err = client(mock).directions("A", "B", None).await.unwrap_err().to_string();
        assert!(err.contains("API key"), "REQUEST_DENIED -> key hint: {err}");
        // The provider's error_message body is never echoed.
        assert!(!err.contains("provided API key is invalid"), "provider body must not be echoed: {err}");
    }

    #[tokio::test]
    async fn unauthorized_http_maps_to_key_hint() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/place/textsearch/json", 403, "{}");
        let err = client(mock).places_search("x", None).await.unwrap_err().to_string();
        assert!(err.contains("key was rejected"), "403 -> key hint: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/maps/api/directions/json", 503, "down");
        let err = client(mock).directions("A", "B", None).await.unwrap_err().to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- HARD SCOPE: no booking/payment method or code path exists ------------

    /// VOYAGER is READ-ONLY: routes/places/times only. This is the structural guard:
    /// the whole maps.rs source must name NO reservation/payment endpoint or method —
    /// not even a gated one — and must not import the foundation's consequential gate
    /// or ActionMode. If a future edit adds a booking/payment path, this fails. The
    /// source is read at test time from the crate's own file (no network).
    #[test]
    fn no_booking_or_payment_method_exists() {
        // Scan only the PRODUCTION half of the module — everything before the test
        // module marker — so the test's own assertion literals can't satisfy it.
        let full = include_str!("maps.rs");
        let prod = full
            .split("#[cfg(test)]")
            .next()
            .expect("module has a production section before the tests");

        // No booking/payment Maps surface may appear anywhere in the production code.
        // These are method/endpoint-SHAPED tokens (a path fragment or a `fn` name),
        // chosen so they match an actual booking/payment SURFACE — never the prose in
        // this module's own doc comments (which legitimately use "reservation",
        // "payment", and "unreserved" to EXPLAIN the absence).
        for forbidden in [
            "/booking/",
            "/reservation/",
            "fn book",
            "fn reserve",
            "fn pay",
            "fn purchase",
            "/payment/",
            "place_order",
            "order_ride",
            "checkout",
        ] {
            assert!(
                !prod.contains(forbidden),
                "maps.rs production code must contain NO booking/payment surface, found: {forbidden}"
            );
        }
        // The client must NOT pull in the consequential gate or ActionMode — there is
        // nothing to gate because no booking/payment action exists.
        assert!(
            !prod.contains("ActionMode,")
                && !prod.contains("ActionMode}")
                && !prod.contains(": ActionMode")
                && !prod.contains("ActionMode::"),
            "maps.rs must not import or use ActionMode — Voyager has no consequential surface"
        );
        assert!(
            !prod.contains("super::gate(") && !prod.contains("integrations::gate("),
            "maps.rs must never call the consequential gate — there is no action to gate"
        );
        // The three public methods are exactly the read trio — assert each exists.
        for read_method in [
            "pub async fn directions",
            "pub async fn places_search",
            "pub async fn eta",
        ] {
            assert!(prod.contains(read_method), "missing read method: {read_method}");
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn normalize_mode_defaults_and_validates() {
        assert_eq!(normalize_mode(None), "driving");
        assert_eq!(normalize_mode(Some("walking")), "walking");
        assert_eq!(normalize_mode(Some("WALKING")), "walking");
        assert_eq!(normalize_mode(Some("transit")), "transit");
        assert_eq!(normalize_mode(Some("teleport")), "driving", "unknown falls back to default");
        assert_eq!(normalize_mode(Some("")), "driving");
    }

    #[test]
    fn encode_escapes_unsafe_chars_only() {
        assert_eq!(encode("Cupertino"), "Cupertino");
        assert_eq!(encode("1 Infinite Loop"), "1%20Infinite%20Loop");
        // An address with an ampersand cannot smuggle a query parameter.
        assert_eq!(encode("A & B"), "A%20%26%20B");
        // Unreserved chars are kept; a lat,lng comma is encoded.
        assert_eq!(encode("37.77,-122.41"), "37.77%2C-122.41");
    }

    #[test]
    fn measure_or_falls_back_when_blank() {
        assert_eq!(measure_or(&Measure { text: "5 mi".into() }, "x"), "5 mi");
        assert_eq!(measure_or(&Measure { text: String::new() }, "distance unknown"), "distance unknown");
    }

    #[test]
    fn map_provider_status_table() {
        assert!(map_provider_status("OK", "x").is_ok());
        assert!(map_provider_status("", "x").is_ok());
        assert!(map_provider_status("ZERO_RESULTS", "x").unwrap_err().to_string().contains("no results"));
        assert!(map_provider_status("REQUEST_DENIED", "x").unwrap_err().to_string().contains("API key"));
        assert!(map_provider_status("OVER_QUERY_LIMIT", "x").unwrap_err().to_string().contains("quota"));
        assert!(map_provider_status("WHATEVER", "x").unwrap_err().to_string().contains("transient"));
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(403, "x").unwrap_err().to_string().contains("key was rejected"));
        assert!(map_status(401, "x").unwrap_err().to_string().contains("key was rejected"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(503, "x").unwrap_err().to_string().contains("transient"));
    }
}
