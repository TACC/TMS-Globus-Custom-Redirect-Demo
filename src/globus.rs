//! Globus Auth client pieces: authorize URL, code exchange, userinfo.
//! Mirrors how tms_portal's `login_handler` builds the authorize URL with
//! `Url::parse_with_params`, plus the one extra parameter: `redirect_idp_domain`.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use url::Url;

pub const GLOBUS_AUTHORIZE_URL: &str = "https://auth.globus.org/v2/oauth2/authorize";
pub const GLOBUS_TOKEN_URL: &str = "https://auth.globus.org/v2/oauth2/token";
pub const GLOBUS_USERINFO_URL: &str = "https://auth.globus.org/v2/oauth2/userinfo";
pub const GLOBUS_REVOKE_URL: &str = "https://auth.globus.org/v2/oauth2/token/revoke";
pub const GLOBUS_LOGOUT_URL: &str = "https://auth.globus.org/v2/web/logout";
pub const SCOPES: &str = "openid profile email";

#[derive(Debug, Clone)]
pub struct GlobusConfig {
    /// `None` => offline mock mode (no real Globus calls).
    pub client: Option<(String, String)>,
    pub authorize_url: String,
    pub callback_url: String,
}

impl GlobusConfig {
    pub fn is_mock(&self) -> bool {
        self.client.is_none()
    }

    fn client_id(&self) -> &str {
        self.client.as_ref().map(|(id, _)| id.as_str()).unwrap_or("mock-client-id")
    }
}

/// `redirect_idp_domain: None` gives Globus's default login (the organization picker).
pub fn build_authorize_url(cfg: &GlobusConfig, state: &str, redirect_idp_domain: Option<&str>) -> Url {
    let mut params = vec![
        ("response_type", "code"),
        ("client_id", cfg.client_id()),
        ("redirect_uri", cfg.callback_url.as_str()),
        ("scope", SCOPES),
        ("state", state),
    ];
    if let Some(domain) = redirect_idp_domain {
        // Users not yet authenticated go straight to this domain's IdP,
        // bypassing the Globus login page. Replaces the documented
        // `idp_redirect_domain`, which Globus does not act on.
        params.push(("redirect_idp_domain", domain));
    }
    Url::parse_with_params(&cfg.authorize_url, params).expect("authorize_url must be a valid URL")
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Present because SCOPES includes `openid`.
    id_token: Option<String>,
}

/// The decoded payload of a JWT, without verifying its signature. Only for
/// tokens received directly from a token endpoint over TLS; the landing page
/// shows these claims so the token itself never reaches the browser.
pub fn jwt_claims(token: &str) -> Result<serde_json::Value, String> {
    let payload = token.split('.').nth(1).ok_or("token is not a JWT")?;
    let raw = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| format!("bad JWT payload: {e}"))?;
    serde_json::from_slice(&raw).map_err(|e| format!("bad JWT payload: {e}"))
}

/// Subset of the Globus OIDC userinfo response we care about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub sub: String,
    pub preferred_username: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub organization: Option<String>,
    pub identity_provider_display_name: Option<String>,
}

