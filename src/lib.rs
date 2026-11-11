#![deny(unsafe_code)]

pub mod access;
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
pub mod logs;
pub mod manual;
pub mod media;
pub mod net;
pub mod netstack;
pub mod numbering;
pub mod organizer;
pub mod pack;
pub mod pki;
pub mod plex;
pub mod profiles;
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

/// Entry points for `examples/parser_fuzz.rs` into parsers that are not
/// otherwise public. Each takes untrusted bytes and must never panic.
#[doc(hidden)]
pub mod fuzzing {
    pub fn metainfo(data: &[u8]) {
        crate::torrent::fuzz_metainfo(data);
    }
    pub fn tracker(data: &[u8]) {
        crate::torrent::fuzz_tracker(data);
    }
    pub fn tls(data: &[u8]) {
        crate::tls::fuzz(data);
    }
    pub fn packets(data: &[u8]) {
        crate::netstack::fuzz(data);
    }
    pub fn utp(data: &[u8]) {
        crate::torrent::fuzz_utp(data);
    }
    pub fn dht(data: &[u8]) {
        crate::torrent::fuzz_dht(data);
    }
    pub fn xml(data: &[u8]) {
        if let Ok(text) = std::str::from_utf8(data) {
            let _ = crate::xml::parse_xml(text);
        }
    }
}

#[cfg(test)]
mod growth;
