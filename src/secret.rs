// Project:   factbook
// File:      src/secret.rs
// Purpose:   String newtype that will not print itself
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! A credential that survives being formatted into a log line.
//!
//! Provider credentials (a MaxMind licence key, an IPinfo token) travel through
//! request-plan structs that get `{:?}`-formatted into traces and error reports.
//! A derived `Debug` on any of those prints the secret, so the type carries its
//! own redaction rather than relying on every holder to remember.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What a [`Secret`] renders as.
const REDACTED: &str = "***REDACTED***";

/// A string that never reveals itself except on an explicit call.
///
/// `Debug`, `Display` and `Serialize` all render the redaction. Reading the
/// value is deliberately [`Secret::expose`], so every use of the plaintext is
/// greppable.
///
/// ```
/// use factbook::Secret;
///
/// let key = Secret::from("licence-abcd");
/// assert_eq!(format!("{key:?}"), "***REDACTED***");
/// assert_eq!(serde_json::to_string(&key).unwrap(), "\"***REDACTED***\"");
/// assert_eq!(key.expose(), "licence-abcd");
/// ```
///
/// # Serialising does not round-trip
///
/// A config carrying one of these serialises with the redaction in place, so
/// writing a config back out and reading it again loses the credential. That is
/// the intended direction: a config is read from a secrets layer, and a dump of
/// it is the thing that ends up in a log aggregator.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Serialize for Secret {
    /// Writes the redaction, never the value.
    ///
    /// A derived implementation would put the plaintext into any config dump,
    /// which is exactly the leak this type exists to prevent.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(REDACTED)
    }
}

impl Secret {
    /// Read the plaintext.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the credential is empty, which reads as "not configured".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Redacts as well, so a `{}` in a format string is no more revealing than a
/// `{:?}`.
impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_both_redact() {
        let secret = Secret::from("token-wxyz");
        assert_eq!(format!("{secret:?}"), REDACTED);
        assert_eq!(format!("{secret}"), REDACTED);
    }

    #[test]
    fn a_struct_holding_one_does_not_leak_it_through_a_derived_debug() {
        // Both fields are read only by the derived Debug, which rustc excludes
        // from dead-code analysis -- and that Debug is the subject of the test.
        #[derive(Debug)]
        #[allow(dead_code)]
        struct RequestPlan {
            url: &'static str,
            token: Secret,
        }

        let rendered = format!(
            "{:?}",
            RequestPlan {
                url: "https://example.invalid/db.mmdb",
                token: Secret::from("token-wxyz"),
            }
        );

        assert!(!rendered.contains("token-wxyz"), "{rendered}");
        assert!(rendered.contains(REDACTED), "{rendered}");
        // The redaction is targeted: the rest of the plan still reports itself,
        // which is what makes the struct worth logging at all.
        assert!(
            rendered.contains("https://example.invalid/db.mmdb"),
            "{rendered}"
        );
    }

    #[test]
    fn expose_returns_the_plaintext() {
        assert_eq!(Secret::from("licence-abcd").expose(), "licence-abcd");
    }

    #[test]
    fn serialising_writes_the_redaction_not_the_value() {
        // A derived Serialize put the plaintext into any config dump, which is
        // the leak this type exists to prevent.
        let secret = Secret::from("token-wxyz");
        let json = serde_json::to_string(&secret).unwrap();

        assert_eq!(json, r#""***REDACTED***""#);
        assert!(!json.contains("token-wxyz"), "{json}");
    }

    #[test]
    fn deserialising_reads_the_plaintext_a_secrets_layer_supplied() {
        let secret: Secret = serde_json::from_str(r#""token-wxyz""#).unwrap();
        assert_eq!(secret.expose(), "token-wxyz");
    }

    #[test]
    fn a_whole_config_serialises_without_its_credentials() {
        // The failure this guards is a consumer logging its resolved config as
        // JSON, which the config module's own example invites.
        let config = crate::geoip::AutoDownloadConfig {
            maxmind_account_id: Some("account-1234".into()),
            maxmind_license_key: Some("licence-abcd".into()),
            ipinfo_token: Some("token-wxyz".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();

        assert!(!json.contains("account-1234"), "{json}");
        assert!(!json.contains("licence-abcd"), "{json}");
        assert!(!json.contains("token-wxyz"), "{json}");
    }

    #[test]
    fn empty_reads_as_not_configured() {
        assert!(Secret::from("").is_empty());
        assert!(!Secret::from("x").is_empty());
    }
}
