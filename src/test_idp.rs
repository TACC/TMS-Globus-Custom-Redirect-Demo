//! A minimal OpenID Connect provider standing in for a TACC login page.
//!
//! Globus Auth's `redirect_idp_domain` sends unauthenticated users straight to
//! the authorization endpoint of the IdP registered for that domain. Register
//! this server with Globus (`globus-connect-server oidc register
//! --discovery-url <BASE_URL>/idp/.well-known/openid-configuration ...`) and that
//! endpoint, `GET /idp/authorize`, is the "custom login page" users land on.
//!
//! Routes:
//!   GET  /idp/.well-known/openid-configuration
//!   GET  /idp/jwks.json
//!   GET  /idp/authorize    custom login page (authorization endpoint)
//!   POST /idp/authorize    login form submit -> redirect back with ?code
//!   POST /idp/token        code -> access_token + RS256 id_token
//!   GET  /idp/userinfo
//!
//! `relying_party_login` performs the same checks Globus does as the client
//! (discovery, token exchange, id_token signature/iss/aud/nonce), so the server
//! can be exercised offline before it is registered.

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Form, Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use rsa::{
    pkcs1v15::{Signature, SigningKey, VerifyingKey},
    pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding},
    signature::{SignatureEncoding, Signer, Verifier},
    traits::PublicKeyParts,
    BigUint, RsaPrivateKey, RsaPublicKey,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where Globus Auth returns users after an IdP login.
pub const GLOBUS_IDP_CALLBACK: &str = "https://auth.globus.org/p/authenticate/callback";
const KEY_ID: &str = "test-idp-1";
const CODE_TTL: Duration = Duration::from_secs(120);
const TOKEN_TTL: Duration = Duration::from_secs(3600);

pub struct TestIdp {
    pub issuer: String,
    /// Domain this IdP asserts; usernames come back as `<user>@<domain>`.
    pub domain: String,
    /// The client Globus Auth uses against this server (`--client-id` / `--client-secret`).
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uris: Vec<String>,
    key: RsaPrivateKey,
    codes: Mutex<HashMap<String, CodeGrant>>,
    tokens: Mutex<HashMap<String, (Value, SystemTime)>>,
}

struct CodeGrant {
    client_id: String,
    redirect_uri: String,
    nonce: Option<String>,
    username: String,
    expires: SystemTime,
}

impl TestIdp {
    pub fn new(
        issuer: String,
        domain: String,
        client_id: String,
        client_secret: String,
        redirect_uris: Vec<String>,
        key: RsaPrivateKey,
    ) -> Self {
        Self {
            issuer,
            domain,
            client_id,
            client_secret,
            redirect_uris,
            key,
            codes: Mutex::default(),
            tokens: Mutex::default(),
        }
    }

    fn check_client(&self, client_id: &str, redirect_uri: &str) -> Result<(), String> {
        if client_id != self.client_id {
            return Err(format!("Unknown client_id '{client_id}'."));
        }
        if !self.redirect_uris.iter().any(|u| u == redirect_uri) {
            return Err(format!("redirect_uri '{redirect_uri}' is not registered for this client."));
        }
        Ok(())
    }

    /// Issue a one-time authorization code for `username` (local part only).
    pub fn issue_code(&self, client_id: &str, redirect_uri: &str, nonce: Option<String>, username: &str) -> String {
        let code = uuid::Uuid::new_v4().simple().to_string();
        self.codes.lock().unwrap().insert(
            code.clone(),
            CodeGrant {
                client_id: client_id.into(),
                redirect_uri: redirect_uri.into(),
                nonce,
                username: username.into(),
                expires: SystemTime::now() + CODE_TTL,
            },
        );
        code
    }

