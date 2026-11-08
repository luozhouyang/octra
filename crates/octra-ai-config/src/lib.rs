//! AI-native LLM control-plane entities and compiler.
//!
//! The control plane is stored as flat, independently addressable records
//! ([`AiProvider`], [`AiModel`], [`AiPolicy`], ...). Data-plane consumers need a
//! resolved view instead: provider accounts grouped under their provider, model
//! targets expanded into either a provider reference or a nested model, and
//! policy chains ordered from the most specific scope to the global scope.
//!
//! [`compile`] validates the flat records and produces a
//! [`CompiledControlPlane`]. Compilation is pure: it performs no I/O and does
//! not mutate its input. All failures are reported at once as a
//! `Vec<CompileError>` so operators can fix a whole configuration in one pass.

mod compile;
mod types;

pub use compile::{
    CompileError, CompiledControlPlane, CompiledModel, CompiledProvider, CompiledProviderRef,
    CompiledTarget, MAX_GROUP_DEPTH, compile,
};
pub use types::{
    AiBalancer, AiControlPlaneData, AiModel, AiModelKind, AiModelTarget, AiPolicy,
    AiPolicyAttachment, AiPolicyScope, AiPolicyType, AiProvider, AiProviderAccount,
};
