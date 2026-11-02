//! End-to-end tests for built-in API gateway plugins.
//!
//! Each plugin lives in its own module under `tests/plugins/`; the shared
//! harness lives in `tests/common/mod.rs`.

mod common;

mod plugins {
    mod acl;
    mod basic_auth;
    mod cors;
    mod ip_restriction;
    mod jwt;
    mod key_auth;
    mod rate_limit;
}
