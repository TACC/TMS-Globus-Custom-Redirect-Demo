//! idp_redirect_demo — pick an approved institution, get sent straight to its
//! IdP via Globus Auth's `redirect_idp_domain`, then verify on callback that the
//! returned identity really belongs to that institution.
//!
//! Routes (shaped like tms_portal's routes/login.rs):
//!   GET /                       landing page (a single Login button to /demo)
//!   GET /demo                   demo page
//!   GET /login/institutions     approved institutions (from the mocked JSON)
//!   GET /login?institution=ID   build authorize URL + redirect (add &dry_run=true to just see it)
//!   GET /login/callback         code -> userinfo -> institution check
//!   GET /login/status           whether this browser has a Globus login session with the app
//!   POST /logout                revoke the Globus token, clear cookies, end the Globus session, back to /
//!   GET /mock-globus/authorize  stand-in for auth.globus.org when GLOBUS_MOCK=true
//!   GET /mock-globus/approve    mock "user authenticated" step
//!   GET /mock-globus/idp-callback  mock Globus acting as the test IdP's client
//!   /idp/...                    test OIDC provider (see test_idp.rs)

mod globus;
mod institutions;
mod test_idp;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use globus::GlobusConfig;
use institutions::{identity_matches, Institution, Registry};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use test_idp::TestIdp;

const STATE_COOKIE: &str = "idp_demo_state";
const SESSION_COOKIE: &str = "idp_demo_session";

#[derive(Clone)]
struct AppState {
    globus: Arc<GlobusConfig>,
    registry: Arc<Registry>,
    idp: Arc<TestIdp>,
    base_url: String,
    /// Session id -> Globus access token (None in mock mode), held server-side
    /// so logout can revoke it.
    sessions: Arc<Mutex<HashMap<String, Option<String>>>>,
}

