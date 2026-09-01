// Project:   factbook
// File:      src/geoip/config.rs
// Purpose:   GeoIP database provisioning configuration types
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Configuration for GeoIP database provisioning.
//!
//! Describes which databases a service wants and how to obtain them. It carries
//! nothing about what the databases are used for -- lookup caches, field
//! mappings and enrichment toggles stay in the consuming application.
//!
//! ## Config cascade example
//!
//! ```yaml
//! geoip:
//!   enabled: true
//!   # One provider for both databases, or one per database kind, and either
//!   # form may name a paid tier:
//!   #   provider:
//!   #     city:
//!   #       provider: max_mind
//!   #       tier: paid
//!   #     asn: sapics_origin_asn
//!   provider: db_ip
//!   # An explicit path overrides the provider for that database alone, and
//!   # nothing is downloaded for it.
//!   city_db_path: null
//!   asn_db_path: null
//!   auto_download:
//!     enabled: true
//!     data_dir: /var/lib/geoip
//!     max_age_days: 30
//!     # Link quality is a deployment property: connect fast, then tolerate a
//!     # slow but progressing transfer.
//!     connect_timeout_secs: 30
//!     read_timeout_secs: 60
//!     # A download that parses but answers nothing is refused, as is one a
//!     # fraction of the size of the copy it would replace. Both are advisory:
//!     # turn either off for a database this crate models badly.
//!     verify_content: true
//!     min_size_percent: 50
//!     # Credentials are Secret: redacted in Debug and Display. Supply them
//!     # through the secrets layer, not as literals.
//!     maxmind_account_id: null
//!     maxmind_license_key: null
//!     ipinfo_token: null
//! ```

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Secret;

/// Default directory for downloaded database files.
const DEFAULT_DATA_DIR: &str = "/var/lib/geoip";

/// Default staleness threshold, in days, before a re-download is attempted.
const DEFAULT_MAX_AGE_DAYS: u32 = 30;

/// Default seconds to wait for the connection itself.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 30;

/// Default seconds a transfer may go without receiving any bytes.
const DEFAULT_READ_TIMEOUT_SECS: u64 = 60;

/// Default floor, as a percentage of the copy it replaces, under which a
/// downloaded database is refused.
///
/// Provider builds move by single-digit percentages month to month, and a
/// rebuild that genuinely halves a database is not something any of the modelled
/// providers has done, so half leaves a wide margin over normal variation while
/// still catching a stub or a partial build.
const DEFAULT_MIN_SIZE_PERCENT: u8 = 50;

/// Source of the databases.
///
/// The tier is a separate axis -- see [`ProviderTier`] -- so this names the
/// provider, not one of its products. Which provider, tier and database kind
/// combinations are modelled is data rather than documentation: a selection the
/// source table has no row for is reported by
/// [`validate`](super::download::validate) rather than guessed at.
///
/// Nothing is bundled with this crate. The deploying organisation fetches from
/// the provider's own endpoint under that provider's terms, and what those
/// terms require -- a licence, an attribution line, a fetch ceiling -- is
/// readable at runtime through
/// [`source_terms`](super::download::source_terms).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeoIpProvider {
    /// DB-IP. The free line is DB-IP Lite: anonymous, on a dated monthly URL.
    #[default]
    DbIp,

    /// MaxMind, on an account id plus licence key for either line. The free
    /// line is GeoLite2, the paid line GeoIP2.
    MaxMind,

    /// IPinfo. The free line is IPinfo Lite, on a token, country and ASN only.
    IpInfo,

    /// The `origin-asn` dataset in `sapics/ip-location-db` -- ASN origins from
    /// public routing data, carrying operator names. Anonymous, undated URL,
    /// public domain, and the widest ASN coverage of the free sources.
    SapicsOriginAsn,

    /// The `iptoasn-asn` dataset in `sapics/ip-location-db` -- the same
    /// provenance as [`Self::SapicsOriginAsn`], naming operators by their registry
    /// handle rather than their legal name.
    SapicsIpToAsn,

    /// Caller supplies the database paths directly; nothing is downloaded.
    Custom,
}

