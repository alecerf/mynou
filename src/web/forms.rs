//! Strict bounded form parsing shared by navigation and browser actions.
use crate::{Result, store::Request};
use std::collections::BTreeMap;

pub const MAX_FORM: usize = 65_536;
pub const MAX_BULK: usize = 32;

pub struct Form(BTreeMap<String, Vec<String>>);

impl Form {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_FORM {
            return Err("This form is too large".into());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| "Invalid form encoding")?;
        let mut values = BTreeMap::<String, Vec<String>>::new();
        if !text.is_empty() {
            for (index, pair) in text.split('&').enumerate() {
                if index >= 64 {
                    return Err("Too many form fields".into());
                }
                let (key, value) = pair.split_once('=').ok_or("Invalid form field")?;
                let key = decode(key)?;
                let value = decode(value)?;
                if key.is_empty()
                    || key.len() > 40
                    || !key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                    || value.len() > 8_192
                    || value.chars().any(char::is_control)
                {
                    return Err("Invalid form field".into());
                }
                let entries = values.entry(key).or_default();
                if entries.len() >= MAX_BULK {
                    return Err("Select at most 32 entries".into());
                }
                entries.push(value);
            }
        }
        Ok(Self(values))
    }

    pub fn only(&self, allowed: &[&str]) -> Result<()> {
        if self.0.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err("Unexpected form field".into());
        }
        for (key, values) in &self.0 {
            if key != "id" && values.len() != 1 {
                return Err("Duplicate form field".into());
            }
        }
        Ok(())
    }

    pub fn value(&self, key: &str) -> Result<&str> {
        match self.0.get(key).map(Vec::as_slice) {
            None => Ok(""),
            Some([value]) => Ok(value),
            Some(_) => Err("Duplicate form field".into()),
        }
    }

    pub fn ids(&self, native: bool) -> Result<Vec<String>> {
        let values = self.0.get("id").ok_or("Select at least one entry")?;
        if values.is_empty() || values.len() > MAX_BULK {
            return Err("Select between 1 and 32 entries".into());
        }
        let mut result = Vec::with_capacity(values.len());
        for value in values {
            if !valid_id(value, native) {
                return Err("Invalid entry identifier".into());
            }
            let id = value.to_ascii_lowercase();
            if result.contains(&id) {
                return Err("Select each entry only once".into());
            }
            result.push(id);
        }
        Ok(result)
    }

    pub fn request(&self) -> Result<Request> {
        self.only(&[
            "csrf",
            "kind",
            "title",
            "year",
            "season",
            "episode",
            "tmdb_id",
            "source_kind",
            "source_value",
        ])?;
        let optional =
            |name: &str, maximum: u64| -> Result<u64> { decimal(self.value(name)?, maximum, name) };
        let source = self.value("source_value")?.trim();
        let (source_path, source_url) = match self.value("source_kind")? {
            "" | "auto" if source.is_empty() => (None, None),
            "file" if !source.is_empty() => (Some(source.to_owned()), None),
            "url" if !source.is_empty() => (None, Some(source.to_owned())),
            _ => {
                return Err(
                    "Choose automatic search or provide a source for the chosen type".into(),
                );
            }
        };
        let tmdb = optional("tmdb_id", 9_007_199_254_740_991)?;
        let request = Request {
            kind: self.value("kind")?.to_owned(),
            title: self.value("title")?.trim().to_owned(),
            year: optional("year", 9999)? as u32,
            season: optional("season", 9999)? as u32,
            episode: optional("episode", 99999)? as u32,
            tmdb_id: (tmdb != 0).then_some(tmdb),
            source_path,
            source_url,
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn valid_id(id: &str, native: bool) -> bool {
    (if native {
        [40, 64].contains(&id.len())
    } else {
        id.len() == 32
    }) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn decimal(text: &str, maximum: u64, name: &str) -> Result<u64> {
    if text.is_empty() {
        return Ok(0);
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{name} must be a whole number"));
    }
    text.parse::<u64>()
        .ok()
        .filter(|number| *number <= maximum)
        .ok_or_else(|| format!("{name} is outside the allowed range"))
}

pub fn encode(text: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut result = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(byte as char);
        } else {
            result.push('%');
            result.push(HEX[(byte >> 4) as usize] as char);
            result.push(HEX[(byte & 15) as usize] as char);
        }
    }
    result
}

fn decode(text: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut input = text.bytes();
    while let Some(byte) = input.next() {
        bytes.push(match byte {
            b'+' => b' ',
            b'%' => {
                let high = input.next().and_then(hex).ok_or("Invalid form escape")?;
                let low = input.next().and_then(hex).ok_or("Invalid form escape")?;
                (high << 4) | low
            }
            byte => byte,
        });
    }
    String::from_utf8(bytes).map_err(|_| "Invalid form encoding".into())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_forms_reject_ambiguous_and_invalid_input() {
        for input in [
            "title=%",
            "title=%FF",
            "title=%00",
            "Title=x",
            "=x",
            "title",
        ] {
            assert!(Form::parse(input.as_bytes()).is_err(), "Accepted {input}");
        }
        assert!(
            Form::parse(b"title=a&title=b")
                .unwrap()
                .only(&["title"])
                .is_err()
        );
        assert!(Form::parse(b"redirect=x").unwrap().only(&["csrf"]).is_err());
        assert_eq!(
            Form::parse(b"title=caf%C3%A9+movie")
                .unwrap()
                .value("title")
                .unwrap(),
            "café movie"
        );
        assert!(Form::parse(&vec![b'x'; MAX_FORM + 1]).is_err());
    }

    #[test]
    fn bulk_ids_are_prevalidated_and_unique_after_normalization() {
        let id = "a".repeat(32);
        assert!(
            Form::parse(format!("id={id}&id={}", id.to_uppercase()).as_bytes())
                .unwrap()
                .ids(false)
                .is_err()
        );
        assert!(
            Form::parse(format!("id={id}&id=invalid").as_bytes())
                .unwrap()
                .ids(false)
                .is_err()
        );
        assert!(
            Form::parse(
                format!("id={id}&")
                    .repeat(33)
                    .trim_end_matches('&')
                    .as_bytes()
            )
            .is_err()
        );
        assert!(Form::parse(b"").unwrap().ids(false).is_err());
    }
}
