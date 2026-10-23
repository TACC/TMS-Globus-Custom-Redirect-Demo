//! The public institutions API, as specified in api/openapi.yaml:
//!
//!   GET     /api/v1/institutions                      the approved institutions
//!   OPTIONS /api/v1/institutions                      CORS preflight for that list
//!   GET     /api/v1/institutions.context.r<n>.jsonld  each revision of the JSON-LD context for that list
//!
//! Other methods get a 405 and other paths under /api/v1 a 404, both as
//! RFC 9457 problems. Kept apart from the demo's login flow so it depends
//! only on the registry, the deployment's base URL and its CORS setting.

use crate::institutions::{ListType, RegistryError, RegistrySource};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use serde_json::json;

pub const LIST_PATH: &str = "/api/v1/institutions";
/// The `Allow` header of the list's preflight and 405s.
const LIST_ALLOW: &str = "GET, HEAD, OPTIONS";
/// The response headers cross-origin scripts may read from the list.
const EXPOSE_HEADERS: &str = "ETag, Link, Retry-After";
const CONTEXT_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// One published revision of the JSON-LD context: api/institutions.context.r<n>.jsonld,
/// served at `context_path(n)`.
pub struct ContextRevision {
    pub revision: u32,
    pub body: &'static str,
}

/// Every published revision, oldest first. A published revision never
/// changes, so a changed context is a new row (and file) for the next
/// revision; earlier rows stay, so their URLs keep working, and the list's
/// Link header moves to the last one. The tests pin each row's content.
pub const CONTEXTS: &[ContextRevision] = &[
    ContextRevision { revision: 1, body: include_str!("../api/institutions.context.r1.jsonld") },
    // r1 without `@version: 1.1`, which it did not need.
    ContextRevision { revision: 2, body: include_str!("../api/institutions.context.r2.jsonld") },
];

pub fn context_path(revision: u32) -> String {
    format!("/api/v1/institutions.context.r{revision}.jsonld")
}

/// The revision the list's Link header names.
fn latest_context() -> &'static ContextRevision {
    CONTEXTS.last().expect("at least one context revision")
}

/// The published revision a file name under /api/v1 names, if any. The
/// number is decimal without leading zeros, so each revision has one URL.
fn context_revision(file: &str) -> Option<&'static ContextRevision> {
    let digits = file.strip_prefix("institutions.context.r")?.strip_suffix(".jsonld")?;
    let revision = digits.parse::<u32>().ok().filter(|n| n.to_string() == digits)?;
    CONTEXTS.iter().find(|c| c.revision == revision)
}

/// Which other origins may read the list (`CORS_ALLOWED_ORIGINS`), as the
/// spec's CORS rule for `/api/v1/institutions` describes.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Cors {
    /// Same-origin clients only: no CORS headers.
    #[default]
    SameOrigin,
    /// `*`: any origin.
    Any,
    /// These exact origins, serialized as `scheme://host[:port]`.
    Origins(Vec<String>),
}

impl Cors {
    /// An empty setting, `*`, or comma-separated origins such as
    /// `https://portal.example.org, http://localhost:3000`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let items: Vec<&str> = raw.split(',').map(str::trim).filter(|o| !o.is_empty()).collect();
        match items.as_slice() {
            [] => Ok(Cors::SameOrigin),
            ["*"] => Ok(Cors::Any),
            _ => items
                .iter()
                .map(|o| match url::Url::parse(o) {
                    Ok(u) if matches!(u.scheme(), "http" | "https") && u.origin().ascii_serialization() == *o => Ok(o.to_string()),
                    _ => Err(format!("CORS_ALLOWED_ORIGINS: {o:?} is not an origin like https://host[:port] (or a lone *)")),
                })
                .collect::<Result<_, _>>()
                .map(Cors::Origins),
        }
    }

    /// Adds `Vary` and `Access-Control-Allow-Origin` as the setting asks, and
    /// says whether the request's origin is allowed.
    fn allow_origin(&self, request: &HeaderMap, out: &mut HeaderMap) -> bool {
        let allow = match self {
            Cors::SameOrigin => return false,
            Cors::Any => HeaderValue::from_static("*"),
            Cors::Origins(origins) => {
                out.insert(header::VARY, HeaderValue::from_static("Origin"));
                match request.get(header::ORIGIN).filter(|o| origins.iter().any(|a| o.as_bytes() == a.as_bytes())) {
                    Some(origin) => origin.clone(),
                    None => return false,
                }
            }
        };
        out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, allow);
        true
    }

    /// The CORS headers every non-preflight response from the list's path
    /// carries, errors included.
    fn list_headers(&self, request: &HeaderMap) -> HeaderMap {
        let mut out = HeaderMap::new();
        if self.allow_origin(request, &mut out) {
            out.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(EXPOSE_HEADERS));
        }
        out
    }
}

#[derive(Clone)]
struct ApiState {
    registry: RegistrySource,
    base_url: String,
    cors: Cors,
}

/// The API's routes, ready to merge into an app with any state.
pub fn router<S: Clone + Send + Sync + 'static>(registry: RegistrySource, base_url: String, cors: Cors) -> Router<S> {
    Router::new()
        .route(LIST_PATH, get(list_institutions).options(preflight_institutions).fallback(list_method_not_allowed))
        // The contexts, and a 404 for every other path in the API's namespace,
        // the namespace itself included (`{*path}` needs a non-empty tail).
        .route("/api/v1/{*path}", any(api_path))
        .route("/api/v1", any(api_root))
        .route("/api/v1/", any(api_root))
        .with_state(ApiState { registry, base_url, cors })
}

