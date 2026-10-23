//! The public institutions API, as specified in api/openapi.yaml:
//!
//!   GET     /api/v1/institutions                 the approved institutions
//!   OPTIONS /api/v1/institutions                 CORS preflight for that list
//!   GET     /api/v1/institutions.context.r1.jsonld  the JSON-LD context for that list
//!
//! Kept apart from the demo's login flow so it depends only on the registry,
//! the deployment's base URL and its CORS setting.

use crate::institutions::{ListType, Registry};
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;
use std::sync::Arc;

pub const LIST_PATH: &str = "/api/v1/institutions";
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
];

pub fn context_path(revision: u32) -> String {
    format!("/api/v1/institutions.context.r{revision}.jsonld")
}

/// The revision the list's Link header names.
fn latest_context() -> &'static ContextRevision {
    CONTEXTS.last().expect("at least one context revision")
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
}

#[derive(Clone)]
struct ApiState {
    registry: Arc<Registry>,
    base_url: String,
    cors: Cors,
}

/// Both routes, ready to merge into an app with any state.
pub fn router<S: Clone + Send + Sync + 'static>(registry: Arc<Registry>, base_url: String, cors: Cors) -> Router<S> {
    let mut router = Router::new().route(LIST_PATH, get(list_institutions).options(preflight_institutions));
    for ctx in CONTEXTS {
        let body = ctx.body;
        router = router.route(&context_path(ctx.revision), get(move || async move { institutions_context(body) }));
    }
    router.with_state(ApiState { registry, base_url, cors })
}

/// Whether an `If-None-Match` header matches `etag`. Uses the weak comparison
/// RFC 9110 prescribes for it, so `W/"v"` matches `"v"`.
fn none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim())
        .any(|t| t == "*" || t.trim_start_matches("W/") == etag)
}

async fn list_institutions(State(api): State<ApiState>, headers: HeaderMap) -> Response {
    // `version` is checked to be ETag-safe when the registry loads.
    let etag = api.registry.version().map(|v| format!("\"{v}\""));
    let mut out = HeaderMap::new();
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if api.cors.allow_origin(&headers, &mut out) {
        out.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("ETag, Link"));
    }
    if let Some(tag) = &etag {
        out.insert(header::ETAG, HeaderValue::from_str(tag).expect("version is ETag-safe"));
        if none_match(&headers, tag) {
            return (StatusCode::NOT_MODIFIED, out).into_response();
        }
    }
    // `type` makes the list a schema:ItemList under the JSON-LD context, and
    // the Link header tells JSON-LD processors where that context is.
    let link = format!(
        r#"<{}{}>; rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json""#,
        api.base_url,
        context_path(latest_context().revision)
    );
    out.insert(header::LINK, HeaderValue::from_str(&link).expect("BASE_URL is a valid header value"));
    let mut body = json!({
        "type": ListType::ItemList,
        "status": "success",
        "message": "Approved institutions.",
        "result": api.registry.approved(),
    });
    if let Some(v) = api.registry.version() {
        body["version"] = v.into();
    }
    (out, Json(body)).into_response()
}

/// The preflight before a cross-origin GET that sets If-None-Match. The
/// browser checks the requested method and headers against these.
async fn preflight_institutions(State(api): State<ApiState>, headers: HeaderMap) -> Response {
    let mut out = HeaderMap::new();
    out.insert(header::ALLOW, HeaderValue::from_static("GET, HEAD, OPTIONS"));
    if api.cors.allow_origin(&headers, &mut out) {
        out.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET"));
        out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("If-None-Match"));
        out.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("7200"));
    }
    (StatusCode::NO_CONTENT, out).into_response()
}

/// Open to any origin, since JSON-LD processors in other sites' pages fetch
/// it too. Its URL never changes content, so it can be cached indefinitely.
fn institutions_context(body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/ld+json"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
}

