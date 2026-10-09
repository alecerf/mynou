#![forbid(unsafe_code)]

pub mod bencode;
pub mod config;
pub mod crypto;
pub mod date;
pub mod demo;
pub mod engine;
pub mod indexers;
pub mod integrations;
pub mod irc;
pub mod json;
pub mod library;
pub mod media;
pub mod net;
pub mod notifications;
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
mod xml;

pub type Result<T> = std::result::Result<T, String>;
