// Project:   factbook
// File:      src/geoip/download/fetch.rs
// Purpose:   Streaming download + decompression for GeoIP database files
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Transfer plumbing behind [`ensure_databases`](super::ensure_databases).
//!
//! The body streams to a sibling temp file rather than into memory: a city
//! database is hundreds of megabytes, and a memory-capped pod cannot afford to
//! hold the compressed and decompressed copies at once.
//!
//! Decompression and tar extraction run on
//! [`spawn_blocking`](tokio::task::spawn_blocking) -- both are synchronous
//! CPU-plus-disk work and would otherwise stall a runtime worker for the length
//! of the file.
//!
//! The free tiers these databases come from are rate limited, so a transfer is
//! assumed to be slow rather than quick: it resumes from the part file with a
//! `Range` request, reports progress while it runs, and is bounded by an idle
//! timeout rather than a total one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use flate2::read::GzDecoder;
use reqwest::{Client, RequestBuilder, StatusCode};
use tracing::{error, info, warn};

use super::verify::Guard;
use super::{DatabaseFormat, GeoIpDownloadError};
use crate::Secret;

/// Extension of the in-flight transfer file, a sibling of the destination so
/// the final rename stays on one filesystem and is therefore atomic.
const PART_EXT: &str = "part";

/// Extension of the fully-materialised file awaiting its rename.
const STAGE_EXT: &str = "staged";

/// How often a transfer in flight reports progress.
///
/// A half-hour transfer that says nothing is indistinguishable from a hang, and
/// someone kills it.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// How long a part file stays worth resuming from.
///
/// Providers that date their URLs publish a different file each month, so an
/// old prefix is discarded rather than continued into the wrong body.
const RESUME_WINDOW_SECS: u64 = 24 * 60 * 60;

/// Marker that opens the metadata section of a MaxMind DB file.
const MMDB_MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";

/// How much of a file's tail is searched for the metadata marker. The format
/// bounds the metadata section to the last 128 KiB.
const METADATA_TAIL_BYTES: u64 = 128 * 1024;

/// Length of a SHA-256 digest written as hex.
const SHA256_HEX_LEN: usize = 64;

/// Read size while digesting a file, which runs to tens of megabytes.
const DIGEST_BUFFER_BYTES: usize = 64 * 1024;

/// Openers of a markup document, lowercase, for a payload that should be text.
const MARKUP_OPENERS: [&[u8]; 3] = [b"<!doctype", b"<html", b"<?xml"];

/// How much of a text payload is read to look for a markup opener.
const MARKUP_HEAD_BYTES: usize = 64;

/// How the downloaded bytes are packaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Archive {
    /// The body is the database file itself.
    Raw,
    /// The body is a gzip stream wrapping the database file.
    Gzip,
    /// The body is a gzip-compressed tar carrying `member` somewhere inside.
    TarGz { member: &'static str },
}

/// Credential attached to the request.
///
/// `Debug` is hand-written: a derived one would print the secret into any error
/// report or trace that formats a request plan.
#[derive(Clone)]
pub(crate) enum Credential {
    /// Anonymous download.
    None,
    /// HTTP basic auth (MaxMind account id + licence key).
    Basic {
        username: Secret,
        password: Secret,
        /// Config fields it came from, for the message a rejection produces.
        fields: &'static str,
    },
    /// Token carried as a query parameter (IPinfo).
    QueryToken {
        name: &'static str,
        value: Secret,
        /// Config field it came from.
        fields: &'static str,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::None => "None",
            Self::Basic { .. } => "Basic(***REDACTED***)",
            Self::QueryToken { .. } => "QueryToken(***REDACTED***)",
        };
        f.write_str(kind)
    }
}

impl Credential {
    /// Attach the credential to a request.
    ///
    /// The token goes on as a query parameter here rather than being formatted
    /// into the URL string, so the URL the caller logs never carries it.
    fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        match self {
            Self::None => request,
            Self::Basic {
                username, password, ..
            } => request.basic_auth(username.expose(), Some(password.expose())),
            Self::QueryToken { name, value, .. } => request.query(&[(*name, value.expose())]),
        }
    }

    /// Config fields this credential was read from, when there is one.
    const fn fields(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Basic { fields, .. } | Self::QueryToken { fields, .. } => Some(fields),
        }
    }
}

