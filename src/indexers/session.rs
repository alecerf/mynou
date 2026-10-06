//! Explicit single-cookie form adapter. No interactive login or cookie jar inference.
use super::{Form, remaining, secret};
use crate::{
    Result,
    net::{self, HttpClient},
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
pub(super) struct Session {
    name: String,
    value: String,
    origin: String,
    path: String,
    pub expires: Instant,
}
impl Session {
    pub(super) fn header(&self, url: &str) -> Result<String> {
        let u = net::parse_url(url)?;
        let path = u.path.split('?').next().unwrap_or("/");
        if u.origin() != self.origin
            || !(path == self.path
                || (path.starts_with(&self.path)
                    && (self.path.ends_with('/')
                        || path.as_bytes().get(self.path.len()) == Some(&b'/'))))
        {
            return Err("Indexer: session scope mismatch".into());
        }
        Ok(format!("{}={}", self.name, self.value))
    }
}
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char)
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}
pub(super) fn login(form: &Form, source: &str, deadline: Instant) -> Result<Session> {
    let src = net::parse_url(source)?;
    let login = net::parse_url(&form.login_url)?;
    if src.origin() != login.origin() {
        return Err("Indexer: login origin changed".into());
    }
    let body = format!(
        "{}={}&{}={}",
        form.username_field,
        encode(&secret(&form.username_env)?),
        form.password_field,
        encode(&secret(&form.password_env)?)
    );
    let r = HttpClient::new()
        .with_timeout(remaining(deadline)?.min(Duration::from_secs(10)))
        .with_max_body(65536)
        .without_redirects()
        .request(
            "POST",
            &form.login_url,
            &[(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
            body.as_bytes(),
        )
        .map_err(|_| "Indexer: session login failed")?;
    remaining(deadline)?;
    if !(200..300).contains(&r.status) {
        return Err("Indexer: session login rejected".into());
    }
    parse(
        form,
        source,
        r.headers
            .get("set-cookie")
            .ok_or("Indexer: missing session cookie")?,
    )
}
fn cookie_date(s: &str) -> Result<u64> {
    let fields: Vec<_> = s.split_whitespace().collect();
    if fields.len() != 6
        || !["Mon,", "Tue,", "Wed,", "Thu,", "Fri,", "Sat,", "Sun,"].contains(&fields[0])
        || fields[5] != "GMT"
    {
        return Err("Indexer: invalid cookie expiry".into());
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == fields[2])
    .ok_or("Indexer: invalid cookie expiry")?
        + 1;
    let day = fields[1]
        .parse::<u32>()
        .map_err(|_| "Indexer: invalid cookie expiry")?;
    let year = fields[3]
        .parse::<u32>()
        .map_err(|_| "Indexer: invalid cookie expiry")?;
    let date = crate::date::day(&format!("{year:04}-{month:02}-{day:02}"))?;
    let time: Vec<_> = fields[4].split(':').collect();
    if time.len() != 3 {
        return Err("Indexer: invalid cookie expiry".into());
    }
    let h = time[0]
        .parse::<u64>()
        .map_err(|_| "Indexer: invalid cookie expiry")?;
    let m = time[1]
        .parse::<u64>()
        .map_err(|_| "Indexer: invalid cookie expiry")?;
    let sec = time[2]
        .parse::<u64>()
        .map_err(|_| "Indexer: invalid cookie expiry")?;
    if date < 0 || h > 23 || m > 59 || sec > 59 {
        return Err("Indexer: invalid cookie expiry".into());
    }
    Ok(date as u64 * 86400 + h * 3600 + m * 60 + sec)
}
fn parse(form: &Form, source: &str, header: &str) -> Result<Session> {
    let u = net::parse_url(source)?;
    if header.len() > 8192 || header.chars().any(char::is_control) {
        return Err("Indexer: invalid session cookie".into());
    }
    let mut parts = header.split(';');
    let (name, value) = parts
        .next()
        .and_then(|s| s.trim().split_once('='))
        .ok_or("Indexer: invalid session cookie")?;
    if name != form.cookie_name
        || value.is_empty()
        || value.len() > 4096
        || !value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b',' | b';' | b'\\'))
    {
        return Err("Indexer: invalid session cookie".into());
    }
    let mut path = "/".to_owned();
    let mut secure = false;
    let mut same_none = false;
    let mut age = form.max_age_secs;
    let mut expiry = None;
    let mut seen = BTreeSet::new();
    for part in parts {
        let (key, v) = part.trim().split_once('=').unwrap_or((part.trim(), ""));
        let key = key.to_ascii_lowercase();
        if !seen.insert(key.clone()) || seen.len() > 16 {
            return Err("Indexer: ambiguous cookie attributes".into());
        }
        match key.as_str() {
            "path" if v.starts_with('/') && !v.contains([',', '?', '#']) && v.len() <= 8192 => {
                path = v.into()
            }
            "domain" if v.trim_start_matches('.').eq_ignore_ascii_case(&u.host) => {}
            "secure" if v.is_empty() => secure = true,
            "httponly" if v.is_empty() => {}
            "samesite" if ["Lax", "Strict", "None"].contains(&v) => same_none = v == "None",
            "max-age" => {
                let n = v
                    .parse::<u64>()
                    .map_err(|_| "Indexer: invalid cookie age")?;
                if n == 0 {
                    return Err("Indexer: expired session cookie".into());
                }
                age = age.min(n)
            }
            "expires" => expiry = Some(cookie_date(v)?),
            _ => return Err("Indexer: unsupported cookie attribute".into()),
        }
    }
    if (secure && u.scheme != "https") || (same_none && !secure) {
        return Err("Indexer: invalid cookie transport".into());
    }
    if !seen.contains("max-age")
        && let Some(expiry) = expiry
    {
        age = age.min(
            expiry
                .checked_sub(crate::store::now())
                .filter(|n| *n > 0)
                .ok_or("Indexer: expired session cookie")?,
        );
    }
    if age == 0 || age > 3600 {
        return Err("Indexer: invalid session duration".into());
    }
    let s = Session {
        name: name.into(),
        value: value.into(),
        origin: u.origin(),
        path,
        expires: Instant::now() + Duration::from_secs(age),
    };
    s.header(source)?;
    Ok(s)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn form() -> Form {
        Form {
            login_url: "http://127.0.0.1:1/login".into(),
            username_env: "PATH".into(),
            password_env: "PWD".into(),
            username_field: "user".into(),
            password_field: "password".into(),
            cookie_name: "sid".into(),
            max_age_secs: 60,
        }
    }
    #[test]
    fn cookie_scope_transport_expiry_and_ambiguity_are_bounded() {
        let f = form();
        let s = parse(
            &f,
            "http://127.0.0.1:1/api",
            "sid=original; Path=/api; Max-Age=30; HttpOnly",
        )
        .unwrap();
        assert_eq!(
            s.header("http://127.0.0.1:1/api?q=x").unwrap(),
            "sid=original"
        );
        assert!(s.header("http://127.0.0.1:1/apix").is_err());
        assert!(s.header("http://127.0.0.1:2/api").is_err());
        for h in [
            "other=value",
            "sid=value, other=bad",
            "sid=value; Path=/other",
            "sid=value; Domain=example.test",
            "sid=value; Max-Age=0",
            "sid=value; Secure",
            "sid=value; SameSite=None",
            "sid=value; Path=/; Path=/",
            "sid=value; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            "sid=value; Unknown=x",
        ] {
            assert!(
                parse(&f, "http://127.0.0.1:1/api", h).is_err(),
                "Cookie fixture should fail"
            );
        }
    }
    #[test]
    fn original_form_encoding_and_canonical_cookie_dates() {
        assert_eq!(encode("a &+é"), "a%20%26%2B%C3%A9");
        assert_eq!(cookie_date("Thu, 01 Jan 1970 00:00:00 GMT").unwrap(), 0);
        assert!(cookie_date("Wed, 31 Feb 2026 00:00:00 GMT").is_err());
        assert!(cookie_date("Wed, 21 Oct 2026 99:00:00 GMT").is_err());
    }
}
