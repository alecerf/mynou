//! HTTP/1.1 client with bounded parsing, explicit timeouts and authenticated TLS.
use crate::Result;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const MAX_HEADER: usize = 65_536;
const MAX_LINE: usize = 8_192;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn origin(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{}://{}:{}", self.scheme, host, self.port)
    }

    pub fn as_string(&self) -> String {
        format!("{}{}", self.origin(), self.path)
    }
}

pub fn parse_url(input: &str) -> Result<Url> {
    if input.len() > MAX_LINE || input.chars().any(|c| c.is_control()) {
        return Err("URL is too long or contains control characters".into());
    }
    let (scheme, rest) = input
        .split_once("://")
        .ok_or("An absolute HTTP or HTTPS URL is required")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return Err("Unsupported URL scheme".into()),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || authority.contains('@') || authority.contains('\\') {
        return Err("Invalid URL authority; embedded credentials are forbidden".into());
    }
    let (host, port_text) = if let Some(ip) = authority.strip_prefix('[') {
        let close = ip.find(']').ok_or("Invalid IPv6 URL address")?;
        let host = &ip[..close];
        if !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
            return Err("Invalid IPv6 URL address".into());
        }
        let tail = &ip[close + 1..];
        let port = if tail.is_empty() {
            None
        } else {
            Some(tail.strip_prefix(':').ok_or("Invalid URL port")?)
        };
        (host.to_owned(), port)
    } else {
        if authority.matches(':').count() > 1 {
            return Err("IPv6 URL addresses must be enclosed in brackets".into());
        }
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty()
        || host.len() > 253
        || (host.parse::<IpAddr>().is_err()
            && !host.split('.').all(|part| {
                !part.is_empty()
                    && part.len() <= 63
                    && !part.starts_with('-')
                    && !part.ends_with('-')
                    && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }))
    {
        return Err("Invalid URL hostname; ASCII or Punycode is required".into());
    }
    let port = match port_text {
        Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            text.parse::<u16>().map_err(|_| "Invalid URL port")?
        }
        Some(_) => return Err("Invalid URL port".into()),
        None => default_port,
    };
    if port == 0 {
        return Err("URL port zero is forbidden".into());
    }
    let suffix = rest[end..].split('#').next().unwrap_or_default();
    let suffix = if suffix.is_empty() {
        "/".to_owned()
    } else if suffix.starts_with('?') {
        format!("/{suffix}")
    } else {
        suffix.to_owned()
    };
    let mut path = String::with_capacity(suffix.len());
    let bytes = suffix.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return Err("Invalid URL escape".into());
            }
            path.push('%');
            path.push(bytes[i + 1] as char);
            path.push(bytes[i + 2] as char);
            i += 3;
        } else if b == b'\\' {
            return Err("Backslashes are forbidden in URLs".into());
        } else if b.is_ascii() && b != b' ' {
            path.push(b as char);
            i += 1;
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            path.push('%');
            path.push(HEX[(b >> 4) as usize] as char);
            path.push(HEX[(b & 15) as usize] as char);
            i += 1;
        }
    }
    Ok(Url {
        scheme,
        host,
        port,
        path,
    })
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct HttpClient {
    timeout: Duration,
    max_body: usize,
    redirects: usize,
    environment_proxy: bool,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            max_body: 16 * 1024 * 1024,
            redirects: 5,
            environment_proxy: true,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    pub fn with_max_body(mut self, max_body: usize) -> Self {
        self.max_body = max_body;
        self
    }

    /// Keep endpoint-bound requests from redirecting to another handler.
    pub fn without_redirects(mut self) -> Self {
        self.redirects = 0;
        self
    }

    /// Ignore HTTP_PROXY/HTTPS_PROXY/NO_PROXY, useful for isolated local peers.
    pub fn without_proxy(mut self) -> Self {
        self.environment_proxy = false;
        self
    }

    pub fn get(&self, url: &str) -> Result<Response> {
        self.request("GET", url, &[], &[])
    }

    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<Response> {
        if method.is_empty() || method.len() > 32 || !method.bytes().all(token_byte) {
            return Err("Invalid HTTP method".into());
        }
        if body.len() > self.max_body {
            return Err("HTTP request body exceeds the limit".into());
        }
        if headers.len() > 256 {
            return Err("Too many HTTP request headers".into());
        }
        let mut header_size = 0usize;
        for (name, value) in headers {
            header_size = header_size
                .checked_add(name.len())
                .and_then(|size| size.checked_add(value.len()))
                .and_then(|size| size.checked_add(4))
                .ok_or("HTTP request headers are too large")?;
            if header_size > MAX_HEADER - MAX_LINE - 512 {
                return Err("HTTP request headers exceed the limit".into());
            }
            validate_header(name, value)?;
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "host"
                    | "connection"
                    | "content-length"
                    | "transfer-encoding"
                    | "proxy-authorization"
                    | "proxy-connection"
            ) {
                return Err("Reserved HTTP framing header".into());
            }
        }
        let mut current = parse_url(url)?;
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or("HTTP timeout is too large")?;
        let mut method = method.to_owned();
        let mut headers = headers.to_vec();
        let mut body = body;
        for hop in 0..=self.redirects {
            let response = self.single_request(&method, &current, &headers, body, deadline)?;
            if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response.headers.get("location") else {
                return Ok(response);
            };
            if hop == self.redirects {
                return Err("Too many HTTP redirects".into());
            }
            let next = resolve_redirect(&current, location)?;
            if current.scheme == "https" && next.scheme != "https" {
                return Err("HTTPS to HTTP redirect rejected".into());
            }
            if current.origin() != next.origin() {
                let changes_to_get = (response.status == 303 && method != "HEAD")
                    || (matches!(response.status, 301 | 302) && method == "POST");
                if !body.is_empty() && !changes_to_get {
                    return Err("Cross-origin redirect of an HTTP request body rejected".into());
                }
                headers.retain(|(name, _)| {
                    matches!(
                        name.to_ascii_lowercase().as_str(),
                        "accept"
                            | "accept-language"
                            | "accept-encoding"
                            | "range"
                            | "if-none-match"
                            | "if-modified-since"
                            | "cache-control"
                            | "user-agent"
                    )
                });
            }
            if (response.status == 303 && method != "HEAD")
                || (matches!(response.status, 301 | 302) && method == "POST")
            {
                method = "GET".into();
                body = &[];
                headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
            }
            current = next;
        }
        Err("Unable to follow HTTP redirect".into())
    }

    fn single_request(
        &self,
        method: &str,
        url: &Url,
        headers: &[(String, String)],
        body: &[u8],
        deadline: Instant,
    ) -> Result<Response> {
        let proxy = if self.environment_proxy {
            environment_proxy(url)?
        } else {
            None
        };
        let endpoint = proxy.as_ref().unwrap_or(url);
        let addresses = (endpoint.host.as_str(), endpoint.port)
            .to_socket_addrs()
            .map_err(|_| "Unable to resolve DNS")?;
        let mut connection = None;
        for address in addresses.take(16) {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|duration| !duration.is_zero())
                .ok_or("HTTP deadline exceeded")?;
            if let Ok(stream) = TcpStream::connect_timeout(&address, remaining) {
                connection = Some(stream);
                break;
            }
        }
        let stream = connection.ok_or("Unable to establish HTTP connection")?;
        stream
            .set_read_timeout(Some(self.timeout))
            .and_then(|()| stream.set_write_timeout(Some(self.timeout)))
            .map_err(|_| "Unable to configure network timeouts")?;
        let _ = stream.set_nodelay(true);
        let mut stream = DeadlineStream::new(stream, deadline);
        if proxy.is_some() && url.scheme == "https" {
            connect_tunnel(&mut stream, url)?;
        }
        let mut transport = if url.scheme == "https" {
            Transport::Tls(Box::new(crate::tls::TlsStream::connect_stream(
                stream, &url.host,
            )?))
        } else {
            Transport::Tcp(stream)
        };
        let host = if url.host.contains(':') {
            format!("[{}]", url.host)
        } else {
            url.host.clone()
        };
        let host_header = if (url.scheme == "http" && url.port == 80)
            || (url.scheme == "https" && url.port == 443)
        {
            host
        } else {
            format!("{host}:{}", url.port)
        };
        let target = if proxy.is_some() && url.scheme == "http" {
            url.as_string()
        } else {
            url.path.clone()
        };
        let mut head = format!(
            "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nUser-Agent: Mynou/{}\r\nContent-Length: {}\r\n",
            target,
            env!("CARGO_PKG_VERSION"),
            body.len()
        );
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("accept-encoding"))
        {
            head.push_str("Accept-Encoding: identity\r\n");
        }
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        if head.len() > MAX_HEADER {
            return Err("HTTP request headers exceed the limit".into());
        }
        head.push_str("\r\n");
        transport
            .write_all(head.as_bytes())
            .and_then(|()| transport.write_all(body))
            .and_then(|()| transport.flush())
            .map_err(|_| "Unable to write HTTP request")?;
        read_response(BufReader::new(transport), method == "HEAD", self.max_body)
    }
}

