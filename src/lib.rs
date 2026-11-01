//! MCP server for GTFS static schedules and optional GTFS-Realtime.
//!
//! The crate loads one static GTFS zip into memory and, when configured, overlays
//! TripUpdates, VehiclePositions, and Alerts. An MCP client talks to it over stdio
//! ([`serve_stdio`]) or streamable HTTP ([`serve_http`]). The binary reads the same
//! configuration from the environment before the first tool call.
//!
//! # Architecture
//!
//! * `config` parses `GTFS_MCP_*` variables and the matching CLI flags into one
//!   [`RegisterRequest`].
//! * `source` downloads a static zip and realtime protobufs, with a byte cap.
//! * `gtfs` loads the zip into an in-memory SQLite database.
//! * `realtime` decodes a GTFS-Realtime [`gtfs_rt::FeedMessage`].
//! * `query` answers trip, board, vehicle, and alert questions from that database
//!   and the decoded messages.
//! * `catalog` keeps the loaded feeds, refreshes them on a TTL, and is what the
//!   tools call.
//! * `mcp` is the tool surface and the stdio / HTTP servers.
//! * [`fixtures`] builds the Harbor Line feed used by tests. It does not call the network.
//! * `error` is the [`AppError`] returned to agents.
//! * [`gtfs_rt`] is the vendored GTFS-Realtime protobuf schema.
//!
//! # Configuration
//!
//! `GTFS_MCP_STATIC` is required (URL or filesystem path). These are optional:
//! `GTFS_MCP_TRIP_UPDATES`, `GTFS_MCP_VEHICLE_POSITIONS`, `GTFS_MCP_ALERTS`,
//! `GTFS_MCP_HTTP_HEADERS`, `GTFS_MCP_FEED_ID` (default `default`), `GTFS_MCP_RT_TTL`,
//! and `GTFS_MCP_STATIC_TTL`. A missing static location or a malformed optional value
//! fails before the server listens. Header values are not copied into errors.
//!
//! The server does not exchange a username and password for a token. Credentials are
//! extra HTTP headers on a feed, or a query string already present on a URL.
//!
//! # Examples
//!
//! Register a placeholder static feed without contacting the network. A path that is
//! not `http://` or `https://` is a file, and this call only parses configuration.
//!
//! ```
//! use gtfs_mcp::{startup_plan, StartupOverrides};
//!
//! let request = startup_plan(
//!     [(
//!         "GTFS_MCP_STATIC".to_string(),
//!         "https://example.com/gtfs.zip".to_string(),
//!     )],
//!     &StartupOverrides::default(),
//! )
//! .expect("static URL");
//! assert_eq!(request.feed_id.as_deref(), Some("default"));
//! assert_eq!(
//!     request.static_url.as_deref(),
//!     Some("https://example.com/gtfs.zip")
//! );
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![warn(clippy::missing_docs_in_private_items)]
#![warn(clippy::missing_errors_doc)]
#![warn(clippy::missing_panics_doc)]

mod catalog;
mod config;
mod error;
pub mod fixtures;
mod gtfs;
mod mcp;
mod query;
mod realtime;
mod source;

/// Vendored GTFS-Realtime protobuf types generated from `proto/gtfs-realtime.proto`.
///
/// The schema is the MobilityData spec (Apache-2.0). `prost-build` writes the Rust
/// items, so they keep the comments from the `.proto` file and are not hand-documented
/// here.
pub mod gtfs_rt {
    // Generated items. Their docs come from the .proto comments, which are not
    // written to satisfy this crate's rustdoc lints.
    #![allow(missing_docs)]
    #![allow(clippy::missing_docs_in_private_items)]
    #![allow(clippy::missing_errors_doc)]
    #![allow(clippy::missing_panics_doc)]
    #![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]
    include!(concat!(env!("OUT_DIR"), "/transit_realtime.rs"));
}

pub use catalog::{Catalog, RegisterRequest};
pub use config::{startup_plan, ConfigError, StartupOverrides};
pub use error::AppError;
pub use mcp::{serve_http, serve_stdio, spawn_http, GtfsServer};
pub use query::{AlertQuery, TripUpdateQuery, VehicleQuery};