/// An RFC 9457 problem (`about:blank`, so the title is the status phrase),
/// never stored.
fn problem(status: StatusCode, detail: &str, mut headers: HeaderMap) -> Response {
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let body = json!({
        "type": "about:blank",
        "title": status.canonical_reason().unwrap_or_default(),
        "status": status.as_u16(),
        "detail": detail,
    });
    (status, headers, body.to_string()).into_response()
}

/// 503 for institution data that could not be read (worth a retry), 500 for
/// data that is invalid. The cause goes to the server's log, not to the public.
fn registry_problem(e: &RegistryError, headers: HeaderMap) -> Response {
    eprintln!("institutions list: {e}");
    match e {
        RegistryError::Unavailable(_) => {
            problem(StatusCode::SERVICE_UNAVAILABLE, "The institution data cannot be read right now.", headers)
        }
        RegistryError::Invalid(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "The institution data is invalid.", headers),
    }
}

/// A request's field lines for `name`, joined into one value as RFC 9110
/// allows; None when there are none.
fn field(headers: &HeaderMap, name: header::HeaderName) -> Option<Vec<u8>> {
    let lines: Vec<&[u8]> = headers.get_all(name).iter().map(HeaderValue::as_bytes).collect();
    (!lines.is_empty()).then(|| lines.join(&b", "[..]))
}

fn trim_ows(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !matches!(c, b' ' | b'\t')).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !matches!(c, b' ' | b'\t')).map_or(start, |i| i + 1);
    &b[start..end.max(start)]
}

/// The opaque tags (`"..."`, quotes included, `W/` dropped) of the entity
/// tags in an `If-None-Match` list. Members that are not entity tags are
/// skipped, so the rest of the list still counts.
fn entity_tags(field: &[u8]) -> Vec<&[u8]> {
    // etagc: %x21 / %x23-7E / obs-text, so commas may sit inside a tag.
    let etagc = |c: u8| c == 0x21 || (0x23..=0x7e).contains(&c) || c >= 0x80;
    let mut tags = Vec::new();
    let mut rest = field;
    while !rest.is_empty() {
        let member = trim_ows(rest);
        let body = member.strip_prefix(b"W/").unwrap_or(member);
        let tag = body
            .strip_prefix(b"\"")
            .and_then(|inner| inner.iter().position(|&c| c == b'"'))
            .map(|end| (&body[..end + 2], trim_ows(&body[end + 2..])))
            .filter(|(tag, after)| tag[1..tag.len() - 1].iter().all(|&c| etagc(c)) && (after.is_empty() || after[0] == b','));
        match tag {
            Some((tag, after)) => {
                tags.push(tag);
                rest = after.strip_prefix(b",").unwrap_or(after);
            }
            // Not an entity tag: skip to the next member.
            None => rest = member.iter().position(|&c| c == b',').map_or(&[][..], |i| &member[i + 1..]),
        }
    }
    tags
}

/// Evaluates `If-Match` and then `If-None-Match` against a resource that
/// exists and has the weak `etag` (None if it has none), as RFC 9110
/// §13.2.2 orders them: the status to answer with instead, if any.
fn preconditions(headers: &HeaderMap, etag: Option<&str>) -> Option<StatusCode> {
    // If-Match compares strongly, and every tag here is weak, so only `*` passes.
    if field(headers, header::IF_MATCH).is_some_and(|f| trim_ows(&f) != b"*") {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    let field = field(headers, header::IF_NONE_MATCH)?;
    // `*` is only valid as the whole field, never as a list member.
    let ours = etag.map(|e| e.strip_prefix("W/").unwrap_or(e).as_bytes());
    let matched = trim_ows(&field) == b"*" || ours.is_some_and(|ours| entity_tags(&field).contains(&ours));
    matched.then_some(StatusCode::NOT_MODIFIED)
}

async fn list_institutions(State(api): State<ApiState>, headers: HeaderMap) -> Response {
    let mut out = api.cors.list_headers(&headers);
    let registry = match api.registry.current() {
        Ok(registry) => registry,
        Err(e) => return registry_problem(&e, out),
    };
    let revision = latest_context().revision;
    // Weak: it names the data's and the context's revisions, not these exact
    // bytes, and changes when either does. `version` is checked to be
    // ETag-safe when the registry loads.
    let etag = registry.version().map(|v| format!("W/\"{v};r{revision}\""));
    match preconditions(&headers, etag.as_deref()) {
        Some(StatusCode::PRECONDITION_FAILED) => return problem(StatusCode::PRECONDITION_FAILED, "If-Match did not match.", out),
        Some(status) => {
            add_list_validators(&mut out, etag.as_deref());
            return (status, out).into_response();
        }
        None => add_list_validators(&mut out, etag.as_deref()),
    }
    // `type` makes the list a schema:ItemList under the JSON-LD context, and
    // the Link header tells JSON-LD processors where that context is.
    let link = format!(
        r#"<{}{}>; rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json""#,
        api.base_url,
        context_path(revision)
    );
    out.insert(header::LINK, HeaderValue::from_str(&link).expect("BASE_URL is a valid header value"));
    let mut body = json!({
        "type": ListType::ItemList,
        "status": "success",
        "message": "Approved institutions.",
        "result": registry.approved(),
    });
    if let Some(v) = registry.version() {
        body["version"] = v.into();
    }
    (out, Json(body)).into_response()
}

/// The caching headers a 200 and a 304 of the list share.
fn add_list_validators(out: &mut HeaderMap, etag: Option<&str>) {
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Some(tag) = etag {
        out.insert(header::ETAG, HeaderValue::from_str(tag).expect("version is ETag-safe"));
    }
}

/// The preflight before a cross-origin GET that sets If-None-Match. The
/// browser checks the requested method and headers against these.
async fn preflight_institutions(State(api): State<ApiState>, headers: HeaderMap) -> Response {
    let mut out = HeaderMap::new();
    out.insert(header::ALLOW, HeaderValue::from_static(LIST_ALLOW));
    if api.cors.allow_origin(&headers, &mut out) {
        out.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET"));
        out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("If-None-Match"));
        out.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("7200"));
    }
    (StatusCode::NO_CONTENT, out).into_response()
}

