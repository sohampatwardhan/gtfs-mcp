//! Download a static GTFS zip or a GTFS-Realtime protobuf.
//!
//! [`Loc`] is either an `http(s)` URL or a filesystem path. [`fetch_bytes`]
//! applies the feed's header map only to HTTP. Files ignore it. Downloads stop
//! at [`MAX_STATIC_BYTES`] or [`MAX_RT_BYTES`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::error::{err, AppError};

/// Largest static zip [`fetch_bytes`] will read, 250 MiB.
pub const MAX_STATIC_BYTES: usize = 250 * 1024 * 1024;
/// Largest realtime protobuf [`fetch_bytes`] will read, 80 MiB.
pub const MAX_RT_BYTES: usize = 80 * 1024 * 1024;

/// Where a feed file lives.
#[derive(Debug, Clone)]
pub enum Loc {
    /// An `http://` or `https://` URL, fetched with the feed's headers.
    Http(String),
    /// A filesystem path. Headers are not applied.
    File(PathBuf),
}

/// One agency feed: a static GTFS zip plus optional GTFS-Realtime protobufs.
#[derive(Debug, Clone)]
pub struct FeedSpec {
    /// Static GTFS zip. Required.
    pub static_gtfs: Loc,
    /// TripUpdates protobuf, when the feed publishes one.
    pub trip_updates: Option<Loc>,
    /// VehiclePositions protobuf, when the feed publishes one.
    pub vehicle_positions: Option<Loc>,
    /// Alerts protobuf, when the feed publishes one.
    pub alerts: Option<Loc>,
}

/// Reject header names or values that could split the request.
///
/// # Arguments
///
/// * `headers` - names and values that will be sent on HTTP fetches.
///
/// # Returns
///
/// `Ok(())` when every header is safe to send.
///
/// # Errors
///
/// Returns an error when a name is empty or contains CR, LF, or `:`, or when a
/// value contains CR or LF.
pub fn validate_headers(headers: &BTreeMap<String, String>) -> Result<(), AppError> {
    for (name, value) in headers {
        if name.is_empty()
            || name.bytes().any(|b| b == b'\r' || b == b'\n' || b == b':')
            || value.bytes().any(|b| b == b'\r' || b == b'\n')
        {
            return Err(err(format!("refusing header '{name}'"))
                .hint("header names and values cannot contain CR, LF, or ':'"));
        }
    }
    Ok(())
}

/// Read a [`Loc`] into memory, stopping at `max_bytes`.
///
/// HTTP requests send `headers`. Filesystem reads ignore them.
///
/// # Arguments
///
/// * `http` - client used for [`Loc::Http`]. Its User-Agent is `gtfs-mcp/<version>`.
/// * `loc` - URL or path.
/// * `headers` - extra headers for an HTTP GET. Not logged.
/// * `max_bytes` - cap for the body. [`MAX_STATIC_BYTES`] or [`MAX_RT_BYTES`].
///
/// # Returns
///
/// The raw bytes.
///
/// # Errors
///
/// Returns an error when the file cannot be read, the HTTP status is not
/// success (the status code is stored in `extra.status`), the server declares or
/// sends more than `max_bytes`, or the HTTP client fails.
pub async fn fetch_bytes(
    http: &reqwest::Client,
    loc: &Loc,
    headers: &BTreeMap<String, String>,
    max_bytes: usize,
) -> Result<Vec<u8>, AppError> {
    match loc {
        Loc::File(path) => {
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| err(format!("reading {}: {e}", path.display())))?;
            if bytes.len() > max_bytes {
                return Err(err(format!(
                    "{} is {} bytes, over the {} byte limit",
                    path.display(),
                    bytes.len(),
                    max_bytes
                )));
            }
            Ok(bytes)
        }
        Loc::Http(url) => {
            let mut request = http.get(url);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            let response = request.send().await?;
            let status = response.status();
            if !status.is_success() {
                let snippet = response
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(180)
                    .collect::<String>();
                return Err(err(format!("GET {url} returned {status}: {snippet}"))
                    .extra("status", Value::from(status.as_u16())));
            }
            read_capped(response, max_bytes, url).await
        }
    }
}

/// Read an HTTP body, refusing a declared or actual size over `max_bytes`.
///
/// # Arguments
///
/// * `response` - a successful response whose body is still unread.
/// * `max_bytes` - maximum accepted length.
/// * `url` - included in error text. A query string on this URL is not redacted.
///
/// # Returns
///
/// The body bytes.
///
/// # Errors
///
/// Returns an error when `Content-Length` or the streamed body exceeds
/// `max_bytes`, or when reading the body fails.
async fn read_capped(
    mut response: reqwest::Response,
    max_bytes: usize,
    url: &str,
) -> Result<Vec<u8>, AppError> {
    if let Some(len) = response.content_length() {
        if len as usize > max_bytes {
            return Err(err(format!(
                "{url} declared {len} bytes, over the {max_bytes} byte limit"
            )));
        }
    }
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(err(format!(
                "{url} exceeded the {max_bytes} byte download limit"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// URL or path text for health JSON and errors.
///
/// # Arguments
///
/// * `loc` - the location to show.
///
/// # Returns
///
/// The URL, or the filesystem path's display form. Query strings are kept.
pub fn loc_label(loc: &Loc) -> String {
    match loc {
        Loc::Http(url) => url.clone(),
        Loc::File(path) => path.display().to_string(),
    }
}

/// HTTP client used for feed downloads.
///
/// # Returns
///
/// A client with a 45 second timeout, a 10 second connect timeout, and at most
/// five redirects. The User-Agent is `gtfs-mcp/` plus the crate version.
///
/// # Errors
///
/// Returns an error when the client cannot be built.
pub fn http_client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .user_agent(concat!("gtfs-mcp/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(45))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| err(format!("http client: {e}")))
}