/// Client for a transfer: the caller's when one was injected, else a default.
///
/// Cloning an injected client is cheap -- it is a handle over a shared
/// connection pool -- and it keeps the caller's proxy, root store and timeouts.
pub(crate) fn client(
    injected: Option<&Client>,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<Client, GeoIpDownloadError> {
    if let Some(client) = injected {
        return Ok(client.clone());
    }

    Ok(Client::builder()
        .tls_backend_rustls()
        .user_agent(format!("factbook/{}", crate::VERSION))
        .connect_timeout(connect_timeout)
        // An idle timeout, and deliberately no whole-request one: a database
        // body is hundreds of megabytes, so a total-time limit caps the link
        // speed a deployment is allowed to have. This fails a connection that
        // has stopped delivering, and lets a slow one finish.
        .read_timeout(read_timeout)
        .build()?)
}

/// One database transfer: where from, where to, and how it is packaged.
#[derive(Debug)]
pub(crate) struct Transfer {
    pub(crate) url: String,
    /// URL tried when `url` returns 404, for a provider that dates its files
    /// and has not published the current one yet.
    pub(crate) fallback_url: Option<String>,
    /// URL of the digest the provider publishes beside the file, where it
    /// publishes one.
    pub(crate) checksum_url: Option<String>,
    pub(crate) dest: PathBuf,
    pub(crate) archive: Archive,
    /// Format of the file this transfer produces, which decides how its
    /// contents are checked.
    pub(crate) format: DatabaseFormat,
    pub(crate) credential: Credential,
}

impl Transfer {
    /// The same transfer with nothing checked beyond the format and the digest.
    ///
    /// This is the shape the transport, archive and status cases are asserted
    /// against, where the bodies are stand-ins rather than databases.
    #[cfg(test)]
    pub(crate) async fn run(self, client: &Client) -> Result<PathBuf, GeoIpDownloadError> {
        self.run_guarded(client, Guard::OFF).await
    }

    /// Fetch, materialise and atomically move the database into place, refusing
    /// a staged file the guard will not admit.
    ///
    /// Returns the destination path on success.
    pub(crate) async fn run_guarded(
        self,
        client: &Client,
        guard: Guard,
    ) -> Result<PathBuf, GeoIpDownloadError> {
        if let Some(parent) = self.dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let part = with_extension(&self.dest, PART_EXT);
        info!(
            url = %self.url,
            dest = %self.dest.display(),
            archive = ?self.archive,
            "downloading GeoIP database"
        );

        // A failure keeps the part file: what it holds is a valid prefix of the
        // body, and the next run resumes from it rather than re-fetching tens of
        // megabytes. Only materialise writes the destination, so a part file is
        // never mistaken for a database.
        let bytes = self.stream_with_fallback(client, &part).await?;

        if let Some(checksum_url) = self.checksum_url.clone() {
            let expected = fetch_checksum(client, &checksum_url, &self.credential).await?;
            let checked = part.clone();
            let actual = tokio::task::spawn_blocking(move || sha256_of(&checked)).await??;
            if actual != expected {
                // The bytes are known bad, so they are not kept for a resume.
                let _ = fs::remove_file(&part);
                return Err(self.rejected(GeoIpDownloadError::ChecksumMismatch {
                    url: checksum_url,
                    expected,
                    actual,
                }));
            }
        }

        let dest = self.dest.clone();
        let archive = self.archive;
        let format = self.format;
        let staged = with_extension(&dest, STAGE_EXT);
        let final_size = tokio::task::spawn_blocking(move || {
            let result = materialise_guarded(&part, &staged, &dest, archive, format, guard);
            let _ = fs::remove_file(&part);
            if result.is_err() {
                let _ = fs::remove_file(&staged);
            }
            result
        })
        .await?
        .map_err(|e| self.rejected(e))?;

        info!(
            dest = %self.dest.display(),
            downloaded_bytes = bytes,
            database_bytes = final_size,
            "GeoIP database ready"
        );
        Ok(self.dest)
    }

    /// Report a download that arrived but was not usable.
    ///
    /// Logged at error because the copy already on disk is now the one being
    /// served, and it will keep being served until a later download passes.
    fn rejected(&self, error: GeoIpDownloadError) -> GeoIpDownloadError {
        error!(
            url = %self.url,
            dest = %self.dest.display(),
            reason = %error,
            "rejected a GeoIP download, keeping the file already on disk"
        );
        error
    }

    /// Stream the primary URL, retrying the fallback on a 404 alone.
    ///
    /// Any other status is the provider's answer about this request and is
    /// reported as it stands; only a missing file is worth asking twice for.
    async fn stream_with_fallback(
        &self,
        client: &Client,
        part: &Path,
    ) -> Result<u64, GeoIpDownloadError> {
        let primary = self.stream_to(client, &self.url, part).await;

        let Some(fallback) = self.fallback_url.as_deref() else {
            return primary;
        };
        if !matches!(
            primary,
            Err(GeoIpDownloadError::UnexpectedStatus { status: 404, .. })
        ) {
            return primary;
        }

        warn!(
            url = %self.url,
            fallback = %fallback,
            "GeoIP database not published at that URL, trying the previous one"
        );
        // The fallback is a different file, so anything already transferred
        // belongs to the other URL and cannot be resumed into this one.
        let _ = fs::remove_file(part);
        self.stream_to(client, fallback, part).await
    }

    /// Stream the response body of `url` to `part`, returning the total bytes
    /// the part file now holds.
    async fn stream_to(
        &self,
        client: &Client,
        url: &str,
        part: &Path,
    ) -> Result<u64, GeoIpDownloadError> {
        let resume_from = resumable_offset(part);
        let mut request = self.credential.apply(client.get(url));
        if resume_from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
        }

        // One attempt per call. A failure leaves the part file for the next run
        // to continue from, so a retry layer would only repeat inside a process
        // what the freshness check already does across runs.
        //
        // The URL is stripped from any transport error because the request URL
        // carries the IPinfo token as a query parameter and the error is logged
        // verbatim by the non-fatal path above.
        let mut response = request.send().await.map_err(reqwest::Error::without_url)?;
        let status = response.status();

        // reqwest reports a 4xx or 5xx as a completed round trip, so the status
        // check is ours to make.
        if !status.is_success() {
            return Err(refused(url, status, response.headers(), &self.credential));
        }

        // Only a 206 is the server agreeing to continue. Any other success is
        // the whole body, so the prefix is discarded rather than appended to --
        // appending it would silently build a corrupt file.
        let resuming = resume_from > 0 && status == StatusCode::PARTIAL_CONTENT;
        let mut file = if resuming {
            fs::OpenOptions::new().append(true).open(part)?
        } else {
            fs::File::create(part)?
        };
        let mut written = if resuming { resume_from } else { 0 };
        let expected = response.content_length().map(|length| length + written);

        if resume_from > 0 {
            info!(
                url = %url,
                resumed_from = resume_from,
                honoured = resuming,
                "continuing an interrupted GeoIP download"
            );
        }

        let started = Instant::now();
        let mut reported = Instant::now();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)?
        {
            // A write that fails leaves the part file in a state this process
            // cannot describe -- a full disk truncates it silently -- so it goes
            // rather than being resumed from.
            if let Err(e) = io::Write::write_all(&mut file, &chunk) {
                drop(file);
                let _ = fs::remove_file(part);
                return Err(e.into());
            }
            written += chunk.len() as u64;

            if reported.elapsed() >= PROGRESS_INTERVAL {
                info!(
                    url = %url,
                    downloaded_bytes = written,
                    expected_bytes = expected,
                    rate_kib_per_sec = rate_kib_per_sec(written - resume_from, started.elapsed()),
                    "downloading GeoIP database"
                );
                reported = Instant::now();
            }
        }

        if let Err(e) = io::Write::flush(&mut file) {
            drop(file);
            let _ = fs::remove_file(part);
            return Err(e.into());
        }

        // A body that ends early is otherwise indistinguishable from a small
        // database. The HTTP layer rejects an incomplete body first; this is the
        // guard for a response that ends cleanly and short anyway. What arrived
        // is kept: it is a valid prefix to resume from.
        if let Some(expected) = expected
            && written != expected
        {
            return Err(GeoIpDownloadError::Truncated {
                url: url.to_string(),
                expected,
                actual: written,
            });
        }

        Ok(written)
    }
}

