// Project:   factbook
// File:      src/geoip/download/source.rs
// Purpose:   The source table: what every provider publishes, as data
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! What every provider publishes, one row per database.
//!
//! Acquisition and interpretation vary independently: nothing about how bytes
//! are fetched predicts what they mean, and the same endpoint shape serves
//! several formats. A row states both axes, so adding a source is a row here
//! rather than an arm in a match.
//!
//! The table is internal. [`SourceTerms`] is the public window onto it, because
//! what a provider commits a deployer to has to be readable at runtime rather
//! than only in a comment.

use std::slice;
use std::time::Duration;

use chrono::{DateTime, Months, Utc};

use super::fetch::{Archive, Credential, Transfer};
use super::{DatabaseFormat, DatabaseSpec, GeoIpDownloadError, Kind, SECS_PER_DAY};
use crate::Secret;
use crate::geoip::config::{
    AutoDownloadConfig, GeoIpProvider, ProviderChoice, ProviderSelection, ProviderTier,
};

/// A day, the unit every cadence below is a multiple of.
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Published on two fixed days a week.
const TWICE_WEEKLY: Duration = Duration::from_secs(7 * 24 * 60 * 60 / 2);

/// Published once a month, on a URL carrying the month.
const MONTHLY: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Placeholder a dated URL substitutes the publication month into.
const MONTH: &str = "{month}";

/// Placeholder an edition URL substitutes the product id into.
const EDITION: &str = "{edition}";

/// How a source's download URL is built.
///
/// Three shapes cover every source: one that never changes, one dated by the
/// month it was published in, and one endpoint serving whichever product
/// edition is named in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UrlStrategy {
    /// One fixed URL, which always serves the current build.
    Static(&'static str),

    /// A URL carrying its publication month, with the previous month behind it
    /// when the provider has not published the current one yet.
    MonthlyDated {
        /// URL with [`MONTH`] where the month goes.
        template: &'static str,
        /// Whether the previous month is kept online to fall back to.
        fallback: bool,
    },

    /// One endpoint serving whichever edition is named in it.
    Edition {
        /// URL with [`EDITION`] where the product id goes.
        endpoint: &'static str,
    },
}

impl UrlStrategy {
    /// The URL to fetch, and the one tried behind it on a 404.
    fn resolve(self, edition: &str) -> (String, Option<String>) {
        match self {
            Self::Static(url) => (url.to_string(), None),

            Self::MonthlyDated { template, fallback } => {
                let now = Utc::now();
                let previous = if fallback {
                    now.checked_sub_months(Months::new(1))
                        .map(|month| dated(template, month))
                } else {
                    None
                };
                (dated(template, now), previous)
            }

            Self::Edition { endpoint } => (endpoint.replace(EDITION, edition), None),
        }
    }
}

/// One month's URL, dated the way a provider writes it.
fn dated(template: &str, month: DateTime<Utc>) -> String {
    template.replace(MONTH, &month.format("%Y-%m").to_string())
}

/// What the fetched bytes mean, and the files they become.
///
/// This is the axis acquisition does not predict, so it is stated rather than
/// inferred from the URL or the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Payload {
    /// MaxMind DB binary format, published as one file.
    Mmdb {
        /// Name the file is written under.
        file: &'static str,
    },
}

impl Payload {
    /// Format a consumer picks its reader by.
    const fn format(self) -> DatabaseFormat {
        match self {
            Self::Mmdb { .. } => DatabaseFormat::Mmdb,
        }
    }

    /// Name the file is written under.
    const fn file(self) -> &'static str {
        match self {
            Self::Mmdb { file } => file,
        }
    }

    /// Product id the provider names the file by, which is the file without
    /// the extension its format takes.
    fn edition(self) -> &'static str {
        match self {
            Self::Mmdb { file } => file.strip_suffix(".mmdb").unwrap_or(file),
        }
    }
}

/// A credential in the download settings, and where an operator sets it.
///
/// The path is carried so a missing or refused credential names the field to
/// populate rather than the provider's own error.
#[derive(Debug, Clone, Copy)]
struct CredentialSlot {
    /// Config field the credential is read from.
    path: &'static str,
    /// Reads it out of the download settings.
    read: fn(&AutoDownloadConfig) -> Option<&Secret>,
}

