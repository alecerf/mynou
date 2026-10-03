#![forbid(unsafe_code)]

pub mod bencode;
pub mod config;
pub mod crypto;
pub mod demo;
pub mod engine;
pub mod integrations;
pub mod json;
pub mod media;
pub mod net;
pub mod organizer;
pub mod pki;
pub mod server;
pub mod store;
pub mod tls;
pub mod torrent;

pub type Result<T> = std::result::Result<T, String>;
