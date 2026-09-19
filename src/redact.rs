//! Redact credentials so logs never print Access URLs or tokens.

use url::Url;

/// Replace userinfo in https URLs. Non-URLs become a short placeholder.
pub fn redact(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match Url::parse(trimmed) {
        Ok(mut url) => {
            if !url.username().is_empty() || url.password().is_some() {
                let _ = url.set_username("***");
                let _ = url.set_password(Some("***"));
            }
            url.to_string()
        }
        Err(_) => {
            if trimmed.len() > 12 {
                format!("{}…({} chars)", &trimmed[..4], trimmed.len())
            } else {
                "(redacted)".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_basic_auth() {
        let s = redact("https://user:secret@bridge.simplefin.org/simplefin");
        assert!(!s.contains("secret"));
        assert!(!s.contains("user:"));
        assert!(s.contains("bridge.simplefin.org"));
    }
}
