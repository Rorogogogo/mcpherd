use std::sync::LazyLock;

use regex::Regex;

/// A secret found in a piece of config text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub kind: &'static str,
    pub value: String,
}

struct Pattern {
    kind: &'static str,
    re: Regex,
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    let p = |kind, re: &str| Pattern {
        kind,
        re: Regex::new(re).unwrap(),
    };
    vec![
        // The secret is always capture group 1.
        p(
            "token",
            r"\b(ntn_[A-Za-z0-9]{20,}|secret_[A-Za-z0-9]{30,}|sk-ant-[A-Za-z0-9_-]{20,}|sk-[A-Za-z0-9_-]{20,}|gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,}|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|glpat-[A-Za-z0-9_-]{20,}|AIza[0-9A-Za-z_-]{35})",
        ),
        p(
            "url credentials",
            r"[a-z][a-z0-9+.-]*://[^/\s:@]+:([^@\s/]+)@",
        ),
        p(
            "secret variable",
            r"\b[A-Z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIALS?|AUTH)[A-Z0-9_]*=([^\s'$][^\s']*)",
        ),
        p(
            "secret flag",
            r"(?i)--(?:api[-_]?key|token|secret|password|auth[-_]?token)(?:=|\s+)([^\s'$-][^\s']*)",
        ),
    ]
});

static SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|AUTH)").unwrap()
});

pub fn scan(text: &str) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    for pattern in PATTERNS.iter() {
        for caps in pattern.re.captures_iter(text) {
            let value = caps[1].to_string();
            if value.len() >= 8 && !hits.iter().any(|h| h.value == value) {
                hits.push(Hit {
                    kind: pattern.kind,
                    value,
                });
            }
        }
    }
    hits
}

/// Replace every secret in `text` with a masked form.
pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for hit in scan(text) {
        out = out.replace(&hit.value, &mask(&hit.value));
    }
    out
}

/// Keep a short prefix so the user can tell which secret it is.
pub fn mask(value: &str) -> String {
    let prefix: String = value.chars().take(4).collect();
    format!("{prefix}…")
}

pub fn is_secret_name(name: &str) -> bool {
    SECRET_NAME.is_match(name)
}

/// Values like `${VAR}` or `${credentials.x}` point elsewhere instead of holding the secret.
pub fn is_reference(value: &str) -> bool {
    value.is_empty() || value.contains("${") || value.starts_with('$')
}

#[cfg(test)]
mod tests {
    use super::*;

    // All values below are fake.
    const FAKE_NOTION: &str = "ntn_FAKEFAKEFAKEFAKEFAKEFAKE0000";

    #[test]
    fn finds_inline_env_assignment() {
        let hits = scan(&format!(
            "NOTION_API_KEY={FAKE_NOTION} npx notion-mcp-server"
        ));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].value, FAKE_NOTION);
    }

    #[test]
    fn finds_url_credentials_and_flags() {
        let hits =
            scan("postgres-mcp postgresql://admin:notreal123@db:5432/x --api-key notreal456");
        let values: Vec<_> = hits.iter().map(|h| h.value.as_str()).collect();
        assert_eq!(values, ["notreal123", "notreal456"]);
    }

    #[test]
    fn ignores_references_and_short_values() {
        assert!(scan("API_KEY=${API_KEY} --token $TOKEN PASSWORD=x").is_empty());
    }

    #[test]
    fn redact_masks_but_keeps_prefix() {
        let out = redact("A_TOKEN=notarealvalue run");
        assert_eq!(out, "A_TOKEN=nota… run");
    }

    #[test]
    fn secret_names() {
        assert!(is_secret_name("JOBJOURNEY_API_KEY"));
        assert!(!is_secret_name("JOBJOURNEY_API_URL"));
        assert!(is_reference("${credentials.jobjourney_api_key}"));
    }
}