/// Any other method on the list, with the list's CORS headers like every
/// response there.
async fn list_method_not_allowed(State(api): State<ApiState>, method: Method, headers: HeaderMap) -> Response {
    let mut out = api.cors.list_headers(&headers);
    out.insert(header::ALLOW, HeaderValue::from_static(LIST_ALLOW));
    problem(StatusCode::METHOD_NOT_ALLOWED, &format!("{method} is not supported here."), out)
}

/// Headers for everything under /api/v1 except the list: open to any origin.
fn any_origin() -> HeaderMap {
    let mut out = HeaderMap::new();
    out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    out
}

/// `/api/v1` itself, which names no resource.
async fn api_root() -> Response {
    problem(StatusCode::NOT_FOUND, "No such resource.", any_origin())
}

/// The context revisions, and a 404 for any other path under /api/v1. Open
/// to any origin, since JSON-LD processors in other sites' pages fetch the
/// contexts. A revision's URL never changes content, so it can be cached
/// indefinitely, and it has no ETag. (axum drops the body of a HEAD response.)
async fn api_path(method: Method, Path(path): Path<String>, headers: HeaderMap) -> Response {
    let mut out = any_origin();
    let Some(ctx) = context_revision(&path) else {
        return problem(StatusCode::NOT_FOUND, "No such resource.", out);
    };
    if method != Method::GET && method != Method::HEAD {
        out.insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return problem(StatusCode::METHOD_NOT_ALLOWED, &format!("{method} is not supported here."), out);
    }
    match preconditions(&headers, None) {
        Some(StatusCode::PRECONDITION_FAILED) => problem(StatusCode::PRECONDITION_FAILED, "If-Match did not match.", out),
        Some(status) => {
            out.insert(header::CACHE_CONTROL, HeaderValue::from_static(CONTEXT_CACHE_CONTROL));
            (status, out).into_response()
        }
        None => {
            out.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/ld+json"));
            out.insert(header::CACHE_CONTROL, HeaderValue::from_static(CONTEXT_CACHE_CONTROL));
            (out, ctx.body).into_response()
        }
    }
}

