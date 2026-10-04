//! Original HTTP/form fixture: no browser framework or dependency.
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
        let engine = Engine::open(cfg).unwrap();
        let api = Api::bind(engine.clone(), TOKEN.into()).unwrap();
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
                "Browser page exposed {forbidden}"
            );
            assert!(
                !self.headers.values().any(|value| value.contains(forbidden)),
                "Response header exposed {forbidden}"
            );
        }
    }
}

pub struct Browser {
    pub cookie: String,
    pub csrf: String,
}

impl Browser {
    pub fn challenge(server: &Server) -> Self {
        let response = server.call("GET", "/ui/login", &[], "");
        assert_eq!(response.status, 200);
        response.no_secrets();
        Self {
            cookie: response.headers["set-cookie"]
                .split(';')
                .next()
                .unwrap()
                .to_owned(),
            csrf: csrf(&response.body),
        }
    }

    pub fn login(server: &Server) -> Self {
        let mut browser = Self::challenge(server);
        let old_cookie = browser.cookie.clone();
        let body = fields(&[("csrf", &browser.csrf), ("token", TOKEN)]);
        let response = server.call(
            "POST",
            "/ui/login",
            &[
                ("Cookie", &browser.cookie),
                ("Origin", &server.origin()),
                ("Content-Type", "application/x-www-form-urlencoded"),
            ],
            &body,
        );
        assert_eq!(response.status, 303, "{}", response.body);
        assert_eq!(response.headers["location"], "/ui");
        response.no_secrets();
        browser.cookie = response.headers["set-cookie"]
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert_ne!(old_cookie, browser.cookie);
        let page = browser.get(server, "/ui");
        assert_eq!(page.status, 200);
        browser.csrf = csrf(&page.body);
        page.no_secrets();
        browser
    }

    pub fn get(&self, server: &Server, route: &str) -> Reply {
        server.call("GET", route, &[("Cookie", &self.cookie)], "")
    }

    pub fn post(&self, server: &Server, route: &str, values: &[(&str, &str)]) -> Reply {
        let mut entries = vec![("csrf", self.csrf.as_str())];
        entries.extend_from_slice(values);
        self.raw_post(server, route, &fields(&entries))
    }

    pub fn raw_post(&self, server: &Server, route: &str, body: &str) -> Reply {
        server.call(
            "POST",
            route,
            &[
                ("Cookie", &self.cookie),
                ("Origin", &server.origin()),
                ("Content-Type", "application/x-www-form-urlencoded"),
            ],
            body,
        )
    }
}

pub fn csrf(page: &str) -> String {
    page.split("name=\"csrf\" value=\"")
        .nth(1)
        .expect("CSRF field")
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

pub fn fields(values: &[(&str, &str)]) -> String {
    values
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode(text: &str) -> String {
    let mut result = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}