    fn redeem_code(&self, code: &str, client_id: &str, redirect_uri: &str) -> Result<CodeGrant, &'static str> {
        // Removed on first use, so a replayed code is rejected.
        let grant = self.codes.lock().unwrap().remove(code).ok_or("unknown or used code")?;
        if grant.expires < SystemTime::now() {
            return Err("code expired");
        }
        if grant.client_id != client_id || grant.redirect_uri != redirect_uri {
            return Err("code was issued to a different client or redirect_uri");
        }
        Ok(grant)
    }

    fn user_claims(&self, username: &str) -> Value {
        let preferred = format!("{username}@{}", self.domain);
        json!({
            "sub": preferred,
            "preferred_username": preferred,
            "email": preferred,
            "name": format!("{username} (test IdP)"),
        })
    }

    fn sign_jwt(&self, claims: &Value) -> String {
        let header = json!({ "alg": "RS256", "typ": "JWT", "kid": KEY_ID });
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let sig = SigningKey::<Sha256>::new(self.key.clone()).sign(input.as_bytes());
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    fn discovery(&self) -> Value {
        let iss = &self.issuer;
        json!({
            "issuer": iss,
            "authorization_endpoint": format!("{iss}/authorize"),
            "token_endpoint": format!("{iss}/token"),
            "userinfo_endpoint": format!("{iss}/userinfo"),
            "jwks_uri": format!("{iss}/jwks.json"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
            "scopes_supported": ["openid", "profile", "email"],
            "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
            "claims_supported": ["iss", "sub", "aud", "exp", "iat", "nonce",
                                 "preferred_username", "email", "name"],
        })
    }

    fn jwks(&self) -> Value {
        let public = self.key.to_public_key();
        json!({ "keys": [{
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": KEY_ID,
            "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
        }]})
    }
}

/// Load the signing key from `path`, creating it on first run so the JWKS stays
/// stable across restarts (Globus may cache it).
pub fn load_or_create_key(path: &str) -> Result<RsaPrivateKey, String> {
    if let Ok(pem) = std::fs::read_to_string(path) {
        return RsaPrivateKey::from_pkcs8_pem(&pem).map_err(|e| format!("parsing {path}: {e}"));
    }
    let key = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).map_err(|e| e.to_string())?;
    let pem = key.to_pkcs8_pem(LineEnding::LF).map_err(|e| e.to_string())?;
    std::fs::write(path, pem.as_bytes()).map_err(|e| format!("writing {path}: {e}"))?;
    Ok(key)
}

pub fn router<S: Clone + Send + Sync + 'static>(idp: Arc<TestIdp>) -> Router<S> {
    Router::new()
        .route("/idp/.well-known/openid-configuration", get(discovery))
        .route("/idp/jwks.json", get(jwks))
        .route("/idp/authorize", get(login_page).post(login_submit))
        .route("/idp/token", axum::routing::post(token))
        .route("/idp/userinfo", get(userinfo))
        .with_state(idp)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ---- routes -------------------------------------------------------------

async fn discovery(State(idp): State<Arc<TestIdp>>) -> Json<Value> {
    Json(idp.discovery())
}

async fn jwks(State(idp): State<Arc<TestIdp>>) -> Json<Value> {
    Json(idp.jwks())
}

#[derive(Deserialize)]
struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    nonce: Option<String>,
    scope: Option<String>,
}

