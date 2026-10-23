//! idp_redirect_demo — pick an institution, get sent straight to its IdP via
//! Globus Auth's `redirect_idp_domain`, then on callback check that it is on
//! the approved list and that the returned identity really belongs to it. Two extra entries show
//! Globus's default login (no `redirect_idp_domain`) and a direct Tapis
//! OAuth2 login (tms_portal's TaccTapis provider).
//!
//! Routes (shaped like tms_portal's routes/login.rs):
//!   GET /                       landing page (Science Gateway template; Log in goes to /demo)
//!   GET /static/...             the landing page's stylesheet, images and favicon
//!   GET /demo                   demo page
//!   GET /login/institutions     the approved institutions (from the mocked registry), with a Link
//!                               header naming the JSON-LD context (see institutions_api.rs)
//!   GET /api/institutions.context.jsonld  that JSON-LD context
//!   GET /login?institution=ID   build authorize URL + redirect (add &dry_run=true to just see it);
//!                               unapproved institutions get the "Filtered" page instead
//!   GET /login/callback         code -> userinfo -> approved-list and institution checks
//!   GET /login/status           whether this browser has a login session with the app
//!   POST /logout                revoke the token, clear cookies, end the Globus session, back to /
//!                               (?local=true: revoke and clear only, 204 instead of a redirect)
//!   GET /logout/idp?institution=ID  after Globus sign-out, on to that institution's IdP sign-out
//!   GET <TAPIS_CALLBACK_URL>    relays Tapis's response to /login/callback (when set)
//!   GET /mock-globus/authorize  stand-in for auth.globus.org when GLOBUS_MOCK=true
//!   GET /mock-globus/approve    mock "user authenticated" step
//!   GET /mock-globus/idp-callback  mock Globus acting as the test IdP's client
//!   /idp/...                    test OIDC provider (see test_idp.rs)

mod globus;
mod institutions;
mod institutions_api;
mod settings;
mod tapis;
mod test_idp;