#[tokio::main]
async fn main() {
    let base_url = env_or("BASE_URL", "http://localhost:8080");
    let bind = env_or("BIND_ADDR", "127.0.0.1:8080");
    let inst_file = env_or("INSTITUTIONS_FILE", "mock/approved_institutions.json");

    // Real Globus Auth unless the offline mock is asked for explicitly, so a
    // missing credential fails loudly instead of quietly serving the mock.
    let client = if env_or("GLOBUS_MOCK", "").eq_ignore_ascii_case("true") {
        None
    } else {
        match (std::env::var("GLOBUS_CLIENT_ID"), std::env::var("GLOBUS_CLIENT_SECRET")) {
            (Ok(id), Ok(secret)) if !id.trim().is_empty() && !secret.trim().is_empty() => Some((id, secret)),
            _ => {
                eprintln!(
                    "GLOBUS_CLIENT_ID and GLOBUS_CLIENT_SECRET must be set for real Globus Auth \
                     (or set GLOBUS_MOCK=true to run against the built-in mock)."
                );
                std::process::exit(1);
            }
        }
    };
    let authorize_url = if client.is_some() {
        globus::GLOBUS_AUTHORIZE_URL.to_string()
    } else {
        format!("{base_url}/mock-globus/authorize")
    };
    let globus = GlobusConfig {
        client,
        authorize_url,
        callback_url: format!("{base_url}/login/callback"),
    };

    let registry = Registry::load(&inst_file).unwrap_or_else(|e| panic!("{e}"));
    println!(
        "Loaded {} approved institution(s) from {inst_file}",
        registry.approved().len()
    );
    println!(
        "Mode: {}",
        if globus.is_mock() { "MOCK Globus (GLOBUS_MOCK=true)" } else { "real Globus Auth" }
    );

    let idp = Arc::new(build_test_idp(&base_url));
    println!(
        "Test IdP: issuer {} (domain {}, client_id {})",
        idp.issuer, idp.domain, idp.client_id
    );

    let state = AppState {
        globus: Arc::new(globus),
        registry: Arc::new(registry),
        idp: idp.clone(),
        base_url: base_url.clone(),
        sessions: Arc::default(),
    };
    let app = Router::new()
        .route("/", get(landing))
        .route("/demo", get(index))
        .route("/login/institutions", get(list_institutions))
        .route("/login", get(login))
        .route("/login/callback", get(callback))
        .route("/login/status", get(login_status))
        .route("/logout", post(logout))
        .route("/mock-globus/authorize", get(mock_authorize))
        .route("/mock-globus/approve", get(mock_approve))
        .route("/mock-globus/idp-callback", get(mock_idp_callback))
        .merge(test_idp::router(idp))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("Listening on {base_url}");
    axum::serve(listener, app).await.unwrap();
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn build_test_idp(base_url: &str) -> TestIdp {
    let key_file = env_or("IDP_SIGNING_KEY", "idp_signing_key.pem");
    let key = test_idp::load_or_create_key(&key_file).unwrap_or_else(|e| panic!("{e}"));
    let client_secret = std::env::var("IDP_CLIENT_SECRET").unwrap_or_else(|_| {
        let s = uuid::Uuid::new_v4().simple().to_string();
        println!("IDP_CLIENT_SECRET not set; generated one for this run: {s}");
        s
    });
    let redirect_uris = env_or("IDP_REDIRECT_URIS", test_idp::GLOBUS_IDP_CALLBACK)
        .split(',')
        .map(|u| u.trim().to_string())
        .chain([format!("{base_url}/mock-globus/idp-callback")])
        .collect();
    TestIdp::new(
        format!("{base_url}/idp"),
        env_or("IDP_DOMAIN", "tacc.utexas.edu"),
        env_or("IDP_CLIENT_ID", "globus-auth-client"),
        client_secret,
        redirect_uris,
        key,
    )
}

fn tapis_style(status: StatusCode, message: &str, result: serde_json::Value) -> Response {
    let s = if status.is_success() { "success" } else { "error" };
    (status, Json(json!({ "status": s, "message": message, "result": result }))).into_response()
}

// ---- routes -------------------------------------------------------------

async fn landing() -> Html<&'static str> {
    Html(include_str!("../static/landing.html"))
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn list_institutions(State(app): State<AppState>) -> Response {
    let list = app.registry.approved();
    tapis_style(StatusCode::OK, "Approved institutions.", json!(list))
}

#[derive(Deserialize)]
struct LoginParams {
    institution: String,
    #[serde(default)]
    dry_run: bool,
}

async fn login(
    State(app): State<AppState>,
    jar: CookieJar,
    Query(p): Query<LoginParams>,
) -> Response {
    let Some(inst) = app.registry.find_approved(&p.institution) else {
        return tapis_style(
            StatusCode::FORBIDDEN,
            &format!("Institution '{}' is not on the approved list.", p.institution),
            json!(null),
        );
    };

    // State binds this browser's login attempt to the institution it picked.
    let nonce = uuid::Uuid::new_v4().to_string();
    let url = globus::build_authorize_url(&app.globus, &nonce, &inst.redirect_idp_domain);

    if p.dry_run {
        return tapis_style(
            StatusCode::OK,
            "Authorize URL (not redirecting: dry_run=true).",
            json!({
                "institution": inst,
                "mock_mode": app.globus.is_mock(),
                "authorize_url": url.as_str(),
            }),
        );
    }

    let cookie = Cookie::build((STATE_COOKIE, format!("{nonce}|{}", inst.id)))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax); // Lax so the top-level redirect back from Globus carries it
    (jar.add(cookie), Redirect::temporary(url.as_str())).into_response()
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(
    State(app): State<AppState>,
    jar: CookieJar,
    Query(p): Query<CallbackParams>,
) -> Response {
    let jar_cleared = jar.clone().remove(Cookie::build(STATE_COOKIE).path("/"));

    if let Some(err) = p.error {
        return (jar_cleared, page_error(StatusCode::BAD_REQUEST, &format!("Globus returned error: {err}")))
            .into_response();
    }
    let (Some(code), Some(state)) = (p.code, p.state) else {
        return page_error(StatusCode::BAD_REQUEST, "Missing code or state.").into_response();
    };
    let Some((nonce, inst_id)) = jar.get(STATE_COOKIE).and_then(|c| {
        c.value().split_once('|').map(|(a, b)| (a.to_string(), b.to_string()))
    }) else {
        return page_error(StatusCode::UNAUTHORIZED, "No state cookie found.").into_response();
    };
    if nonce != state {
        return (jar_cleared, page_error(StatusCode::UNAUTHORIZED, "State mismatch.")).into_response();
    }
    let Some(inst) = app.registry.find_approved(&inst_id).cloned() else {
        return (jar_cleared, page_error(StatusCode::FORBIDDEN, "Institution is no longer approved."))
            .into_response();
    };

    let user = if app.globus.is_mock() {
        globus::mock_user_from_code(&code).map(|u| (u, None))
    } else {
        globus::fetch_user(&app.globus, &code).await.map(|(u, t)| (u, Some(t)))
    };
    let (user, token) = match user {
        Ok(u) => u,
        Err(e) => return (jar_cleared, page_error(StatusCode::BAD_GATEWAY, &e)).into_response(),
    };

    // Keep the token even when the identity is rejected: it is still a live
    // Globus credential, and logout should revoke it.
    let session = uuid::Uuid::new_v4().to_string();
    app.sessions.lock().unwrap().insert(session.clone(), token);
    let jar = jar_cleared.add(
        Cookie::build((SESSION_COOKIE, session)).path("/").http_only(true).same_site(SameSite::Lax),
    );

    let ok = identity_matches(&inst, &user.preferred_username);
    let status = if ok { StatusCode::OK } else { StatusCode::FORBIDDEN };
    (jar, (status, Html(page_result(&inst, &user, ok)))).into_response()
}

/// Whether this browser completed a Globus login through the app and has not
/// logged out since. The app cannot see the auth.globus.org session itself.
async fn login_status(State(app): State<AppState>, jar: CookieJar) -> Response {
    let logged_in = jar
        .get(SESSION_COOKIE)
        .is_some_and(|c| app.sessions.lock().unwrap().contains_key(c.value()));
    tapis_style(StatusCode::OK, "Login status.", json!({ "logged_in": logged_in }))
}

async fn logout(State(app): State<AppState>, jar: CookieJar) -> Response {
    let token = jar
        .get(SESSION_COOKIE)
        .and_then(|c| app.sessions.lock().unwrap().remove(c.value()))
        .flatten();
    if let Some(token) = token {
        if let Err(e) = globus::revoke_token(&app.globus, &token).await {
            eprintln!("[logout] {e}");
        }
    }
    let jar = jar
        .remove(Cookie::build(SESSION_COOKIE).path("/"))
        .remove(Cookie::build(STATE_COOKIE).path("/"));

    // The browser's auth.globus.org session is what lets a later login skip
    // the IdP redirect, so end it there too. Globus's sign-out page does not
    // redirect by itself; it links back to the landing page.
    if app.globus.is_mock() {
        return (jar, Redirect::to("/")).into_response();
    }
    let url = globus::logout_url(&app.globus, &format!("{}/", app.base_url));
    (jar, Redirect::to(url.as_str())).into_response()
}

// ---- mock Globus --------------------------------------------------------

#[derive(Deserialize)]
struct MockAuthorizeParams {
    redirect_uri: String,
    state: String,
    redirect_idp_domain: Option<String>,
    client_id: Option<String>,
    scope: Option<String>,
}

async fn mock_authorize(State(app): State<AppState>, Query(p): Query<MockAuthorizeParams>) -> Response {
    let domain = p.redirect_idp_domain.clone().unwrap_or_default();

    // What Globus does once an IdP is registered for the domain: send the user
    // to that IdP's authorization endpoint (the custom login page).
    if domain.eq_ignore_ascii_case(&app.idp.domain) {
        let nonce = uuid::Uuid::new_v4().to_string();
        let relay = json!({ "redirect_uri": p.redirect_uri, "state": p.state, "nonce": nonce });
        let url = url::Url::parse_with_params(
            &format!("{}/authorize", app.idp.issuer),
            [
                ("response_type", "code"),
                ("client_id", app.idp.client_id.as_str()),
                ("redirect_uri", &format!("{}/mock-globus/idp-callback", app.base_url)),
                ("scope", globus::SCOPES),
                ("state", &URL_SAFE_NO_PAD.encode(relay.to_string())),
                ("nonce", &nonce),
            ],
        )
        .expect("issuer is a valid URL");
        return Redirect::to(url.as_str()).into_response();
    }

    let row = |k: &str, v: &str| format!("<tr><th>{}</th><td><code>{}</code></td></tr>", esc(k), esc(v));
    let body = format!(
        r#"<h1>Mock Globus Auth</h1>
<p>This page stands in for <code>auth.globus.org/v2/oauth2/authorize</code>. It received:</p>
<table>{}{}{}{}</table>
<p class="note">With real Globus, an unauthenticated user would skip the Globus login page and land
directly on the <b>{}</b> identity provider.</p>
<h2>Pretend to sign in at {}</h2>
<form action="/mock-globus/approve" method="get">
  <input type="hidden" name="redirect_uri" value="{}">
  <input type="hidden" name="state" value="{}">
  <label>Identity returned to the app
    <input name="username" value="jdoe@{}" size="32"></label>
  <button>Continue</button>
</form>
<p class="note">Try changing the identity to <code>jdoe@globusid.org</code> to see the callback reject it.</p>"#,
        row("redirect_idp_domain", &domain),
        row("client_id", p.client_id.as_deref().unwrap_or("")),
        row("scope", p.scope.as_deref().unwrap_or("")),
        row("redirect_uri", &p.redirect_uri),
        esc(&domain),
        esc(&domain),
        esc(&p.redirect_uri),
        esc(&p.state),
        esc(&domain),
    );
    Html(shell("Mock Globus Auth", &body)).into_response()
}

#[derive(Deserialize)]
struct MockIdpCallbackParams {
    code: String,
    state: String,
}

/// Mock Globus receiving the test IdP's response: redeem the code exactly as a
/// real client would, then hand the verified identity back to the app.
async fn mock_idp_callback(State(app): State<AppState>, Query(p): Query<MockIdpCallbackParams>) -> Response {
    let relay: Option<serde_json::Value> = URL_SAFE_NO_PAD
        .decode(&p.state)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok());
    let Some(relay) = relay else {
        return page_error(StatusCode::BAD_REQUEST, "Bad relay state").into_response();
    };
    let field = |k: &str| relay[k].as_str().unwrap_or_default().to_string();

    let claims = test_idp::relying_party_login(
        &app.idp.issuer,
        &app.idp.client_id,
        &app.idp.client_secret,
        &format!("{}/mock-globus/idp-callback", app.base_url),
        &p.code,
        Some(&field("nonce")),
    )
    .await;
    let username = match claims {
        Ok(c) => c["preferred_username"].as_str().unwrap_or_default().to_string(),
        Err(e) => {
            return page_error(StatusCode::BAD_GATEWAY, &format!("Test IdP failed the client checks: {e}"))
                .into_response()
        }
    };

    let Ok(mut url) = url::Url::parse(&field("redirect_uri")) else {
        return page_error(StatusCode::BAD_REQUEST, "Bad redirect_uri").into_response();
    };
    url.query_pairs_mut()
        .append_pair("code", &globus::mock_code_for(&username))
        .append_pair("state", &field("state"));
    Redirect::to(url.as_str()).into_response()
}