impl GeoIpProvider {
    /// Label used in error messages and log fields.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DbIp => "DbIp",
            Self::MaxMind => "MaxMind",
            Self::IpInfo => "IpInfo",
            Self::SapicsOriginAsn => "SapicsOriginAsn",
            Self::SapicsIpToAsn => "SapicsIpToAsn",
            Self::Custom => "Custom",
        }
    }
}

/// Which product line of a provider to take a database from.
///
/// The tier is stated, never inferred from which credential happens to be set,
/// so a paid selection missing its credential is a config error rather than a
/// 401 at download time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTier {
    /// The provider's free line.
    #[default]
    Free,

    /// The provider's paid line, which needs whatever credential that line
    /// requires.
    Paid,
}

impl ProviderTier {
    /// Label used in error messages and log fields.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Paid => "paid",
        }
    }
}

/// Where one database comes from: the provider and its product line.
///
/// Written as a bare provider for the free line, or as a map to name the tier:
///
/// ```yaml
/// provider: db_ip
/// ```
///
/// ```yaml
/// provider:
///   provider: max_mind
///   tier: paid
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderChoice {
    /// Provider the database comes from.
    pub provider: GeoIpProvider,
    /// Product line of that provider.
    pub tier: ProviderTier,
}

impl From<GeoIpProvider> for ProviderChoice {
    /// A bare provider is its free line.
    fn from(provider: GeoIpProvider) -> Self {
        Self {
            provider,
            tier: ProviderTier::Free,
        }
    }
}

/// Provider for each database kind.
///
/// The providers are not symmetrical -- some publish only ASN, some only city
/// -- so each kind selects its own. The single-provider form stays the common
/// case and applies to both kinds:
///
/// ```yaml
/// provider: db_ip
/// ```
///
/// ```yaml
/// provider:
///   city:
///     provider: max_mind
///     tier: paid
///   asn: sapics_origin_asn
/// ```
///
/// A kind omitted from the map takes the default provider on its free tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderSelection {
    /// Where the city database comes from.
    pub city: ProviderChoice,
    /// Where the ASN database comes from.
    pub asn: ProviderChoice,
}

impl From<GeoIpProvider> for ProviderSelection {
    /// One provider, free tier, for both kinds.
    fn from(provider: GeoIpProvider) -> Self {
        Self::from(ProviderChoice::from(provider))
    }
}

impl From<ProviderChoice> for ProviderSelection {
    /// One choice for both kinds.
    fn from(choice: ProviderChoice) -> Self {
        Self {
            city: choice,
            asn: choice,
        }
    }
}

/// Serialised shape of [`ProviderChoice`]: a bare provider, or a tiered map.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ChoiceWire {
    Bare(GeoIpProvider),
    Tiered(TieredWire),
}

/// Map form of [`ProviderChoice`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TieredWire {
    provider: GeoIpProvider,
    #[serde(default)]
    tier: ProviderTier,
}

impl Serialize for ProviderChoice {
    /// Emits the bare form for the free tier, which is what an operator writes
    /// when they have not asked for a paid line.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.tier == ProviderTier::Free {
            self.provider.serialize(serializer)
        } else {
            TieredWire {
                provider: self.provider,
                tier: self.tier,
            }
            .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for ProviderChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match ChoiceWire::deserialize(deserializer)? {
            ChoiceWire::Bare(provider) => Self::from(provider),
            ChoiceWire::Tiered(TieredWire { provider, tier }) => Self { provider, tier },
        })
    }
}

/// Serialised shape of [`ProviderSelection`]: one choice, or one per kind.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum ProviderWire {
    Uniform(ProviderChoice),
    PerKind(PerKindWire),
}

/// Map form of [`ProviderSelection`].
///
/// `deny_unknown_fields` is what stops a misspelt key deserialising into an
/// all-defaults selection the operator never asked for.
#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PerKindWire {
    city: Option<ProviderChoice>,
    asn: Option<ProviderChoice>,
}

