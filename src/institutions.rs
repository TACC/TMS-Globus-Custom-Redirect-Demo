//! Approved-institution registry.
//!
//! In production this list would come from TMS (e.g. a table next to
//! `identity_providers` / `allowed_redirects`) or an admin API. Here it is
//! read from a mocked registry file, `{ "data_version", "institutions": [...] }`,
//! holding approved and unapproved entries alike. `institutions_api` serves the
//! approved ones in the shape api/openapi.yaml defines.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

/// Which login flow an entry uses, named after tms_portal's provider types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// Globus Auth, optionally steered by `redirect_idp_domain`.
    Globus,
    /// Tapis OAuth2 directly (tacc.tapis.io), not through Globus.
    TaccTapis,
}

/// JSON-LD type of an entry (`type`, aliased to `@type` by the JSON-LD
/// context, api/institutions.context.r<n>.jsonld). Written out, never read from the registry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub enum InstitutionType {
    #[default]
    Organization,
}

/// JSON-LD type of the list as a whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub enum ListType {
    #[default]
    ItemList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Institution {
    #[serde(rename = "type", skip_deserializing)]
    pub kind: InstitutionType,
    /// Stable id used by the UI (`/login?institution=<id>`).
    pub id: String,
    pub name: String,
    pub provider: Provider,
    /// Domain Globus associates with the institution's IdP, e.g. "uchicago.edu".
    /// Passed verbatim as `redirect_idp_domain` on the authorize request. `None`
    /// means a plain login (Globus's organization picker, or Tapis) that accepts
    /// whatever identity comes back.
    pub redirect_idp_domain: Option<String>,
    /// Sign-out URL of the institution's own IdP. Globus does not pass
    /// `prompt=login` on to CILogon or the IdP, so ending the IdP's session is
    /// the only way to make the next login ask for a password again.
    pub idp_logout_url: Option<String>,
    pub approved: bool,
}

/// The registry file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    /// Revision of the data; served as the list's `data_version` and `ETag`.
    #[serde(default)]
    data_version: Option<String>,
    institutions: Vec<Institution>,
}

#[derive(Debug, Clone)]
pub struct Registry {
    data_version: Option<String>,
    institutions: Vec<Institution>,
}

impl Registry {
    /// Parses a registry file and checks every entry against the rules
    /// api/openapi.yaml sets, so the listing never serves an invalid entry.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let file: RegistryFile = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let reg = Self { data_version: file.data_version, institutions: file.institutions };
        reg.validate()?;
        Ok(reg)
    }

    pub fn load(path: &str) -> Result<Self, RegistryError> {
        let raw = std::fs::read_to_string(path).map_err(|e| RegistryError::Unavailable(format!("reading {path}: {e}")))?;
        Self::from_json(&raw).map_err(|e| RegistryError::Invalid(format!("parsing {path}: {e}")))
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(v) = &self.data_version {
            if !is_valid_data_version(v) {
                return Err(format!("data_version {v:?} must be 1-64 visible ASCII characters other than '\"' and ','"));
            }
        }
        let mut seen = HashSet::new();
        for i in &self.institutions {
            let id = &i.id;
            if !is_valid_id(id) {
                return Err(format!("id {id:?} must be 1-64 lower-case letters, digits or _"));
            }
            if !seen.insert(id.as_str()) {
                return Err(format!("id {id:?} appears more than once"));
            }
            if i.name.trim().is_empty() || i.name.chars().count() > 256 {
                return Err(format!("{id}: name must be 1-256 characters, not all white space"));
            }
            match (&i.redirect_idp_domain, i.provider) {
                (Some(_), Provider::TaccTapis) => {
                    return Err(format!("{id}: tacc_tapis entries must have no redirect_idp_domain"))
                }
                (Some(d), _) if !is_valid_domain(d) => {
                    return Err(format!("{id}: redirect_idp_domain {d:?} is not a lower-case domain name"))
                }
                _ => {}
            }
            if let Some(u) = &i.idp_logout_url {
                if !is_valid_sign_out_url(u) {
                    return Err(format!("{id}: idp_logout_url {u:?} must be an https URI (RFC 3986) of at most 2048 characters"));
                }
            }
        }
        Ok(())
    }

    pub fn data_version(&self) -> Option<&str> {
        self.data_version.as_deref()
    }

    pub fn approved(&self) -> Vec<Institution> {
        self.institutions.iter().filter(|i| i.approved).cloned().collect()
    }

    /// Lookup by id, approved or not: unapproved institutions can sign in, and
    /// the callback then filters them out. Unknown ids are refused, so a
    /// hand-edited `?institution=` query string can't steer the redirect.
    pub fn find(&self, id: &str) -> Option<&Institution> {
        self.institutions.iter().find(|i| i.id == id)
    }
}

/// Why a request could not get the registry.
#[derive(Debug, Clone, PartialEq)]
pub enum RegistryError {
    /// It could not be read (e.g. the file is missing); a retry may succeed.
    Unavailable(String),
    /// It was read but breaks the spec's rules, so none of it is served.
    Invalid(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Unavailable(e) | RegistryError::Invalid(e) => f.write_str(e),
        }
    }
}

