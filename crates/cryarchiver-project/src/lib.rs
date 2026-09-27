//! The archival project layer (sub-project 2): one source tree mirrored into one destination, resumable across
//! quits and crashes.

pub mod dedup;
pub mod issues;
pub mod keys;
pub mod record;
pub mod references;
pub mod scan;
pub mod schedule;
pub mod settings;
pub mod store;
pub mod tree;