impl Serialize for ProviderSelection {
    /// Emits the single-provider form when both kinds agree, so a round trip
    /// through a config file keeps the shape an operator would write.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.city == self.asn {
            self.city.serialize(serializer)
        } else {
            PerKindWire {
                city: Some(self.city),
                asn: Some(self.asn),
            }
            .serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for ProviderSelection {
    /// Both wire forms land on the same struct, so equality compares what the
    /// selection means rather than how it was written.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match ProviderWire::deserialize(deserializer)? {
            ProviderWire::Uniform(choice) => Self::from(choice),
            ProviderWire::PerKind(PerKindWire { city, asn }) => Self {
                city: city.unwrap_or_default(),
                asn: asn.unwrap_or_default(),
            },
        })
    }
}

/// Auto-download settings.
///
/// The three credential fields are [`Secret`], so they are redacted in `Debug`
/// and `Display` output -- config dumps, trace fields, error reports. Only the
/// download call itself exposes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoDownloadConfig {
    /// Download a database when the local copy is missing or stale.
    ///
    /// When false, [`ensure_databases`](super::ensure_databases) returns only
    /// the files that already exist on disk.
    pub enabled: bool,

    /// Directory the downloaded files are written to.
    pub data_dir: PathBuf,

    /// MaxMind account id, required by [`GeoIpProvider::MaxMind`] on either
    /// tier.
    pub maxmind_account_id: Option<Secret>,

    /// MaxMind licence key, required by [`GeoIpProvider::MaxMind`] on either
    /// tier.
    pub maxmind_license_key: Option<Secret>,

    /// IPinfo API token, required by [`GeoIpProvider::IpInfo`].
    pub ipinfo_token: Option<Secret>,

    /// Age, in days, past which a local database is treated as stale.
    pub max_age_days: u32,

    /// Seconds to wait for the connection to the provider. A dead host fails
    /// here.
    pub connect_timeout_secs: u64,

    /// Seconds a transfer may go without receiving any bytes.
    ///
    /// This is an idle timeout, not a budget for the whole download: a database
    /// runs to hundreds of megabytes, so a total-time limit would make a slow
    /// link impossible rather than slow. A connection that has stopped
    /// delivering still fails inside this window.
    pub read_timeout_secs: u64,

    /// Refuse a downloaded database that resolves nothing for a well-known
    /// address.
    ///
    /// A file can be a structurally valid database, match the digest its
    /// provider published and still answer nothing: `dbip-asn` ships a valid
    /// database whose operator-name column is blank on every row. Set it false
    /// for a database whose schema this crate models badly, which gives up this
    /// check alone.
    ///
    /// Reading the file needs the lookup engine, so a build without
    /// `geoip-lookup` accepts the setting and has nothing to ask the file with.
    pub verify_content: bool,

    /// Smallest percentage of the copy it replaces a downloaded database may
    /// be, zero for no floor.
    ///
    /// This is what refuses a truncated but parseable file or a provider
    /// shipping a stub. A first download has nothing to compare against and is
    /// always accepted.
    pub min_size_percent: u8,
}

impl Default for AutoDownloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            maxmind_account_id: None,
            maxmind_license_key: None,
            ipinfo_token: None,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            read_timeout_secs: DEFAULT_READ_TIMEOUT_SECS,
            verify_content: true,
            min_size_percent: DEFAULT_MIN_SIZE_PERCENT,
        }
    }
}

