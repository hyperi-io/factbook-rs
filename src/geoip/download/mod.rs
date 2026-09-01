// Project:   factbook
// File:      src/geoip/download/mod.rs
// Purpose:   GeoIP database provisioning (resolve, freshness, download)
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! GeoIP database provisioning.
//!
//! Resolves the database files a service needs and downloads them when the
//! local copy is missing or stale. This module provisions files and holds no
//! lookup engine: the returned paths are handed to whatever reader the caller
//! uses -- an MMDB reader, an enrichment table, a sidecar.
//!
//! # Non-fatal by contract
//!
//! [`ensure_databases`] returns `Ok` with `None` entries when a download fails.
//! A GeoIP database going missing degrades enrichment; it must not stop a
//! service from starting. Failures are logged at `warn` and a stale local file
//! is preferred over no file at all.
//!
//! # Credentials
//!
//! MaxMind and IPinfo credentials are [`Secret`] and never reach a log line.
//! The IPinfo token is attached as a query parameter by the request builder
//! rather than being formatted into the URL string, and a transport error is
//! stripped of its URL before it is reported, so neither the logged URL nor the
//! error text can carry it.
//!
//! # Example
//!
//! ```rust,no_run
//! use factbook::geoip::{GeoIpConfig, ensure_databases};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let databases = ensure_databases(&GeoIpConfig::default()).await?;
//! if let Some(city) = databases.city {
//!     println!("city database is {:?} in {:?}", city.format, city.files);
//! }
//! # Ok(())
//! # }
//! ```

// Acquisition is not a geoip concern: `crate::table` fetches a user-supplied
// source through the same transfer, guard and interpretation types.
pub(crate) mod fetch;
pub(crate) mod source;
pub(crate) mod verify;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::config::{GeoIpConfig, ProviderChoice, ProviderSelection};
use fetch::Transfer;
use source::SourceSpec;
use verify::Guard;

pub use source::{SourceTerms, source_terms};

// The provider selection and the transfer shapes are the table's inputs and
// outputs, named here so the test module reaches them through this one.
#[cfg(test)]
use super::config::{GeoIpProvider, ProviderTier};
#[cfg(test)]
use fetch::{Archive, Credential};

/// Seconds in a day, for the staleness comparison.
pub(crate) const SECS_PER_DAY: u64 = 86_400;

/// Errors raised while provisioning a database.
///
/// Every variant is reachable only from the per-database download helpers.
/// [`ensure_databases`] absorbs them all -- see the module-level non-fatal
/// contract.
#[derive(Debug, thiserror::Error)]
pub enum GeoIpDownloadError {
    /// The HTTP client could not be built, or the request failed at the
    /// transport level.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The server answered, but not with a database.
    #[error("download of {url} returned HTTP {status}")]
    UnexpectedStatus {
        /// URL that was requested, without any credential.
        url: String,
        /// Status the server returned.
        status: u16,
    },

    /// A filesystem operation failed.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// The blocking decompress/extract task did not complete.
    #[error("decompression task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// The provider needs a credential that was not configured.
    #[error("provider {provider} requires {field} but it was not configured")]
    MissingCredential {
        /// Provider that demanded the credential.
        provider: &'static str,
        /// Config field the operator has to populate.
        field: &'static str,
    },

    /// The downloaded archive did not contain the expected member.
    #[error("{member} not found in the downloaded archive")]
    ArchiveMemberMissing {
        /// File name expected inside the archive.
        member: &'static str,
    },

    /// The provider offers no database of the requested kind.
    #[error("no {kind} database available for provider {provider}")]
    NoDatabases {
        /// Provider that was asked.
        provider: String,
        /// Database kind: `city` or `asn`.
        kind: &'static str,
    },

    /// The selected tier has no endpoint in this crate's provider table.
    #[error("the {tier} tier of provider {provider} is not modelled")]
    UnsupportedTier {
        /// Provider that was asked.
        provider: String,
        /// Tier that was asked for: `free` or `paid`.
        tier: &'static str,
    },