#[derive(Deserialize)]
struct MockApproveParams {
    redirect_uri: String,
    state: String,
    username: String,
}

async fn mock_approve(Query(p): Query<MockApproveParams>) -> Response {
    let Ok(mut url) = url::Url::parse(&p.redirect_uri) else {
        return page_error(StatusCode::BAD_REQUEST, "Bad redirect_uri").into_response();
    };
    url.query_pairs_mut()
        .append_pair("code", &globus::mock_code_for(&p.username))
        .append_pair("state", &p.state);
    Redirect::to(url.as_str()).into_response()
}

// ---- tiny HTML helpers --------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn shell(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{}</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<style>
 body{{font:16px/1.5 system-ui,sans-serif;max-width:44rem;margin:2rem auto;padding:0 1rem;color:#1d2330}}
 table{{border-collapse:collapse;margin:1rem 0}} th,td{{text-align:left;padding:.3rem .8rem;border-bottom:1px solid #ddd}}
 code{{background:#f2f3f5;padding:.1rem .3rem;border-radius:3px}} .note{{color:#5b6475}}
 .ok{{color:#13703a}} .bad{{color:#a12121}} button{{font:inherit;padding:.4rem 1rem}} input{{font:inherit}}
</style></head><body>{}<p><a href="/demo">&larr; back to start</a></p></body></html>"#,
        esc(title),
        body
    )
}

const LOGOUT_FORM: &str =
    r#"<form action="/logout" method="post"><button>Log out of Globus</button></form>"#;

fn page_error(status: StatusCode, msg: &str) -> (StatusCode, Html<String>) {
    (status, Html(shell("Login error", &format!("<h1 class=\"bad\">Login failed</h1><p>{}</p>", esc(msg)))))
}

fn page_result(inst: &Institution, u: &globus::UserInfo, ok: bool) -> String {
    let verdict = if ok {
        format!("<h1 class=\"ok\">&#10003; Welcome</h1><p>Your identity belongs to <b>{}</b>, an approved institution.</p>", esc(&inst.name))
    } else {
        format!(
            "<h1 class=\"bad\">&#10007; Not allowed</h1><p>You chose <b>{}</b> but came back as an identity outside <code>@{}</code>. \
             <code>redirect_idp_domain</code> only steers the login page, so the server enforces the match here.</p>",
            esc(&inst.name),
            esc(&inst.redirect_idp_domain)
        )
    };
    let row = |k: &str, v: &Option<String>| {
        format!("<tr><th>{}</th><td>{}</td></tr>", k, esc(v.as_deref().unwrap_or("—")))
    };
    let body = format!(
        "{verdict}<table>{}{}{}{}{}{}</table>",
        row("preferred_username", &Some(u.preferred_username.clone())),
        row("name", &u.name),
        row("email", &u.email),
        row("organization", &u.organization),
        row("identity_provider_display_name", &u.identity_provider_display_name),
        row("sub", &Some(u.sub.clone())),
    );
    shell("Login result", &format!("{body}{LOGOUT_FORM}"))
}
