//! Exercise the public connector API without a resolver or a physical stream.
use mynou::{
    Result,
    net::{HttpClient, HttpConnector, NetworkStream},
};
use std::{
    collections::VecDeque,
    io::{self, Cursor, Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Requests = Arc<Mutex<Vec<Vec<u8>>>>;

#[derive(Default)]
struct Fixture {
    replies: Mutex<VecDeque<Vec<u8>>>,
    requests: Requests,
    endpoints: Mutex<Vec<(String, u16, Instant)>>,
    timeouts: Arc<Mutex<Vec<Duration>>>,
    late: bool,
}

impl Fixture {
    fn new(replies: &[&[u8]]) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.iter().map(|reply| reply.to_vec()).collect()),
            ..Self::default()
        })
    }

    fn request(&self, index: usize) -> String {
        String::from_utf8(self.requests.lock().unwrap()[index].clone()).unwrap()
    }
}

struct MemoryStream {
    response: Cursor<Vec<u8>>,
    requests: Requests,
    index: usize,
    timeouts: Arc<Mutex<Vec<Duration>>>,
}

impl Read for MemoryStream {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        self.response.read(data)
    }
}

impl Write for MemoryStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.requests.lock().unwrap()[self.index].extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl NetworkStream for MemoryStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.timeouts.lock().unwrap().push(timeout.unwrap());
        Ok(())
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.timeouts.lock().unwrap().push(timeout.unwrap());
        Ok(())
    }
}

impl HttpConnector for Fixture {
    fn connect(&self, host: &str, port: u16, deadline: Instant) -> Result<Box<dyn NetworkStream>> {
        self.endpoints
            .lock()
            .unwrap()
            .push((host.into(), port, deadline));
        let response = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or("Synthetic connector refused destination")?;
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(Vec::new());
            requests.len() - 1
        };
        if self.late {
            // Simulate a buggy connector returning a stream after its deadline.
            std::thread::sleep(Duration::from_millis(30));
        }
        Ok(Box::new(MemoryStream {
            response: Cursor::new(response),
            requests: self.requests.clone(),
            index,
            timeouts: self.timeouts.clone(),
        }))
    }
}

