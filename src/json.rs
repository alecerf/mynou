//! JSON borné et sans dépendance. Les entiers dépassant 2^53-1 sont refusés.
use std::collections::BTreeMap;

use crate::Result;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        if let Self::Bool(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        if let Self::Number(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        self.as_f64()
            .filter(|v| {
                v.is_finite() && *v >= 0.0 && *v <= 9_007_199_254_740_991.0 && v.fract() == 0.0
            })
            .map(|v| v as u64)
    }
    pub fn as_i64(&self) -> Option<i64> {
        self.as_f64()
            .filter(|v| v.is_finite() && v.abs() <= 9_007_199_254_740_991.0 && v.fract() == 0.0)
            .map(|v| v as i64)
    }
    pub fn as_array(&self) -> Option<&[Value]> {
        if let Self::Array(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_object(&self) -> Option<&BTreeMap<String, Value>> {
        if let Self::Object(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.get(key)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Self::Object(map) => map.get_mut(key),
            _ => None,
        }
    }
    pub fn object() -> Self {
        Self::Object(BTreeMap::new())
    }
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        if let Self::Object(map) = self {
            map.insert(key.into(), value.into());
        }
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Self::Number(f64::from(v))
    }
}

pub fn parse(input: &str) -> Result<Value> {
    if input.len() > 16 * 1024 * 1024 {
        return Err("JSON : document trop volumineux".into());
    }
    let mut parser = Parser {
        bytes: input.as_bytes(),
        at: 0,
        remaining: 1_000_000,
    };
    let value = parser.value(0)?;
    parser.whitespace();
    if parser.at != parser.bytes.len() {
        return Err(parser.error("données après le document"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    remaining: usize,
}
impl Parser<'_> {
    fn error(&self, text: &str) -> String {
        format!("JSON à l’octet {} : {text}", self.at)
    }
    fn whitespace(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > 64 || self.remaining == 0 {
            return Err(self.error("complexité excessive"));
        }
        self.remaining -= 1;
        self.whitespace();
        match self.bytes.get(self.at) {
            Some(b'"') => self.string().map(Value::String),
            Some(b'{') => {
                self.at += 1;
                let mut map = BTreeMap::new();
                self.whitespace();
                if self.take(b'}') {
                    return Ok(Value::Object(map));
                }
                loop {
                    self.whitespace();
                    let key = self.string()?;
                    self.whitespace();
                    if !self.take(b':') {
                        return Err(self.error("deux-points attendu"));
                    }
                    let value = self.value(depth + 1)?;
                    if map.insert(key, value).is_some() {
                        return Err(self.error("clé dupliquée"));
                    }
                    self.whitespace();
                    if self.take(b'}') {
                        break;
                    }
                    if !self.take(b',') {
                        return Err(self.error("virgule attendue"));
                    }
                }
                Ok(Value::Object(map))
            }
            Some(b'[') => {
                self.at += 1;
                let mut array = Vec::new();
                self.whitespace();
                if self.take(b']') {
                    return Ok(Value::Array(array));
                }
                loop {
                    array.push(self.value(depth + 1)?);
                    self.whitespace();
                    if self.take(b']') {
                        break;
                    }
                    if !self.take(b',') {
                        return Err(self.error("virgule attendue"));
                    }
                }
                Ok(Value::Array(array))
            }
            Some(b't') => {
                self.literal(b"true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.literal(b"null")?;
                Ok(Value::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.error("valeur attendue")),
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn literal(&mut self, word: &[u8]) -> Result<()> {
        if self.bytes.get(self.at..self.at + word.len()) == Some(word) {
            self.at += word.len();
            Ok(())
        } else {
            Err(self.error("mot invalide"))
        }
    }
    fn digits(&mut self) -> Result<()> {
        let start = self.at;
        while matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        if start == self.at {
            Err(self.error("chiffre attendu"))
        } else {
            Ok(())
        }
    }
    fn number(&mut self) -> Result<Value> {
        let start = self.at;
        self.take(b'-');
        if self.take(b'0') {
            if matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
                return Err(self.error("zéro initial interdit"));
            }
        } else {
            self.digits()?;
        }
        let mut integer = true;
        if self.take(b'.') {
            integer = false;
            self.digits()?;
        }
        if self.take(b'e') || self.take(b'E') {
            integer = false;
            if !self.take(b'+') {
                self.take(b'-');
            }
            self.digits()?;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|_| self.error("nombre invalide"))?;
        let number = text
            .parse::<f64>()
            .map_err(|_| self.error("nombre invalide"))?;
        if !number.is_finite() || integer && number.abs() > 9_007_199_254_740_991.0 {
            return Err(self.error("nombre hors de la plage exacte prise en charge"));
        }
        Ok(Value::Number(number))
    }
    fn hex4(&mut self) -> Result<u16> {
        let bytes = self
            .bytes
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.error("échappement Unicode tronqué"))?;
        let mut n = 0_u16;
        for byte in bytes {
            let value = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("échappement Unicode invalide")),
            };
            n = (n << 4) | u16::from(value);
        }
        self.at += 4;
        Ok(n)
    }
    fn string(&mut self) -> Result<String> {
        if !self.take(b'"') {
            return Err(self.error("chaîne attendue"));
        }
        let mut output = Vec::new();
        loop {
            let byte = *self
                .bytes
                .get(self.at)
                .ok_or_else(|| self.error("chaîne tronquée"))?;
            self.at += 1;
            match byte {
                b'"' => return String::from_utf8(output).map_err(|_| self.error("UTF-8 invalide")),
                0..=31 => return Err(self.error("caractère de contrôle interdit")),
                b'\\' => {
                    let escape = *self
                        .bytes
                        .get(self.at)
                        .ok_or_else(|| self.error("échappement tronqué"))?;
                    self.at += 1;
                    match escape {
                        b'"' | b'\\' | b'/' => output.push(escape),
                        b'b' => output.push(8),
                        b'f' => output.push(12),
                        b'n' => output.push(b'\n'),
                        b'r' => output.push(b'\r'),
                        b't' => output.push(b'\t'),
                        b'u' => {
                            let first = self.hex4()?;
                            let scalar = if (0xD800..=0xDBFF).contains(&first) {
                                if !self.take(b'\\') || !self.take(b'u') {
                                    return Err(self.error("surrogate incomplet"));
                                }
                                let second = self.hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&second) {
                                    return Err(self.error("surrogate invalide"));
                                }
                                0x10000 + ((u32::from(first) - 0xD800) << 10) + u32::from(second)
                                    - 0xDC00
                            } else {
                                u32::from(first)
                            };
                            let ch = char::from_u32(scalar)
                                .ok_or_else(|| self.error("Unicode invalide"))?;
                            let mut buffer = [0_u8; 4];
                            output.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                        }
                        _ => return Err(self.error("échappement inconnu")),
                    }
                }
                _ => output.push(byte),
            }
            if output.len() > 4 * 1024 * 1024 {
                return Err(self.error("chaîne trop longue"));
            }
        }
    }
}

pub fn stringify(value: &Value) -> String {
    fn quoted(text: &str, output: &mut String) {
        output.push('"');
        for ch in text.chars() {
            match ch {
                '"' => output.push_str("\\\""),
                '\\' => output.push_str("\\\\"),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                ch if ch < '\u{20}' => {
                    use std::fmt::Write;
                    let _ = write!(output, "\\u{:04x}", ch as u32);
                }
                ch => output.push(ch),
            }
        }
        output.push('"');
    }
    fn write(value: &Value, output: &mut String) {
        match value {
            Value::Null => output.push_str("null"),
            Value::Bool(v) => output.push_str(if *v { "true" } else { "false" }),
            Value::Number(v) => {
                if v.is_finite() {
                    output.push_str(&v.to_string());
                } else {
                    output.push_str("null");
                }
            }
            Value::String(v) => quoted(v, output),
            Value::Array(v) => {
                output.push('[');
                for (i, child) in v.iter().enumerate() {
                    if i > 0 {
                        output.push(',');
                    }
                    write(child, output);
                }
                output.push(']');
            }
            Value::Object(v) => {
                output.push('{');
                for (i, (key, child)) in v.iter().enumerate() {
                    if i > 0 {
                        output.push(',');
                    }
                    quoted(key, output);
                    output.push(':');
                    write(child, output);
                }
                output.push('}');
            }
        }
    }
    let mut output = String::new();
    write(value, &mut output);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_surrogates_and_roundtrip() {
        let v = parse(r#"{"film":"Démo \ud83d\udc08","empty":[],"n":-1.25e2}"#).unwrap();
        assert_eq!(v.get("film").unwrap().as_str(), Some("Démo 🐈"));
        assert_eq!(parse(&stringify(&v)).unwrap(), v);
    }
    #[test]
    fn malformed_is_rejected() {
        for text in [
            "01",
            "1.",
            "[1,]",
            "{\"a\":1,\"a\":2}",
            "truefalse",
            "\"\\ud800\"",
            "1e9999",
            "9007199254740993",
            "\"bad\nline\"",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }
    #[test]
    fn deep_input_is_bounded() {
        assert!(parse(&format!("{}0{}", "[".repeat(65), "]".repeat(65))).is_err());
    }
}
