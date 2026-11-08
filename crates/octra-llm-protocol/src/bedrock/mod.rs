//! AWS Bedrock Converse adapter.
//!
//! `converse` uses the JSON Converse API; `converse-stream` uses the binary
//! AWS event-stream framing decoded by [`eventstream::Framer`]. Authentication
//! is a Bedrock API key sent as a bearer token; SigV4 is out of scope.

pub mod eventstream;
pub mod request;
pub mod response;
pub mod stream;

pub use eventstream::{EventFrame, Framer};
