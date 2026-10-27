#![forbid(unsafe_code)]

pub mod bencode;
pub mod config;
pub mod crypto;
pub mod date;
pub mod engine;
pub mod indexers;
pub mod integrations;
pub mod inventory;
pub mod irc;
pub mod json;
pub mod library;
pub mod manual;
pub mod media;
pub mod net;
pub mod netstack;
pub mod numbering;
pub mod organizer;
pub mod pack;
pub mod pki;
pub mod profiles;
pub mod progress;
pub mod requesters;
pub mod selection;
pub mod series;
pub mod server;
pub mod service;
pub mod settings;
pub mod source_network;
pub mod store;
pub mod tls;
pub mod torrent;
pub mod tunnel;
pub mod tunnel_http;
pub mod ui;
pub mod update;
pub mod vpn;
pub mod wireguard;
pub mod ws;
mod xml;

pub type Result<T> = std::result::Result<T, String>;

#[cfg(test)]
mod growth;
