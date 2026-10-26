//! The public institutions API, as specified in api/openapi.yaml, with both
//! parts of its optional addendum, api/openapi.addendum.yaml:
//!
//!   GET     /v1/tms/institutions                  the approved institutions (core)
//!   OPTIONS /v1/tms/institutions                  CORS preflight for that list (addendum)
//!   GET     /v1/tms/contexts/institutions/<n>     each revision of the JSON-LD context for that list (addendum)
//!
//! Every response carries `X-Content-Type-Options: nosniff`. Beyond the
//! spec, it follows RFC 9110 for HEAD, If-Match (412) and other methods
//! (405), as tms_server `HttpResult` errors. Kept apart from the demo's login flow so it
//! depends only on the registry, the deployment's base URL and its CORS
//! setting.

use crate::institutions::{Institution, Provider, RegistryError, RegistrySource};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::map_response,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use log::error;
use serde::Serialize;

pub const LIST_PATH: &str = "/v1/tms/institutions";
/// The route of every context revision.
const CONTEXT_ROUTE: &str = "/v1/tms/contexts/institutions/{revision}";
/// The `Allow` header of the list's preflight and 405s.
const LIST_ALLOW: &str = "GET, HEAD, OPTIONS";
/// The `Allow` header of a context's 405s.
const CONTEXT_ALLOW: &str = "GET, HEAD";
/// The response headers cross-origin scripts may read from the list.
const EXPOSE_HEADERS: &str = "ETag, Link, Retry-After";
const CONTEXT_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Seconds a browser may reuse the list's preflight answer.
const PREFLIGHT_MAX_AGE: &str = "7200";
/// One published revision of the JSON-LD context: api/institutions.context.r<n>.jsonld,
/// served at `context_path(n)`.
pub struct ContextRevision {
    pub revision: u32,
    pub body: &'static str,
}

/// Every published revision, oldest first. A published revision never
/// changes, so a changed context is a new row (and file) for the next
/// revision; earlier rows stay, so their URLs keep working, and the list's
/// Link header moves to the last one. The tests pin each row's content to
/// the addendum's `x-context-sha256`.
pub const CONTEXTS: &[ContextRevision] = &[ContextRevision { revision: 1, body: include_str!("../api/institutions.context.r1.jsonld") }];

/// The list as the API sends it, in tms_server's response envelope
/// (`result_code`, `result_msg`, then `num_<items>` and the items): the API's
/// own model, kept apart from the registry's `Institution` so the stored
/// data and the response can change separately. The `type` fields are the
/// addendum's linked-data view.
#[derive(Debug, Serialize)]
struct InstitutionsResponse<'a> {
    /// JSON-LD type (aliased to `@type` by the context): schema:ItemList.
    #[serde(rename = "type")]
    kind: &'static str,
    result_code: &'static str,
    result_msg: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_version: Option<&'a str>,
    num_institutions: usize,
    institutions: Vec<InstitutionEntry<'a>>,
}

/// tms_server's error body (`utils::errors::HttpResult` there).
#[derive(Debug, Serialize)]
struct HttpResult {
    /// The HTTP status code, as a string.
    result_code: String,
    result_msg: String,
}

/// One entry of the list. Every field is written, `null` where it does not
/// apply; approval is not, since the list holds only approved entries.
#[derive(Debug, Serialize)]
struct InstitutionEntry<'a> {
    /// JSON-LD type: schema:Organization.
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    name: &'a str,
    provider: Provider,
    redirect_idp_domain: Option<&'a str>,
    idp_logout_url: Option<&'a str>,
}

impl<'a> From<&'a Institution> for InstitutionEntry<'a> {
    fn from(i: &'a Institution) -> Self {
        InstitutionEntry {
            kind: "Organization",
            id: &i.id,
            name: &i.name,
            provider: i.provider,
            redirect_idp_domain: i.redirect_idp_domain.as_deref(),
            idp_logout_url: i.idp_logout_url.as_deref(),
        }
    }
}

/// Checks the deployment's base URL (`BASE_URL`) once, at startup: an
/// absolute http(s) URL in canonical form, with no query, fragment or
/// trailing `/`, since the context paths are appended to it in the list's
/// Link header.
pub fn parse_base_url(raw: &str) -> Result<String, String> {
    let url = url::Url::parse(raw).map_err(|e| format!("BASE_URL {raw:?}: {e}"))?;
    let canonical = url.as_str().strip_suffix('/').unwrap_or(url.as_str());
    let ok = matches!(url.scheme(), "http" | "https") && url.query().is_none() && url.fragment().is_none() && canonical == raw;
    ok.then(|| raw.to_string()).ok_or_else(|| {
        format!("BASE_URL {raw:?} must be an http(s) URL like https://host[:port][/path], with no query, fragment or trailing '/'")
    })
}

