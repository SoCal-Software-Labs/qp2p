// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under the MIT license <LICENSE-MIT
// http://opensource.org/licenses/MIT> or the Modified BSD license <LICENSE-BSD
// https://opensource.org/licenses/BSD-3-Clause>, at your option. This file may not be copied,
// modified, or distributed except according to those terms. Please review the Licences for the
// specific language governing permissions and limitations relating to use of the SAFE Network
// Software.

//! qp2p enables communication within a peer to peer network over the QUIC protocol.

// For explanation of lint checks, run `rustc -W help`
#![forbid(
    arithmetic_overflow,
    mutable_transmutes,
    no_mangle_const_items,
    unknown_crate_types
)]
#![deny(
    bad_style,
    deprecated,
    improper_ctypes,
    missing_docs,
    non_shorthand_field_patterns,
    overflowing_literals,
    stable_features,
    unconditional_recursion,
    unknown_lints,
    unsafe_code,
    unused_allocation,
    unused_attributes,
    unused_comparisons,
    unused_features,
    unused_parens,
    while_true,
    clippy::unicode_not_nfc
)]
#![warn(
    trivial_casts,
    trivial_numeric_casts,
    unreachable_pub,
    unused,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications,
    unused_results
)]

pub mod config;
mod connection_deduplicator;
mod connection_pool;
mod connections;
mod endpoint;
mod error;
#[cfg(feature = "igd")]
mod igd;
mod utils;
mod wire_msg;

pub use config::{Config, ConfigError, RetryConfig};
pub use connection_pool::ConnId;
pub use connections::{Connection, DisconnectionEvents, RecvStream, SendStream};
pub use endpoint::{Endpoint, IncomingConnections, IncomingMessages};
#[cfg(feature = "igd")]
pub use error::UpnpError;
pub use error::{
    ClientEndpointError, Close, ConnectionError, EndpointError, InternalConfigError, RecvError,
    RpcError, SendError, SerializationError, StreamError, TransportErrorCode,
    UnsupportedStreamOperation,
};

#[cfg(test)]
mod tests;