async fn login_page(
    State(idp): State<Arc<TestIdp>>,
    headers: HeaderMap,
    Query(p): Query<AuthorizeParams>,
) -> Response {
    // Validate before showing anything: never bounce users to an unregistered redirect_uri.
    if let Err(e) = idp.check_client(&p.client_id, &p.redirect_uri) {
        return error_page(StatusCode::BAD_REQUEST, &e);
    }
    if p.response_type != "code" {
        return error_page(StatusCode::BAD_REQUEST, "Only response_type=code is supported.");
    }
    let referer = headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(none)");
    println!(
        "[test-idp] authorize: client_id={} redirect_uri={} referer={referer}",
        p.client_id, p.redirect_uri
    );

    let hidden = |k: &str, v: &str| format!(r#"<input type="hidden" name="{k}" value="{}">"#, esc(v));
    let row = |k: &str, v: &str| format!("<tr><th>{k}</th><td><code>{}</code></td></tr>", esc(v));
    let body = format!(
        r#"<h1>TACC test login</h1>
<p>This is the IdP's authorization endpoint, the custom login page. If you reached it from Globus
without seeing the Globus organization picker, <code>redirect_idp_domain</code> worked.</p>
<table>{}{}{}</table>
<form method="post" action="/idp/authorize">
  {}{}{}{}{}
  <label>TACC username <input name="username" value="jdoe" required></label>
  <span>@{}</span>
  <button>Sign in</button>
</form>"#,
        row("redirect_uri", &p.redirect_uri),
        row("scope", p.scope.as_deref().unwrap_or("")),
        row("referer", referer),
        hidden("client_id", &p.client_id),
        hidden("redirect_uri", &p.redirect_uri),
        hidden("state", p.state.as_deref().unwrap_or("")),
        hidden("nonce", p.nonce.as_deref().unwrap_or("")),
        hidden("scope", p.scope.as_deref().unwrap_or("")),
        esc(&idp.domain),
    );
    Html(shell("TACC test login", &body)).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    client_id: String,
    redirect_uri: String,
    state: String,
    nonce: String,
    username: String,
}

async fn login_submit(State(idp): State<Arc<TestIdp>>, Form(f): Form<LoginForm>) -> Response {
    if let Err(e) = idp.check_client(&f.client_id, &f.redirect_uri) {
        return error_page(StatusCode::BAD_REQUEST, &e);
    }
    let username = f.username.trim();
    if username.is_empty() || username.contains('@') {
        return error_page(StatusCode::BAD_REQUEST, "Enter a username without a domain.");
    }
    let nonce = Some(f.nonce).filter(|n| !n.is_empty());
    let code = idp.issue_code(&f.client_id, &f.redirect_uri, nonce, username);

    let mut url = url::Url::parse(&f.redirect_uri).expect("registered redirect_uri is a URL");
    url.query_pairs_mut().append_pair("code", &code);
    if !f.state.is_empty() {
        url.query_pairs_mut().append_pair("state", &f.state);
    }
    Redirect::to(url.as_str()).into_response()
}

#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    println!("[test-idp] token error: {error}: {description}");
    (status, Json(json!({ "error": error, "error_description": description }))).into_response()
}

/// client_secret_basic, falling back to client_secret_post.
fn client_credentials(headers: &HeaderMap, f: &TokenForm) -> Option<(String, String)> {
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| STANDARD.decode(b).ok())
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|s| s.split_once(':').map(|(id, secret)| (id.to_string(), secret.to_string())));
    basic.or_else(|| Some((f.client_id.clone()?, f.client_secret.clone()?)))
}

async fn token(State(idp): State<Arc<TestIdp>>, headers: HeaderMap, Form(f): Form<TokenForm>) -> Response {
    let Some((client_id, secret)) = client_credentials(&headers, &f) else {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "missing client credentials");
    };
    if client_id != idp.client_id || secret != idp.client_secret {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "bad client credentials");
    }
    if f.grant_type != "authorization_code" {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", &f.grant_type);
    }
    let (Some(code), Some(redirect_uri)) = (f.code, f.redirect_uri) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_request", "code and redirect_uri are required");
    };
    let grant = match idp.redeem_code(&code, &client_id, &redirect_uri) {
        Ok(g) => g,
        Err(e) => return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", e),
    };

    let user = idp.user_claims(&grant.username);
    let mut id_claims = user.clone();
    let iat = now();
    id_claims["iss"] = json!(idp.issuer);
    id_claims["aud"] = json!(client_id);
    id_claims["iat"] = json!(iat);
    id_claims["exp"] = json!(iat + TOKEN_TTL.as_secs());
    if let Some(nonce) = grant.nonce {
        id_claims["nonce"] = json!(nonce);
    }

    let access_token = uuid::Uuid::new_v4().simple().to_string();
    idp.tokens
        .lock()
        .unwrap()
        .insert(access_token.clone(), (user, SystemTime::now() + TOKEN_TTL));
    println!("[test-idp] token issued for {}", id_claims["preferred_username"]);

    Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": TOKEN_TTL.as_secs(),
        "id_token": idp.sign_jwt(&id_claims),
    }))
    .into_response()
}