/// Exchange the authorization code and fetch userinfo (real Globus). Also
/// returns the access token so logout can revoke it, and the id_token's
/// claims (Globus access tokens are opaque; the id_token is the JWT).
pub async fn fetch_user(
    cfg: &GlobusConfig,
    code: &str,
) -> Result<(UserInfo, String, Option<serde_json::Value>), String> {
    let (client_id, client_secret) = cfg.client.as_ref().ok_or("not configured for Globus")?;
    let http = reqwest::Client::new();

    let resp = http
        .post(GLOBUS_TOKEN_URL)
        .basic_auth(client_id.trim(), Some(client_secret.trim()))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", cfg.callback_url.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    if !resp.status().is_success() {
        // Surface Globus's error JSON, e.g. {"error":"invalid_client"} or {"error":"invalid_grant"}.
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token request rejected ({status}): {body}"));
    }
    let token: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("bad token response: {e}"))?;

    let user = http
        .get(GLOBUS_USERINFO_URL)
        .bearer_auth(&token.access_token)
        .send()
        .await
        .map_err(|e| format!("userinfo request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("userinfo rejected: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad userinfo response: {e}"))?;
    let claims = token.id_token.as_deref().map(jwt_claims).transpose()?;
    Ok((user, token.access_token, claims))
}

/// Revoke an access token so it stops working before it expires.
pub async fn revoke_token(cfg: &GlobusConfig, token: &str) -> Result<(), String> {
    let (client_id, client_secret) = cfg.client.as_ref().ok_or("not configured for Globus")?;
    reqwest::Client::new()
        .post(GLOBUS_REVOKE_URL)
        .basic_auth(client_id.trim(), Some(client_secret.trim()))
        .form(&[("token", token)])
        .send()
        .await
        .map_err(|e| format!("revoke request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("revoke rejected: {e}"))?;
    Ok(())
}

/// Globus's sign-out page, which ends the browser's auth.globus.org session
/// and offers a "Continue to `return_name`" link to `return_to`.
pub fn logout_url(cfg: &GlobusConfig, return_to: &str, return_name: &str) -> Url {
    Url::parse_with_params(
        GLOBUS_LOGOUT_URL,
        [
            ("client_id", cfg.client_id()),
            ("redirect_uri", return_to),
            ("redirect_name", return_name),
        ],
    )
    .expect("logout URL is valid")
}

// ---- mock mode ----------------------------------------------------------

/// The mock authorize page encodes the chosen username into the "code".
pub fn mock_code_for(username: &str) -> String {
    format!("mock.{}", URL_SAFE_NO_PAD.encode(username))
}

pub fn mock_user_from_code(code: &str) -> Result<UserInfo, String> {
    let raw = code.strip_prefix("mock.").ok_or("not a mock code")?;
    let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|e| e.to_string())?;
    let username = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let domain = username.rsplit_once('@').map(|(_, d)| d.to_string());
    Ok(UserInfo {
        sub: uuid::Uuid::new_v4().to_string(),
        name: Some("Mock User".into()),
        email: Some(username.clone()),
        organization: domain.clone(),
        identity_provider_display_name: domain.map(|d| format!("{d} (mock IdP)")),
        preferred_username: username,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GlobusConfig {
        GlobusConfig {
            client: Some(("abc".into(), "secret".into())),
            authorize_url: GLOBUS_AUTHORIZE_URL.into(),
            callback_url: "http://localhost:8080/login/callback".into(),
        }
    }

    #[test]
    fn authorize_url_carries_redirect_idp_domain() {
        let url = build_authorize_url(&cfg(), "s123", Some("uchicago.edu"));
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("auth.globus.org"));
        assert_eq!(q["redirect_idp_domain"], "uchicago.edu");
        assert_eq!(q["client_id"], "abc");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["state"], "s123");
        assert_eq!(q["redirect_uri"], "http://localhost:8080/login/callback");
    }

    #[test]
    fn default_login_has_no_redirect_idp_domain() {
        let url = build_authorize_url(&cfg(), "s123", None);
        assert!(url.query_pairs().all(|(k, _)| k != "redirect_idp_domain"));
    }

    #[test]
    fn logout_url_returns_to_the_app() {
        let url = logout_url(&cfg(), "http://localhost:8080/", "IdP redirect demo");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.as_str().split('?').next(), Some(GLOBUS_LOGOUT_URL));
        assert_eq!(q["client_id"], "abc");
        assert_eq!(q["redirect_uri"], "http://localhost:8080/");
    }

    #[test]
    fn jwt_claims_decodes_the_payload() {
        let token = format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(r#"{"sub":"abc","iss":"x"}"#));
        assert_eq!(jwt_claims(&token).unwrap()["sub"], "abc");
        assert!(jwt_claims("opaque-token").is_err());
    }

    #[test]
    fn mock_code_round_trips() {
        let u = mock_user_from_code(&mock_code_for("jdoe@utexas.edu")).unwrap();
        assert_eq!(u.preferred_username, "jdoe@utexas.edu");
        assert_eq!(u.organization.as_deref(), Some("utexas.edu"));
    }
}
