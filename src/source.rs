use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::error::{err, AppError};

pub const MAX_STATIC_BYTES: usize = 250 * 1024 * 1024;
pub const MAX_RT_BYTES: usize = 80 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum Loc {
    Http(String),
    File(PathBuf),
}

/// One agency feed: a static GTFS zip plus optional GTFS-Realtime protobufs.
#[derive(Debug, Clone)]
pub struct FeedSpec {
    pub static_gtfs: Loc,
    pub trip_updates: Option<Loc>,
    pub vehicle_positions: Option<Loc>,
    pub alerts: Option<Loc>,
}

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

pub fn loc_label(loc: &Loc) -> String {
    match loc {
        Loc::Http(url) => url.clone(),
        Loc::File(path) => path.display().to_string(),
    }
}

pub fn http_client() -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .user_agent(concat!("gtfs-rt-mcp/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(45))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| err(format!("http client: {e}")))
}