/// Contract tests: the responses are checked against api/openapi.yaml itself,
/// and the spec against the JSON-LD context, so the three cannot drift apart.
/// conformance/ checks the same contract over HTTP against any deployment.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::institutions::Registry;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::Value;
    use std::sync::Arc;
    use tower::ServiceExt;

    const SPEC: &str = include_str!("../api/openapi.yaml");
    const MOCK: &str = include_str!("../mock/approved_institutions.json");
    const BASE: &str = "https://tms.example.org";
    /// The spec's path for every context revision.
    const CONTEXT_SPEC_PATH: &str = "/api/v1/institutions.context.r{revision}.jsonld";
    /// SHA-256 of each published api/institutions.context.r<n>.jsonld, line
    /// endings normalised. Add a row with each new revision; never edit one.
    const CONTEXT_SHA256: &[(u32, &str)] = &[
        (1, "0c053922807aef6eb421cffc9fe70b6dc266886907109972530d2d44a6851f93"),
        (2, "615ed222b4642a186c6bd17d4dbbe895cc10c33fbab6f08ca672b917989b14b0"),
    ];

    /// The mock registry's ETag: its version and the latest context revision.
    fn mock_etag() -> String {
        format!("W/\"mock-0.1.0;r{}\"", latest_context().revision)
    }

    fn sha256(text: &str) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(text.replace("\r\n", "\n").as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn link_to(path: &str) -> String {
        format!(r#"<{BASE}{path}>; rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json""#)
    }

    fn spec() -> Value {
        serde_yaml::from_str(SPEC).expect("api/openapi.yaml parses")
    }

    /// A validator for one of the spec's component schemas.
    fn validator(name: &str) -> jsonschema::Validator {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": format!("#/components/schemas/{name}"),
            "components": spec()["components"],
        });
        jsonschema::options().should_validate_formats(true).build(&schema).expect("schema compiles")
    }

    fn assert_valid(name: &str, value: &Value) {
        let errors: Vec<String> = validator(name).iter_errors(value).map(|e| format!("{} at {}", e, e.instance_path())).collect();
        assert!(errors.is_empty(), "{name}: {errors:#?}");
    }

    /// Whether `value` fits the schema of one of the spec's component headers.
    fn header_fits(name: &str, value: &str) -> bool {
        jsonschema::validator_for(&spec()["components"]["headers"][name]["schema"]).unwrap().is_valid(&json!(value))
    }

    fn fixed(registry: &str) -> RegistrySource {
        RegistrySource::Fixed(Arc::new(Registry::from_json(registry).unwrap()))
    }

    async fn get(registry: &str, path: &str, if_none_match: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let headers: Vec<_> = if_none_match.map(|tag| (header::IF_NONE_MATCH, tag)).into_iter().collect();
        request(registry, Cors::SameOrigin, path, &headers).await
    }

    async fn request(registry: &str, cors: Cors, path: &str, headers: &[(header::HeaderName, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
        send(registry, cors, Method::GET, path, headers).await
    }

    async fn send(registry: &str, cors: Cors, method: Method, path: &str, headers: &[(header::HeaderName, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
        let headers: Vec<_> = headers.iter().map(|(n, v)| (n.clone(), HeaderValue::from_str(v).unwrap())).collect();
        send_to(fixed(registry), cors, method, path, &headers).await
    }

    async fn send_to(
        source: RegistrySource,
        cors: Cors,
        method: Method,
        path: &str,
        headers: &[(header::HeaderName, HeaderValue)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let app: Router = router(source, BASE.into(), cors);
        let mut req = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let (parts, body) = resp.into_parts();
        (parts.status, parts.headers, to_bytes(body, usize::MAX).await.unwrap().to_vec())
    }

    fn header<'a>(headers: &'a HeaderMap, name: header::HeaderName) -> Option<&'a str> {
        headers.get(name).map(|v| v.to_str().unwrap())
    }

    /// An RFC 9457 problem as the spec gives it, with the given status.
    fn assert_problem(status: StatusCode, expected: StatusCode, headers: &HeaderMap, body: &[u8]) {
        assert_eq!(status, expected);
        assert_eq!(header(headers, header::CONTENT_TYPE), Some("application/problem+json"));
        assert_eq!(header(headers, header::CACHE_CONTROL), Some(spec_header("ErrorCacheControl").as_str()));
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_valid("Problem", &body);
        assert_eq!(body["status"], expected.as_u16());
        assert_eq!(body["title"], expected.canonical_reason().unwrap(), "title is the status phrase");
    }

    #[tokio::test]
    async fn list_conforms_to_the_spec() {
        let (status, headers, body) = get(MOCK, LIST_PATH, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/json"));
        assert_eq!(header(&headers, header::LINK), Some(link_to(&context_path(latest_context().revision)).as_str()));
        assert_eq!(header(&headers, header::CACHE_CONTROL), Some(spec_header("ListCacheControl").as_str()));
        assert_eq!(header(&headers, header::ETAG), Some(mock_etag().as_str()));
        assert!(header_fits("ETag", &mock_etag()));
        // Same-origin only: no CORS headers at all.
        assert!(!headers.keys().any(|k| k.as_str().starts_with("access-control-")));
        assert!(header(&headers, header::VARY).is_none());

        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_valid("InstitutionsResponse", &body);
        assert_eq!(body["version"], "mock-0.1.0");
    }

    #[test]
    fn spec_etag_is_weak_and_names_the_context() {
        assert!(header_fits("ETag", "W/\"2026-10-23.1;r2\""));
        for bad in ["\"2026-10-23.1;r2\"", "W/\"2026-10-23.1\"", "W/\"2026;r0\"", "W/\"a,b;r2\"", "W/\";r2\""] {
            assert!(!header_fits("ETag", bad), "{bad}");
        }
    }

    /// Query strings and Accept change nothing.
    #[tokio::test]
    async fn query_and_accept_are_ignored() {
        let (_, _, plain) = get(MOCK, LIST_PATH, None).await;
        let (status, _, body) = get(MOCK, &format!("{LIST_PATH}?format=xml&x=1"), None).await;
        assert_eq!((status, &body), (StatusCode::OK, &plain));
        let (status, headers, body) = request(MOCK, Cors::SameOrigin, LIST_PATH, &[(header::ACCEPT, "application/xml")]).await;
        assert_eq!((status, &body), (StatusCode::OK, &plain));
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/json"));
        let context = context_path(latest_context().revision);
        let (status, headers, _) = request(MOCK, Cors::SameOrigin, &format!("{context}?x=1"), &[(header::ACCEPT, "text/html")]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/ld+json"));
    }

    /// HEAD gets GET's status and headers, without the body, on the list
    /// (also when If-None-Match matches) and on the contexts.
    #[tokio::test]
    async fn head_matches_get() {
        let context = context_path(latest_context().revision);
        let etag = mock_etag();
        let cors = Cors::Origins(vec![PORTAL.into()]);
        for (path, tag) in [(LIST_PATH, None), (LIST_PATH, Some(etag.as_str())), (context.as_str(), None), (context.as_str(), Some("*"))] {
            let headers: Vec<_> = [(header::ORIGIN, PORTAL)].into_iter().chain(tag.map(|t| (header::IF_NONE_MATCH, t))).collect();
            let (get_status, get_headers, _) = send(MOCK, cors.clone(), Method::GET, path, &headers).await;
            let (status, out, body) = send(MOCK, cors.clone(), Method::HEAD, path, &headers).await;
            assert_eq!(status, get_status, "{path} {tag:?}");
            assert!(body.is_empty());
            assert_eq!(out, get_headers, "{path} {tag:?}");
        }
    }

    const PORTAL: &str = "https://portal.example.org";

    /// The spec's `const` for one of its component headers.
    fn spec_header(name: &str) -> String {
        spec()["components"]["headers"][name]["schema"]["const"].as_str().unwrap().to_string()
    }

    /// The spec's `const` for a header of the preflight's 204, as sent.
    fn preflight_header(name: &str) -> String {
        match &spec()["paths"][LIST_PATH]["options"]["responses"]["204"]["headers"][name]["schema"]["const"] {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        }
    }

    async fn preflight(cors: Cors, origin: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let headers: Vec<_> = origin
            .map(|o| (header::ORIGIN, o))
            .into_iter()
            .chain([(header::ACCESS_CONTROL_REQUEST_METHOD, "GET"), (header::ACCESS_CONTROL_REQUEST_HEADERS, "if-none-match")])
            .collect();
        send(MOCK, cors, Method::OPTIONS, LIST_PATH, &headers).await
    }

    #[tokio::test]
    async fn preflight_allows_listed_origins_to_send_if_none_match() {
        let cors = Cors::Origins(vec![PORTAL.into()]);
        let (status, headers, body) = preflight(cors.clone(), Some(PORTAL)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(body.is_empty());
        for (name, spec_name) in [
            (header::ALLOW, "Allow"),
            (header::ACCESS_CONTROL_ALLOW_METHODS, "Access-Control-Allow-Methods"),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, "Access-Control-Allow-Headers"),
            (header::ACCESS_CONTROL_MAX_AGE, "Access-Control-Max-Age"),
        ] {
            assert_eq!(header(&headers, name), Some(preflight_header(spec_name).as_str()), "{spec_name}");
        }
        assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL));
        assert_eq!(header(&headers, header::VARY), Some("Origin"));
        assert!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS).is_none());
        assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());

        for origin in [None, Some("https://evil.example.org")] {
            let (status, headers, _) = preflight(cors.clone(), origin).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(header(&headers, header::VARY), Some("Origin"), "{origin:?}");
            assert!(!headers.keys().any(|k| k.as_str().starts_with("access-control-")), "{origin:?}");
        }
    }

    #[tokio::test]
    async fn preflight_follows_the_any_and_same_origin_settings() {
        let (_, headers, _) = preflight(Cors::Any, Some(PORTAL)).await;
        assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
        assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_HEADERS), Some("If-None-Match"));
        assert!(header(&headers, header::VARY).is_none());

        let (status, headers, _) = preflight(Cors::SameOrigin, Some(PORTAL)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(header(&headers, header::ALLOW), Some(LIST_ALLOW));
        assert!(header(&headers, header::VARY).is_none());
        assert!(!headers.keys().any(|k| k.as_str().starts_with("access-control-")));
    }

    #[tokio::test]
    async fn listed_origins_may_read_the_list() {
        let cors = Cors::Origins(vec![PORTAL.into(), "http://localhost:3000".into()]);
        let etag = mock_etag();
        for (extra, expected) in [(None, StatusCode::OK), (Some((header::IF_NONE_MATCH, etag.as_str())), StatusCode::NOT_MODIFIED)] {
            let headers: Vec<_> = [(header::ORIGIN, PORTAL)].into_iter().chain(extra).collect();
            let (status, headers, _) = request(MOCK, cors.clone(), LIST_PATH, &headers).await;
            assert_eq!(status, expected);
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL), "{status}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(spec_header("CorsExposeHeaders").as_str()));
            assert_eq!(header(&headers, header::VARY), Some(spec_header("VaryOrigin").as_str()));
            assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
        }
        for origin in [None, Some("https://evil.example.org"), Some("https://portal.example.org.evil.org")] {
            let headers: Vec<_> = origin.map(|o| (header::ORIGIN, o)).into_iter().collect();
            let (status, headers, _) = request(MOCK, cors.clone(), LIST_PATH, &headers).await;
            assert_eq!(status, StatusCode::OK);
            assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(), "{origin:?}");
            assert!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS).is_none(), "{origin:?}");
            assert_eq!(header(&headers, header::VARY), Some("Origin"), "{origin:?}");
        }
    }

    /// `*` gives the same headers on 200 and 304, with or without an Origin.
    #[tokio::test]
    async fn any_origin_may_read_the_list() {
        let etag = mock_etag();
        for origin in [Some(PORTAL), None] {
            for tag in [None, Some(etag.as_str())] {
                let headers: Vec<_> = origin
                    .map(|o| (header::ORIGIN, o))
                    .into_iter()
                    .chain(tag.map(|t| (header::IF_NONE_MATCH, t)))
                    .collect();
                let (status, headers, _) = request(MOCK, Cors::Any, LIST_PATH, &headers).await;
                let expected = if tag.is_some() { StatusCode::NOT_MODIFIED } else { StatusCode::OK };
                assert_eq!(status, expected, "{origin:?}");
                assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"), "{origin:?} {status}");
                assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(EXPOSE_HEADERS), "{origin:?} {status}");
                assert!(header(&headers, header::VARY).is_none(), "{origin:?} {status}");
                assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
            }
        }
    }

    #[test]
    fn cors_setting_parses() {
        assert_eq!(Cors::parse(""), Ok(Cors::SameOrigin));
        assert_eq!(Cors::parse(" * "), Ok(Cors::Any));
        assert_eq!(
            Cors::parse("https://portal.example.org, http://localhost:3000,"),
            Ok(Cors::Origins(vec![PORTAL.into(), "http://localhost:3000".into()]))
        );
        for bad in ["https://portal.example.org/", "https://portal.example.org/login", "portal.example.org", "ftp://a.org", "https://A.org", "https://a.org:443", "*, https://a.org"] {
            assert!(Cors::parse(bad).is_err(), "{bad}");
        }
    }

    /// The approved entries, in the mock registry's order (the demo's stable
    /// order), and nothing else.
    #[tokio::test]
    async fn list_holds_only_approved_entries_in_a_stable_order() {
        let (_, _, body) = get(MOCK, LIST_PATH, None).await;
        let body: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = body["result"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["globus", "uchicago", "utexas", "sdsc", "stanford", "tacc_tapis"]);
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "each id at most once");
        let (_, _, again) = get(MOCK, LIST_PATH, None).await;
        assert_eq!(serde_json::from_slice::<Value>(&again).unwrap(), body, "same order on every request");
    }

    #[tokio::test]
    async fn matching_etag_gets_not_modified() {
        let etag = mock_etag();
        let strong = etag.trim_start_matches("W/").to_string();
        let listed = format!("\"old\", {etag}");
        let malformed_first = format!("garbage, W/\"x, {etag}");
        for tag in [etag.as_str(), &strong, &listed, &malformed_first, "*", " * "] {
            let (status, headers, body) = get(MOCK, LIST_PATH, Some(tag)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{tag}");
            assert_eq!(header(&headers, header::ETAG), Some(etag.as_str()));
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some("no-cache"));
            assert!(header(&headers, header::LINK).is_none(), "{tag}");
            assert!(header(&headers, header::CONTENT_TYPE).is_none(), "{tag}");
            assert!(body.is_empty());
        }
        // Another tag; one naming only the data's version (an older context);
        // members that are not entity tags; `*` inside a list.
        for tag in ["\"old\"", "W/\"old\"", "W/\"mock-0.1.0\"", "mock-0.1.0;r2", "w/\"mock-0.1.0;r2\"", "\"old\", *", "*, \"old\""] {
            let (status, _, _) = get(MOCK, LIST_PATH, Some(tag)).await;
            assert_eq!(status, StatusCode::OK, "{tag}");
        }
    }

    /// A field that is not valid UTF-8 (obs-text) still has its entity tags read.
    #[tokio::test]
    async fn non_ascii_if_none_match_is_still_read() {
        let mut bytes = format!("{}, \"", mock_etag()).into_bytes();
        bytes.extend_from_slice(b"\xff\"");
        let headers = [(header::IF_NONE_MATCH, HeaderValue::from_bytes(&bytes).unwrap())];
        let (status, _, _) = send_to(fixed(MOCK), Cors::SameOrigin, Method::GET, LIST_PATH, &headers).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn entity_tags_are_parsed_by_the_grammar() {
        assert_eq!(entity_tags(b"\"a\", W/\"b\",,  \"c,d\""), [&b"\"a\""[..], b"\"b\"", b"\"c,d\""]);
        assert_eq!(entity_tags(b"*, \"a\""), [&b"\"a\""[..]]);
        assert_eq!(entity_tags(b"\"a\" \"b\", \"c\""), [&b"\"c\""[..]]);
        assert!(entity_tags(b"\"a").is_empty());
        assert!(entity_tags(b"\"a\"b\"").is_empty());
        assert!(entity_tags(b"").is_empty());
    }

    /// Every tag here is weak, so If-Match (strong comparison) only passes as `*`.
    #[tokio::test]
    async fn if_match_passes_only_as_any() {
        let cors = Cors::Origins(vec![PORTAL.into()]);
        let etag = mock_etag();
        let (status, _, _) = request(MOCK, cors.clone(), LIST_PATH, &[(header::IF_MATCH, "*")]).await;
        assert_eq!(status, StatusCode::OK);
        for tag in [etag.as_str(), "\"mock-0.1.0;r2\"", "\"x\""] {
            let headers = [(header::ORIGIN, PORTAL), (header::IF_MATCH, tag), (header::IF_NONE_MATCH, "*")];
            let (status, headers, body) = request(MOCK, cors.clone(), LIST_PATH, &headers).await;
            assert_problem(status, StatusCode::PRECONDITION_FAILED, &headers, &body);
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL), "{tag}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(EXPOSE_HEADERS), "{tag}");
        }
        let context = context_path(latest_context().revision);
        let (status, headers, body) = request(MOCK, Cors::SameOrigin, &context, &[(header::IF_MATCH, "\"x\"")]).await;
        assert_problem(status, StatusCode::PRECONDITION_FAILED, &headers, &body);
        assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
        let (status, _, _) = request(MOCK, Cors::SameOrigin, &context, &[(header::IF_MATCH, "*")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unversioned_list_has_no_version_or_etag() {
        let registry = r#"{"institutions":[{"id":"a","name":"A","provider":"globus","redirect_idp_domain":null,"idp_logout_url":null,"approved":true}]}"#;
        let (status, headers, body) = get(registry, LIST_PATH, Some("W/\"a\"")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(header(&headers, header::ETAG).is_none());
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body.get("version").is_none());
        assert_valid("InstitutionsResponse", &body);

        // `*` matches any current list, version or not.
        let (status, headers, _) = get(registry, LIST_PATH, Some("*")).await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(header(&headers, header::ETAG).is_none());
    }

    /// A registry file that is missing is a 503, and one that is invalid a
    /// 500, whatever If-None-Match says; both carry the list's CORS headers
    /// and keep the cause out of the public body.
    #[tokio::test]
    async fn unreadable_registry_is_a_problem() {
        let path = std::env::temp_dir().join(format!("registry-{}.json", uuid::Uuid::new_v4()));
        let source = RegistrySource::File(path.to_str().unwrap().into());
        let cors = Cors::Origins(vec![PORTAL.into()]);
        let headers = [(header::ORIGIN, HeaderValue::from_static(PORTAL)), (header::IF_NONE_MATCH, HeaderValue::from_static("*"))];
        for (contents, expected) in [(None, StatusCode::SERVICE_UNAVAILABLE), (Some("{"), StatusCode::INTERNAL_SERVER_ERROR)] {
            if let Some(c) = contents {
                std::fs::write(&path, c).unwrap();
            }
            let (status, out, body) = send_to(source.clone(), cors.clone(), Method::GET, LIST_PATH, &headers).await;
            assert_problem(status, expected, &out, &body);
            assert_eq!(header(&out, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL));
            assert_eq!(header(&out, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(EXPOSE_HEADERS));
            assert_eq!(header(&out, header::VARY), Some("Origin"));
            assert!(!String::from_utf8_lossy(&body).contains("registry-"), "no file name in {expected}");
        }
        std::fs::write(&path, MOCK).unwrap();
        let (status, _, _) = send_to(source, cors, Method::GET, LIST_PATH, &[]).await;
        assert_eq!(status, StatusCode::OK);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn other_methods_get_method_not_allowed() {
        let cors = Cors::Origins(vec![PORTAL.into()]);
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH, Method::TRACE] {
            let (status, headers, body) = send(MOCK, cors.clone(), method.clone(), LIST_PATH, &[(header::ORIGIN, PORTAL)]).await;
            assert_problem(status, StatusCode::METHOD_NOT_ALLOWED, &headers, &body);
            assert_eq!(header(&headers, header::ALLOW), Some(LIST_ALLOW), "{method}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL), "{method}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(EXPOSE_HEADERS), "{method}");
            assert_eq!(header(&headers, header::VARY), Some("Origin"), "{method}");
        }
        let context = context_path(latest_context().revision);
        for method in [Method::POST, Method::OPTIONS, Method::DELETE] {
            let (status, headers, body) = send(MOCK, Cors::SameOrigin, method.clone(), &context, &[]).await;
            assert_problem(status, StatusCode::METHOD_NOT_ALLOWED, &headers, &body);
            assert_eq!(header(&headers, header::ALLOW), Some("GET, HEAD"), "{method}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"), "{method}");
        }
    }

    #[tokio::test]
    async fn unknown_api_paths_are_not_found() {
        let next = format!("r{}", CONTEXTS.len() + 1);
        for file in ["r0", "r01", "r+1", next.as_str()] {
            let path = format!("/api/v1/institutions.context.{file}.jsonld");
            let (status, headers, body) = get(MOCK, &path, None).await;
            assert_problem(status, StatusCode::NOT_FOUND, &headers, &body);
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"), "{path}");
        }
        for path in ["/api/v1", "/api/v1/", "/api/v1/institutions/", "/api/v1/institutions.context.jsonld", "/api/v1/nope", "/api/v1/a/b/c"] {
            for method in [Method::GET, Method::POST] {
                let (status, headers, body) = send(MOCK, Cors::SameOrigin, method.clone(), path, &[]).await;
                assert_problem(status, StatusCode::NOT_FOUND, &headers, &body);
                assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"), "{method} {path}");
            }
        }
    }

    /// Every revision ever published is still served, unchanged, with the
    /// headers that let it be cached forever; `If-None-Match: *` gets a 304.
    #[tokio::test]
    async fn every_context_revision_is_served() {
        for ctx in CONTEXTS {
            let path = context_path(ctx.revision);
            let (status, headers, body) = get(MOCK, &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/ld+json"));
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some(spec_header("ContextCacheControl").as_str()));
            assert_eq!(body, ctx.body.as_bytes(), "{path}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(body["@context"].is_object(), "{path}");

            let (status, headers, body) = get(MOCK, &path, Some("*")).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{path}");
            assert!(body.is_empty());
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some(CONTEXT_CACHE_CONTROL));
            assert!(header(&headers, header::CONTENT_TYPE).is_none());
            let (status, _, _) = get(MOCK, &path, Some("W/\"x\"")).await;
            assert_eq!(status, StatusCode::OK, "contexts have no ETag to match");
        }
    }

    /// Published revisions are cached indefinitely, so they are frozen. To
    /// change the context, add api/institutions.context.r<n+1>.jsonld, a
    /// CONTEXTS row and a CONTEXT_SHA256 row for it, add n+1 to the spec's
    /// `revision` enum and its hash to `x-context-sha256`, and point the
    /// spec's Link header, ETag example and context example at it; the tests
    /// below catch a missed step.
    #[test]
    fn published_contexts_do_not_change() {
        let revisions: Vec<u32> = CONTEXTS.iter().map(|c| c.revision).collect();
        assert_eq!(revisions, (1..=CONTEXTS.len() as u32).collect::<Vec<_>>(), "revisions run r1, r2, ... with no gaps");
        let pinned: Vec<u32> = CONTEXT_SHA256.iter().map(|(r, _)| *r).collect();
        assert_eq!(pinned, revisions, "every revision has exactly one pinned SHA-256");
        for (ctx, (_, sha)) in CONTEXTS.iter().zip(CONTEXT_SHA256) {
            assert_eq!(sha256(ctx.body), *sha, "context r{} changed: publish it as r{} instead", ctx.revision, CONTEXTS.len() + 1);
        }
    }

    /// The spec publishes the same hashes, so a separate server's copy can be
    /// checked against them.
    #[test]
    fn spec_publishes_the_context_hashes() {
        let spec = spec();
        let published = spec["paths"][CONTEXT_SPEC_PATH]["get"]["x-context-sha256"].as_object().unwrap().clone();
        let pinned: serde_json::Map<String, Value> = CONTEXT_SHA256.iter().map(|(r, sha)| (r.to_string(), json!(sha))).collect();
        assert_eq!(published, pinned);
    }

    /// The files in api/ are exactly the served revisions, each named for its
    /// revision, so a file cannot be added without serving it or served
    /// under the wrong number.
    #[test]
    fn context_files_match_the_revisions() {
        let api = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api");
        let mut files: Vec<String> = std::fs::read_dir(&api)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|f| f.starts_with("institutions.context."))
            .collect();
        files.sort();
        let mut expected: Vec<String> = CONTEXTS.iter().map(|c| format!("institutions.context.r{}.jsonld", c.revision)).collect();
        expected.sort();
        assert_eq!(files, expected);
        for ctx in CONTEXTS {
            let on_disk = std::fs::read_to_string(api.join(format!("institutions.context.r{}.jsonld", ctx.revision))).unwrap();
            assert_eq!(on_disk, ctx.body, "r{} is served from its own file", ctx.revision);
        }
    }

    /// The spec's `revision` enum lists every served revision and no other,
    /// and everywhere it names a revision (the Link header's text and
    /// example, the ETag examples) it names the latest one.
    #[test]
    fn spec_documents_every_revision_and_links_the_latest() {
        let spec = spec();
        let context_paths: Vec<&String> = spec["paths"].as_object().unwrap().keys().filter(|p| p.contains("institutions.context.")).collect();
        assert_eq!(context_paths, [CONTEXT_SPEC_PATH], "one templated path for every revision");
        let param = &spec["components"]["parameters"]["Revision"];
        assert_eq!(param["name"], "revision");
        let documented: Vec<&str> = param["schema"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        let served: Vec<String> = CONTEXTS.iter().map(|c| c.revision.to_string()).collect();
        assert_eq!(documented, served);

        let latest = latest_context().revision;
        let mut mentions = 0;
        for marker in ["institutions.context.r", ";r"] {
            for line in SPEC.lines() {
                for (at, _) in line.match_indices(marker) {
                    let digits: String = line[at + marker.len()..].chars().take_while(char::is_ascii_digit).collect();
                    if !digits.is_empty() {
                        assert_eq!(digits.parse::<u32>().unwrap(), latest, "spec names r{digits}, not the latest r{latest}: {line}");
                        mentions += 1;
                    }
                }
            }
        }
        assert!(mentions > 0, "the spec names the latest revision");
        let example = spec["components"]["headers"]["ContextLink"]["example"].as_str().unwrap();
        assert_eq!(example, link_to(&context_path(latest)), "the spec's Link example");
    }

    /// The spec carries the latest context in full, so an implementer has
    /// its content without the repository.
    #[test]
    fn spec_context_example_is_the_latest_revision() {
        let spec = spec();
        let ok = &spec["paths"][CONTEXT_SPEC_PATH]["get"]["responses"]["200"];
        let example = &ok["content"]["application/ld+json"]["examples"]["latest"]["value"];
        let latest: Value = serde_json::from_str(latest_context().body).unwrap();
        assert_eq!(*example, latest);
        assert_eq!(ok["content"]["application/ld+json"]["examples"]["latest"]["summary"], format!("Revision {}", latest_context().revision));
    }

    #[test]
    fn spec_examples_conform_to_the_spec() {
        let spec = spec();
        let examples = &spec["paths"][LIST_PATH]["get"]["responses"]["200"]["content"]["application/json"]["examples"];
        for (name, example) in examples.as_object().unwrap() {
            assert_valid("InstitutionsResponse", &example["value"]);
            assert!(!name.contains("mock"), "example {name:?} names the demo");
        }
        let responses = spec["components"]["responses"].as_object().unwrap();
        let mut checked = 0;
        for response in responses.values() {
            if let Some(examples) = response["content"]["application/problem+json"]["examples"].as_object() {
                for example in examples.values() {
                    assert_valid("Problem", &example["value"]);
                    let status = StatusCode::from_u16(example["value"]["status"].as_u64().unwrap() as u16).unwrap();
                    assert_eq!(example["value"]["title"], status.canonical_reason().unwrap());
                    checked += 1;
                }
            }
        }
        assert!(checked >= 4, "problem examples checked: {checked}");
    }

    #[test]
    fn spec_rejects_entries_the_registry_refuses() {
        let good = json!({
            "type": "Organization", "id": "a", "name": "A", "provider": "globus",
            "redirect_idp_domain": "a.edu", "idp_logout_url": "https://idp.a.edu/logout", "approved": true
        });
        assert_valid("Institution", &good);
        let v = validator("Institution");
        for (field, bad) in [
            ("id", json!("A")),
            ("id", json!("a".repeat(65))),
            ("name", json!("")),
            ("name", json!("   ")),
            ("redirect_idp_domain", json!("..")),
            ("redirect_idp_domain", json!("UChicago.edu")),
            ("redirect_idp_domain", json!("10.0.0.1")),
            ("idp_logout_url", json!("http://idp.a.edu/logout")),
            ("idp_logout_url", json!("https://")),
            ("idp_logout_url", json!("https://idp.a.edu/log out")),
            ("approved", json!(false)),
            ("type", json!("Thing")),
        ] {
            let mut entry = good.clone();
            entry[field] = bad.clone();
            assert!(!v.is_valid(&entry), "{field} = {bad}");
        }
        let mut tapis = good.clone();
        tapis["provider"] = json!("tacc_tapis");
        assert!(!v.is_valid(&tapis), "tacc_tapis with a domain");

        let list = validator("InstitutionsResponse");
        let response = |version: &str| json!({ "type": "ItemList", "status": "success", "message": "", "version": version, "result": [] });
        assert!(list.is_valid(&response("2026-10-23.1")));
        for bad in ["a,b", "a\"b", "a b", ""] {
            assert!(!list.is_valid(&response(bad)), "version {bad:?}");
        }
    }

    /// Every field the spec maps with `x-schema-org` must map to the same term
    /// in the latest JSON-LD context, and the envelope-only fields must stay out.
    #[test]
    fn spec_and_context_agree() {
        let spec = spec();
        let ctx: Value = serde_json::from_str(latest_context().body).unwrap();
        let ctx = &ctx["@context"];
        let term = |key: &str| match &ctx[key] {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("@id").and_then(Value::as_str).map(String::from),
            _ => None,
        };
        let schemas = &spec["components"]["schemas"];
        for schema in ["InstitutionsResponse", "Institution"] {
            for (field, prop) in schemas[schema]["properties"].as_object().unwrap() {
                let mapped = prop.get("x-schema-org").or_else(|| {
                    let r = prop.get("$ref")?.as_str()?.rsplit('/').next()?;
                    schemas[r].get("x-schema-org")
                });
                match mapped.and_then(Value::as_str) {
                    Some(iri) => assert_eq!(term(field).as_deref(), Some(iri), "{schema}.{field}"),
                    None => assert!(ctx[field].is_null() && ctx.get(field).is_some(), "{schema}.{field} must be null in the context"),
                }
            }
            let ty = schemas[schema]["x-schema-org"]["@type"].as_str().unwrap();
            let name = schemas[schema]["properties"]["type"]["const"].as_str().unwrap();
            assert_eq!(term(name).as_deref(), Some(ty), "{schema} type");
        }
        for provider in schemas["Provider"]["enum"].as_array().unwrap() {
            let p = provider.as_str().unwrap();
            assert!(term(p).is_some_and(|t| t.starts_with("tms:")), "provider {p}");
            let parsed: crate::institutions::Provider = serde_json::from_value(provider.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), *provider);
        }
    }
}