/// Where each request gets the registry from.
#[derive(Debug, Clone)]
pub enum RegistrySource {
    /// One registry, held in memory (the tests' source).
    #[cfg(test)]
    Fixed(Arc<Registry>),
    /// A registry file, read again for every request, so edits take effect
    /// without a restart and a broken file is reported rather than served.
    File(String),
}

impl RegistrySource {
    pub fn current(&self) -> Result<Arc<Registry>, RegistryError> {
        match self {
            #[cfg(test)]
            RegistrySource::Fixed(reg) => Ok(reg.clone()),
            // The file is small; a blocking read keeps this simple.
            RegistrySource::File(path) => Registry::load(path).map(Arc::new),
        }
    }
}

/// Whether `id` has the shape api/openapi.yaml gives institution ids
/// (`^[a-z0-9_]+$`, at most 64 long), so a malformed one can be refused as a
/// bad request rather than looked up.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The spec's `redirect_idp_domain` pattern: two or more labels of lower-case
/// letters, digits and inner hyphens, each 1-63 long, 253 at most in all. The
/// top-level label starts with a letter, so an IP address does not pass.
fn is_valid_domain(d: &str) -> bool {
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    let tld_ok = d.rsplit('.').next().is_some_and(|l| l.starts_with(|c: char| c.is_ascii_lowercase()));
    d.len() <= 253 && d.split('.').count() >= 2 && d.split('.').all(label_ok) && tld_ok
}

/// The spec's `idp_logout_url` rules: `format: uri` (RFC 3986, so ASCII only,
/// well-formed percent-encodings, and `[` `]` only around an IP-literal host),
/// `https` with a non-empty host, and at most 2048 characters.
fn is_valid_sign_out_url(u: &str) -> bool {
    // pchar without pct-encoded: unreserved / sub-delims / ":" / "@".
    let pchar = |c: u8| c.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&c);
    // Every byte allowed by `ok`, and every `%` followed by two hex digits.
    let chars_ok = |s: &str, ok: &dyn Fn(u8) -> bool| {
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                if !(b.get(i + 1).is_some_and(u8::is_ascii_hexdigit) && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)) {
                    return false;
                }
                i += 3;
            } else if ok(b[i]) {
                i += 1;
            } else {
                return false;
            }
        }
        true
    };
    let Some(rest) = u.strip_prefix("https://") else { return false };
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let ip_literal = |c: u8| c == b'[' || c == b']';
    u.len() <= 2048
        && !authority.is_empty()
        && chars_ok(authority, &|c| pchar(c) || ip_literal(c))
        && chars_ok(path, &|c| pchar(c) || c == b'/')
        && chars_ok(query, &|c| pchar(c) || c == b'/' || c == b'?')
        && chars_ok(fragment, &|c| pchar(c) || c == b'/' || c == b'?')
        && url::Url::parse(u).is_ok_and(|p| p.host_str().is_some_and(|h| !h.is_empty()))
}