    /// The download did not match the digest the provider published for it.
    #[error("download does not match the digest at {url}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// URL the digest was read from.
        url: String,
        /// Digest the provider published.
        expected: String,
        /// Digest of what arrived.
        actual: String,
    },

    /// The published digest was not a SHA-256.
    #[error("the digest at {url} is not a sha256")]
    MalformedChecksum {
        /// URL the digest was read from.
        url: String,
    },

    /// The bytes arrived with a success status but are not a database.
    #[error("{url} did not deliver a MaxMind DB")]
    NotADatabase {
        /// What was being fetched.
        url: String,
    },

    /// The bytes arrived intact but hold no rows of the stated format.
    #[error("{path} does not parse as the table it states: {detail}")]
    Unparseable {
        /// File that was being replaced.
        path: String,
        /// What the parser refused it for.
        detail: String,
    },

    /// The provider rejected the credential that was sent.
    #[error("{url} rejected the credential with HTTP {status}; check {fields}")]
    CredentialRejected {
        /// URL that refused.
        url: String,
        /// Status it refused with: 401 or 403.
        status: u16,
        /// Config fields the credential was read from.
        fields: &'static str,
    },

    /// The provider is rate limiting this client.
    #[error("{url} is rate limiting this client")]
    RateLimited {
        /// URL that refused.
        url: String,
        /// Seconds the provider asked for, when it said.
        retry_after_secs: Option<u64>,
    },

    /// The database parsed but answered nothing for the field it exists to
    /// carry.
    #[error("{path} resolved no {field} for any probe address")]
    Unpopulated {
        /// File that was being replaced.
        path: String,
        /// What a database of that kind exists to answer with.
        field: &'static str,
    },

    /// The replacement is a fraction of the size of the copy already on disk.
    #[error(
        "{path} is {actual} bytes against the {existing} on disk, under the {floor_percent}% floor"
    )]
    Undersized {
        /// File that was being replaced.
        path: String,
        /// Length of what arrived.
        actual: u64,
        /// Length of the copy it would have replaced.
        existing: u64,
        /// Floor the operator configured.
        floor_percent: u8,
    },

    /// The body ended before the length the provider promised.
    #[error("{url} delivered {actual} of {expected} bytes")]
    Truncated {
        /// URL that was being read.
        url: String,
        /// Length the response declared.
        expected: u64,
        /// Length that arrived.
        actual: u64,
    },
}

impl GeoIpDownloadError {
    /// Whether coming back later, without changing the configuration, cannot
    /// help.
    ///
    /// A rejected or missing credential is a config fault: retrying it spends
    /// the provider's quota and hides the cause. A rate limit, a server error
    /// and a broken transfer are all worth another attempt later, which the
    /// freshness check makes on its own.
    #[must_use]
    pub const fn is_permanent(&self) -> bool {
        match self {
            Self::CredentialRejected { .. }
            | Self::MissingCredential { .. }
            | Self::NoDatabases { .. }
            | Self::UnsupportedTier { .. } => true,
            // 408 is the server inviting a retry; the rest of the 4xx range is
            // about this request and will answer the same way again.
            Self::UnexpectedStatus { status, .. } => {
                *status >= 400 && *status < 500 && *status != 408
            }
            _ => false,
        }
    }
}

/// Format of a provisioned database.
///
/// The consumer picks its reader from what it was handed rather than from the
/// provider name, so the format travels with the files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseFormat {
    /// MaxMind DB binary format, published as one file.
    #[default]
    Mmdb,

    /// Comma-separated rows, published as one file per address family for a
    /// geo database and as a single file for a table.
    Csv,

    /// JSON rows, published as one file.
    Json,
}

/// One provisioned database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    /// Format of the files.
    pub format: DatabaseFormat,

    /// The files on disk, in the order the provider publishes them: one file
    /// for [`DatabaseFormat::Mmdb`], IPv4 then IPv6 for
    /// [`DatabaseFormat::Csv`].
    pub files: Vec<PathBuf>,
}

/// Resolved databases. Either or both may be `None` -- the provider may not
/// offer that kind, or the download may have failed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Databases {
    /// City-level database, when one is available.
    pub city: Option<Database>,
    /// ASN database, when one is available.
    pub asn: Option<Database>,
}

/// What a provider publishes for one database kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DatabaseSpec {
    /// Format of the files.
    format: DatabaseFormat,
    /// File names written into the data directory, in publication order.
    names: &'static [&'static str],
}

