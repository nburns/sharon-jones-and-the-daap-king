//! Subsonic authentication modes.

use md5::{Digest, Md5};

/// Credentials for talking to a Subsonic-compatible server.
#[derive(Debug, Clone)]
pub enum Credentials {
    /// OpenSubsonic API token — preferred. Sent as `?apiKey=...`.
    ApiKey(String),
    /// Legacy Subsonic auth. On each request we generate a fresh salt and
    /// send `?u=<user>&t=MD5(password+salt)&s=<salt>`.
    UserPassword { user: String, password: String },
}

impl Credentials {
    pub fn identity(&self) -> String {
        match self {
            Credentials::ApiKey(k) => format!("apiKey:{}", short_hash(k)),
            Credentials::UserPassword { user, .. } => format!("user:{}", user),
        }
    }
}

/// Emit the auth query params for a given call: `[(key, value), ...]`.
/// Callers append these to their request query string.
pub(crate) fn auth_params(creds: &Credentials) -> Vec<(&'static str, String)> {
    match creds {
        Credentials::ApiKey(k) => vec![("apiKey", k.clone())],
        Credentials::UserPassword { user, password } => {
            let salt = random_salt();
            let token = md5_hex(&format!("{}{}", password, salt));
            vec![
                ("u", user.clone()),
                ("t", token),
                ("s", salt),
            ]
        }
    }
}

fn md5_hex(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

fn random_salt() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    // 16-char hex salt — plenty of entropy for per-request freshness.
    (0..16).map(|_| format!("{:x}", rng.random_range(0u8..=15))).collect()
}

fn short_hash(s: &str) -> String {
    let full = md5_hex(s);
    full.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_matches_reference_vector() {
        // From the Subsonic API docs: password="sesame", salt="c19b2d"
        // → token="26719a1196d2a940705a59634eb18eab"
        let token = md5_hex(&format!("{}{}", "sesame", "c19b2d"));
        assert_eq!(token, "26719a1196d2a940705a59634eb18eab");
    }

    #[test]
    fn api_key_mode_sends_single_param() {
        let p = auth_params(&Credentials::ApiKey("abc123".into()));
        assert_eq!(p, vec![("apiKey", "abc123".to_string())]);
    }

    #[test]
    fn user_pass_mode_sends_three_params_with_fresh_salt() {
        let creds = Credentials::UserPassword { user: "u".into(), password: "p".into() };
        let p1 = auth_params(&creds);
        let p2 = auth_params(&creds);
        // salts differ between calls
        let s1 = p1.iter().find(|(k, _)| *k == "s").unwrap().1.clone();
        let s2 = p2.iter().find(|(k, _)| *k == "s").unwrap().1.clone();
        assert_ne!(s1, s2);
        // fixed keys
        assert!(p1.iter().any(|(k, v)| *k == "u" && v == "u"));
        assert!(p1.iter().any(|(k, _)| *k == "t"));
    }

    #[test]
    fn salt_is_16_hex_chars() {
        let s = random_salt();
        assert_eq!(s.len(), 16);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
