//! Demo settings, loaded from an untracked `dev.env` file (`KEY=VALUE` lines;
//! see `dev.env.example`). A real environment variable with the same name
//! takes precedence, so one-off overrides still work. Empty values count as
//! unset, so the defaults apply.

use std::collections::HashMap;

/// Names the settings file; the only setting read straight from the environment.
pub const FILE_VAR: &str = "DEV_ENV_FILE";
pub const DEFAULT_FILE: &str = "dev.env";

pub struct Settings {
    file: HashMap<String, String>,
}

impl Settings {
    /// Load the settings file. A missing file is fine (environment only); an
    /// unreadable one is not.
    pub fn load() -> Result<(Self, String), String> {
        let path = std::env::var(FILE_VAR).unwrap_or_else(|_| DEFAULT_FILE.to_string());
        let file = match std::fs::read_to_string(&path) {
            Ok(raw) => parse(&raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(format!("reading {path}: {e}")),
        };
        Ok((Self { file }, path))
    }

    pub fn len(&self) -> usize {
        self.file.len()
    }

    pub fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.file.get(key).cloned())
            .filter(|v| !v.trim().is_empty())
    }

    pub fn or(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or_else(|| default.to_string())
    }
}

/// `KEY=VALUE` per line; blank lines, `#` comments and a leading `export ` are
/// ignored, and one pair of surrounding quotes is stripped. No interpolation.
fn parse(raw: &str) -> HashMap<String, String> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            let v = v.trim();
            let unquoted = [('"', '"'), ('\'', '\'')]
                .iter()
                .find_map(|(a, b)| v.strip_prefix(*a)?.strip_suffix(*b))
                .unwrap_or(v);
            (k.trim().to_string(), unquoted.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_lines() {
        let m = parse(
            "# comment\n\nBASE_URL=http://localhost:8080\nexport A = 'x y'\nB=\"q=1\"\nEMPTY=\nnot a setting\n",
        );
        assert_eq!(m["BASE_URL"], "http://localhost:8080");
        assert_eq!(m["A"], "x y");
        assert_eq!(m["B"], "q=1");
        assert_eq!(m["EMPTY"], "");
        assert_eq!(m.len(), 4);
    }

    #[test]
    fn empty_values_fall_back_to_defaults() {
        let s = Settings { file: parse("IDP_DEMO_TEST_EMPTY=\nIDP_DEMO_TEST_SET=v\n") };
        assert_eq!(s.or("IDP_DEMO_TEST_EMPTY", "default"), "default");
        assert_eq!(s.or("IDP_DEMO_TEST_SET", "default"), "v");
        assert_eq!(s.get("IDP_DEMO_TEST_MISSING"), None);
    }
}
