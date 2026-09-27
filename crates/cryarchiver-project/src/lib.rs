//! The archival project layer (sub-project 2): one source tree mirrored into one destination, resumable across
//! quits and crashes.

pub mod dedup;
pub mod issues;
pub mod keys;
pub mod project;
pub mod record;
pub mod references;
pub mod run;
pub mod scan;
pub mod schedule;
pub mod settings;
pub mod store;
pub mod tree;
pub mod view;
pub mod watch;

pub use project::{
	Backend, Command, PROJECT_EXTENSION, Project, ProjectError, ProjectOptions, SystemSample, Tuning, create,
};
