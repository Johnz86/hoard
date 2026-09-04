//! Upstreams: the Hub protocol spoken outwards, over one pooled client.
//!
//! `huggingface.co` and a peer hoard node speak the same API, so both are a [`hub::Hub`]
//! over a different base URL and a peer pull is an ordinary block-cached resolve.

pub mod client;
pub mod hub;
pub mod meta;
pub mod peer;
pub mod range;

pub use client::{
    Authorization, JSON_CONTENT_TYPE, MAX_PASS_THROUGH_BYTES, ProxyResponse, RequestBody,
    RetryPolicy, Retryability, UpstreamClient, UpstreamRequest, classify, read_capped,
};
pub use hub::{BlockFetch, HEADER_CONTENT_SHA256, Hub, RemoteFile, UpstreamKind, UpstreamRevision};
pub use meta::{DEFAULT_REVISION, MetaKind, MetaRequest};
pub use peer::{DEFAULT_PEER_PORT, peer_base_url, peer_upstream};
pub use range::{ByteRange, RangeOutcome, parse_ranges};