/// GeoIP database provisioning configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeoIpConfig {
    /// Provision databases at all. Defaults to true: calling
    /// [`ensure_databases`](super::ensure_databases) is the opt-in, so this is
    /// the config-side opt-out for an app that wires the call unconditionally.
    pub enabled: bool,

    /// Where each database comes from.
    pub provider: ProviderSelection,

    /// Explicit city database path. Set it and the city provider is bypassed:
    /// nothing is downloaded and nothing is checked for that kind.
    pub city_db_path: Option<PathBuf>,

    /// Explicit ASN database path. See [`city_db_path`](Self::city_db_path).
    pub asn_db_path: Option<PathBuf>,

    /// Auto-download settings.
    pub auto_download: AutoDownloadConfig,

    /// HTTP client the transfers ride on.
    ///
    /// `None` builds a default rustls client carrying the timeouts above.
    /// `Some` uses the caller's client as it stands -- its proxy, its root
    /// store, its timeouts -- which is how a deployment behind a proxy or
    /// trusting a private CA configures the transfers.
    ///
    /// Not a config-file field: it is a live handle, so serde skips it and
    /// [`with_http_client`](Self::with_http_client) sets it.
    #[serde(skip)]
    pub http_client: Option<reqwest::Client>,
}

impl Default for GeoIpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: ProviderSelection::default(),
            city_db_path: None,
            asn_db_path: None,
            auto_download: AutoDownloadConfig::default(),
            http_client: None,
        }
    }
}

/// Equality is over the configuration, not the transport.
///
/// A consumer nests this inside its own config and compares old against new to
/// decide whether a reload needs a restart, so the comparison has to cover every
/// field an operator can set. The injected client is excluded because it is a
/// handle rather than a setting, and `reqwest::Client` has no equality to defer
/// to.
impl PartialEq for GeoIpConfig {
    fn eq(&self, other: &Self) -> bool {
        self.enabled == other.enabled
            && self.provider == other.provider
            && self.city_db_path == other.city_db_path
            && self.asn_db_path == other.asn_db_path
            && self.auto_download == other.auto_download
    }
}

impl Eq for GeoIpConfig {}

impl GeoIpConfig {
    /// Run the transfers through a client the caller has already configured.
    #[must_use]
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_default_is_dbip() {
        assert_eq!(GeoIpProvider::default(), GeoIpProvider::DbIp);
        assert_eq!(ProviderTier::default(), ProviderTier::Free);
    }

    #[test]
    fn provider_variants_compare_distinctly() {
        assert_eq!(GeoIpProvider::DbIp, GeoIpProvider::DbIp);
        assert_ne!(GeoIpProvider::DbIp, GeoIpProvider::MaxMind);
        assert_ne!(GeoIpProvider::Custom, GeoIpProvider::SapicsOriginAsn);
        assert_ne!(GeoIpProvider::SapicsOriginAsn, GeoIpProvider::IpInfo);
    }

    #[test]
    fn a_tier_is_part_of_the_choice_not_of_the_provider() {
        // The same provider on two tiers is two different selections, which is
        // what stops a paid selection reading as its free line.
        let free = ProviderChoice::from(GeoIpProvider::MaxMind);
        let paid = ProviderChoice {
            provider: GeoIpProvider::MaxMind,
            tier: ProviderTier::Paid,
        };

        assert_eq!(free.tier, ProviderTier::Free);
        assert_ne!(free, paid);
        assert_eq!(free.provider, paid.provider);
    }

    #[test]
    fn a_changed_credential_compares_unequal() {
        let mut changed = GeoIpConfig::default();
        changed.auto_download.ipinfo_token = Some(Secret::from("a-token"));

        assert_ne!(GeoIpConfig::default(), changed);
        assert_eq!(changed, changed.clone());
    }

    #[test]
    fn an_injected_client_is_not_a_config_change() {
        // The client is a transport handle, so swapping it must not read as a
        // config edit that a consumer would restart on.
        let plain = GeoIpConfig::default();
        let injected = GeoIpConfig::default().with_http_client(reqwest::Client::new());

        assert_eq!(plain, injected);
        assert!(injected.http_client.is_some());
    }

