//! GTFS static and GTFS-Realtime explorer for MCP clients.

mod catalog;
mod config;
mod error;
pub mod fixtures;
mod gtfs;
mod mcp;
mod query;
mod realtime;
mod source;

pub mod gtfs_rt {
    // The vendored GTFS-Realtime proto's comments are not written for rustdoc.
    #![allow(clippy::doc_lazy_continuation, clippy::doc_overindented_list_items)]
    include!(concat!(env!("OUT_DIR"), "/transit_realtime.rs"));
}

pub use catalog::{Catalog, RegisterRequest};
pub use config::{startup_plan, ConfigError, StartupOverrides};
pub use error::AppError;
pub use mcp::{serve_http, serve_stdio, spawn_http, GtfsServer};
pub use query::{AlertQuery, TripUpdateQuery, VehicleQuery};
