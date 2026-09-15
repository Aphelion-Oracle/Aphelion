//! Subcommands that belong to the binary rather than to the library.
//!
//! Declared from `main.rs`, so nothing here is part of the `aphelion_node`
//! crate's public surface. The split is by audience: `engine`, `chain` and the
//! rest are what the node *is*, and this is how an operator talks to it. The
//! committee commands in particular are a lot of argument parsing and
//! formatting wrapped around a small amount of judgement, and the judgement
//! lives in [`aphelion_node::engine::duty`] where it can be tested without a
//! chain.

pub mod beacon;
pub mod committee;
pub mod replay;
pub mod status;
pub mod verify_evidence;
