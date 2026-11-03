//! End-to-end tests for built-in API gateway plugins.
//!
//! Each plugin lives in its own module under `tests/plugins/`; the shared
//! harness lives in `tests/common/mod.rs`.

mod common;

mod plugins {
    mod acl;
    mod basic_auth;
    mod circuit_breaker;
    mod correlation_id;
    mod cors;
    mod header_rewrite;
    mod ip_restriction;
    mod jwt;
    mod key_auth;
    mod rate_limiting;
    mod request_size_limiting;
    mod request_termination;
    mod request_transformer;
    mod response_transformer;
    mod wasm_admin;
}