pub fn context_path(revision: u32) -> String {
    format!("/v1/tms/contexts/institutions/{revision}")
}

/// The revision the list's Link header names.
fn latest_context() -> &'static ContextRevision {
    CONTEXTS.last().expect("at least one context revision")
}

/// The published revision a path segment names, if any. The number is
/// decimal without leading zeros, so each revision has one URL.
fn context_revision(digits: &str) -> Option<&'static ContextRevision> {
    let revision = digits.parse::<u32>().ok().filter(|n| n.to_string() == digits)?;
    CONTEXTS.iter().find(|c| c.revision == revision)
}

/// Which other origins may read the list (`CORS_ALLOWED_ORIGINS`), as the
/// addendum's CORS rule for `/v1/tms/institutions` describes.
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
    /// The list's Link header, naming the latest context.
    link: HeaderValue,
    cors: Cors,
}

/// The API's routes, ready to merge into an app with any state. `base_url`
/// must have passed `parse_base_url`.
pub fn router<S: Clone + Send + Sync + 'static>(registry: RegistrySource, base_url: String, cors: Cors) -> Router<S> {
    // The Link header tells JSON-LD processors where the context is.
    let link = format!(
        r#"<{base_url}{}>; rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json""#,
        context_path(latest_context().revision)
    );
    let link = HeaderValue::from_str(&link).unwrap_or_else(|_| panic!("BASE_URL {base_url:?} was not checked by parse_base_url"));
    Router::new()
        .route(LIST_PATH, get(list_institutions).options(preflight_institutions).fallback(list_method_not_allowed))
        .route(CONTEXT_ROUTE, get(get_context).fallback(context_method_not_allowed))
        .layer(map_response(no_sniff))
        .with_state(ApiState { registry, link, cors })
}

/// Every response from the API: browsers must not guess another media type.
async fn no_sniff(mut resp: Response) -> Response {
    resp.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    resp
}

/// An `HttpResult` error, its message prefixed `ERROR: ` as tms_server's
/// are, never stored.
fn error_result(status: StatusCode, msg: &str, mut headers: HeaderMap) -> Response {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let body = HttpResult { result_code: status.as_u16().to_string(), result_msg: format!("ERROR: {msg}") };
    (status, headers, Json(body)).into_response()
}

/// 503 for institution data that could not be read (worth a retry), 500 for
/// data that is invalid. The cause goes to the server's log, not to the public.
fn registry_error(e: &RegistryError, headers: HeaderMap) -> Response {
    error!("institutions list: {e}");
    match e {
        RegistryError::Unavailable(_) => {
            error_result(StatusCode::SERVICE_UNAVAILABLE, "The institution data cannot be read right now.", headers)
        }
        RegistryError::Invalid(_) => error_result(StatusCode::INTERNAL_SERVER_ERROR, "The institution data is invalid.", headers),
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
        Err(e) => return registry_error(&e, out),
    };
    let revision = latest_context().revision;
    // Weak: it names the data's and the context's revisions, not these exact
    // bytes, and changes when either does. The data's version is checked to be
    // ETag-safe when the registry loads.
    let etag = registry.data_version().map(|v| format!("W/\"{v};r{revision}\""));
    match preconditions(&headers, etag.as_deref()) {
        Some(StatusCode::PRECONDITION_FAILED) => return error_result(StatusCode::PRECONDITION_FAILED, "If-Match did not match.", out),
        Some(status) => {
            add_list_validators(&mut out, etag.as_deref());
            return (status, out).into_response();
        }
        None => add_list_validators(&mut out, etag.as_deref()),
    }
    out.insert(header::LINK, api.link.clone());
    let institutions: Vec<_> = registry.approved().map(InstitutionEntry::from).collect();
    let body = InstitutionsResponse {
        kind: "ItemList",
        result_code: "0",
        result_msg: "success",
        data_version: registry.data_version(),
        num_institutions: institutions.len(),
        institutions,
    };
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
        out.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(PREFLIGHT_MAX_AGE));
    }
    (StatusCode::NO_CONTENT, out).into_response()
}

/// Any other method on the list, with the list's CORS headers like every
/// response there.
async fn list_method_not_allowed(State(api): State<ApiState>, method: Method, headers: HeaderMap) -> Response {
    let mut out = api.cors.list_headers(&headers);
    out.insert(header::ALLOW, HeaderValue::from_static(LIST_ALLOW));
    error_result(StatusCode::METHOD_NOT_ALLOWED, &format!("{method} is not supported here."), out)
}

