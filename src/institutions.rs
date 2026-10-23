//! Approved-institution registry.
//!
//! In production this list would come from TMS (e.g. a table next to
//! `identity_providers` / `allowed_redirects`) or an admin API. Here it is
//! loaded from a mocked JSON response that uses a Tapis-style envelope:
//! `{ "status", "message", "version", "result": [...] }`.

use serde::{Deserialize, Serialize};

/// Which login flow an entry uses, named after tms_portal's provider types.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Globus Auth, optionally steered by `redirect_idp_domain`.
    #[default]
    Globus,
    /// Tapis OAuth2 directly (tacc.tapis.io), not through Globus.
    TaccTapis,
}

/// JSON-LD type of an entry (`type`, aliased to `@type` in
/// api/institutions.context.jsonld). Optional in the input, always written out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstitutionType {
    #[default]
    Organization,
}

/// JSON-LD type of the list as a whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListType {
    #[default]
    ItemList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Institution {
    #[serde(rename = "type", default)]
    pub kind: InstitutionType,
    /// Stable id used by the UI (`/login?institution=<id>`).
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub provider: Provider,
    /// Domain Globus associates with the institution's IdP, e.g. "uchicago.edu".
    /// Passed verbatim as `redirect_idp_domain` on the authorize request. `None`
    /// means a plain login (Globus's organization picker, or Tapis) that accepts
    /// whatever identity comes back.
    #[serde(default)]
    pub redirect_idp_domain: Option<String>,
    /// Sign-out URL of the institution's own IdP. Globus does not pass
    /// `prompt=login` on to CILogon or the IdP, so ending the IdP's session is
    /// the only way to make the next login ask for a password again.
    #[serde(default)]
    pub idp_logout_url: Option<String>,
    pub approved: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstitutionsResponse {
    #[serde(rename = "type", default)]
    pub kind: ListType,
    pub status: String,
    pub message: String,
    /// Revision of the list; optional in api/openapi.yaml.
    #[serde(default)]
    pub version: Option<String>,
    pub result: Vec<Institution>,
}

#[derive(Debug, Clone)]
pub struct Registry {
    institutions: Vec<Institution>,
}

impl Registry {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let resp: InstitutionsResponse = serde_json::from_str(json)?;
        Ok(Self { institutions: resp.result })
    }

    pub fn load(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        Self::from_json(&raw).map_err(|e| format!("parsing {path}: {e}"))
    }

    pub fn approved(&self) -> Vec<Institution> {
        self.institutions.iter().filter(|i| i.approved).cloned().collect()
    }

    /// Every institution, approved or not, for display.
    pub fn all(&self) -> &[Institution] {
        &self.institutions
    }

    /// Lookup by id, approved or not: unapproved institutions can sign in, and
    /// the callback then filters them out. Unknown ids are refused, so a
    /// hand-edited `?institution=` query string can't steer the redirect.
    pub fn find(&self, id: &str) -> Option<&Institution> {
        self.institutions.iter().find(|i| i.id == id)
    }
}

/// `redirect_idp_domain` only changes what the user *sees* (it skips the Globus
/// IdP picker). It does not stop someone from coming back with a different
/// identity, so the callback must check the identity it actually received.
///
/// Globus institutional usernames look like `jdoe@uchicago.edu`. Entries with
/// no domain accept any identity.
pub fn identity_matches(inst: &Institution, username: &str) -> bool {
    let Some(domain) = &inst.redirect_idp_domain else {
        return true;
    };
    let username = username.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    username
        .rsplit_once('@')
        .map(|(_, d)| d == domain)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOCK: &str = include_str!("../mock/approved_institutions.json");

    #[test]
    fn lookup_covers_all_known_institutions_only() {
        let reg = Registry::from_json(MOCK).unwrap();
        assert!(reg.approved().iter().all(|i| i.approved));
        assert!(!reg.find("example").unwrap().approved);
        assert!(reg.find("uchicago").unwrap().approved);
        assert!(reg.find("nope").is_none());
    }

    #[test]
    fn identity_must_be_from_the_institution_domain() {
        let reg = Registry::from_json(MOCK).unwrap();
        let uc = reg.find("uchicago").unwrap();
        assert!(identity_matches(uc, "jdoe@uchicago.edu"));
        assert!(identity_matches(uc, "JDoe@UChicago.edu"));
        assert!(!identity_matches(uc, "jdoe@globusid.org"));
        assert!(!identity_matches(uc, "jdoe@evil-uchicago.edu"));
        assert!(!identity_matches(uc, "uchicago.edu"));
    }

    #[test]
    fn entries_without_a_domain_accept_any_identity() {
        let reg = Registry::from_json(MOCK).unwrap();
        let globus = reg.find("globus").unwrap();
        assert_eq!(globus.provider, Provider::Globus);
        assert!(identity_matches(globus, "jdoe@globusid.org"));
        let tapis = reg.find("tacc_tapis").unwrap();
        assert_eq!(tapis.provider, Provider::TaccTapis);
        assert!(identity_matches(tapis, "jdoe@tacc"));
    }

    #[test]
    fn json_ld_types_are_optional_and_always_written() {
        let reg = Registry::from_json(MOCK).unwrap();
        let out = serde_json::to_value(reg.find("uchicago").unwrap()).unwrap();
        assert_eq!(out["type"], "Organization");

        let untyped = r#"{"status":"success","message":"","version":"","result":[{"id":"a","name":"A","approved":true}]}"#;
        let reg = Registry::from_json(untyped).unwrap();
        assert_eq!(reg.find("a").unwrap().kind, InstitutionType::Organization);

        let wrong = r#"{"status":"success","message":"","version":"","result":[{"type":"Thing","id":"a","name":"A","approved":true}]}"#;
        assert!(Registry::from_json(wrong).is_err());
    }

    #[test]
    fn version_is_optional() {
        let unversioned = r#"{"status":"success","message":"","result":[{"id":"a","name":"A","approved":true}]}"#;
        assert!(Registry::from_json(unversioned).is_ok());
    }

    #[test]
    fn institutions_name_their_idp_sign_out() {
        let reg = Registry::from_json(MOCK).unwrap();
        assert!(reg.find("utexas").unwrap().idp_logout_url.is_some());
        assert!(reg.find("globus").unwrap().idp_logout_url.is_none());
    }
}