/// Contract tests: the responses are checked against api/openapi.yaml itself,
/// and the spec against the JSON-LD context, so the three cannot drift apart.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request};
    use serde_json::Value;
    use tower::ServiceExt;

    const SPEC: &str = include_str!("../api/openapi.yaml");
    const MOCK: &str = include_str!("../mock/approved_institutions.json");
    const BASE: &str = "https://tms.example.org";
    /// SHA-256 of each published api/institutions.context.r<n>.jsonld, line
    /// endings normalised. Add a row with each new revision; never edit one.
    const CONTEXT_SHA256: &[(u32, &str)] = &[(1, "0c053922807aef6eb421cffc9fe70b6dc266886907109972530d2d44a6851f93")];

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

    async fn get(registry: &str, path: &str, if_none_match: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let headers: Vec<_> = if_none_match.map(|tag| (header::IF_NONE_MATCH, tag)).into_iter().collect();
        request(registry, Cors::SameOrigin, path, &headers).await
    }

    async fn request(registry: &str, cors: Cors, path: &str, headers: &[(header::HeaderName, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
        send(registry, cors, Method::GET, path, headers).await
    }

    async fn send(registry: &str, cors: Cors, method: Method, path: &str, headers: &[(header::HeaderName, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
        let app: Router = router(Arc::new(Registry::from_json(registry).unwrap()), BASE.into(), cors);
        let mut req = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            req = req.header(name, *value);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let (parts, body) = resp.into_parts();
        (parts.status, parts.headers, to_bytes(body, usize::MAX).await.unwrap().to_vec())
    }

    fn header<'a>(headers: &'a HeaderMap, name: header::HeaderName) -> Option<&'a str> {
        headers.get(name).map(|v| v.to_str().unwrap())
    }

    #[tokio::test]
    async fn list_conforms_to_the_spec() {
        let (status, headers, body) = get(MOCK, LIST_PATH, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/json"));
        assert_eq!(header(&headers, header::LINK), Some(link_to(&context_path(latest_context().revision)).as_str()));
        assert_eq!(header(&headers, header::CACHE_CONTROL), Some("no-cache"));
        assert_eq!(header(&headers, header::ETAG), Some("\"mock-0.1.0\""));
        assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        assert!(header(&headers, header::VARY).is_none());

        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_valid("InstitutionsResponse", &body);
        assert_eq!(body["version"], "mock-0.1.0");
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
        assert_eq!(header(&headers, header::ALLOW), Some("GET, HEAD, OPTIONS"));
        assert!(header(&headers, header::VARY).is_none());
        assert!(!headers.keys().any(|k| k.as_str().starts_with("access-control-")));
    }

    #[tokio::test]
    async fn listed_origins_may_read_the_list() {
        let cors = Cors::Origins(vec![PORTAL.into(), "http://localhost:3000".into()]);
        for extra in [None, Some((header::IF_NONE_MATCH, "\"mock-0.1.0\""))] {
            let headers: Vec<_> = [(header::ORIGIN, PORTAL)].into_iter().chain(extra).collect();
            let (status, headers, _) = request(MOCK, cors.clone(), LIST_PATH, &headers).await;
            assert!(matches!(status, StatusCode::OK | StatusCode::NOT_MODIFIED));
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

    #[tokio::test]
    async fn any_origin_may_read_the_list() {
        let (_, headers, _) = request(MOCK, Cors::Any, LIST_PATH, &[(header::ORIGIN, PORTAL)]).await;
        assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
        assert_eq!(header(&headers, header::ACCESS_CONTROL_EXPOSE_HEADERS), Some("ETag, Link"));
        assert!(header(&headers, header::VARY).is_none());
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

    #[tokio::test]
    async fn list_holds_only_approved_entries_in_registry_order() {
        let (_, _, body) = get(MOCK, LIST_PATH, None).await;
        let body: Value = serde_json::from_slice(&body).unwrap();
        let ids: Vec<&str> = body["result"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
        let expected: Vec<String> = Registry::from_json(MOCK).unwrap().approved().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, expected);
        for hidden in ["illinois", "usc", "mit", "example"] {
            assert!(!ids.contains(&hidden), "{hidden}");
        }
    }

    #[tokio::test]
    async fn matching_etag_gets_not_modified() {
        for tag in ["\"mock-0.1.0\"", "W/\"mock-0.1.0\"", "\"old\", \"mock-0.1.0\"", "*"] {
            let (status, headers, body) = get(MOCK, LIST_PATH, Some(tag)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{tag}");
            assert_eq!(header(&headers, header::ETAG), Some("\"mock-0.1.0\""));
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some("no-cache"));
            assert!(body.is_empty());
        }
        let (status, _, _) = get(MOCK, LIST_PATH, Some("\"old\"")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unversioned_list_has_no_version_or_etag() {
        let registry = r#"{"institutions":[{"id":"a","name":"A","provider":"globus","redirect_idp_domain":null,"idp_logout_url":null,"approved":true}]}"#;
        let (status, headers, body) = get(registry, LIST_PATH, Some("*")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(header(&headers, header::ETAG).is_none());
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body.get("version").is_none());
        assert_valid("InstitutionsResponse", &body);
    }

    /// Every revision ever published is still served, unchanged, with the
    /// headers that let it be cached forever.
    #[tokio::test]
    async fn every_context_revision_is_served() {
        for ctx in CONTEXTS {
            let path = context_path(ctx.revision);
            let (status, headers, body) = get(MOCK, &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/ld+json"));
            assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
            assert_eq!(header(&headers, header::CACHE_CONTROL), Some("public, max-age=31536000, immutable"));
            assert_eq!(body, ctx.body.as_bytes(), "{path}");
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(body["@context"].is_object(), "{path}");
        }
    }

    /// Published revisions are cached indefinitely, so they are frozen. To
    /// change the context, add api/institutions.context.r<n+1>.jsonld, a
    /// CONTEXTS row and a CONTEXT_SHA256 row for it, and point the spec's
    /// Link header text and example at it; the tests below catch a missed step.
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

    /// The files in api/ are exactly the served revisions, each named for its
    /// revision, so a file cannot be added without serving it or served
    /// under the wrong number.
    #[test]
    fn context_files_match_the_revisions() {
        let api = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("api");
        let mut files: Vec<String> = std::fs::read_dir(&api)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|f| f.starts_with("institutions.context.") && !f.contains(':'))
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

    /// The spec documents every served revision and no other, and everywhere
    /// it names a revision outside those path keys (the Link header's text
    /// and example) it names the latest one.
    #[test]
    fn spec_documents_every_revision_and_links_the_latest() {
        let spec = spec();
        let mut documented: Vec<String> =
            spec["paths"].as_object().unwrap().keys().filter(|p| p.contains("institutions.context.")).cloned().collect();
        documented.sort();
        let mut served: Vec<String> = CONTEXTS.iter().map(|c| context_path(c.revision)).collect();
        served.sort();
        assert_eq!(documented, served);

        let latest = latest_context().revision;
        let marker = "institutions.context.r";
        let mut mentions = 0;
        for line in SPEC.lines().filter(|l| !(l.trim_start().starts_with("/api/") && l.trim_end().ends_with(':'))) {
            for (at, _) in line.match_indices(marker) {
                let digits: String = line[at + marker.len()..].chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() {
                    assert_eq!(digits.parse::<u32>().unwrap(), latest, "spec names r{digits}, not the latest r{latest}: {line}");
                    mentions += 1;
                }
            }
        }
        assert!(mentions > 0, "the spec names the latest revision");
        let example = spec["paths"][LIST_PATH]["get"]["responses"]["200"]["headers"]["Link"]["example"].as_str().unwrap();
        assert_eq!(example, link_to(&context_path(latest)), "the spec's Link example");
    }

    #[test]
    fn spec_examples_conform_to_the_spec() {
        let spec = spec();
        let examples = &spec["paths"][LIST_PATH]["get"]["responses"]["200"]["content"]["application/json"]["examples"];
        for (name, example) in examples.as_object().unwrap() {
            assert_valid("InstitutionsResponse", &example["value"]);
            assert!(!name.contains("mock"), "example {name:?} names the demo");
        }
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