fn environment_proxy(url: &Url) -> Result<Option<Url>> {
    if url.host.eq_ignore_ascii_case("localhost")
        || url
            .host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    {
        return Ok(None);
    }
    let exclusions = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if exclusions
        .split(',')
        .any(|rule| proxy_exclusion(url, rule.trim()))
    {
        return Ok(None);
    }
    let (upper, lower) = if url.scheme == "https" {
        ("HTTPS_PROXY", "https_proxy")
    } else {
        ("HTTP_PROXY", "http_proxy")
    };
    let value = std::env::var(lower)
        .or_else(|_| std::env::var(upper))
        .or_else(|_| std::env::var("ALL_PROXY"))
        .or_else(|_| std::env::var("all_proxy"));
    let Ok(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let proxy = parse_url(&value).map_err(|_| "Invalid HTTP proxy URL")?;
    if proxy.scheme != "http" || proxy.path != "/" {
        return Err(
            "Proxy: an HTTP URL without a path is required; TLS to the proxy is unsupported".into(),
        );
    }
    Ok(Some(proxy))
}

fn proxy_exclusion(url: &Url, rule: &str) -> bool {
    if rule == "*" {
        return true;
    }
    if rule.is_empty() {
        return false;
    }
    if let Some((address, bits)) = rule.split_once('/') {
        if let (Ok(address), Ok(target), Ok(bits)) = (
            address.parse::<IpAddr>(),
            url.host.parse::<IpAddr>(),
            bits.parse::<u32>(),
        ) {
            return match (address, target) {
                (IpAddr::V4(a), IpAddr::V4(b)) if bits <= 32 => {
                    let mask = if bits == 0 {
                        0
                    } else {
                        u32::MAX << (32 - bits)
                    };
                    u32::from(a) & mask == u32::from(b) & mask
                }
                (IpAddr::V6(a), IpAddr::V6(b)) if bits <= 128 => {
                    let mask = if bits == 0 {
                        0
                    } else {
                        u128::MAX << (128 - bits)
                    };
                    u128::from(a) & mask == u128::from(b) & mask
                }
                _ => false,
            };
        }
        return false;
    }
    let (host, port) = if let Some(ip) = rule.strip_prefix('[') {
        let Some((host, tail)) = ip.split_once(']') else {
            return false;
        };
        (
            host,
            if tail.is_empty() {
                None
            } else {
                tail.strip_prefix(':')
            },
        )
    } else if rule.matches(':').count() == 1 {
        match rule.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rule, None),
        }
    } else {
        (rule, None)
    };
    if port.is_some_and(|port| port.parse::<u16>().ok() != Some(url.port)) {
        return false;
    }
    let host = host
        .strip_prefix("*.")
        .unwrap_or(host)
        .trim_start_matches('.')
        .to_ascii_lowercase();
    url.host == host || url.host.ends_with(&format!(".{host}"))
}

