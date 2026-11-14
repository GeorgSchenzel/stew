//! stew's hub: the daemon every client talks to, and the host CLI as one of its clients.

pub mod catalog;
pub mod config;
pub mod daemon;
pub mod error;
pub mod git;
pub mod id;
pub mod mirror;
pub mod paths;
pub mod refs;
pub mod state;
pub mod url;