#[test]
fn supplied_stream_handles_unresolvable_names_and_bounded_io() {
    let fixture = Fixture::new(&[b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong"]);
    let client = HttpClient::new()
        .with_timeout(Duration::from_secs(2))
        .with_connector(fixture.clone());
    let response = client
        .clone()
        .get("http://only-inside.invalid:8123/a?q=b")
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"pong");
    let request = fixture.request(0);
    assert!(request.starts_with("GET /a?q=b HTTP/1.1\r\n"));
    assert!(request.contains("Host: only-inside.invalid:8123\r\n"));
    let endpoints = fixture.endpoints.lock().unwrap();
    assert_eq!(endpoints[0].0, "only-inside.invalid");
    assert_eq!(endpoints[0].1, 8123);
    let timeouts = fixture.timeouts.lock().unwrap();
    assert!(!timeouts.is_empty());
    assert!(
        timeouts
            .iter()
            .all(|t| !t.is_zero() && *t <= Duration::from_secs(2))
    );
    assert!(timeouts.windows(2).all(|pair| pair[1] <= pair[0]));
    assert!(format!("{client:?}").contains("custom_connector: true"));
}

#[test]
fn every_redirect_keeps_the_connector_deadline_and_credential_policy() {
    let fixture = Fixture::new(&[
        b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 302 Found\r\nLocation: http://second.invalid/final\r\nContent-Length: 0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ndone\r\n0\r\n\r\n",
    ]);
    let response = HttpClient::new()
        .with_connector(fixture.clone())
        .request(
            "GET",
            "http://first.invalid/start",
            &[
                ("Authorization".into(), "Bearer synthetic".into()),
                ("Cookie".into(), "synthetic=1".into()),
                ("Accept".into(), "text/plain".into()),
            ],
            &[],
        )
        .unwrap();
    assert_eq!(response.body, b"done");
    assert!(
        fixture
            .request(1)
            .contains("Authorization: Bearer synthetic\r\n")
    );
    assert!(fixture.request(1).starts_with("GET /next HTTP/1.1\r\n"));
    let final_request = fixture.request(2);
    assert!(!final_request.contains("Authorization:"));
    assert!(!final_request.contains("Cookie:"));
    assert!(final_request.contains("Accept: text/plain\r\n"));
    let endpoints = fixture.endpoints.lock().unwrap();
    assert_eq!(endpoints.len(), 3);
    assert_eq!(endpoints[2].0, "second.invalid");
    assert!(
        endpoints
            .iter()
            .all(|endpoint| endpoint.2 == endpoints[0].2)
    );
}

#[test]
fn connector_failure_never_falls_back_to_the_physical_endpoint() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let fixture = Fixture::new(&[]);
    let error = HttpClient::new()
        .with_connector(fixture.clone())
        .get(&format!("http://{}/", trap.local_addr().unwrap()))
        .unwrap_err();
    assert_eq!(error, "Synthetic connector refused destination");
    assert_eq!(fixture.endpoints.lock().unwrap().len(), 1);
    assert_eq!(trap.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn a_stream_returned_after_the_deadline_cannot_send_a_request() {
    let fixture = Arc::new(Fixture {
        replies: Mutex::new(VecDeque::from([
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
        ])),
        late: true,
        ..Fixture::default()
    });
    assert!(
        HttpClient::new()
            .with_timeout(Duration::from_millis(10))
            .with_connector(fixture.clone())
            .get("http://late.invalid/")
            .is_err()
    );
    assert!(fixture.request(0).is_empty());
    assert!(fixture.timeouts.lock().unwrap().is_empty());
}

#[test]
fn supplied_transport_preserves_body_and_redirect_limits() {
    let fixture = Fixture::new(&[b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nlarge"]);
    assert!(
        HttpClient::new()
            .with_max_body(4)
            .with_connector(fixture)
            .get("http://large.invalid/")
            .is_err()
    );
    let fixture =
        Fixture::new(&[b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\n\r\n"]);
    assert!(
        HttpClient::new()
            .without_redirects()
            .with_connector(fixture.clone())
            .get("http://loop.invalid/")
            .is_err()
    );
    assert_eq!(fixture.endpoints.lock().unwrap().len(), 1);
}

#[test]
fn https_uses_tls_on_the_supplied_stream_without_plaintext_fallback() {
    let fixture = Fixture::new(&[b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"]);
    assert!(
        HttpClient::new()
            .with_connector(fixture.clone())
            .get("https://secure.invalid/private")
            .is_err()
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let hello = &requests[0];
    assert_eq!(&hello[..3], &[22, 3, 1]);
    assert!(
        hello
            .windows(b"secure.invalid".len())
            .any(|w| w == b"secure.invalid")
    );
    assert!(
        !hello
            .windows(b"GET /private".len())
            .any(|w| w == b"GET /private")
    );
}

#[test]
fn connector_ignores_environment_proxies() {
    const CHILD: &str = "MYNOU_CONNECTOR_PROXY_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        // Environment mutation is confined to a child; other tests keep theirs.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "connector_ignores_environment_proxies",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HTTP_PROXY", "malformed-proxy")
            .env("http_proxy", "malformed-proxy")
            .env("HTTPS_PROXY", "malformed-proxy")
            .env("https_proxy", "malformed-proxy")
            .env("ALL_PROXY", "malformed-proxy")
            .env("all_proxy", "malformed-proxy")
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fixture = Fixture::new(&[b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"]);
    HttpClient::new()
        .with_connector(fixture.clone())
        .get("http://inside.invalid/")
        .unwrap();
    assert_eq!(fixture.endpoints.lock().unwrap()[0].0, "inside.invalid");
    assert!(fixture.request(0).starts_with("GET / HTTP/1.1\r\n"));
    let fixture = Fixture::new(&[b""]);
    assert!(
        HttpClient::new()
            .with_connector(fixture.clone())
            .get("https://inside.invalid/")
            .is_err()
    );
    assert_eq!(fixture.endpoints.lock().unwrap()[0].0, "inside.invalid");
    assert_eq!(fixture.requests.lock().unwrap()[0][0], 22);
}
