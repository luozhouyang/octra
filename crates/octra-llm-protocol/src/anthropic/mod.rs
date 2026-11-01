//! Anthropic Messages adapter.
//!
//! Lossy mappings (documented where implemented):
//! - Consecutive tool messages are merged into one user turn of
//!   `tool_result` blocks, the only shape Anthropic accepts.
//! - Tool call ids follow Anthropic's `[a-zA-Z0-9_-]` constraint; foreign ids
//!   are rewritten for the upstream and restored in the response.
//! - `presence_penalty`, `frequency_penalty`, `logprobs`, `n` and `logit_bias`
//!   have no Anthropic equivalent and are dropped.
//! - `result`-style OpenAI provider tool results have no block type; they are
//!   encoded as text.

pub mod request;
pub mod response;
pub mod stream;