/// Headers for every context response: open to any origin.
fn any_origin() -> HeaderMap {
    let mut out = HeaderMap::new();
    out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    out
}

/// A context revision. Open to any origin, since JSON-LD processors in
/// other sites' pages fetch the contexts. A revision's URL never changes
/// content, so it can be cached indefinitely, and it has no ETag. (axum
/// answers HEAD with this handler and drops the body.)
async fn get_context(Path(revision): Path<String>, headers: HeaderMap) -> Response {
    let mut out = any_origin();
    let Some(ctx) = context_revision(&revision) else {
        return error_result(StatusCode::NOT_FOUND, "No such resource.", out);
    };
    match preconditions(&headers, None) {
        Some(StatusCode::PRECONDITION_FAILED) => error_result(StatusCode::PRECONDITION_FAILED, "If-Match did not match.", out),
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

/// Any other method on a context path: a 404 for an unpublished revision,
/// else a 405.
async fn context_method_not_allowed(method: Method, Path(revision): Path<String>) -> Response {
    let mut out = any_origin();
    if context_revision(&revision).is_none() {
        return error_result(StatusCode::NOT_FOUND, "No such resource.", out);
    }
    out.insert(header::ALLOW, HeaderValue::from_static(CONTEXT_ALLOW));
    error_result(StatusCode::METHOD_NOT_ALLOWED, &format!("{method} is not supported here."), out)
}

/// Contract tests: the responses are checked against api/openapi.yaml and its
/// addendum themselves, and the RFC 9110 behaviour beyond them (HEAD,
/// If-Match, 405) against the constants here. The documents' own
/// consistency (examples, context agreement, revision bookkeeping) is
/// checked by scripts/check_spec.py, and conformance/ checks the same
/// contract over HTTP against any deployment.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::institutions::{Registry, FIXTURE, FIXTURE_VERSION};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    const SPEC: &str = include_str!("../api/openapi.yaml");
    const ADDENDUM: &str = include_str!("../api/openapi.addendum.yaml");
    const BASE: &str = "https://tms.example.org";
    /// The addendum's path for every context revision.
    const CONTEXT_SPEC_PATH: &str = "/v1/tms/contexts/institutions/{revision}";

    /// The fixture registry's ETag: its version and the latest context revision.
    fn fixture_etag() -> String {
        format!("W/\"{FIXTURE_VERSION};r{}\"", latest_context().revision)
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

    fn addendum() -> Value {
        serde_yaml::from_str(ADDENDUM).expect("api/openapi.addendum.yaml parses")
    }

    /// A validator for one of a document's component schemas.
    fn validator(doc: &Value, name: &str) -> jsonschema::Validator {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$ref": format!("#/components/schemas/{name}"),
            "components": doc["components"],
        });
        jsonschema::options().should_validate_formats(true).build(&schema).expect("schema compiles")
    }

    fn assert_valid_in(doc: &Value, name: &str, value: &Value) {
        let errors: Vec<String> = validator(doc, name).iter_errors(value).map(|e| format!("{} at {}", e, e.instance_path())).collect();
        assert!(errors.is_empty(), "{name}: {errors:#?}");
    }

    fn assert_valid(name: &str, value: &Value) {
        assert_valid_in(&spec(), name, value);
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

    fn header(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
        headers.get(name).map(|v| v.to_str().unwrap())
    }

    /// An `HttpResult` error as the spec gives it, with the given status.
    fn assert_error(status: StatusCode, expected: StatusCode, headers: &HeaderMap, body: &[u8]) {
        assert_eq!(status, expected);
        assert_eq!(header(headers, header::CONTENT_TYPE), Some("application/json"));
        assert_eq!(header(headers, header::CACHE_CONTROL), Some(spec_header("ErrorCacheControl").as_str()));
        let body: Value = serde_json::from_slice(body).unwrap();
        assert_valid("HttpResult", &body);
        assert_eq!(body["result_code"], expected.as_u16().to_string(), "result_code is the status");
        assert!(body["result_msg"].as_str().unwrap().starts_with("ERROR: "), "{body}");
    }

    #[tokio::test]
    async fn list_conforms_to_the_spec_and_addendum() {
        let (status, headers, body) = get(FIXTURE, LIST_PATH, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/json"));
        assert_eq!(header(&headers, header::LINK), Some(link_to(&context_path(latest_context().revision)).as_str()));
        assert_eq!(header(&headers, header::X_CONTENT_TYPE_OPTIONS), Some(spec_header("NoSniff").as_str()));
        assert_eq!(header(&headers, header::CACHE_CONTROL), Some(spec_header("ListCacheControl").as_str()));
        assert_eq!(header(&headers, header::ETAG), Some(fixture_etag().as_str()));
        assert!(header_fits("ETag", &fixture_etag()));
        // Same-origin only: no CORS headers at all.
        assert!(!headers.keys().any(|k| k.as_str().starts_with("access-control-")));
        assert!(header(&headers, header::VARY).is_none());

        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_valid("InstitutionsResponse", &body);
        assert_valid_in(&addendum(), "LinkedList", &body);
        assert_eq!(body["data_version"], FIXTURE_VERSION);
        // The API model's fields, not the registry's: no `approved`.
        let fields = ["type", "id", "name", "provider", "redirect_idp_domain", "idp_logout_url"];
        assert_eq!(body["num_institutions"], body["institutions"].as_array().unwrap().len());
        for entry in body["institutions"].as_array().unwrap() {
            let mut keys: Vec<&str> = entry.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort();
            let mut expected = fields.to_vec();
            expected.sort();
            assert_eq!(keys, expected, "{entry}");
        }
    }

    #[test]
    fn base_url_is_checked_at_startup() {
        for good in ["https://tms.example.org", "http://127.0.0.1:8080", "https://a.org/tms"] {
            assert_eq!(parse_base_url(good).as_deref(), Ok(good), "{good}");
        }
        for bad in ["", "tms.example.org", "ftp://a.org", "https://a.org/", "https://a.org?x=1", "https://a.org#x", "https://A.org", "https://a.org/a b", "https://a.org/<x>"] {
            assert!(parse_base_url(bad).is_err(), "{bad:?}");
        }
    }

    /// Query strings and Accept change nothing.
    #[tokio::test]
    async fn query_and_accept_are_ignored() {
        let (_, _, plain) = get(FIXTURE, LIST_PATH, None).await;
        let (status, _, body) = get(FIXTURE, &format!("{LIST_PATH}?format=xml&x=1"), None).await;
        assert_eq!((status, &body), (StatusCode::OK, &plain));
        let (status, headers, body) = request(FIXTURE, Cors::SameOrigin, LIST_PATH, &[(header::ACCEPT, "application/xml")]).await;
        assert_eq!((status, &body), (StatusCode::OK, &plain));
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/json"));
        let context = context_path(latest_context().revision);
        let (status, headers, _) = request(FIXTURE, Cors::SameOrigin, &format!("{context}?x=1"), &[(header::ACCEPT, "text/html")]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/ld+json"));
    }

    /// HEAD (RFC 9110) gets GET's status and headers, without the body, on
    /// the list (also when If-None-Match matches) and on the contexts.
    #[tokio::test]
    async fn head_matches_get() {
        let context = context_path(latest_context().revision);
        let etag = fixture_etag();
        let cors = Cors::Origins(vec![PORTAL.into()]);
        for (path, tag) in [(LIST_PATH, None), (LIST_PATH, Some(etag.as_str())), (context.as_str(), None), (context.as_str(), Some("*"))] {
            let headers: Vec<_> = [(header::ORIGIN, PORTAL)].into_iter().chain(tag.map(|t| (header::IF_NONE_MATCH, t))).collect();
            let (get_status, get_headers, _) = send(FIXTURE, cors.clone(), Method::GET, path, &headers).await;
            let (status, out, body) = send(FIXTURE, cors.clone(), Method::HEAD, path, &headers).await;
            assert_eq!(status, get_status, "{path} {tag:?}");
            assert!(body.is_empty());
            assert_eq!(out, get_headers, "{path} {tag:?}");
        }
    }

    const PORTAL: &str = "https://portal.example.org";

    /// The value a header object gives: its `const`, or for a list-valued
    /// header its `x-required-members`, which the demo sends with nothing added.
    fn header_value(header: &Value) -> String {
        match header["schema"]["const"].as_str() {
            Some(exact) => exact.to_string(),
            None => header["x-required-members"].as_array().unwrap().iter().map(|m| m.as_str().unwrap()).collect::<Vec<_>>().join(", "),
        }
    }

    /// The value the spec gives one of its component headers.
    fn spec_header(name: &str) -> String {
        header_value(&spec()["components"]["headers"][name])
    }

    /// The value the addendum gives one of its component headers.
    fn addendum_header(name: &str) -> String {
        header_value(&addendum()["components"]["headers"][name])
    }

    /// The value the addendum gives a header of the preflight's 204.
    fn preflight_header(name: &str) -> String {
        header_value(&addendum()["paths"][LIST_PATH]["options"]["responses"]["204"]["headers"][name])
    }

    async fn preflight(cors: Cors, origin: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let headers: Vec<_> = origin
            .map(|o| (header::ORIGIN, o))
            .into_iter()
            .chain([(header::ACCESS_CONTROL_REQUEST_METHOD, "GET"), (header::ACCESS_CONTROL_REQUEST_HEADERS, "if-none-match")])
            .collect();
        send(FIXTURE, cors, Method::OPTIONS, LIST_PATH, &headers).await
    }

    #[tokio::test]
    async fn preflight_allows_listed_origins_to_send_if_none_match() {
        let cors = Cors::Origins(vec![PORTAL.into()]);
        let (status, headers, body) = preflight(cors.clone(), Some(PORTAL)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(body.is_empty());
        assert_eq!(header(&headers, header::ALLOW), Some(LIST_ALLOW));
        for (name, spec_name) in [
            (header::ACCESS_CONTROL_ALLOW_METHODS, "Access-Control-Allow-Methods"),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, "Access-Control-Allow-Headers"),
        ] {
            assert_eq!(header(&headers, name), Some(preflight_header(spec_name).as_str()), "{spec_name}");
        }
        assert_eq!(header(&headers, header::ACCESS_CONTROL_MAX_AGE), Some(PREFLIGHT_MAX_AGE));
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
        let etag = fixture_etag();
        for (extra, expected) in [(None, StatusCode::OK), (Some((header::IF_NONE_MATCH, etag.as_str())), StatusCode::NOT_MODIFIED)] {
            let headers: Vec<_> = [(header::ORIGIN, PORTAL)].into_iter().chain(extra).collect();
            let (status, headers, _) = request(FIXTURE, cors.clone(), LIST_PATH, &headers).await;
            assert_eq!(status, expected);
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL), "{status}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(addendum_header("CorsExposeHeaders").as_str()));
            assert_eq!(header(&headers, header::VARY), Some(addendum_header("VaryOrigin").as_str()));
            assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
        }
        for origin in [None, Some("https://evil.example.org"), Some("https://portal.example.org.evil.org")] {
            let headers: Vec<_> = origin.map(|o| (header::ORIGIN, o)).into_iter().collect();
            let (status, headers, _) = request(FIXTURE, cors.clone(), LIST_PATH, &headers).await;
            assert_eq!(status, StatusCode::OK);
            assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none(), "{origin:?}");
            assert!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS).is_none(), "{origin:?}");
            assert_eq!(header(&headers, header::VARY), Some("Origin"), "{origin:?}");
        }
    }

    /// `*` gives the same headers on 200 and 304, with or without an Origin.
    #[tokio::test]
    async fn any_origin_may_read_the_list() {
        let etag = fixture_etag();
        for origin in [Some(PORTAL), None] {
            for tag in [None, Some(etag.as_str())] {
                let headers: Vec<_> = origin
                    .map(|o| (header::ORIGIN, o))
                    .into_iter()
                    .chain(tag.map(|t| (header::IF_NONE_MATCH, t)))
                    .collect();
                let (status, headers, _) = request(FIXTURE, Cors::Any, LIST_PATH, &headers).await;
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

    /// The approved entries, in the registry's order (the demo's stable
    /// order), and nothing else. The fixture interleaves approved and
    /// unapproved entries out of alphabetical order, so a sorted or unfiltered
    /// list fails.
    #[tokio::test]
    async fn list_holds_only_approved_entries_in_a_stable_order() {
        let (_, _, body) = get(FIXTURE, LIST_PATH, None).await;
        let body: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = body["institutions"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["zeta", "globus", "alpha", "tacc_tapis"]);
        assert_eq!(body["num_institutions"], ids.len());
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "each id at most once");
        let (_, _, again) = get(FIXTURE, LIST_PATH, None).await;
        assert_eq!(serde_json::from_slice::<Value>(&again).unwrap(), body, "same order on every request");
    }

    #[tokio::test]
    async fn matching_etag_gets_not_modified() {
        let etag = fixture_etag();
        let strong = etag.trim_start_matches("W/").to_string();
        let listed = format!("\"old\", {etag}");
        let malformed_first = format!("garbage, W/\"x, {etag}");
        for tag in [etag.as_str(), &strong, &listed, &malformed_first, "*", " * "] {
            let (status, headers, body) = get(FIXTURE, LIST_PATH, Some(tag)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{tag}");
            assert_eq!(header(&headers, header::ETAG), Some(etag.as_str()));
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some("no-cache"));
            assert!(header(&headers, header::LINK).is_none(), "{tag}");
            assert!(header(&headers, header::CONTENT_TYPE).is_none(), "{tag}");
            assert!(body.is_empty());
        }
        // Another tag; one naming only the data's version (an older context);
        // members that are not entity tags; `*` inside a list.
        let version_only = format!("W/\"{FIXTURE_VERSION}\"");
        let unquoted = strong.trim_matches('"').to_string();
        let lower_weak = format!("w/{strong}");
        for tag in ["\"old\"", "W/\"old\"", &version_only, &unquoted, &lower_weak, "\"old\", *", "*, \"old\""] {
            let (status, _, _) = get(FIXTURE, LIST_PATH, Some(tag)).await;
            assert_eq!(status, StatusCode::OK, "{tag}");
        }
    }

    /// A field that is not valid UTF-8 (obs-text) still has its entity tags read.
    #[tokio::test]
    async fn non_ascii_if_none_match_is_still_read() {
        let mut bytes = format!("{}, \"", fixture_etag()).into_bytes();
        bytes.extend_from_slice(b"\xff\"");
        let headers = [(header::IF_NONE_MATCH, HeaderValue::from_bytes(&bytes).unwrap())];
        let (status, _, _) = send_to(fixed(FIXTURE), Cors::SameOrigin, Method::GET, LIST_PATH, &headers).await;
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

    /// Every tag here is weak, so If-Match (strong comparison, RFC 9110
    /// §13.1.1) only passes as `*`.
    #[tokio::test]
    async fn if_match_passes_only_as_any() {
        let cors = Cors::Origins(vec![PORTAL.into()]);
        let etag = fixture_etag();
        let (status, _, _) = request(FIXTURE, cors.clone(), LIST_PATH, &[(header::IF_MATCH, "*")]).await;
        assert_eq!(status, StatusCode::OK);
        // `*` is valid only as the whole field, so a list holding it fails too.
        let listed = format!("*, {etag}");
        let strong = etag.trim_start_matches("W/");
        for tag in [etag.as_str(), strong, "\"x\"", "*, \"x\"", &listed] {
            let headers = [(header::ORIGIN, PORTAL), (header::IF_MATCH, tag), (header::IF_NONE_MATCH, "*")];
            let (status, headers, body) = request(FIXTURE, cors.clone(), LIST_PATH, &headers).await;
            assert_error(status, StatusCode::PRECONDITION_FAILED, &headers, &body);
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL), "{tag}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(EXPOSE_HEADERS), "{tag}");
        }
        let context = context_path(latest_context().revision);
        let (status, headers, body) = request(FIXTURE, Cors::SameOrigin, &context, &[(header::IF_MATCH, "\"x\"")]).await;
        assert_error(status, StatusCode::PRECONDITION_FAILED, &headers, &body);
        assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
        let (status, _, _) = request(FIXTURE, Cors::SameOrigin, &context, &[(header::IF_MATCH, "*")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unversioned_list_has_no_version_or_etag() {
        let registry = r#"{"institutions":[{"id":"a","name":"A","provider":"globus","redirect_idp_domain":null,"idp_logout_url":null,"approved":true}]}"#;
        let (status, headers, body) = get(registry, LIST_PATH, Some("W/\"a\"")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(header(&headers, header::ETAG).is_none());
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body.get("data_version").is_none());
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
    async fn unreadable_registry_is_an_error() {
        let path = std::env::temp_dir().join(format!("registry-{}.json", uuid::Uuid::new_v4()));
        let source = RegistrySource::File(path.to_str().unwrap().into());
        let cors = Cors::Origins(vec![PORTAL.into()]);
        let headers = [(header::ORIGIN, HeaderValue::from_static(PORTAL)), (header::IF_NONE_MATCH, HeaderValue::from_static("*"))];
        for (contents, expected) in [(None, StatusCode::SERVICE_UNAVAILABLE), (Some("{"), StatusCode::INTERNAL_SERVER_ERROR)] {
            if let Some(c) = contents {
                std::fs::write(&path, c).unwrap();
            }
            let (status, out, body) = send_to(source.clone(), cors.clone(), Method::GET, LIST_PATH, &headers).await;
            assert_error(status, expected, &out, &body);
            assert_eq!(header(&out, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL));
            assert_eq!(header(&out, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(EXPOSE_HEADERS));
            assert_eq!(header(&out, header::VARY), Some("Origin"));
            assert!(!String::from_utf8_lossy(&body).contains("registry-"), "no file name in {expected}");
        }
        std::fs::write(&path, FIXTURE).unwrap();
        let (status, _, _) = send_to(source, cors, Method::GET, LIST_PATH, &[]).await;
        assert_eq!(status, StatusCode::OK);
        std::fs::remove_file(&path).unwrap();
    }

    /// A new `data_version` gives a new ETag: the old tag no longer matches,
    /// and the new one does.
    #[tokio::test]
    async fn etag_follows_the_data() {
        let path = std::env::temp_dir().join(format!("registry-{}.json", uuid::Uuid::new_v4()));
        let source = RegistrySource::File(path.to_str().unwrap().into());
        let get_with = |tag: Option<HeaderValue>| {
            let source = source.clone();
            async move {
                let headers: Vec<_> = tag.map(|t| (header::IF_NONE_MATCH, t)).into_iter().collect();
                send_to(source, Cors::SameOrigin, Method::GET, LIST_PATH, &headers).await
            }
        };
        std::fs::write(&path, FIXTURE).unwrap();
        let (_, headers, _) = get_with(None).await;
        let old = headers[header::ETAG].clone();
        assert_eq!(get_with(Some(old.clone())).await.0, StatusCode::NOT_MODIFIED);

        let mut changed: Value = serde_json::from_str(FIXTURE).unwrap();
        changed["data_version"] = "fixture-2".into();
        let entries = changed["institutions"].as_array_mut().unwrap();
        entries.iter_mut().for_each(|e| e["approved"] = true.into());
        let all = entries.len();
        std::fs::write(&path, changed.to_string()).unwrap();
        let (status, headers, body) = get_with(Some(old.clone())).await;
        assert_eq!(status, StatusCode::OK, "the old tag no longer matches");
        let new = headers[header::ETAG].clone();
        assert_ne!(new, old);
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["data_version"], "fixture-2");
        assert_eq!(body["num_institutions"], all, "every entry now approved");
        assert_eq!(get_with(Some(new)).await.0, StatusCode::NOT_MODIFIED);
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn other_methods_get_method_not_allowed() {
        let cors = Cors::Origins(vec![PORTAL.into()]);
        for method in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH, Method::TRACE] {
            let (status, headers, body) = send(FIXTURE, cors.clone(), method.clone(), LIST_PATH, &[(header::ORIGIN, PORTAL)]).await;
            assert_error(status, StatusCode::METHOD_NOT_ALLOWED, &headers, &body);
            assert_eq!(header(&headers, header::ALLOW), Some(LIST_ALLOW), "{method}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(PORTAL), "{method}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some(EXPOSE_HEADERS), "{method}");
            assert_eq!(header(&headers, header::VARY), Some("Origin"), "{method}");
        }
        let context = context_path(latest_context().revision);
        for method in [Method::POST, Method::OPTIONS, Method::DELETE] {
            let (status, headers, body) = send(FIXTURE, Cors::SameOrigin, method.clone(), &context, &[]).await;
            assert_error(status, StatusCode::METHOD_NOT_ALLOWED, &headers, &body);
            assert_eq!(header(&headers, header::ALLOW), Some(CONTEXT_ALLOW), "{method}");
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"), "{method}");
        }
    }

    /// An unpublished revision is a 404 for any method. Other paths are
    /// outside the API, so it says nothing about them.
    #[tokio::test]
    async fn unpublished_revisions_are_not_found() {
        let next = (CONTEXTS.len() + 1).to_string();
        for revision in ["0", "01", "+1", "r1", next.as_str()] {
            let path = format!("/v1/tms/contexts/institutions/{revision}");
            for method in [Method::GET, Method::HEAD, Method::POST] {
                let (status, headers, body) = send(FIXTURE, Cors::SameOrigin, method.clone(), &path, &[]).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
                if method != Method::HEAD {
                    assert_error(status, StatusCode::NOT_FOUND, &headers, &body);
                }
                assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"), "{method} {path}");
            }
        }
    }

    /// A request to send: its method, path and headers.
    type Case<'a> = (Method, &'a str, &'a [(header::HeaderName, &'a str)]);

    /// Every response, errors and 304s included, forbids content sniffing.
    #[tokio::test]
    async fn every_response_forbids_sniffing() {
        let context = context_path(latest_context().revision);
        let etag = fixture_etag();
        let cases: &[Case] = &[
            (Method::GET, LIST_PATH, &[]),
            (Method::GET, LIST_PATH, &[(header::IF_NONE_MATCH, etag.as_str())]),
            (Method::GET, LIST_PATH, &[(header::IF_MATCH, "\"x\"")]),
            (Method::OPTIONS, LIST_PATH, &[]),
            (Method::POST, LIST_PATH, &[]),
            (Method::GET, &context, &[]),
            (Method::GET, &context, &[(header::IF_NONE_MATCH, "*")]),
            (Method::POST, &context, &[]),
            (Method::GET, "/v1/tms/contexts/institutions/0", &[]),
        ];
        for (method, path, headers) in cases {
            let (status, out, _) = send(FIXTURE, Cors::Any, method.clone(), path, headers).await;
            assert_eq!(header(&out, header::X_CONTENT_TYPE_OPTIONS), Some(spec_header("NoSniff").as_str()), "{method} {path} {status}");
        }
    }

    /// Every revision ever published is still served, unchanged, with the
    /// headers that let it be cached forever; `If-None-Match: *` gets a 304.
    #[tokio::test]
    async fn every_context_revision_is_served() {
        for ctx in CONTEXTS {
            let path = context_path(ctx.revision);
            let (status, headers, body) = get(FIXTURE, &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/ld+json"));
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some(addendum_header("ContextCacheControl").as_str()));
            assert_eq!(body, ctx.body.as_bytes(), "{path}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(body["@context"].is_object(), "{path}");

            let (status, headers, body) = get(FIXTURE, &path, Some("*")).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{path}");
            assert!(body.is_empty());
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some(CONTEXT_CACHE_CONTROL));
            assert!(header(&headers, header::CONTENT_TYPE).is_none());
            let (status, _, _) = get(FIXTURE, &path, Some("W/\"x\"")).await;
            assert_eq!(status, StatusCode::OK, "contexts have no ETag to match");
        }
    }

    /// Published revisions are cached indefinitely, so they are frozen: each
    /// served revision has the SHA-256 the addendum publishes for it, and the
    /// revisions run r1, r2, ... with no gaps. To change the context, add
    /// api/institutions.context.r<n+1>.jsonld and a CONTEXTS row for it, add
    /// n+1 to the addendum's `revision` enum and its hash to
    /// `x-context-sha256`, and point the addendum's Link header and context
    /// example at it; these tests and scripts/check_spec.py catch a missed step.
    #[test]
    fn published_contexts_do_not_change() {
        let revisions: Vec<u32> = CONTEXTS.iter().map(|c| c.revision).collect();
        assert_eq!(revisions, (1..=CONTEXTS.len() as u32).collect::<Vec<_>>(), "revisions run r1, r2, ... with no gaps");
        let addendum = addendum();
        let published = addendum["paths"][CONTEXT_SPEC_PATH]["get"]["x-context-sha256"].as_object().unwrap();
        assert_eq!(published.len(), CONTEXTS.len(), "the addendum publishes one SHA-256 per served revision");
        for ctx in CONTEXTS {
            let sha = published[&ctx.revision.to_string()].as_str().unwrap();
            assert_eq!(sha256(ctx.body), sha, "context r{} changed: publish it as r{} instead", ctx.revision, CONTEXTS.len() + 1);
        }
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

    /// The addendum's `revision` enum lists every served revision and no
    /// other, and its Link example names the latest one at the served path.
    #[test]
    fn addendum_documents_every_revision_and_links_the_latest() {
        let addendum = addendum();
        let param = &addendum["components"]["parameters"]["Revision"];
        assert_eq!(param["name"], "revision");
        let documented: Vec<&str> = param["schema"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        let served: Vec<String> = CONTEXTS.iter().map(|c| c.revision.to_string()).collect();
        assert_eq!(documented, served);
        let example = addendum["components"]["headers"]["ContextLink"]["example"].as_str().unwrap();
        assert_eq!(example, link_to(&context_path(latest_context().revision)), "the addendum's Link example");
    }

    /// The registry's providers are exactly the spec's known values, under
    /// the same names.
    #[test]
    fn registry_providers_are_the_spec_known_values() {
        use crate::institutions::Provider;
        let spec = spec();
        let known = spec["components"]["schemas"]["Provider"]["x-known-values"].as_array().unwrap().clone();
        let ours: Vec<Value> = [Provider::Globus, Provider::TaccTapis].iter().map(|p| serde_json::to_value(p).unwrap()).collect();
        assert_eq!(ours, known);
        for value in &known {
            let parsed: Provider = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), *value);
        }
    }
}
