//! The public institutions API, as specified in api/openapi.yaml:
//!
//!   GET /login/institutions               the approved institutions
//!   GET /api/institutions.context.jsonld  the JSON-LD context for that list
//!
//! Kept apart from the demo's login flow so it depends only on the registry
//! and the deployment's base URL.

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

pub const LIST_PATH: &str = "/login/institutions";
pub const CONTEXT_PATH: &str = "/api/institutions.context.jsonld";
const CONTEXT: &str = include_str!("../api/institutions.context.jsonld");

#[derive(Clone)]
struct ApiState {
    registry: Arc<Registry>,
    base_url: String,
}

/// Both routes, ready to merge into an app with any state.
pub fn router<S: Clone + Send + Sync + 'static>(registry: Arc<Registry>, base_url: String) -> Router<S> {
    Router::new()
        .route(LIST_PATH, get(list_institutions))
        .route(CONTEXT_PATH, get(institutions_context))
        .with_state(ApiState { registry, base_url })
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
    if let Some(tag) = &etag {
        out.insert(header::ETAG, HeaderValue::from_str(tag).expect("version is ETag-safe"));
        if none_match(&headers, tag) {
            return (StatusCode::NOT_MODIFIED, out).into_response();
        }
    }
    // `type` makes the list a schema:ItemList under the JSON-LD context, and
    // the Link header tells JSON-LD processors where that context is.
    let link = format!(
        r#"<{}{CONTEXT_PATH}>; rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json""#,
        api.base_url
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

/// Open to any origin, since JSON-LD processors in other sites' pages fetch
/// it too. It only changes with the API, so it can be cached for a day.
async fn institutions_context() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/ld+json"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        CONTEXT,
    )
}

/// Contract tests: the responses are checked against api/openapi.yaml itself,
/// and the spec against the JSON-LD context, so the three cannot drift apart.
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;

    const SPEC: &str = include_str!("../api/openapi.yaml");
    const MOCK: &str = include_str!("../mock/approved_institutions.json");
    const BASE: &str = "https://tms.example.org";

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
        let app: Router = router(Arc::new(Registry::from_json(registry).unwrap()), BASE.into());
        let mut req = Request::get(path);
        if let Some(tag) = if_none_match {
            req = req.header(header::IF_NONE_MATCH, tag);
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
        assert_eq!(
            header(&headers, header::LINK),
            Some(r#"<https://tms.example.org/api/institutions.context.jsonld>; rel="http://www.w3.org/ns/json-ld#context"; type="application/ld+json""#)
        );
        assert_eq!(header(&headers, header::CACHE_CONTROL), Some("no-cache"));
        assert_eq!(header(&headers, header::ETAG), Some("\"mock-0.1.0\""));
        assert!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());

        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_valid("InstitutionsResponse", &body);
        assert_eq!(body["version"], "mock-0.1.0");
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

    #[tokio::test]
    async fn context_is_public_and_cacheable() {
        let (status, headers, body) = get(MOCK, CONTEXT_PATH, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, header::CONTENT_TYPE), Some("application/ld+json"));
        assert_eq!(header(&headers, header::ACCESS_CONTROL_ALLOW_ORIGIN), Some("*"));
        assert_eq!(header(&headers, header::CACHE_CONTROL), Some("public, max-age=86400"));
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body["@context"].is_object());
    }

    #[test]
    fn spec_examples_conform_to_the_spec() {
        let spec = spec();
        let examples = &spec["paths"]["/login/institutions"]["get"]["responses"]["200"]["content"]["application/json"]["examples"];
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
            ("redirect_idp_domain", json!("..")),
            ("redirect_idp_domain", json!("UChicago.edu")),
            ("idp_logout_url", json!("http://idp.a.edu/logout")),
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
    }

    /// Every field the spec maps with `x-schema-org` must map to the same term
    /// in the JSON-LD context, and the envelope-only fields must stay out.
    #[test]
    fn spec_and_context_agree() {
        let spec = spec();
        let ctx: Value = serde_json::from_str(CONTEXT).unwrap();
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