fn connect_tunnel<R: Read + Write>(stream: &mut R, url: &Url) -> Result<()> {
    let host = if url.host.contains(':') {
        format!("[{}]", url.host)
    } else {
        url.host.clone()
    };
    let target = format!("{host}:{}", url.port);
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|_| "Unable to write HTTP tunnel request")?;
    let mut reader = BufReader::new(stream);
    let mut budget = MAX_HEADER;
    let line = read_line(&mut reader, &mut budget)?;
    let mut fields = line.splitn(3, ' ');
    if !matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1")) || fields.next() != Some("200") {
        return Err("Proxy rejected the HTTPS tunnel".into());
    }
    let headers = read_headers(&mut reader, &mut budget)?;
    if headers.contains_key("transfer-encoding")
        || headers
            .get("content-length")
            .is_some_and(|length| length != "0")
        || !reader.buffer().is_empty()
    {
        return Err("Ambiguous HTTP tunnel response".into());
    }
    Ok(())
}

enum Transport {
    Tcp(DeadlineStream),
    Tls(Box<crate::tls::TlsStream>),
}

/// Bounds complete I/O operations, including peers that send a byte at a time.
pub(crate) struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
    pub(crate) fn new(stream: TcpStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }

    pub(crate) fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "Network deadline exceeded")
            })
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(data)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(data)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

