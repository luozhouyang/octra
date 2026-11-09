//! OpenAI Chat Completions adapter.
//!
//! Lossy in the other direction only:
//! - Anthropic `thinking` blocks and `signature` have no OpenAI equivalent in
//!   the request; reasoning content received from OpenAI (`reasoning_content`)
//!   is surfaced as a canonical reasoning block.
//! - Anthropic provider-executed tools (`web_search_*`, `computer`) are sent
//!   to OpenAI only when they also exist as OpenAI function tools.

pub mod request;
pub mod response;
pub mod stream;