/// The spec's `data_version` pattern (1-64 visible ASCII characters other than `"`
/// and `,`): usable inside a quoted `ETag`, and never split apart by the commas
/// that separate tags in `If-None-Match`.
fn is_valid_data_version(v: &str) -> bool {
    (1..=64).contains(&v.len()) && v.bytes().all(|b| (b'!'..=b'~').contains(&b) && b != b'"' && b != b',')
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

    /// A one-entry registry with `field` set to `value` in its entry.
    fn with(field: &str, value: serde_json::Value) -> Result<Registry, String> {
        let mut entry = serde_json::json!({
            "id": "a", "name": "A", "provider": "globus",
            "redirect_idp_domain": "a.edu", "idp_logout_url": null, "approved": true
        });
        entry[field] = value;
        Registry::from_json(&serde_json::json!({ "institutions": [entry] }).to_string())
    }

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
    fn json_ld_type_is_written_but_not_read() {
        let reg = Registry::from_json(MOCK).unwrap();
        let out = serde_json::to_value(reg.find("uchicago").unwrap()).unwrap();
        assert_eq!(out["type"], "Organization");
        assert!(with("type", "Organization".into()).is_err());
    }

    #[test]
    fn ids_must_match_the_spec_pattern() {
        assert!(Registry::from_json(MOCK).is_ok());
        for bad in ["", "UChicago", "u chicago", "uchicago.edu", "../x", &"a".repeat(65)] {
            assert!(!is_valid_id(bad), "{bad:?}");
            assert!(with("id", bad.into()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ids_must_be_unique() {
        let entry = r#"{"id":"a","name":"A","provider":"globus","redirect_idp_domain":null,"idp_logout_url":null,"approved":true}"#;
        let twice = format!(r#"{{"institutions":[{entry},{entry}]}}"#);
        assert!(Registry::from_json(&twice).unwrap_err().contains("more than once"));
    }

    #[test]
    fn every_field_is_required_and_unknown_ones_are_refused() {
        for field in ["id", "name", "provider", "redirect_idp_domain", "idp_logout_url", "approved"] {
            let mut entry = serde_json::json!({
                "id": "a", "name": "A", "provider": "globus",
                "redirect_idp_domain": null, "idp_logout_url": null, "approved": true
            });
            entry.as_object_mut().unwrap().remove(field);
            let json = serde_json::json!({ "institutions": [entry] }).to_string();
            // Option fields default to None when missing, which the spec's
            // output always writes as null, so only the others must be present.
            let optional = matches!(field, "redirect_idp_domain" | "idp_logout_url");
            assert_eq!(Registry::from_json(&json).is_ok(), optional, "{field}");
        }
        assert!(with("extra", 1.into()).is_err());
    }

    #[test]
    fn version_is_optional_and_etag_safe() {
        let unversioned = r#"{"institutions":[]}"#;
        assert_eq!(Registry::from_json(unversioned).unwrap().data_version(), None);
        assert_eq!(Registry::from_json(MOCK).unwrap().data_version(), Some("mock-0.1.0"));
        // The pre-0.8 key is refused, so a stale file cannot silently lose its revision.
        assert!(Registry::from_json(r#"{"version":"x","institutions":[]}"#).is_err());
        for bad in ["", "a b", "a\"b", "a,b", &"v".repeat(65)] {
            let json = serde_json::json!({ "data_version": bad, "institutions": [] }).to_string();
            assert!(Registry::from_json(&json).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn domains_must_be_lower_case_domain_names() {
        for good in ["uchicago.edu", "a.b-c.edu", "x1.y2"] {
            assert!(with("redirect_idp_domain", good.into()).is_ok(), "{good:?}");
        }
        for bad in ["..", "-", ".edu", "edu", "UChicago.edu", "a..edu", "-a.edu", "a-.edu", "a_b.edu", "10.0.0.1", "123.456"] {
            assert!(with("redirect_idp_domain", bad.into()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names_are_not_blank() {
        for bad in ["", "   ", "\t"] {
            assert!(with("name", bad.into()).is_err(), "{bad:?}");
        }
        assert!(with("name", " A ".into()).is_ok());
    }

    #[test]
    fn tapis_entries_have_no_domain() {
        let mut entry = serde_json::json!({
            "id": "t", "name": "T", "provider": "tacc_tapis",
            "redirect_idp_domain": "tacc.edu", "idp_logout_url": null, "approved": true
        });
        let json = |e: &serde_json::Value| serde_json::json!({ "institutions": [e] }).to_string();
        assert!(Registry::from_json(&json(&entry)).is_err());
        entry["redirect_idp_domain"] = serde_json::Value::Null;
        assert!(Registry::from_json(&json(&entry)).is_ok());
    }

    #[test]
    fn sign_out_urls_must_be_https() {
        assert!(with("idp_logout_url", "https://idp.a.edu/logout".into()).is_ok());
        for good in ["https://idp.a.edu/a%20b?x=1&y=/z?#frag/?", "https://idp.a.edu", "https://idp.a.edu:8443/logout"] {
            assert!(with("idp_logout_url", good.into()).is_ok(), "{good:?}");
        }
        // Non-ASCII, brackets outside the host, a second `#` and a broken
        // percent-encoding are not RFC 3986 URIs, though url::Url parses them.
        let long = format!("https://idp.a.edu/{}", "a".repeat(2048));
        for bad in [
            "http://idp.a.edu/logout",
            "https://",
            "idp.a.edu/logout",
            "https://idp.a.edu/log out",
            "https://a.edu/\u{fc}[x]",
            "https://a.edu/x[1]",
            "https://a.edu/a#b#c",
            "https://a.edu/%zz",
            "https://a.edu/%2",
            "https://a.edu/a|b",
            long.as_str(),
        ] {
            assert!(with("idp_logout_url", bad.into()).is_err(), "{bad:?}");
        }
        let reg = Registry::from_json(MOCK).unwrap();
        assert!(reg.find("utexas").unwrap().idp_logout_url.is_some());
        assert!(reg.find("globus").unwrap().idp_logout_url.is_none());
    }

    /// A registry file is read per request: edits show at once, a missing
    /// file is unavailable and an invalid one is refused.
    #[test]
    fn file_source_reads_the_file_each_time() {
        let path = std::env::temp_dir().join(format!("registry-{}.json", uuid::Uuid::new_v4()));
        let source = RegistrySource::File(path.to_str().unwrap().into());
        assert!(matches!(source.current(), Err(RegistryError::Unavailable(_))));
        std::fs::write(&path, MOCK).unwrap();
        assert_eq!(source.current().unwrap().data_version(), Some("mock-0.1.0"));
        std::fs::write(&path, MOCK.replace("mock-0.1.0", "mock-0.1.1")).unwrap();
        assert_eq!(source.current().unwrap().data_version(), Some("mock-0.1.1"));
        std::fs::write(&path, "{").unwrap();
        assert!(matches!(source.current(), Err(RegistryError::Invalid(_))));
        std::fs::remove_file(&path).unwrap();
    }
}
