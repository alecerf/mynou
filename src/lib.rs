#![forbid(unsafe_code)]

pub mod bencode;
pub mod config;
pub mod crypto;
pub mod date;
pub mod demo;
pub mod engine;
pub mod integrations;
pub mod json;
pub mod library;
pub mod media;
pub mod net;
pub mod numbering;
pub mod organizer;
pub mod pack;
pub mod pki;
pub mod requesters;
pub mod selection;
pub mod series;
pub mod server;
pub mod store;
pub mod tls;
pub mod torrent;
mod web;

pub type Result<T> = std::result::Result<T, String>;