async fn userinfo(State(idp): State<Arc<TestIdp>>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let claims = token.and_then(|t| {
        idp.tokens
            .lock()
            .unwrap()
            .get(t)
            .filter(|(_, exp)| *exp > SystemTime::now())
            .map(|(c, _)| c.clone())
    });
    match claims {
        Some(c) => Json(c).into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer error=\"invalid_token\"")],
        )
            .into_response(),
    }
}

// ---- relying-party check (what Globus does as this IdP's client) --------

/// Discover the IdP at `issuer`, redeem `code`, and validate the id_token.
/// Returns the verified id_token claims.
pub async fn relying_party_login(
    issuer: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &str,
    nonce: Option<&str>,
) -> Result<Value, String> {
    let http = reqwest::Client::new();
    let get_json = |url: String| {
        let http = http.clone();
        async move {
            http.get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("GET {url}: {e}"))?
                .json::<Value>()
                .await
                .map_err(|e| format!("GET {url}: bad JSON: {e}"))
        }
    };

    let disco = get_json(format!("{issuer}/.well-known/openid-configuration")).await?;
    if disco["issuer"] != issuer {
        return Err(format!("discovery issuer {} != {issuer}", disco["issuer"]));
    }
    let field = |k: &str| {
        disco[k]
            .as_str()
            .map(str::to_string)
            .ok_or(format!("discovery is missing {k}"))
    };
    let (token_url, jwks_url) = (field("token_endpoint")?, field("jwks_uri")?);

    let resp = http
        .post(&token_url)
        .basic_auth(client_id, Some(client_secret))
        .form(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", redirect_uri)])
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| format!("token response: {e}"))?;
    if !status.is_success() {
        return Err(format!("token endpoint returned {status}: {body}"));
    }
    let id_token = body["id_token"].as_str().ok_or("token response has no id_token")?;

    let claims = verify_jwt(id_token, &get_json(jwks_url).await?)?;
    if claims["iss"] != issuer {
        return Err(format!("id_token iss {} != {issuer}", claims["iss"]));
    }
    if claims["aud"] != client_id {
        return Err(format!("id_token aud {} != {client_id}", claims["aud"]));
    }
    if claims["exp"].as_u64().unwrap_or(0) <= now() {
        return Err("id_token is expired".into());
    }
    if nonce.is_some() && claims["nonce"].as_str() != nonce {
        return Err("id_token nonce does not match the request".into());
    }
    Ok(claims)
}