/// Turn a refusing response into the error that describes it.
///
/// The distinction that matters is permanent against transient: a rejected
/// credential is a config fault and retrying it burns quota, while a 429 or a
/// 5xx is worth coming back to.
fn refused(
    url: &str,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    credential: &Credential,
) -> GeoIpDownloadError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return GeoIpDownloadError::RateLimited {
            url: url.to_string(),
            // Providers ban a client that ignores this, so the delay is carried
            // to the caller rather than being retried inside the transfer.
            retry_after_secs: headers
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse().ok()),
        };
    }

    let rejects_credential = status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN;
    if let (true, Some(fields)) = (rejects_credential, credential.fields()) {
        return GeoIpDownloadError::CredentialRejected {
            url: url.to_string(),
            status: status.as_u16(),
            fields,
        };
    }

    GeoIpDownloadError::UnexpectedStatus {
        url: url.to_string(),
        status: status.as_u16(),
    }
}

/// Bytes already transferred that are worth continuing from.
///
/// A part file past the resume window is removed rather than continued: it may
/// belong to a URL the provider has since replaced.
fn resumable_offset(part: &Path) -> u64 {
    let Ok(metadata) = fs::metadata(part) else {
        return 0;
    };

    let age = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
    if age.is_some_and(|age| age.as_secs() < RESUME_WINDOW_SECS) {
        metadata.len()
    } else {
        let _ = fs::remove_file(part);
        0
    }
}