    #[test]
    fn provider_round_trips_through_snake_case() {
        for (provider, wire) in [
            (GeoIpProvider::DbIp, "db_ip"),
            (GeoIpProvider::MaxMind, "max_mind"),
            (GeoIpProvider::IpInfo, "ip_info"),
            (GeoIpProvider::SapicsOriginAsn, "sapics_origin_asn"),
            (GeoIpProvider::SapicsOriginAsn, "sapics_origin_asn"),
            (GeoIpProvider::Custom, "custom"),
        ] {
            let quoted = format!("\"{wire}\"");
            assert_eq!(serde_json::to_string(&provider).unwrap(), quoted);
            let decoded: GeoIpProvider = serde_json::from_str(&quoted).unwrap();
            assert_eq!(decoded, provider);
        }
    }

    #[test]
    fn a_single_provider_applies_to_both_kinds() {
        let selection: ProviderSelection = serde_json::from_str("\"max_mind\"").unwrap();
        assert_eq!(selection.city, ProviderChoice::from(GeoIpProvider::MaxMind));
        assert_eq!(selection.asn, ProviderChoice::from(GeoIpProvider::MaxMind));
        assert_eq!(serde_json::to_string(&selection).unwrap(), "\"max_mind\"");
    }

    #[test]
    fn a_per_kind_map_selects_each_provider() {
        let selection: ProviderSelection =
            serde_json::from_str(r#"{"city": "ip_info", "asn": "sapics_origin_asn"}"#).unwrap();
        assert_eq!(selection.city, ProviderChoice::from(GeoIpProvider::IpInfo));
        assert_eq!(
            selection.asn,
            ProviderChoice::from(GeoIpProvider::SapicsOriginAsn)
        );

        let dumped = serde_json::to_string(&selection).unwrap();
        assert_eq!(
            serde_json::from_str::<ProviderSelection>(&dumped).unwrap(),
            selection
        );
    }

    #[test]
    fn a_tier_is_named_in_the_config_not_inferred() {
        let selection: ProviderSelection =
            serde_json::from_str(r#"{"provider": "max_mind", "tier": "paid"}"#).unwrap();
        assert_eq!(selection.city.provider, GeoIpProvider::MaxMind);
        assert_eq!(selection.city.tier, ProviderTier::Paid);
        assert_eq!(selection.asn.tier, ProviderTier::Paid);

        let dumped = serde_json::to_string(&selection).unwrap();
        assert_eq!(dumped, r#"{"provider":"max_mind","tier":"paid"}"#);
        assert_eq!(
            serde_json::from_str::<ProviderSelection>(&dumped).unwrap(),
            selection
        );
    }

    #[test]
    fn one_kind_may_be_paid_and_the_other_free() {
        let selection: ProviderSelection = serde_json::from_str(
            r#"{"city": {"provider": "max_mind", "tier": "paid"}, "asn": "sapics_origin_asn"}"#,
        )
        .unwrap();

        assert_eq!(selection.city.tier, ProviderTier::Paid);
        assert_eq!(selection.asn.tier, ProviderTier::Free);
        assert_eq!(
            serde_json::from_str::<ProviderSelection>(&serde_json::to_string(&selection).unwrap())
                .unwrap(),
            selection
        );
    }

    #[test]
    fn an_omitted_tier_is_the_free_line() {
        let choice: ProviderChoice = serde_json::from_str(r#"{"provider": "max_mind"}"#).unwrap();
        assert_eq!(choice.tier, ProviderTier::Free);
        // Which is the same selection as naming the provider on its own.
        assert_eq!(choice, ProviderChoice::from(GeoIpProvider::MaxMind));
    }

    #[test]
    fn an_omitted_kind_takes_the_default_provider() {
        let selection: ProviderSelection =
            serde_json::from_str(r#"{"asn": "sapics_origin_asn"}"#).unwrap();
        assert_eq!(selection.city, ProviderChoice::default());
        assert_eq!(
            selection.asn,
            ProviderChoice::from(GeoIpProvider::SapicsOriginAsn)
        );
    }

    #[test]
    fn the_two_wire_forms_of_one_selection_compare_equal() {
        // Both forms normalise into the same struct, so a consumer comparing an
        // old config against a new one does not see a rewrite as a change.
        let uniform: ProviderSelection = serde_json::from_str("\"db_ip\"").unwrap();
        let spelt_out: ProviderSelection =
            serde_json::from_str(r#"{"city": "db_ip", "asn": {"provider": "db_ip"}}"#).unwrap();
        assert_eq!(uniform, spelt_out);
    }

    #[test]
    fn a_misspelt_kind_is_rejected() {
        let err = serde_json::from_str::<ProviderSelection>(r#"{"citty": "db_ip"}"#);
        assert!(err.is_err(), "{err:?}");
    }

    #[test]
    fn a_misspelt_tier_key_is_rejected() {
        let err =
            serde_json::from_str::<ProviderChoice>(r#"{"provider": "max_mind", "teir": "paid"}"#);
        assert!(err.is_err(), "{err:?}");
    }

    #[test]
    fn auto_download_defaults() {
        let auto = AutoDownloadConfig::default();
        assert!(auto.enabled);
        assert_eq!(auto.data_dir, PathBuf::from("/var/lib/geoip"));
        assert_eq!(auto.max_age_days, 30);
        assert!(auto.maxmind_account_id.is_none());
        assert!(auto.maxmind_license_key.is_none());
        assert!(auto.ipinfo_token.is_none());
        // Connect fast, then tolerate a slow but progressing transfer: the read
        // timeout is an idle one, so it is not sized by the file.
        assert_eq!(auto.connect_timeout_secs, 30);
        assert_eq!(auto.read_timeout_secs, 60);
    }

    #[test]
    fn the_timeouts_are_operator_tunable() {
        let json = r#"{"connect_timeout_secs": 5, "read_timeout_secs": 120}"#;
        let auto: AutoDownloadConfig = serde_json::from_str(json).unwrap();

        assert_eq!(auto.connect_timeout_secs, 5);
        assert_eq!(auto.read_timeout_secs, 120);
        // Unset keys keep their defaults, so tuning one does not clear the rest.
        assert_eq!(auto.max_age_days, 30);
        assert!(auto.enabled);
    }

    #[test]
    fn geoip_defaults() {
        let config = GeoIpConfig::default();
        assert!(config.enabled);
        assert_eq!(
            config.provider,
            ProviderSelection::from(GeoIpProvider::DbIp)
        );
        assert!(config.city_db_path.is_none());
        assert!(config.asn_db_path.is_none());
        assert!(config.http_client.is_none());
        assert!(config.auto_download.enabled);
    }

    #[test]
    fn deserialises_with_partial_keys() {
        let json = r#"{
            "provider": "max_mind",
            "auto_download": {
                "data_dir": "/srv/geoip",
                "max_age_days": 7,
                "maxmind_account_id": "123456",
                "maxmind_license_key": "secret-key"
            }
        }"#;
        let config: GeoIpConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.provider.city.provider, GeoIpProvider::MaxMind);
        assert_eq!(config.provider.asn.provider, GeoIpProvider::MaxMind);
        assert_eq!(config.provider.city.tier, ProviderTier::Free);
        assert_eq!(config.auto_download.data_dir, PathBuf::from("/srv/geoip"));
        assert_eq!(config.auto_download.max_age_days, 7);
        assert_eq!(
            config
                .auto_download
                .maxmind_license_key
                .as_ref()
                .map(Secret::expose),
            Some("secret-key")
        );
        // Unset keys fall back to the struct defaults, not to zero values.
        assert!(config.enabled);
        assert!(config.auto_download.enabled);
    }

    #[test]
    fn credentials_are_redacted_in_debug() {
        let config = GeoIpConfig {
            auto_download: AutoDownloadConfig {
                maxmind_account_id: Some("account-1234".into()),
                maxmind_license_key: Some("licence-abcd".into()),
                ipinfo_token: Some("token-wxyz".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("account-1234"), "{rendered}");
        assert!(!rendered.contains("licence-abcd"), "{rendered}");
        assert!(!rendered.contains("token-wxyz"), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
    }
}