use axum::{
    extract::{rejection::QueryRejection, Query, RawQuery, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use globus::GlobusConfig;
use institutions::{identity_matches, is_valid_id, Institution, Provider, Registry};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use settings::Settings;
use tapis::TapisConfig;
use test_idp::TestIdp;
use tower_http::services::ServeDir;

const STATE_COOKIE: &str = "idp_demo_state";
const SESSION_COOKIE: &str = "idp_demo_session";

#[derive(Clone)]
struct AppState {
    globus: Arc<GlobusConfig>,
    tapis: Arc<TapisConfig>,
    registry: Arc<Registry>,
    idp: Arc<TestIdp>,
    base_url: String,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

/// A completed login, keyed by the session cookie. The token is held
/// server-side so logout can revoke it.
struct Session {
    /// The entry logged in with, so the page can turn its button into "Log out".
    institution: String,
    provider: Provider,
    /// Who signed in, for the landing page's "Logged in as".
    user: globus::UserInfo,
    /// Signed in, but turned away (not approved, or the wrong identity). Kept
    /// so logout can still revoke the token, but not reported as logged in.
    rejected: bool,
    /// None in mock mode.
    token: Option<String>,
}

#[tokio::main]
async fn main() {
    let (env, env_file) = Settings::load().unwrap_or_else(|e| panic!("{e}"));
    if env.len() == 0 {
        println!("Settings: no {env_file} (copy dev.env.example); using the environment and defaults");
    } else {
        println!("Settings: {} value(s) from {env_file} (environment variables override)", env.len());
    }

    let base_url = env.or("BASE_URL", "http://localhost:8080");
    let bind = env.or("BIND_ADDR", "127.0.0.1:8080");
    let inst_file = env.or("INSTITUTIONS_FILE", "mock/approved_institutions.json");

    // Real Globus Auth unless the offline mock is asked for explicitly, so a
    // missing credential fails loudly instead of quietly serving the mock.
    let client = if env.or("GLOBUS_MOCK", "").eq_ignore_ascii_case("true") {
        None
    } else {
        match (env.get("GLOBUS_CLIENT_ID"), env.get("GLOBUS_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => Some((id, secret)),
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

    // TaccTapis is optional: without its credentials only that entry fails.
    let tapis = TapisConfig {
        client: env.get("TAPIS_CLIENT_ID").zip(env.get("TAPIS_CLIENT_SECRET")),
        base_url: env.or("TAPIS_BASE_URL", tapis::TAPIS_BASE_URL),
        // Tapis only accepts the callback URL registered for its client.
        callback_url: env.or("TAPIS_CALLBACK_URL", &globus.callback_url),
    };
    let tapis_relay = (tapis.callback_url != globus.callback_url)
        .then(|| url::Url::parse(&tapis.callback_url).expect("TAPIS_CALLBACK_URL must be a valid URL"));

    let registry = Registry::load(&inst_file).unwrap_or_else(|e| panic!("{e}"));
    println!(
        "Loaded {} approved institution(s) from {inst_file}",
        registry.approved().len()
    );
    println!(
        "Mode: {}",
        if globus.is_mock() { "MOCK Globus (GLOBUS_MOCK=true)" } else { "real Globus Auth" }
    );
    if !globus.is_mock() {
        println!(
            "TaccTapis: {}",
            if tapis.client.is_some() { &tapis.base_url } else { "not configured (set TAPIS_CLIENT_ID/SECRET)" }
        );
    }

    let idp = Arc::new(build_test_idp(&env, &base_url));
    println!(
        "Test IdP: issuer {} (domain {}, client_id {})",
        idp.issuer, idp.domain, idp.client_id
    );

    let registry = Arc::new(registry);
    let state = AppState {
        globus: Arc::new(globus),
        tapis: Arc::new(tapis),
        registry: registry.clone(),
        idp: idp.clone(),
        base_url: base_url.clone(),
        sessions: Arc::default(),
    };
    // When Tapis's registered callback is not /login/callback (e.g. the shared
    // TMS-prototyping client's http://127.0.0.1:8000/oauth-callback), serve it
    // too and relay to /login/callback, where the state cookie lives. A
    // different origin gets its own listener.
    let mut relay_router = None;
    if let Some(cb) = &tapis_relay {
        let router = Router::new().route(cb.path(), get(tapis_callback_relay)).with_state(state.clone());
        let base = url::Url::parse(&base_url).expect("BASE_URL must be a valid URL");
        if cb.origin() == base.origin() {
            relay_router = Some(router);
        } else {
            let addr = format!("{}:{}", cb.host_str().unwrap_or("127.0.0.1"), cb.port_or_known_default().unwrap_or(80));
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .unwrap_or_else(|e| panic!("bind {addr} for TAPIS_CALLBACK_URL: {e}"));
            println!("Relaying Tapis callbacks from {cb}");
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        }
    }

    let app = Router::new()
        .route("/", get(landing))
        .route("/demo", get(index))
        .merge(institutions_api::router(registry, base_url.clone()))
        .route("/login", get(login))
        .route("/login/callback", get(callback))
        .route("/login/status", get(login_status))
        .route("/logout", post(logout))
        .route("/logout/idp", get(logout_idp))
        .route("/mock-globus/authorize", get(mock_authorize))
        .route("/mock-globus/approve", get(mock_approve))
        .route("/mock-globus/idp-callback", get(mock_idp_callback))
        .merge(test_idp::router(idp))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state)
        .merge(relay_router.unwrap_or_default());

    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("Listening on {base_url}");
    axum::serve(listener, app).await.unwrap();
}

fn build_test_idp(env: &Settings, base_url: &str) -> TestIdp {
    let key_file = env.or("IDP_SIGNING_KEY", "idp_signing_key.pem");
    let key = test_idp::load_or_create_key(&key_file).unwrap_or_else(|e| panic!("{e}"));
    let client_secret = env.get("IDP_CLIENT_SECRET").unwrap_or_else(|| {
        let s = uuid::Uuid::new_v4().simple().to_string();
        println!("IDP_CLIENT_SECRET not set; generated one for this run: {s}");
        s
    });
    let redirect_uris = env.or("IDP_REDIRECT_URIS", test_idp::GLOBUS_IDP_CALLBACK)
        .split(',')
        .map(|u| u.trim().to_string())
        .chain([format!("{base_url}/mock-globus/idp-callback")])
        .collect();
    TestIdp::new(
        format!("{base_url}/idp"),
        env.or("IDP_DOMAIN", "tacc.utexas.edu"),
        env.or("IDP_CLIENT_ID", "globus-auth-client"),
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

#[derive(Deserialize)]
struct LoginParams {
    institution: String,
    #[serde(default)]
    dry_run: bool,
}

async fn login(
    State(app): State<AppState>,
    jar: CookieJar,
    query: Result<Query<LoginParams>, QueryRejection>,
) -> Response {
    let p = match query {
        Ok(Query(p)) => p,
        Err(e) => return tapis_style(StatusCode::BAD_REQUEST, &e.body_text(), json!(null)),
    };
    if !is_valid_id(&p.institution) {
        return tapis_style(
            StatusCode::BAD_REQUEST,
            &format!("Institution id '{}' is malformed: use lower-case letters, digits and _.", p.institution),
            json!(null),
        );
    }
    let Some(inst) = app.registry.find(&p.institution) else {
        return tapis_style(
            StatusCode::FORBIDDEN,
            &format!("Institution '{}' is not known.", p.institution),
            json!(null),
        );
    };

    // Unapproved institutions never reach Globus or Tapis: no redirect, no
    // state cookie, just the reason. (dry_run still shows the URL it would
    // have used, below.)
    if !inst.approved && !p.dry_run {
        return (StatusCode::FORBIDDEN, Html(page_filtered(inst))).into_response();
    }

    // State binds this browser's login attempt to the institution it picked.
    let nonce = uuid::Uuid::new_v4().to_string();
    let url = match inst.provider {
        Provider::Globus => {
            globus::build_authorize_url(&app.globus, &nonce, inst.redirect_idp_domain.as_deref())
        }
        // The offline mock stands in for Tapis as well as Globus.
        Provider::TaccTapis if app.globus.is_mock() => globus::build_authorize_url(&app.globus, &nonce, None),
        Provider::TaccTapis => match tapis::build_authorize_url(&app.tapis, &nonce) {
            Some(url) => url,
            None => {
                return tapis_style(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "TaccTapis login is not configured: set TAPIS_CLIENT_ID and TAPIS_CLIENT_SECRET.",
                    json!(null),
                )
            }
        },
    };

    if p.dry_run {
        let (status, message) = if inst.approved {
            (StatusCode::OK, "Authorize URL (not redirecting: dry_run=true).".to_string())
        } else {
            (
                StatusCode::FORBIDDEN,
                format!(
                    "Institution '{}' is not on the approved list; login is blocked. \
                     authorize_url is what it would have used if approved.",
                    inst.id
                ),
            )
        };
        return tapis_style(
            status,
            &message,
            json!({
                "institution": inst,
                "mock_mode": app.globus.is_mock(),
                "blocked": !inst.approved,
                "authorize_url": url.as_str(),
            }),
        );
    }

    let cookie = Cookie::build((STATE_COOKIE, format!("{nonce}|{}", inst.id)))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax); // Lax so the top-level redirect back from Globus/Tapis carries it
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
        return (jar_cleared, page_error(StatusCode::BAD_REQUEST, &format!("Login provider returned error: {err}")))
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
    let Some(inst) = app.registry.find(&inst_id).cloned() else {
        return (jar_cleared, page_error(StatusCode::FORBIDDEN, "Institution is no longer known."))
            .into_response();
    };

    let user = if app.globus.is_mock() {
        globus::mock_user_from_code(&code).map(|u| (u, None))
    } else {
        match inst.provider {
            Provider::Globus => globus::fetch_user(&app.globus, &code).await,
            Provider::TaccTapis => tapis::fetch_user(&app.tapis, &code).await,
        }
        .map(|(u, t)| (u, Some(t)))
    };
    let (user, token) = match user {
        Ok(u) => u,
        Err(e) => return (jar_cleared, page_error(StatusCode::BAD_GATEWAY, &e)).into_response(),
    };

    // The approved list is enforced here, after sign-in, so a blocked
    // institution still shows who signed in and why they were filtered.
    let verdict = if !inst.approved {
        Verdict::Filtered
    } else if identity_matches(&inst, &user.preferred_username) {
        Verdict::Welcome
    } else {
        Verdict::NotAllowed
    };

    // A login over an earlier one (often a rejected login retried) replaces
    // its session cookie, so revoke that token now: logout won't see it.
    let earlier = jar.get(SESSION_COOKIE).and_then(|c| app.sessions.lock().unwrap().remove(c.value()));
    if let Some(earlier) = earlier {
        revoke(&app, earlier, "login").await;
    }

    // Keep the token even when the identity is rejected: it is still a live
    // credential, and logout should revoke it.
    let session = uuid::Uuid::new_v4().to_string();
    app.sessions.lock().unwrap().insert(
        session.clone(),
        Session {
            institution: inst.id.clone(),
            provider: inst.provider,
            user: user.clone(),
            rejected: verdict != Verdict::Welcome,
            token,
        },
    );
    let jar = jar_cleared.add(
        Cookie::build((SESSION_COOKIE, session)).path("/").http_only(true).same_site(SameSite::Lax),
    );
    // A successful login goes back to the landing page, which shows who
    // signed in; a rejected one gets the page explaining why.
    if verdict == Verdict::Welcome {
        return (jar, Redirect::to("/")).into_response();
    }
    (jar, (StatusCode::FORBIDDEN, Html(page_result(&inst, &user, verdict)))).into_response()
}

/// Revoke a session's token with its provider. Failures are only logged: the
/// session is gone from the app either way.
async fn revoke(app: &AppState, session: Session, context: &str) {
    let Session { provider, token: Some(token), .. } = session else {
        return; // mock mode: no real token
    };
    let revoked = match provider {
        Provider::Globus => globus::revoke_token(&app.globus, &token).await,
        Provider::TaccTapis => tapis::revoke_token(&app.tapis, &token).await,
    };
    if let Err(e) = revoked {
        eprintln!("[{context}] {e}");
    }
}

/// Hand Tapis's `code`/`state` (or `error`) to the normal callback on BASE_URL.
async fn tapis_callback_relay(State(app): State<AppState>, RawQuery(query): RawQuery) -> Redirect {
    Redirect::to(&format!("{}/login/callback?{}", app.base_url, query.unwrap_or_default()))
}

/// Whether this browser completed a login through the app and has not logged
/// out since. The app cannot see the auth.globus.org or Tapis session itself.
async fn login_status(State(app): State<AppState>, jar: CookieJar) -> Response {
    let session = jar
        .get(SESSION_COOKIE)
        .and_then(|c| app.sessions.lock().unwrap().get(c.value()).map(|s| (s.institution.clone(), s.provider, (!s.rejected).then(|| s.user.clone()))));
    // The provider's own sign-out page, which the page opens in a new tab. The
    // mock has no real Globus or Tapis session to sign out of.
    let logout_url = match &session {
        _ if app.globus.is_mock() => None,
        Some((inst, Provider::Globus, _)) => Some(globus_logout_url(&app, inst)),
        Some((_, Provider::TaccTapis, _)) => Some(tapis::logout_url(&app.tapis)),
        None => None,
    };
    // A rejected login still reports its institution and logout_url, so the
    // demo page can offer "Log out" to revoke its token.
    let (institution, provider, user) = match session {
        Some((i, p, u)) => (Some(i), Some(p), u),
        None => (None, None, None),
    };
    tapis_style(
        StatusCode::OK,
        "Login status.",
        json!({
            "logged_in": user.is_some(),
            "rejected": institution.is_some() && user.is_none(),
            "institution": institution,
            "provider": provider,
            "user": user,
            "logout_url": logout_url,
        }),
    )
}

/// Globus's sign-out page. For an institution with an IdP sign-out URL, its
/// "Continue" link goes on to /logout/idp, so the institution's session ends
/// too and the next login is a complete one; otherwise it returns to the
/// landing page.
fn globus_logout_url(app: &AppState, institution: &str) -> String {
    let url = match app.registry.find(institution) {
        Some(inst) if inst.idp_logout_url.is_some() => globus::logout_url(
            &app.globus,
            &format!("{}/logout/idp?institution={}", app.base_url, url_encode(&inst.id)),
            &format!("{} sign-out", inst.name),
        ),
        _ => globus::logout_url(&app.globus, &format!("{}/", app.base_url), "IdP redirect demo"),
    };
    url.to_string()
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[derive(Deserialize)]
struct LogoutIdpParams {
    institution: String,
}

/// Second hop of an institution logout. Only redirects to a URL from the
/// institutions list, so it cannot be used as an open redirect.
async fn logout_idp(State(app): State<AppState>, query: Result<Query<LogoutIdpParams>, QueryRejection>) -> Response {
    let p = match query {
        Ok(Query(p)) if is_valid_id(&p.institution) => p,
        Ok(_) => return page_error(StatusCode::BAD_REQUEST, "Malformed institution id.").into_response(),
        Err(e) => return page_error(StatusCode::BAD_REQUEST, &e.body_text()).into_response(),
    };
    match app.registry.find(&p.institution).and_then(|i| i.idp_logout_url.as_deref()) {
        Some(url) => Redirect::to(url).into_response(),
        None => Redirect::to("/").into_response(),
    }
}

#[derive(Deserialize)]
struct LogoutParams {
    /// Revoke and clear cookies only, for the page's in-place logout buttons.
    #[serde(default)]
    local: bool,
}

async fn logout(
    State(app): State<AppState>,
    jar: CookieJar,
    query: Result<Query<LogoutParams>, QueryRejection>,
) -> Response {
    let p = match query {
        Ok(Query(p)) => p,
        Err(e) => return tapis_style(StatusCode::BAD_REQUEST, &e.body_text(), json!(null)),
    };
    let session = jar
        .get(SESSION_COOKIE)
        .and_then(|c| app.sessions.lock().unwrap().remove(c.value()));
    let provider = session.as_ref().map(|s| s.provider);
    let institution = session.as_ref().map(|s| s.institution.clone()).unwrap_or_default();
    if let Some(session) = session {
        revoke(&app, session, "logout").await;
    }
    let jar = jar
        .remove(Cookie::build(SESSION_COOKIE).path("/"))
        .remove(Cookie::build(STATE_COOKIE).path("/"));
    if p.local {
        return (jar, StatusCode::NO_CONTENT).into_response();
    }

    // The browser's auth.globus.org session is what lets a later login skip
    // the IdP redirect, so end it there too. Globus's sign-out page does not
    // redirect by itself; it links back to the landing page. Tapis's logout
    // page has no way back to the app, so a Tapis logout stops at revoking.
    if app.globus.is_mock() || provider == Some(Provider::TaccTapis) {
        return (jar, Redirect::to("/")).into_response();
    }
    (jar, Redirect::to(&globus_logout_url(&app, &institution))).into_response()
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

async fn mock_authorize(
    State(app): State<AppState>,
    query: Result<Query<MockAuthorizeParams>, QueryRejection>,
) -> Response {
    let p = match query {
        Ok(Query(p)) => p,
        Err(e) => return page_error(StatusCode::BAD_REQUEST, &e.body_text()).into_response(),
    };
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

    // No domain: the default Globus login (organization picker) or Tapis.
    let (place, user_domain) = if domain.is_empty() {
        ("the organization you pick", "globusid.org")
    } else {
        (domain.as_str(), domain.as_str())
    };
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
        esc(place),
        esc(place),
        esc(&p.redirect_uri),
        esc(&p.state),
        esc(user_domain),
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
async fn mock_idp_callback(
    State(app): State<AppState>,
    query: Result<Query<MockIdpCallbackParams>, QueryRejection>,
) -> Response {
    let p = match query {
        Ok(Query(p)) => p,
        Err(e) => return page_error(StatusCode::BAD_REQUEST, &e.body_text()).into_response(),
    };
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

async fn mock_approve(query: Result<Query<MockApproveParams>, QueryRejection>) -> Response {
    let p = match query {
        Ok(Query(p)) => p,
        Err(e) => return page_error(StatusCode::BAD_REQUEST, &e.body_text()).into_response(),
    };
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
 body{{font:16px/1.5 system-ui,sans-serif;max-width:60rem;margin:1.5rem auto;padding:0 .75rem;color:#1d2330}}
 table{{border-collapse:collapse;margin:1rem 0}} th,td{{text-align:left;padding:.3rem .8rem;border-bottom:1px solid #ddd}}
 code{{background:#f2f3f5;padding:.1rem .3rem;border-radius:3px}} .note{{color:#5b6475}}
 .ok{{color:#13703a}} .bad{{color:#a12121}} button{{font:inherit;padding:.4rem 1rem}} input{{font:inherit}}
 a.btn{{display:inline-block;background:#1f5fbf;color:#fff;padding:.35rem .9rem;border-radius:4px;text-decoration:none}}
</style></head><body>{}<p><a class="btn" href="/">&larr; Back to start</a></p></body></html>"#,
        esc(title),
        body
    )
}

fn page_error(status: StatusCode, msg: &str) -> (StatusCode, Html<String>) {
    (status, Html(shell("Login error", &format!("<h1 class=\"bad\">Login failed</h1><p>{}</p>", esc(msg)))))
}

/// Shown by /login instead of redirecting when the chosen institution is not
/// approved. Mirrors the Welcome page, with the institution's details in
/// place of an identity, since no sign-in happens.
fn page_filtered(inst: &Institution) -> String {
    let row = |k: &str, v: &str| format!("<tr><th>{}</th><td>{}</td></tr>", k, esc(v));
    let provider = match inst.provider {
        Provider::Globus => "Globus Auth",
        Provider::TaccTapis => "Tapis OAuth2",
    };
    let body = format!(
        "<h1 class=\"bad\">&#8856; Filtered</h1>\
         <p><b>{}</b> is not on the approved institutions list (<code>\"approved\": false</code>), \
         so login with it is blocked. You were not sent to {provider}, and no sign-in took place.</p>\
         <p class=\"note\">The approved list decides which institutions may use the service. The server \
         checks it before starting the OAuth flow, so editing the page or the URL cannot get around it.</p>\
         <table>{}{}{}{}{}</table>",
        esc(&inst.name),
        row("institution", &inst.name),
        row("id", &inst.id),
        row("redirect_idp_domain", inst.redirect_idp_domain.as_deref().unwrap_or("—")),
        row("provider", provider),
        row("approved", "false"),
    );
    shell("Login filtered", &body)
}

#[derive(PartialEq, Eq)]
enum Verdict {
    Welcome,
    /// Signed in with an identity outside the chosen institution's domain.
    NotAllowed,
    /// Signed in, but the institution is not on the approved list.
    Filtered,
}

fn page_result(inst: &Institution, u: &globus::UserInfo, verdict: Verdict) -> String {
    let verdict = match verdict {
        Verdict::Welcome => format!(
            "<h1 class=\"ok\">&#10003; Welcome</h1><p>Your identity belongs to <b>{}</b>, an approved institution.</p>",
            esc(&inst.name)
        ),
        // Only when the list changed between /login and the callback; /login
        // normally stops unapproved institutions before sign-in.
        Verdict::Filtered => format!(
            "<h1 class=\"bad\">&#8856; Filtered</h1><p>You signed in, but <b>{}</b> is not on the \
             approved institutions list (<code>\"approved\": false</code>), so this login is blocked.</p>",
            esc(&inst.name)
        ),
        Verdict::NotAllowed => format!(
            "<h1 class=\"bad\">&#10007; Not allowed</h1><p>You chose <b>{}</b> but came back as an identity outside <code>@{}</code>. \
             <code>redirect_idp_domain</code> only steers the login page, so the server enforces the match here.</p>",
            esc(&inst.name),
            esc(inst.redirect_idp_domain.as_deref().unwrap_or_default())
        ),
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
    shell("Login result", &body)
}