impl CredentialSlot {
    /// The configured credential, or the error naming what to set.
    ///
    /// # Errors
    ///
    /// [`GeoIpDownloadError::MissingCredential`] when the field is unset.
    fn require(
        self,
        provider: &'static str,
        auto: &AutoDownloadConfig,
    ) -> Result<Secret, GeoIpDownloadError> {
        (self.read)(auto)
            .cloned()
            .ok_or(GeoIpDownloadError::MissingCredential {
                provider,
                field: self.path,
            })
    }
}

/// Credential an endpoint requires, before it is read out of the settings.
///
/// The kind is what the table states; [`Credential`] is what it resolves to
/// once the operator's values are in hand.
#[derive(Debug, Clone, Copy)]
enum CredentialKind {
    /// Anonymous download.
    None,

    /// HTTP basic auth over two settings.
    Basic {
        /// Field holding the user half.
        username: CredentialSlot,
        /// Field holding the secret half.
        password: CredentialSlot,
        /// Both fields as one phrase, for the message a rejection produces.
        fields: &'static str,
    },

    /// A token carried as a query parameter.
    QueryToken {
        /// Parameter name the provider reads the token from.
        name: &'static str,
        /// Field holding the token.
        token: CredentialSlot,
    },
}

impl CredentialKind {
    /// Read the credential this source needs out of the settings.
    ///
    /// # Errors
    ///
    /// [`GeoIpDownloadError::MissingCredential`] naming the first field that is
    /// not configured.
    fn resolve(
        self,
        provider: &'static str,
        auto: &AutoDownloadConfig,
    ) -> Result<Credential, GeoIpDownloadError> {
        Ok(match self {
            Self::None => Credential::None,

            Self::Basic {
                username,
                password,
                fields,
            } => Credential::Basic {
                username: username.require(provider, auto)?,
                password: password.require(provider, auto)?,
                fields,
            },

            Self::QueryToken { name, token } => Credential::QueryToken {
                name,
                value: token.require(provider, auto)?,
                fields: token.path,
            },
        })
    }
}

/// What using a source's data commits the deployer to.
///
/// Queryable rather than documented: a deployer has to be able to render its
/// attribution line, not read a table in a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obligation {
    /// Licence the data is published under.
    pub licence: &'static str,

    /// Attribution that has to be displayed, where the licence requires one.
    pub attribution: Option<&'static str>,

    /// Whether the provider requires its logo shown beside the attribution.
    pub logo_required: bool,

    /// Where the provider publishes the terms.
    pub terms_url: &'static str,
}

/// What one selected source publishes, and what it commits the deployer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceTerms {
    /// Name of the source.
    pub name: &'static str,

    /// Database kind it provides: `city` or `asn`.
    pub kind: &'static str,

    /// What using the data commits the deployer to.
    pub obligation: Obligation,

    /// The provider's own publish rhythm, which is the freshness default.
    pub cadence: Duration,

    /// Shortest interval between fetches the provider's terms allow, where it
    /// states one.
    pub min_interval: Option<Duration>,
}

/// One database a provider publishes, and everything needed to fetch it.
///
/// The first three fields are the selection the row answers; the rest is what
/// answering it takes.
#[derive(Debug)]
pub(super) struct SourceSpec {
    /// Provider this row answers for.
    provider: GeoIpProvider,

    /// Product line this row answers for.
    tier: ProviderTier,

    /// Database kind this row answers for.
    kind: Kind,

    /// Name of the source, for logs and for the deployer reading its terms.
    name: &'static str,

    /// How the download URL is built.
    url: UrlStrategy,

    /// Credential the endpoint requires.
    credential: CredentialKind,

    /// How the downloaded bytes are packaged.
    archive: Archive,

    /// Digest published beside the file, where the provider publishes one.
    checksum: Option<UrlStrategy>,

    /// The provider's own publish rhythm.
    cadence: Duration,

    /// Shortest interval between fetches the provider's terms allow.
    min_interval: Option<Duration>,

    /// What using the data commits the deployer to.
    obligation: Obligation,

    /// What the bytes mean, and the files they become.
    payload: Payload,
}