impl Read for Transport {
    fn read(&mut self, data: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(data),
            Self::Tls(stream) => stream.read(data),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(data),
            Self::Tls(stream) => stream.write(data),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn validate_header(name: &str, value: &str) -> Result<()> {
    if name.is_empty()
        || !name.bytes().all(token_byte)
        || value.bytes().any(|b| (b < 32 && b != b'\t') || b == 127)
    {
        return Err("Invalid HTTP header".into());
    }
    Ok(())
}

fn read_line<R: BufRead>(reader: &mut R, budget: &mut usize) -> Result<String> {
    let limit = MAX_LINE.min(*budget);
    let mut line = Vec::new();
    let read = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|_| "Unable to read HTTP response or deadline exceeded")?;
    if read == 0 || read > limit || !line.ends_with(b"\r\n") {
        return Err("HTTP line is missing, too long or invalid".into());
    }
    *budget -= read;
    line.truncate(line.len() - 2);
    String::from_utf8(line).map_err(|_| "HTTP header is not UTF-8".into())
}

fn read_headers<R: BufRead>(
    reader: &mut R,
    budget: &mut usize,
) -> Result<BTreeMap<String, String>> {
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for _ in 0..256 {
        let line = read_line(reader, budget)?;
        if line.is_empty() {
            return Ok(headers);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or("HTTP header is missing a separator")?;
        validate_header(name, value)?;
        let name = name.to_ascii_lowercase();
        let value = value.trim_matches([' ', '\t']);
        if let Some(existing) = headers.get_mut(&name) {
            if matches!(
                name.as_str(),
                "content-length" | "transfer-encoding" | "location"
            ) {
                return Err("Ambiguous HTTP framing header".into());
            }
            existing.push_str(", ");
            existing.push_str(value);
        } else {
            headers.insert(name, value.to_owned());
        }
    }
    Err("Too many HTTP headers".into())
}

fn read_response<R: BufRead>(mut reader: R, head_only: bool, max_body: usize) -> Result<Response> {
    let mut budget = MAX_HEADER;
    for interim in 0..=4 {
        let line = read_line(&mut reader, &mut budget)?;
        let mut fields = line.splitn(3, ' ');
        let version = fields.next().unwrap_or_default();
        let status_text = fields.next().unwrap_or_default();
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
            || status_text.len() != 3
            || !status_text.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("Invalid HTTP status".into());
        }
        let status: u16 = status_text.parse().map_err(|_| "Invalid HTTP status")?;
        if !(100..=599).contains(&status) || status == 101 {
            return Err("Unsupported HTTP status".into());
        }
        let headers = read_headers(&mut reader, &mut budget)?;
        if status < 200 {
            if interim == 4 {
                return Err("Too many informational HTTP responses".into());
            }
            continue;
        }
        if headers.contains_key("transfer-encoding") && headers.contains_key("content-length") {
            return Err("Ambiguous HTTP framing".into());
        }
        let body = if head_only || matches!(status, 204 | 304) {
            Vec::new()
        } else if let Some(encoding) = headers.get("transfer-encoding") {
            if !encoding.eq_ignore_ascii_case("chunked") {
                return Err("Unsupported HTTP transfer encoding".into());
            }
            read_chunks(&mut reader, max_body)?
        } else if let Some(length) = headers.get("content-length") {
            if length.is_empty() || !length.bytes().all(|b| b.is_ascii_digit()) {
                return Err("Invalid HTTP content length".into());
            }
            let length = length
                .parse::<usize>()
                .map_err(|_| "HTTP content length exceeds the limit")?;
            if length > max_body {
                return Err("HTTP response body exceeds the limit".into());
            }
            let mut body = vec![0; length];
            reader
                .read_exact(&mut body)
                .map_err(|_| "Incomplete HTTP body")?;
            body
        } else {
            let mut body = Vec::new();
            reader
                .take(max_body.saturating_add(1) as u64)
                .read_to_end(&mut body)
                .map_err(|_| "Incomplete HTTP body or deadline exceeded")?;
            if body.len() > max_body {
                return Err("HTTP response body exceeds the limit".into());
            }
            body
        };
        if let Some(encoding) = headers.get("content-encoding")
            && !encoding.eq_ignore_ascii_case("identity")
        {
            return Err("HTTP compression is unsupported; request identity encoding".into());
        }
        return Ok(Response {
            status,
            headers,
            body,
        });
    }
    Err("Missing HTTP response".into())
}

fn read_chunks<R: BufRead>(reader: &mut R, max_body: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut framing_budget = MAX_HEADER;
    for _ in 0..1_000_000 {
        let line = read_line(reader, &mut framing_budget)?;
        let size = line.split(';').next().unwrap_or_default();
        if size.is_empty() || size.len() > 16 || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("Invalid HTTP chunk size".into());
        }
        let size = usize::from_str_radix(size, 16).map_err(|_| "HTTP chunk exceeds the limit")?;
        if size == 0 {
            let trailers = read_headers(reader, &mut framing_budget)?;
            if trailers.keys().any(|name| {
                matches!(
                    name.as_str(),
                    "content-length" | "transfer-encoding" | "host"
                )
            }) {
                return Err("HTTP framing trailer is forbidden".into());
            }
            return Ok(body);
        }
        let new_len = body
            .len()
            .checked_add(size)
            .ok_or("HTTP body exceeds the limit")?;
        if new_len > max_body {
            return Err("HTTP response body exceeds the limit".into());
        }
        let start = body.len();
        body.resize(new_len, 0);
        reader
            .read_exact(&mut body[start..])
            .map_err(|_| "Incomplete HTTP chunk")?;
        let mut crlf = [0; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|_| "Incomplete HTTP chunk")?;
        if crlf != *b"\r\n" {
            return Err("Invalid HTTP chunk terminator".into());
        }
    }
    Err("Too many HTTP chunks".into())
}

