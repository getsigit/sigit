//! siGit Code as a library: its [`Profile`](ed_acp::Profile) for `ed-acp`, the browser sign-in
//! and the stored session, for hosts that embed the agent instead of launching the `sigit`
//! binary.
//!
//! A host serves [`SiGitCode`] with `ed_acp::serve` over an in-memory channel and runs an ACP
//! client on the other end, which keeps the agent inside an app sandbox that can't spawn it as
//! a subprocess. Inference goes to siGit Code Cloud with the session [`SiGitAuth`] holds; the
//! command-line agent uses the same sign-in and the same prompts.

pub mod auth;
pub mod browser_auth;
pub mod credentials;
pub mod profile;
pub mod prompt;

pub use auth::{METHOD_ID, SiGitAuth};
pub use ed_acp;
pub use profile::{INFO, SiGitCode, official_mcp_servers};
