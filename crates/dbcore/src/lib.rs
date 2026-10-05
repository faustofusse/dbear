//! dbear shared core.
//!
//! Everything that isn't UI lives here: models, drivers, value decoding, SQL handling.
//! - The GPUI (Linux) app depends on this crate directly.
//! - The SwiftUI (macOS) app uses it through `dbcore-ffi` (UniFFI).

pub mod access;
mod config;
mod connection;
pub mod complete;
pub mod dialect;
pub mod dump;
pub mod driver;
pub mod edit;
pub mod export;
pub mod highlight;
pub mod import;
pub mod keyset;
pub mod libsql;
pub mod mock;
pub mod model;
pub mod mysql;
pub mod restore;
pub mod postgres;
pub mod sqlite;
pub mod sqlserver;
pub mod store;

pub use connection::Connection;
pub use driver::{Driver, Error, Result};
pub use keyset::{PageCursor, RowPage};
pub use model::*;
pub use store::ConnectionStore;