impl DatabaseSpec {
    /// The same database, located under `data_dir`.
    fn at(self, data_dir: &Path) -> Database {
        Database {
            format: self.format,
            files: self.names.iter().map(|name| data_dir.join(name)).collect(),
        }
    }
}

/// Which database a call is resolving. Keeps the two near-identical resolve
/// arms as one code path instead of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    City,
    Asn,
}

impl Kind {
    /// Label used in error messages and log fields.
    const fn label(self) -> &'static str {
        match self {
            Self::City => "city",
            Self::Asn => "asn",
        }
    }

    /// Provider and tier selected for this kind.
    const fn choice(self, selection: ProviderSelection) -> ProviderChoice {
        match self {
            Self::City => selection.city,
            Self::Asn => selection.asn,
        }
    }

    /// Operator-supplied path for this kind, which bypasses the provider.
    fn explicit_path(self, config: &GeoIpConfig) -> Option<&PathBuf> {
        match self {
            Self::City => config.city_db_path.as_ref(),
            Self::Asn => config.asn_db_path.as_ref(),
        }
    }

    /// What this provider tier publishes for this kind.
    ///
    /// Provisioning reads the selected row directly, so this is the declared
    /// view the table's own tests assert against.
    ///
    /// # Errors
    ///
    /// [`GeoIpDownloadError::UnsupportedTier`] when the tier is not modelled.
    #[cfg(test)]
    fn spec(self, choice: ProviderChoice) -> Result<Option<DatabaseSpec>, GeoIpDownloadError> {
        Ok(SourceSpec::select(choice, self)?.map(SourceSpec::database))
    }
}

/// Ensure the configured GeoIP databases are on disk, downloading when the
/// local copy is missing or stale.
///
/// Resolution order, applied to each database kind on its own:
/// 1. `enabled: false` -- nothing is resolved.
/// 2. An explicit path for that kind -- returned verbatim, unchecked.
/// 3. The kind's provider publishes no such database -- `None`.
/// 4. `auto_download.enabled: false` -- the files only if they already exist.
/// 5. Otherwise: fresh local files, else download, else stale local files.
///
/// # Errors
///
/// Returns no error today: download failures are absorbed and reported as
/// `None`. The `Result` reserves room for a hard failure without a breaking
/// signature change.
pub async fn ensure_databases(config: &GeoIpConfig) -> Result<Databases, GeoIpDownloadError> {
    if !config.enabled {
        debug!("GeoIP provisioning disabled by config");
        return Ok(Databases::default());
    }

    // Sequential, not joined: two concurrent multi-hundred-megabyte transfers
    // would compete for the same link and the same disk.
    Ok(Databases {
        city: resolve(Kind::City, config).await,
        asn: resolve(Kind::Asn, config).await,
    })
}

/// Resolve one database: an explicit path, else fresh files, else a download,
/// else whatever stale copy is on disk.
async fn resolve(kind: Kind, config: &GeoIpConfig) -> Option<Database> {
    // An explicit path bypasses the provider for this kind alone, and is
    // returned unchecked: the operator asserted the file exists. It is declared
    // MMDB because that is the format a single operator-supplied file takes.
    if let Some(path) = kind.explicit_path(config) {
        return Some(Database {
            format: DatabaseFormat::Mmdb,
            files: vec![path.clone()],
        });
    }

    let choice = kind.choice(config.provider);
    let source = match SourceSpec::select(choice, kind) {
        Ok(source) => source?,
        Err(e) => {
            warn!(kind = kind.label(), error = %e, "GeoIP provider tier is not modelled");
            return None;
        }
    };
    let database = source.database().at(&config.auto_download.data_dir);

    if !config.auto_download.enabled {
        return database
            .files
            .iter()
            .all(|file| file.exists())
            .then_some(database);
    }

    // The provider's own publish rhythm is the freshness default, so a source
    // under a contractual update duty is not left sitting on it.
    let max_age_secs = source.staleness_window(&config.auto_download).as_secs();

    // A database is its whole file set, so one stale half re-downloads both.
    if database
        .files
        .iter()
        .all(|file| is_fresh(file, max_age_secs))
    {
        debug!(kind = kind.label(), files = ?database.files, "GeoIP database is fresh");
        return Some(database);
    }

    match download(kind, config).await {
        Ok(files) => Some(Database {
            format: database.format,
            files,
        }),
        Err(e) => {
            warn!(
                kind = kind.label(),
                error = %e,
                provider = ?choice,
                "GeoIP database download failed"
            );
            // A stale database still answers most lookups correctly, so it
            // beats disabling enrichment outright.
            if database.files.iter().all(|file| file.exists()) {
                warn!(kind = kind.label(), files = ?database.files, "using stale GeoIP database");
                Some(database)
            } else {
                None
            }
        }
    }
}

