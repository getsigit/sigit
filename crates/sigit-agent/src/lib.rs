//! Shared siGit Code internals for hosts that embed the agent and for the `sigit` binary: the
//! prompts, the browser sign-in, and the stored cloud session.
//!
//! The command-line agent and an embedding host use the same sign-in and the same prompts, so
//! they live here once rather than in the binary crate.

pub mod browser_auth;
pub mod credentials;
pub mod prompt;