/// Verify an RS256 JWT against a JWKS and return its claims.
fn verify_jwt(jwt: &str, jwks: &Value) -> Result<Value, String> {
    let mut parts = jwt.split('.');
    let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("id_token is not a three-part JWT".into());
    };
    let decode = |part: &str| -> Result<Value, String> {
        let raw = URL_SAFE_NO_PAD.decode(part).map_err(|e| e.to_string())?;
        serde_json::from_slice(&raw).map_err(|e| e.to_string())
    };
    let header = decode(h)?;
    if header["alg"] != "RS256" {
        return Err(format!("unexpected alg {}", header["alg"]));
    }
    let jwk = jwks["keys"]
        .as_array()
        .and_then(|keys| keys.iter().find(|k| k["kid"] == header["kid"]))
        .ok_or(format!("no JWKS key with kid {}", header["kid"]))?;
    let num = |k: &str| -> Result<BigUint, String> {
        let b64 = jwk[k].as_str().ok_or(format!("JWK is missing {k}"))?;
        Ok(BigUint::from_bytes_be(&URL_SAFE_NO_PAD.decode(b64).map_err(|e| e.to_string())?))
    };
    let public = RsaPublicKey::new(num("n")?, num("e")?).map_err(|e| e.to_string())?;
    let sig_bytes = URL_SAFE_NO_PAD.decode(s).map_err(|e| e.to_string())?;
    let sig = Signature::try_from(sig_bytes.as_slice()).map_err(|e| e.to_string())?;
    VerifyingKey::<Sha256>::new(public)
        .verify(format!("{h}.{p}").as_bytes(), &sig)
        .map_err(|_| "id_token signature does not verify against the JWKS".to_string())?;
    decode(p)
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
 h1{{color:#bf5700}} table{{border-collapse:collapse;margin:1rem 0}}
 th,td{{text-align:left;padding:.3rem .8rem;border-bottom:1px solid #ddd}}
 code{{background:#f2f3f5;padding:.1rem .3rem;border-radius:3px;word-break:break-all}}
 button,input{{font:inherit}} button{{padding:.4rem 1rem;margin-left:.5rem}}
</style></head><body>{}</body></html>"#,
        esc(title),
        body
    )
}

fn error_page(status: StatusCode, msg: &str) -> Response {
    let body = format!("<h1>Test IdP error</h1><p>{}</p>", esc(msg));
    (status, Html(shell("Test IdP error", &body))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    const REDIRECT: &str = "http://127.0.0.1/cb";

    fn key() -> RsaPrivateKey {
        static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
        KEY.get_or_init(|| RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap())
            .clone()
    }

    fn idp(issuer: &str) -> TestIdp {
        TestIdp::new(
            issuer.into(),
            "tacc.utexas.edu".into(),
            "globus-auth-client".into(),
            "s3cret".into(),
            vec![REDIRECT.into()],
            key(),
        )
    }

    #[test]
    fn signed_jwt_verifies_against_jwks() {
        let idp = idp("https://idp.example");
        let jwt = idp.sign_jwt(&json!({ "sub": "x" }));
        assert_eq!(verify_jwt(&jwt, &idp.jwks()).unwrap()["sub"], "x");

        let other = TestIdp::new("i".into(), "d".into(), "c".into(), "s".into(), vec![],
                                 RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap());
        assert!(verify_jwt(&jwt, &other.jwks()).is_err());
    }

    #[test]
    fn codes_are_single_use_and_bound_to_client() {
        let idp = idp("https://idp.example");
        assert!(idp.check_client("globus-auth-client", "https://evil.example/cb").is_err());

        let code = idp.issue_code("globus-auth-client", REDIRECT, None, "jdoe");
        assert!(idp.redeem_code(&code, "globus-auth-client", "http://other/cb").is_err());
        let code = idp.issue_code("globus-auth-client", REDIRECT, None, "jdoe");
        assert!(idp.redeem_code(&code, "globus-auth-client", REDIRECT).is_ok());
        assert!(idp.redeem_code(&code, "globus-auth-client", REDIRECT).is_err());
    }

    /// Serves the IdP over real HTTP and logs in as Globus would.
    #[tokio::test]
    async fn relying_party_login_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}/idp", listener.local_addr().unwrap());
        let idp = Arc::new(idp(&issuer));
        let app: Router = router(idp.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let code = idp.issue_code("globus-auth-client", REDIRECT, Some("n1".into()), "jdoe");
        let claims = relying_party_login(&issuer, "globus-auth-client", "s3cret", REDIRECT, &code, Some("n1"))
            .await
            .unwrap();
        assert_eq!(claims["preferred_username"], "jdoe@tacc.utexas.edu");
        assert_eq!(claims["aud"], "globus-auth-client");

        let code = idp.issue_code("globus-auth-client", REDIRECT, None, "jdoe");
        let err = relying_party_login(&issuer, "globus-auth-client", "wrong", REDIRECT, &code, None)
            .await
            .unwrap_err();
        assert!(err.contains("invalid_client"), "{err}");
    }
}
