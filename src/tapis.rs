//! Direct login against Tapis OAuth2 (tms_portal's `TaccTapis` provider type):
//! authorize URL, code exchange and identity. The token request mirrors
//! tms_portal's `get_token_for_provider`: a form post to `/v3/oauth2/tokens`
//! with HTTP Basic client credentials.

use crate::globus::{jwt_claims, UserInfo};
use url::Url;

pub const TAPIS_BASE_URL: &str = "https://tacc.tapis.io";

#[derive(Debug, Clone)]
pub struct TapisConfig {
    /// `None` => TaccTapis login is not configured (no TAPIS_CLIENT_ID/SECRET).
    pub client: Option<(String, String)>,
    pub base_url: String,
    pub callback_url: String,
}

impl TapisConfig {
    fn oauth2(&self, path: &str) -> String {
        format!("{}/v3/oauth2/{path}", self.base_url.trim_end_matches('/'))
    }
}

pub fn build_authorize_url(cfg: &TapisConfig, state: &str) -> Option<Url> {
    let (client_id, _) = cfg.client.as_ref()?;
    let url = Url::parse_with_params(
        &cfg.oauth2("authorize"),
        [
            ("response_type", "code"),
            ("client_id", client_id.trim()),
            ("redirect_uri", cfg.callback_url.as_str()),
            ("state", state),
        ],
    )
    .expect("TAPIS_BASE_URL must be a valid URL");
    Some(url)
}

/// Exchange the authorization code for a Tapis access token and read the
/// identity from its claims. Also returns the token so logout can revoke it,
/// and its claims for the landing page.
pub async fn fetch_user(
    cfg: &TapisConfig,
    code: &str,
) -> Result<(UserInfo, String, Option<serde_json::Value>), String> {
    let (client_id, client_secret) = cfg.client.as_ref().ok_or("TaccTapis login is not configured")?;
    let resp = reqwest::Client::new()
        .post(cfg.oauth2("tokens"))
        .basic_auth(client_id.trim(), Some(client_secret.trim()))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", cfg.callback_url.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("Tapis token request failed: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("bad Tapis token response ({status}): {e}"))?;
    if !status.is_success() {
        // Tapis envelope: {"status":"error","message":"Invalid client credentials ...", ...}
        return Err(format!("Tapis token request rejected ({status}): {}", body["message"]));
    }
    let token = body["result"]["access_token"]["access_token"]
        .as_str()
        .ok_or("Tapis token response has no result.access_token.access_token")?
        .to_string();
    let claims = jwt_claims(&token).map_err(|e| format!("Tapis access token: {e}"))?;
    Ok((user_from_claims(&claims)?, token, Some(claims)))
}

/// Tapis access tokens are JWTs carrying the identity (`sub` = "user@tenant").
/// The token came straight from the Tapis token endpoint over TLS with client
/// authentication, so its claims are read without re-verifying the signature.
fn user_from_claims(claims: &serde_json::Value) -> Result<UserInfo, String> {
    let claim = |k: &str| claims[k].as_str().map(str::to_string);
    let tenant = claim("tapis/tenant_id");
    let username = claim("tapis/username");
    let sub = claim("sub")
        .or_else(|| Some(format!("{}@{}", username.clone()?, tenant.clone()?)))
        .ok_or("Tapis token has no sub or tapis/username claim")?;
    Ok(UserInfo {
        sub: sub.clone(),
        preferred_username: sub,
        name: username,
        email: None,
        identity_provider_display_name: tenant.as_ref().map(|t| format!("Tapis ({t} tenant)")),
        organization: tenant,
    })
}

/// Tapis's sign-out page. It asks for confirmation and has no link back to
/// the app, so the demo opens it in a new tab.
pub fn logout_url(cfg: &TapisConfig) -> String {
    cfg.oauth2("logout")
}

/// Revoke a Tapis access token so it stops working before it expires.
pub async fn revoke_token(cfg: &TapisConfig, token: &str) -> Result<(), String> {
    reqwest::Client::new()
        .post(cfg.oauth2("tokens/revoke"))
        .json(&serde_json::json!({ "token": token }))
        .send()
        .await
        .map_err(|e| format!("Tapis revoke request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Tapis revoke rejected: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TapisConfig {
        TapisConfig {
            client: Some(("tms-demo".into(), "secret".into())),
            base_url: TAPIS_BASE_URL.into(),
            callback_url: "http://localhost:8080/login/callback".into(),
        }
    }

    #[test]
    fn authorize_url_targets_tapis() {
        let url = build_authorize_url(&cfg(), "s1").unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.as_str().split('?').next(), Some("https://tacc.tapis.io/v3/oauth2/authorize"));
        assert_eq!(q["client_id"], "tms-demo");
        assert_eq!(q["state"], "s1");
        assert!(!q.contains_key("redirect_idp_domain"));
    }

    #[test]
    fn unconfigured_has_no_authorize_url() {
        let cfg = TapisConfig { client: None, ..cfg() };
        assert!(build_authorize_url(&cfg, "s1").is_none());
    }

    #[test]
    fn identity_comes_from_token_claims() {
        let claims = serde_json::json!({"sub":"jdoe@tacc","tapis/username":"jdoe","tapis/tenant_id":"tacc"});
        let u = user_from_claims(&claims).unwrap();
        assert_eq!(u.preferred_username, "jdoe@tacc");
        assert_eq!(u.organization.as_deref(), Some("tacc"));
    }
}