fn resolve_redirect(base: &Url, location: &str) -> Result<Url> {
    if location.contains("://") {
        return parse_url(location);
    }
    if location.starts_with("//") {
        return parse_url(&format!("{}:{location}", base.scheme));
    }
    let path = if location.starts_with('/') {
        location.to_owned()
    } else if location.starts_with('?') {
        format!("{}{location}", base.path.split('?').next().unwrap_or("/"))
    } else if location.starts_with('#') || location.is_empty() {
        base.path.clone()
    } else {
        let directory = base.path.split('?').next().unwrap_or("/");
        let end = directory.rfind('/').unwrap_or(0);
        format!("{}/{location}", &directory[..end])
    };
    let mut next = parse_url(&format!("{}{path}", base.origin()))?;
    let (path, query) = match next.path.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (next.path.as_str(), None),
    };
    let trailing = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    let mut components = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(part),
        }
    }
    let mut normalized = format!("/{}", components.join("/"));
    if trailing && !normalized.ends_with('/') {
        normalized.push('/');
    }
    if let Some(query) = query {
        normalized.push('?');
        normalized.push_str(query);
    }
    next.path = normalized;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn urls_are_explicit_and_bounded() {
        let url = parse_url("HTTPS://Example.org/a b?q=%2f#ignored").unwrap();
        assert_eq!(url.host, "example.org");
        assert_eq!(url.path, "/a%20b?q=%2f");
        assert_eq!(parse_url("http://[::1]:8123?q=a").unwrap().port, 8123);
        for bad in [
            "file:///etc/passwd",
            "http://user:secret@host/",
            "http://x:0/",
            "http://x/%xx",
            "http://x/a\r\nInjected: yes",
            "http://x\\y/",
            "http://[::1]tail/",
        ] {
            assert!(parse_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn chunked_and_interim_responses() {
        let wire = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;extension=yes\r\nde\r\n0\r\nX-Checksum: yes\r\n\r\n";
        assert_eq!(
            read_response(Cursor::new(wire), false, 5).unwrap().body,
            b"abcde"
        );
        assert!(read_response(Cursor::new(wire), false, 4).is_err());
    }

    #[test]
    fn framing_ambiguities_are_rejected() {
        for wire in [
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\na",
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\na",
            "HTTP/1.1 200 OK\nContent-Length: 1\n\na",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\naXX0\r\n\r\n",
            "HTTP/1.1 101 Upgrade\r\n\r\n",
        ] {
            assert!(read_response(Cursor::new(wire), false, 16).is_err());
        }
    }

    #[test]
    fn local_binary_http_and_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                if i == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    assert!(request.starts_with(b"GET /next "));
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n\0\xffab")
                        .unwrap();
                }
            }
        });
        let response = HttpClient::new()
            .get(&format!("http://{address}/start"))
            .unwrap();
        assert_eq!(response.body, [0, 255, b'a', b'b']);
        server.join().unwrap();
    }

    #[test]
    fn redirect_paths_are_resolved() {
        let base = parse_url("https://example.org/dir/file?q=old").unwrap();
        assert_eq!(
            resolve_redirect(&base, "../target?q=new").unwrap().path,
            "/target?q=new"
        );
        assert_eq!(
            resolve_redirect(&base, "?q=new").unwrap().path,
            "/dir/file?q=new"
        );
        assert_eq!(resolve_redirect(&base, "/a/../b/.").unwrap().path, "/b/");
    }

    #[test]
    fn proxy_exclusions_are_exact_or_domain_suffixes() {
        let url = parse_url("https://api.example.org:8443/").unwrap();
        assert!(proxy_exclusion(&url, ".example.org"));
        assert!(proxy_exclusion(&url, "example.org:8443"));
        assert!(!proxy_exclusion(&url, "example.org:443"));
        assert!(!proxy_exclusion(&url, "ample.org"));
        assert!(proxy_exclusion(
            &parse_url("http://10.2.3.4/").unwrap(),
            "10.0.0.0/8"
        ));
        assert!(!proxy_exclusion(
            &parse_url("http://11.2.3.4/").unwrap(),
            "10.0.0.0/8"
        ));
        assert!(proxy_exclusion(
            &parse_url("http://[2001:db8::a]/").unwrap(),
            "2001:db8::/32"
        ));
    }

    #[test]
    fn cross_origin_redirect_does_not_forward_credentials() {
        let source = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        let source_address = source.local_addr().unwrap();
        let destination_address = destination.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = source.accept().unwrap();
            let mut request = BufReader::new(stream.try_clone().unwrap());
            let mut budget = MAX_HEADER;
            read_line(&mut request, &mut budget).unwrap();
            let headers = read_headers(&mut request, &mut budget).unwrap();
            assert_eq!(headers.get("authorization").unwrap(), "Bearer secret");
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://{destination_address}/next\r\nContent-Length: 0\r\n\r\n").unwrap();
            let (mut stream, _) = destination.accept().unwrap();
            let mut request = BufReader::new(stream.try_clone().unwrap());
            let mut budget = MAX_HEADER;
            read_line(&mut request, &mut budget).unwrap();
            let headers = read_headers(&mut request, &mut budget).unwrap();
            assert!(!headers.contains_key("authorization"));
            assert!(!headers.contains_key("x-plex-token"));
            assert!(!headers.contains_key("cookie"));
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .unwrap();
        });
        let headers = vec![
            ("Authorization".into(), "Bearer secret".into()),
            ("Cookie".into(), "session=secret".into()),
            ("X-Plex-Token".into(), "secret".into()),
        ];
        assert_eq!(
            HttpClient::new()
                .request("GET", &format!("http://{source_address}/"), &headers, &[])
                .unwrap()
                .status,
            204
        );
        server.join().unwrap();
    }

    #[test]
    fn header_injection_is_rejected_before_connecting() {
        let client = HttpClient::new();
        assert!(
            client
                .request("GET\r\nINJECT", "http://127.0.0.1:1/", &[], &[])
                .unwrap_err()
                .contains("HTTP method")
        );
        let error = client
            .request(
                "GET",
                "http://127.0.0.1:1/",
                &[("Authorization".into(), "secret\r\nInjected: yes".into())],
                &[],
            )
            .unwrap_err();
        assert!(error.contains("HTTP header"));
        assert!(!error.contains("secret"));
    }

    #[test]
    fn absolute_deadline_stops_a_trickling_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for byte in b"slow response that never finishes" {
                thread::sleep(Duration::from_millis(20));
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
            }
        });
        let stream = TcpStream::connect(address).unwrap();
        let mut stream = DeadlineStream::new(stream, Instant::now() + Duration::from_millis(70));
        let started = Instant::now();
        let error = stream.read_exact(&mut [0; 32]).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(stream);
        server.join().unwrap();
    }
}
