//! The recap server: it loads the edit and prompt history, keeps it current,
//! and serves it over HTTP. The wire types are in `recap-api`.

pub mod blame;
pub mod cache;
pub mod claude;
pub mod cursor_sessions;
pub mod cursor_workspace;
pub mod diff;
pub mod embed;
pub mod embedder;
pub mod history;
pub mod http;
pub mod logger;
pub mod nvim;
pub mod paths;
pub mod search;
pub mod server;
pub mod session;
pub mod sources;
pub mod store;

pub use server::Server;
