//! Original HTTP fixture: no framework or dependency.
#![allow(dead_code)]
use mynou::{config::Config, engine::Engine, server::Api};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle},
    time::Duration,
};

pub const TOKEN: &str = "web-fixture-token-012345678901234567890";

pub struct Server {
    pub engine: Arc<Engine>,
    pub authority: String,
    thread: Option<JoinHandle<mynou::Result<()>>>,
}

impl Server {
    pub fn open(cfg: Config) -> Self {
        Self::open_with_token(cfg, TOKEN)
    }

    pub fn open_with_token(cfg: Config, token: &str) -> Self {
        let engine = Engine::open(cfg).unwrap();
        let api = Api::bind(engine.clone(), token.to_owned()).unwrap();
        Self {
            authority: api.address().unwrap().to_string(),
            engine,
            thread: Some(thread::spawn(move || api.run())),
        }
    }

    pub fn call(&self, method: &str, route: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut stream = TcpStream::connect(&self.authority).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        let authority = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Host"))
            .map(|(_, value)| *value)
            .unwrap_or(&self.authority);
        write!(stream, "{method} {route} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len()).unwrap();
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("Host") {
                write!(stream, "{name}: {value}\r\n").unwrap();
            }
        }
        write!(stream, "\r\n{body}").unwrap();
        let mut bytes = Vec::new();
        stream
            .take(2 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").expect("HTTP response headers");
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers: BTreeMap<_, _> = lines
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (name.to_ascii_lowercase(), value.trim().to_owned())
            })
            .collect();
        assert_eq!(
            headers["content-length"].parse::<usize>().unwrap(),
            body.len()
        );
        Reply {
            status,
            headers,
            body: body.to_owned(),
        }
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.authority)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.engine.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if !thread::panicking() => panic!("Web fixture failed: {error}"),
                Err(error) if !thread::panicking() => std::panic::resume_unwind(error),
                _ => {}
            }
        }
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Reply {
    pub fn no_secrets(&self) {
        for forbidden in [
            TOKEN,
            "library-indexer-fixture-secret",
            "library-download-fixture-secret",
            "magnet:",
            "acquisition_url",
            "source_url",
        ] {
            assert!(
                !self.body.contains(forbidden),
                "Response exposed {forbidden}"
            );
            assert!(
                !self.headers.values().any(|value| value.contains(forbidden)),
                "Response header exposed {forbidden}"
            );
        }
    }
}
