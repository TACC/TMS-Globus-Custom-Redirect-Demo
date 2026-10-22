//! Approved-institution registry.
//!
//! In production this list would come from TMS (e.g. a table next to
//! `identity_providers` / `allowed_redirects`) or an admin API. Here it is
//! loaded from a mocked JSON response that uses a Tapis-style envelope:
//! `{ "status", "message", "version", "result": [...] }`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Institution {
    /// Stable id used by the UI (`/login?institution=<id>`).
    pub id: String,
    pub name: String,
    /// Domain Globus associates with the institution's IdP, e.g. "uchicago.edu".
    /// Passed verbatim as `redirect_idp_domain` on the authorize request.
    pub redirect_idp_domain: String,
    pub approved: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstitutionsResponse {
    pub status: String,
    pub message: String,
    pub version: String,
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

    /// Only approved institutions are ever exposed to the UI.
    pub fn approved(&self) -> Vec<Institution> {
        self.institutions.iter().filter(|i| i.approved).cloned().collect()
    }

    /// Lookup that refuses unapproved or unknown ids, so a hand-edited
    /// `?institution=` query string can't steer the redirect.
    pub fn find_approved(&self, id: &str) -> Option<&Institution> {
        self.institutions.iter().find(|i| i.approved && i.id == id)
    }
}

/// `redirect_idp_domain` only changes what the user *sees* (it skips the Globus
/// IdP picker). It does not stop someone from coming back with a different
/// identity, so the callback must check the identity it actually received.
///
/// Globus institutional usernames look like `jdoe@uchicago.edu`.
pub fn identity_matches(inst: &Institution, username: &str) -> bool {
    let username = username.to_ascii_lowercase();
    let domain = inst.redirect_idp_domain.to_ascii_lowercase();
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
    fn unapproved_institutions_are_hidden() {
        let reg = Registry::from_json(MOCK).unwrap();
        assert!(reg.approved().iter().all(|i| i.approved));
        assert!(reg.find_approved("example").is_none());
        assert!(reg.find_approved("uchicago").is_some());
        assert!(reg.find_approved("nope").is_none());
    }

    #[test]
    fn identity_must_be_from_the_institution_domain() {
        let reg = Registry::from_json(MOCK).unwrap();
        let uc = reg.find_approved("uchicago").unwrap();
        assert!(identity_matches(uc, "jdoe@uchicago.edu"));
        assert!(identity_matches(uc, "JDoe@UChicago.edu"));
        assert!(!identity_matches(uc, "jdoe@globusid.org"));
        assert!(!identity_matches(uc, "jdoe@evil-uchicago.edu"));
        assert!(!identity_matches(uc, "uchicago.edu"));
    }
}