/// What one provider tier publishes, as `(city, asn)`. `None` means it
/// publishes no database of that kind.
///
/// The pairwise view of the table, which is what its tests assert against.
///
/// # Errors
///
/// [`GeoIpDownloadError::UnsupportedTier`] for a tier this crate does not
/// model. Those are gaps in verified provider facts, not statements that the
/// tier does not exist.
#[cfg(test)]
fn provider_files(
    choice: ProviderChoice,
) -> Result<(Option<DatabaseSpec>, Option<DatabaseSpec>), GeoIpDownloadError> {
    Ok((Kind::City.spec(choice)?, Kind::Asn.spec(choice)?))
}

/// Check the configuration can be acted on, before anything is downloaded.
///
/// This is the config-load check: a paid tier with no modelled endpoint, or a
/// provider whose credential is not configured, is reported here by name rather
/// than as a 401 during the first transfer. A provider that publishes only one
/// of the two kinds is not a fault.
///
/// # Errors
///
/// [`GeoIpDownloadError::MissingCredential`] naming the config field, or
/// [`GeoIpDownloadError::UnsupportedTier`] naming the provider and tier.
pub fn validate(config: &GeoIpConfig) -> Result<(), GeoIpDownloadError> {
    // Nothing is fetched in either case, so nothing is required.
    if !config.enabled || !config.auto_download.enabled {
        return Ok(());
    }

    for kind in [Kind::City, Kind::Asn] {
        // An explicit path is the operator's own file and needs no provider.
        if kind.explicit_path(config).is_some() {
            continue;
        }
        match plan(kind, config) {
            Ok(_) | Err(GeoIpDownloadError::NoDatabases { .. }) => {}
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Whether `path` exists and was modified within `max_age_secs`.
pub(crate) fn is_fresh(path: &Path, max_age_secs: u64) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    // A future mtime makes duration_since fail; treat that as stale rather than
    // trusting a clock that has gone backwards.
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age.as_secs() < max_age_secs
}

/// Run every transfer one database needs, returning the files written.
///
/// Sequential, and a failure abandons the rest: a half-fetched file set is not
/// a database, and the next freshness check re-drives all of them.
async fn download(kind: Kind, config: &GeoIpConfig) -> Result<Vec<PathBuf>, GeoIpDownloadError> {
    // Planned before the client is built, so a config fault costs no setup.
    let transfers = plan(kind, config)?;
    let client = fetch::client(
        config.http_client.as_ref(),
        Duration::from_secs(config.auto_download.connect_timeout_secs),
        Duration::from_secs(config.auto_download.read_timeout_secs),
    )?;

    // Resolved once for the whole database: the settings behind it cannot change
    // between two files of one file set.
    let guard = Guard::new(kind, &config.auto_download);

    let mut files = Vec::with_capacity(transfers.len());
    for transfer in transfers {
        files.push(transfer.run_guarded(&client, guard).await?);
    }
    Ok(files)
}

/// The transfers that fetch one database.
///
/// Errors here are configuration faults -- a missing credential, an unmodelled
/// tier, or a provider that does not publish this kind -- and cost no network
/// round trip.
fn plan(kind: Kind, config: &GeoIpConfig) -> Result<Vec<Transfer>, GeoIpDownloadError> {
    let choice = kind.choice(config.provider);
    let Some(source) = SourceSpec::select(choice, kind)? else {
        return Err(GeoIpDownloadError::NoDatabases {
            provider: choice.provider.label().to_string(),
            kind: kind.label(),
        });
    };

    Ok(vec![source.transfer(&config.auto_download)?])
}

#[cfg(test)]
mod tests;