impl SourceSpec {
    /// The row answering one selection, when the table has one.
    fn find(choice: ProviderChoice, kind: Kind) -> Option<&'static Self> {
        SOURCES.iter().find(|source| {
            source.provider == choice.provider && source.tier == choice.tier && source.kind == kind
        })
    }

    /// The row answering one selection, separating a provider that fetches
    /// nothing from a tier the table does not model.
    ///
    /// # Errors
    ///
    /// [`GeoIpDownloadError::UnsupportedTier`] when the provider publishes at
    /// some other tier but not this one.
    pub(super) fn select(
        choice: ProviderChoice,
        kind: Kind,
    ) -> Result<Option<&'static Self>, GeoIpDownloadError> {
        if let Some(source) = Self::find(choice, kind) {
            return Ok(Some(source));
        }

        // A provider with no row at any tier supplies its files some other way,
        // so nothing is missing and nothing is refused.
        if !SOURCES
            .iter()
            .any(|source| source.provider == choice.provider)
        {
            return Ok(None);
        }

        // A tier with no row is real but unverified, so it is refused rather
        // than guessed at.
        if !SOURCES
            .iter()
            .any(|source| source.provider == choice.provider && source.tier == choice.tier)
        {
            return Err(GeoIpDownloadError::UnsupportedTier {
                provider: choice.provider.label().to_string(),
                tier: choice.tier.label(),
            });
        }

        // The provider publishes at this tier, just not this kind.
        Ok(None)
    }

    /// What this source publishes, as the freshness check reads it.
    pub(super) fn database(&'static self) -> DatabaseSpec {
        DatabaseSpec {
            format: self.payload.format(),
            names: match &self.payload {
                Payload::Mmdb { file } => slice::from_ref(file),
            },
        }
    }

    /// The transfer that fetches this source into `auto.data_dir`.
    ///
    /// # Errors
    ///
    /// [`GeoIpDownloadError::MissingCredential`] when the endpoint needs a
    /// credential the operator has not set.
    pub(super) fn transfer(
        &'static self,
        auto: &AutoDownloadConfig,
    ) -> Result<Transfer, GeoIpDownloadError> {
        let edition = self.payload.edition();
        let (url, fallback_url) = self.url.resolve(edition);

        Ok(Transfer {
            url,
            fallback_url,
            checksum_url: self.checksum.map(|checksum| checksum.resolve(edition).0),
            dest: auto.data_dir.join(self.payload.file()),
            archive: self.archive,
            format: self.payload.format(),
            credential: self.credential.resolve(self.provider.label(), auto)?,
        })
    }

    /// How old a local copy may be before it is fetched again.
    ///
    /// The provider's cadence is the default, tightened by the operator's
    /// ceiling and never shortened past a fetch ceiling the terms impose.
    pub(super) fn staleness_window(&self, auto: &AutoDownloadConfig) -> Duration {
        let ceiling = Duration::from_secs(u64::from(auto.max_age_days) * SECS_PER_DAY);
        self.cadence
            .min(ceiling)
            .max(self.min_interval.unwrap_or(Duration::ZERO))
    }

    /// The public window onto this row.
    fn terms(&'static self) -> SourceTerms {
        SourceTerms {
            name: self.name,
            kind: self.kind.label(),
            obligation: self.obligation,
            cadence: self.cadence,
            min_interval: self.min_interval,
        }
    }
}

/// DB-IP publishes the Lite databases under CC BY 4.0.
const DB_IP_LITE: Obligation = Obligation {
    licence: "CC BY 4.0",
    attribution: Some("IP Geolocation by DB-IP -- https://db-ip.com"),
    logo_required: false,
    terms_url: "https://db-ip.com/db/lite.php",
};

/// MaxMind requires the GeoLite2 attribution, and requires an old copy to be
/// replaced within thirty days of a release.
const MAXMIND_GEOLITE2: Obligation = Obligation {
    licence: "MaxMind GeoLite EULA",
    attribution: Some(
        "This product includes GeoLite2 data created by MaxMind, available from \
         https://www.maxmind.com",
    ),
    logo_required: false,
    terms_url: "https://www.maxmind.com/en/geolite/eula",
};

/// The paid line is licensed per subscription and carries no public
/// attribution duty. A separate commercial licence covers redistribution.
const MAXMIND_GEOIP2: Obligation = Obligation {
    licence: "MaxMind Online EULA",
    attribution: None,
    logo_required: false,
    terms_url: "https://www.maxmind.com/en/end-user-license-agreement",
};