/// Observed transfer rate, for the progress line.
///
/// Elapsed time is floored at a second so a fast first interval does not divide
/// by zero.
const fn rate_kib_per_sec(written: u64, elapsed: Duration) -> u64 {
    let seconds = elapsed.as_secs();
    written / 1024 / if seconds == 0 { 1 } else { seconds }
}

/// The digest a provider publishes beside a file.
///
/// The body is `sha256sum` output -- the digest, then the file name -- so only
/// the first field is read.
///
/// The credential is applied here too: MaxMind gates its digest behind the same
/// account as the database.
async fn fetch_checksum(
    client: &Client,
    url: &str,
    credential: &Credential,
) -> Result<String, GeoIpDownloadError> {
    let response = credential
        .apply(client.get(url))
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    let status = response.status();
    if !status.is_success() {
        return Err(GeoIpDownloadError::UnexpectedStatus {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }

    let body = response.text().await.map_err(reqwest::Error::without_url)?;
    let digest = body.split_whitespace().next().unwrap_or_default();
    if digest.len() != SHA256_HEX_LEN || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GeoIpDownloadError::MalformedChecksum {
            url: url.to_string(),
        });
    }

    Ok(digest.to_ascii_lowercase())
}

/// SHA-256 of a file, as lowercase hex. Blocking: it reads the whole file.
fn sha256_of(path: &Path) -> Result<String, GeoIpDownloadError> {
    use sha2::Digest;

    let mut file = fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; DIGEST_BUFFER_BYTES];

    loop {
        let read = io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let mut hex = String::with_capacity(SHA256_HEX_LEN);
    for byte in hasher.finalize() {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Whether the file holds what its declared format requires.
///
/// Providers answer 200 with a login page or an error page often enough that a
/// status check is not evidence of a database, and a rename would then wedge
/// that page in place of the last good copy: its mtime is fresh, so nothing
/// re-downloads it.
fn holds_a_database(path: &Path, format: DatabaseFormat) -> Result<bool, io::Error> {
    match format {
        // The format requires the marker, and the spec bounds the metadata
        // section to the tail, so only the tail is read.
        DatabaseFormat::Mmdb => {
            let mut file = fs::File::open(path)?;
            let length = file.metadata()?.len();
            io::Seek::seek(
                &mut file,
                io::SeekFrom::Start(length.saturating_sub(METADATA_TAIL_BYTES)),
            )?;

            let mut tail = Vec::new();
            io::Read::read_to_end(&mut file, &mut tail)?;
            Ok(tail
                .windows(MMDB_MARKER.len())
                .any(|window| window == MMDB_MARKER))
        }

        // Text has no marker to require, so the check is the other way round:
        // reject the markup an error or login page opens with.
        DatabaseFormat::Csv | DatabaseFormat::Json => {
            let mut head = vec![0u8; MARKUP_HEAD_BYTES];
            let read = io::Read::read(&mut fs::File::open(path)?, &mut head)?;
            let head = head[..read].to_ascii_lowercase();
            Ok(!MARKUP_OPENERS.iter().any(|opener| head.starts_with(opener)))
        }
    }
}

/// Turn the transferred bytes into the destination file, with nothing checked
/// beyond the format.
///
/// This is the shape the archive and format cases are asserted against.
#[cfg(test)]
fn materialise(
    part: &Path,
    staged: &Path,
    dest: &Path,
    archive: Archive,
    format: DatabaseFormat,
) -> Result<u64, GeoIpDownloadError> {
    materialise_guarded(part, staged, dest, archive, format, Guard::OFF)
}

/// Turn the transferred bytes into the destination file. Blocking: gzip and tar
/// decode are synchronous and this runs on the blocking pool.
fn materialise_guarded(
    part: &Path,
    staged: &Path,
    dest: &Path,
    archive: Archive,
    format: DatabaseFormat,
    guard: Guard,
) -> Result<u64, GeoIpDownloadError> {
    let source = fs::File::open(part)?;

    match archive {
        Archive::Raw => {
            fs::rename(part, staged)?;
        }
        Archive::Gzip => {
            let mut decoder = GzDecoder::new(io::BufReader::new(source));
            let mut out = io::BufWriter::new(fs::File::create(staged)?);
            io::copy(&mut decoder, &mut out)?;
            io::Write::flush(&mut out)?;
        }
        Archive::TarGz { member } => {
            extract_member(source, staged, member)?;
        }
    }

    // Checked before the rename, so a body that is not a database leaves the
    // copy already at the destination exactly as it is.
    if !holds_a_database(staged, format)? {
        return Err(GeoIpDownloadError::NotADatabase {
            url: dest.display().to_string(),
        });
    }

    // What the format cannot state: whether the file answers anything, and
    // whether it is the size of a database. Also before the rename, for the same
    // reason.
    guard.admit(staged, dest, format)?;

    let size = fs::metadata(staged)?.len();
    fs::rename(staged, dest)?;
    Ok(size)
}

/// Extract a single named member from a gzip-compressed tar.
///
/// The archives carry the file under a dated directory
/// (`GeoLite2-City_20241231/GeoLite2-City.mmdb`), so the match is on the file
/// name rather than the full path.
fn extract_member(
    source: fs::File,
    staged: &Path,
    member: &'static str,
) -> Result<(), GeoIpDownloadError> {
    let decoder = GzDecoder::new(io::BufReader::new(source));
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let is_match = entry.path()?.file_name().is_some_and(|name| name == member);
        if is_match {
            let mut out = io::BufWriter::new(fs::File::create(staged)?);
            io::copy(&mut entry, &mut out)?;
            io::Write::flush(&mut out)?;
            return Ok(());
        }
    }

    Err(GeoIpDownloadError::ArchiveMemberMissing { member })
}

/// Append an extension rather than replacing one: `foo.mmdb` becomes
/// `foo.mmdb.part`, so two providers writing different databases into the same
/// directory never collide on a temp name.
fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(extension);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_names_append_rather_than_replace() {
        let dest = Path::new("/var/lib/geoip/dbip-city-lite.mmdb");
        assert_eq!(
            with_extension(dest, PART_EXT),
            PathBuf::from("/var/lib/geoip/dbip-city-lite.mmdb.part")
        );
        assert_eq!(
            with_extension(dest, STAGE_EXT),
            PathBuf::from("/var/lib/geoip/dbip-city-lite.mmdb.staged")
        );
    }

    #[test]
    fn credential_debug_never_shows_the_secret() {
        let basic = Credential::Basic {
            username: "account-1234".into(),
            password: "licence-abcd".into(),
            fields: "auto_download.maxmind_account_id",
        };
        let token = Credential::QueryToken {
            name: "token",
            value: "token-wxyz".into(),
            fields: "auto_download.ipinfo_token",
        };
        assert_eq!(format!("{basic:?}"), "Basic(***REDACTED***)");
        assert_eq!(format!("{token:?}"), "QueryToken(***REDACTED***)");
        assert_eq!(format!("{:?}", Credential::None), "None");

        // The field name is config, not a secret, so it survives the redaction.
        assert_eq!(basic.fields(), Some("auto_download.maxmind_account_id"));
        assert_eq!(Credential::None.fields(), None);
    }

    #[test]
    fn transfer_debug_never_shows_the_secret() {
        let transfer = Transfer {
            url: "https://example.invalid/db.mmdb".into(),
            fallback_url: None,
            checksum_url: None,
            dest: PathBuf::from("/tmp/db.mmdb"),
            archive: Archive::Raw,
            format: DatabaseFormat::Mmdb,
            credential: Credential::QueryToken {
                name: "token",
                value: "token-wxyz".into(),
                fields: "auto_download.ipinfo_token",
            },
        };
        let rendered = format!("{transfer:?}");
        assert!(!rendered.contains("token-wxyz"), "{rendered}");
        assert!(rendered.contains("REDACTED"), "{rendered}");
    }

    #[test]
    fn the_default_client_builds() {
        // The default arm sets a TLS backend, a user agent and two timeouts, any
        // of which can fail the build at runtime rather than at compile time.
        let built = client(None, Duration::from_secs(30), Duration::from_secs(60));
        assert!(built.is_ok());
    }

    #[test]
    fn an_injected_client_is_used_as_it_stands() {
        let injected = Client::new();
        let built = client(
            Some(&injected),
            Duration::from_secs(30),
            Duration::from_secs(60),
        );
        assert!(built.is_ok());
    }

    #[test]
    fn the_progress_rate_survives_a_sub_second_interval() {
        // A first report can land inside the first second, and dividing by a
        // zero elapsed time would panic.
        assert_eq!(rate_kib_per_sec(4096, Duration::from_millis(10)), 4);
        assert_eq!(rate_kib_per_sec(0, Duration::ZERO), 0);
        assert_eq!(
            rate_kib_per_sec(10 * 1024 * 30, Duration::from_secs(30)),
            10
        );
    }

    #[test]
    fn an_absent_part_file_resumes_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resumable_offset(&dir.path().join("absent.part")), 0);
    }

    #[test]
    fn a_recent_part_file_offers_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("db.mmdb.part");
        fs::write(&part, b"twelve bytes").unwrap();

        assert_eq!(resumable_offset(&part), 12);
        assert!(part.exists(), "a resumable part file is kept");
    }

    /// Bytes carrying the metadata marker a MaxMind DB ends with.
    fn mmdb_bytes(payload: &[u8]) -> Vec<u8> {
        let mut body = payload.to_vec();
        body.extend_from_slice(MMDB_MARKER);
        body.extend_from_slice(b"binary metadata section");
        body
    }

    #[test]
    fn materialise_gzip_writes_the_decompressed_file() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("db.mmdb");
        let part = with_extension(&dest, PART_EXT);
        let staged = with_extension(&dest, STAGE_EXT);

        let payload = mmdb_bytes(b"a body that round-trips");
        let mut encoder = flate2::write::GzEncoder::new(
            fs::File::create(&part).unwrap(),
            flate2::Compression::fast(),
        );
        encoder.write_all(&payload).unwrap();
        encoder.finish().unwrap();

        let size = materialise(&part, &staged, &dest, Archive::Gzip, DatabaseFormat::Mmdb).unwrap();
        assert_eq!(usize::try_from(size).unwrap(), payload.len());
        assert_eq!(fs::read(&dest).unwrap(), payload);
        assert!(!staged.exists(), "staged file must be renamed away");
    }

    #[test]
    fn materialise_raw_renames_the_body_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("db.mmdb");
        let part = with_extension(&dest, PART_EXT);
        let staged = with_extension(&dest, STAGE_EXT);

        let payload = mmdb_bytes(b"raw body");
        fs::write(&part, &payload).unwrap();
        let size = materialise(&part, &staged, &dest, Archive::Raw, DatabaseFormat::Mmdb).unwrap();

        assert_eq!(usize::try_from(size).unwrap(), payload.len());
        assert_eq!(fs::read(&dest).unwrap(), payload);
    }

    #[test]
    fn materialise_rejects_a_body_that_is_not_a_database() {
        // A provider answering 200 with a page reaches here looking like a
        // successful transfer.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("db.mmdb");
        let part = with_extension(&dest, PART_EXT);
        let staged = with_extension(&dest, STAGE_EXT);

        fs::write(&part, b"<html><title>Log in</title></html>").unwrap();
        let err =
            materialise(&part, &staged, &dest, Archive::Raw, DatabaseFormat::Mmdb).unwrap_err();

        assert!(
            matches!(err, GeoIpDownloadError::NotADatabase { .. }),
            "{err:?}"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn materialise_tar_gz_extracts_the_named_member() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("GeoLite2-City.mmdb");
        let part = with_extension(&dest, PART_EXT);
        let staged = with_extension(&dest, STAGE_EXT);

        // Mirror the real layout: the member sits under a dated directory.
        let payload = mmdb_bytes(b"city database bytes");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            fs::File::create(&part).unwrap(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                "GeoLite2-City_20241231/GeoLite2-City.mmdb",
                &payload[..],
            )
            .unwrap();
        builder
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .flush()
            .unwrap();

        let size = materialise(
            &part,
            &staged,
            &dest,
            Archive::TarGz {
                member: "GeoLite2-City.mmdb",
            },
            DatabaseFormat::Mmdb,
        )
        .unwrap();

        assert_eq!(usize::try_from(size).unwrap(), payload.len());
        assert_eq!(fs::read(&dest).unwrap(), payload);
    }

    #[test]
    fn materialise_tar_gz_reports_a_missing_member() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("GeoLite2-ASN.mmdb");
        let part = with_extension(&dest, PART_EXT);
        let staged = with_extension(&dest, STAGE_EXT);

        let builder = tar::Builder::new(flate2::write::GzEncoder::new(
            fs::File::create(&part).unwrap(),
            flate2::Compression::fast(),
        ));
        builder.into_inner().unwrap().finish().unwrap();

        let err = materialise(
            &part,
            &staged,
            &dest,
            Archive::TarGz {
                member: "GeoLite2-ASN.mmdb",
            },
            DatabaseFormat::Mmdb,
        )
        .unwrap_err();

        assert!(
            matches!(err, GeoIpDownloadError::ArchiveMemberMissing { member } if member == "GeoLite2-ASN.mmdb"),
            "{err:?}"
        );
        assert!(!dest.exists());
    }

    #[test]
    fn a_digest_is_lowercase_hex_of_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("body");
        // The digest of the empty input is a published constant of SHA-256.
        fs::write(&file, b"").unwrap();

        assert_eq!(
            sha256_of(&file).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