/// IPinfo publishes Lite under CC BY-SA 4.0 and asks for a link back.
const IPINFO_LITE: Obligation = Obligation {
    licence: "CC BY-SA 4.0",
    attribution: Some("IP address data powered by IPinfo -- https://ipinfo.io"),
    logo_required: false,
    terms_url: "https://ipinfo.io/developers/ipinfo-lite-database",
};

/// The sapics datasets used here are public domain and owe nothing.
const PUBLIC_DOMAIN: Obligation = Obligation {
    licence: "PDDL 1.0",
    attribution: None,
    logo_required: false,
    terms_url: "https://github.com/sapics/ip-location-db",
};

/// Endpoint both MaxMind lines download from, whichever edition is asked for.
const MAXMIND_ENDPOINT: &str =
    "https://download.maxmind.com/geoip/databases/{edition}/download?suffix=tar.gz";

/// Every database this crate can fetch.
///
/// A provider is expressible here or it is not supported: a shape the table
/// cannot state is a gap in the table rather than a case for code elsewhere.
static SOURCES: &[SourceSpec] = &[
    SourceSpec {
        provider: GeoIpProvider::DbIp,
        tier: ProviderTier::Free,
        kind: Kind::City,
        name: "DB-IP Lite City",
        url: UrlStrategy::MonthlyDated {
            template: "https://download.db-ip.com/free/dbip-city-lite-{month}.mmdb.gz",
            // The current month is not published until some days into it, and
            // the previous one stays online.
            fallback: true,
        },
        credential: CredentialKind::None,
        archive: Archive::Gzip,
        checksum: None,
        cadence: MONTHLY,
        min_interval: None,
        obligation: DB_IP_LITE,
        payload: Payload::Mmdb {
            file: "dbip-city-lite.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::DbIp,
        tier: ProviderTier::Free,
        kind: Kind::Asn,
        name: "DB-IP Lite ASN",
        url: UrlStrategy::MonthlyDated {
            template: "https://download.db-ip.com/free/dbip-asn-lite-{month}.mmdb.gz",
            fallback: true,
        },
        credential: CredentialKind::None,
        archive: Archive::Gzip,
        checksum: None,
        cadence: MONTHLY,
        min_interval: None,
        obligation: DB_IP_LITE,
        payload: Payload::Mmdb {
            file: "dbip-asn-lite.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::MaxMind,
        tier: ProviderTier::Free,
        kind: Kind::City,
        name: "MaxMind GeoLite2 City",
        url: UrlStrategy::Edition {
            endpoint: MAXMIND_ENDPOINT,
        },
        credential: MAXMIND_CREDENTIAL,
        archive: Archive::TarGz {
            member: "GeoLite2-City.mmdb",
        },
        checksum: None,
        cadence: TWICE_WEEKLY,
        // A GeoLite account is capped at thirty downloads a day across every
        // database, so a fetch is allowed once a day per database.
        min_interval: Some(DAY),
        obligation: MAXMIND_GEOLITE2,
        payload: Payload::Mmdb {
            file: "GeoLite2-City.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::MaxMind,
        tier: ProviderTier::Free,
        kind: Kind::Asn,
        name: "MaxMind GeoLite2 ASN",
        url: UrlStrategy::Edition {
            endpoint: MAXMIND_ENDPOINT,
        },
        credential: MAXMIND_CREDENTIAL,
        archive: Archive::TarGz {
            member: "GeoLite2-ASN.mmdb",
        },
        checksum: None,
        cadence: TWICE_WEEKLY,
        min_interval: Some(DAY),
        obligation: MAXMIND_GEOLITE2,
        payload: Payload::Mmdb {
            file: "GeoLite2-ASN.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::MaxMind,
        tier: ProviderTier::Paid,
        kind: Kind::City,
        name: "MaxMind GeoIP2 City",
        url: UrlStrategy::Edition {
            endpoint: MAXMIND_ENDPOINT,
        },
        credential: MAXMIND_CREDENTIAL,
        archive: Archive::TarGz {
            member: "GeoIP2-City.mmdb",
        },
        checksum: None,
        cadence: TWICE_WEEKLY,
        min_interval: Some(DAY),
        obligation: MAXMIND_GEOIP2,
        payload: Payload::Mmdb {
            file: "GeoIP2-City.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::MaxMind,
        tier: ProviderTier::Paid,
        kind: Kind::Asn,
        // The paid line has no ASN database of its own: the ASN fields ship
        // inside GeoIP2-ISP.
        name: "MaxMind GeoIP2 ISP",
        url: UrlStrategy::Edition {
            endpoint: MAXMIND_ENDPOINT,
        },
        credential: MAXMIND_CREDENTIAL,
        archive: Archive::TarGz {
            member: "GeoIP2-ISP.mmdb",
        },
        checksum: None,
        cadence: TWICE_WEEKLY,
        min_interval: Some(DAY),
        obligation: MAXMIND_GEOIP2,
        payload: Payload::Mmdb {
            file: "GeoIP2-ISP.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::IpInfo,
        tier: ProviderTier::Free,
        kind: Kind::City,
        name: "IPinfo Lite",
        url: UrlStrategy::Static("https://ipinfo.io/data/ipinfo_lite.mmdb"),
        credential: CredentialKind::QueryToken {
            name: "token",
            token: CredentialSlot {
                path: "auto_download.ipinfo_token",
                read: |auto| auto.ipinfo_token.as_ref(),
            },
        },
        archive: Archive::Raw,
        checksum: None,
        cadence: DAY,
        // The endpoint is capped at ten downloads a day per address.
        min_interval: Some(DAY),
        obligation: IPINFO_LITE,
        payload: Payload::Mmdb {
            file: "ipinfo-lite.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::SapicsOriginAsn,
        tier: ProviderTier::Free,
        kind: Kind::Asn,
        name: "sapics origin-asn",
        url: UrlStrategy::Static(
            "https://github.com/sapics/ip-location-db/releases/download/latest/origin-asn.mmdb",
        ),
        credential: CredentialKind::None,
        archive: Archive::Raw,
        checksum: Some(UrlStrategy::Static(
            "https://github.com/sapics/ip-location-db/releases/download/checksum/origin-asn.mmdb.sha256",
        )),
        cadence: DAY,
        min_interval: None,
        obligation: PUBLIC_DOMAIN,
        payload: Payload::Mmdb {
            file: "origin-asn.mmdb",
        },
    },
    SourceSpec {
        provider: GeoIpProvider::SapicsIpToAsn,
        tier: ProviderTier::Free,
        kind: Kind::Asn,
        name: "sapics iptoasn-asn",
        url: UrlStrategy::Static(
            "https://github.com/sapics/ip-location-db/releases/download/latest/iptoasn-asn.mmdb",
        ),
        credential: CredentialKind::None,
        archive: Archive::Raw,
        checksum: Some(UrlStrategy::Static(
            "https://github.com/sapics/ip-location-db/releases/download/checksum/iptoasn-asn.mmdb.sha256",
        )),
        cadence: DAY,
        min_interval: None,
        obligation: PUBLIC_DOMAIN,
        payload: Payload::Mmdb {
            file: "iptoasn-asn.mmdb",
        },
    },
];

/// Both MaxMind lines download from the one endpoint under the one credential.
const MAXMIND_CREDENTIAL: CredentialKind = CredentialKind::Basic {
    username: CredentialSlot {
        path: "auto_download.maxmind_account_id",
        read: |auto| auto.maxmind_account_id.as_ref(),
    },
    password: CredentialSlot {
        path: "auto_download.maxmind_license_key",
        read: |auto| auto.maxmind_license_key.as_ref(),
    },
    fields: "auto_download.maxmind_account_id and auto_download.maxmind_license_key",
};

/// Terms of every database a selection provisions.
///
/// One entry per database that will actually be fetched, so a provider that
/// publishes only one kind contributes one entry and a selection that fetches
/// nothing contributes none. This is how a deployment reports what it owes for
/// the data it serves.
///
/// ```
/// use factbook::geoip::{GeoIpProvider, ProviderSelection, source_terms};
///
/// let terms = source_terms(ProviderSelection::from(GeoIpProvider::DbIp));
/// let city = terms.first().unwrap();
///
/// assert_eq!(city.kind, "city");
/// assert_eq!(city.obligation.licence, "CC BY 4.0");
/// assert!(city.obligation.attribution.is_some());
/// ```
#[must_use]
pub fn source_terms(selection: ProviderSelection) -> Vec<SourceTerms> {
    [Kind::City, Kind::Asn]
        .into_iter()
        .filter_map(|kind| SourceSpec::find(kind.choice(selection), kind))
        .map(SourceSpec::terms)
        .collect()
}
