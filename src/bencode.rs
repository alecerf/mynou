//! Bencode borné ; l'infohash utilise toujours les octets d'origine.
use crate::Result;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}
impl Value {
    pub fn as_int(&self) -> Option<i64> {
        if let Self::Int(v) = self {
            Some(*v)
        } else {
            None
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        if let Self::Bytes(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_list(&self) -> Option<&[Value]> {
        if let Self::List(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        if let Self::Dict(v) = self {
            Some(v)
        } else {
            None
        }
    }
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        self.as_dict()?.get(key)
    }
}

struct Parser<'a> {
    data: &'a [u8],
    at: usize,
    remaining: usize,
}
impl Parser<'_> {
    fn error(&self, text: &str) -> String {
        format!("Bencode à l’octet {} : {text}", self.at)
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let start = self.at;
        while matches!(self.data.get(self.at), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        if start == self.at || self.data.get(self.at) != Some(&b':') {
            return Err(self.error("longueur invalide"));
        }
        if self.at - start > 1 && self.data[start] == b'0' {
            return Err(self.error("zéro initial interdit"));
        }
        let length = std::str::from_utf8(&self.data[start..self.at])
            .map_err(|_| self.error("longueur invalide"))?
            .parse::<usize>()
            .map_err(|_| self.error("longueur excessive"))?;
        self.at += 1;
        let end = self
            .at
            .checked_add(length)
            .ok_or_else(|| self.error("longueur excessive"))?;
        let bytes = self
            .data
            .get(self.at..end)
            .ok_or_else(|| self.error("chaîne tronquée"))?
            .to_vec();
        self.at = end;
        Ok(bytes)
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > 64 || self.remaining == 0 {
            return Err(self.error("complexité excessive"));
        }
        self.remaining -= 1;
        match self.data.get(self.at) {
            Some(b'i') => {
                self.at += 1;
                let start = self.at;
                while self.data.get(self.at).is_some_and(|b| *b != b'e') {
                    self.at += 1;
                }
                if self.data.get(self.at) != Some(&b'e') {
                    return Err(self.error("entier tronqué"));
                }
                let text = std::str::from_utf8(&self.data[start..self.at])
                    .map_err(|_| self.error("entier invalide"))?;
                if text.is_empty()
                    || text == "-0"
                    || text.starts_with('+')
                    || text.len() > 1 && text.starts_with('0')
                    || text.starts_with("-0")
                {
                    return Err(self.error("entier non canonique"));
                }
                let n = text
                    .parse::<i64>()
                    .map_err(|_| self.error("entier invalide ou excessif"))?;
                self.at += 1;
                Ok(Value::Int(n))
            }
            Some(b'l') => {
                self.at += 1;
                let mut list = Vec::new();
                while self.data.get(self.at) != Some(&b'e') {
                    list.push(self.value(depth + 1)?);
                }
                self.at += 1;
                Ok(Value::List(list))
            }
            Some(b'd') => {
                self.at += 1;
                let mut map = BTreeMap::new();
                let mut previous: Option<Vec<u8>> = None;
                while self.data.get(self.at) != Some(&b'e') {
                    let key = self.bytes()?;
                    if previous.as_ref().is_some_and(|p| p >= &key) {
                        return Err(self.error("clés du dictionnaire non canoniques"));
                    }
                    previous = Some(key.clone());
                    let value = self.value(depth + 1)?;
                    map.insert(key, value);
                }
                self.at += 1;
                Ok(Value::Dict(map))
            }
            Some(b'0'..=b'9') => self.bytes().map(Value::Bytes),
            _ => Err(self.error("valeur invalide ou tronquée")),
        }
    }
}
pub fn parse_prefix(data: &[u8]) -> Result<(Value, usize)> {
    if data.len() > 16 * 1024 * 1024 {
        return Err("Bencode : document trop volumineux".into());
    }
    let mut p = Parser {
        data,
        at: 0,
        remaining: 1_000_000,
    };
    let v = p.value(0)?;
    Ok((v, p.at))
}
pub fn parse(data: &[u8]) -> Result<Value> {
    let (v, end) = parse_prefix(data)?;
    if end != data.len() {
        return Err("Bencode : données après le document".into());
    }
    Ok(v)
}
pub fn parse_info_raw(data: &[u8]) -> Result<Vec<u8>> {
    let value = parse(data)?;
    if value.get(b"info").and_then(Value::as_dict).is_none() {
        return Err("Torrent : dictionnaire info absent".into());
    }
    let mut p = Parser {
        data,
        at: 1,
        remaining: 1_000_000,
    };
    while p.data.get(p.at) != Some(&b'e') {
        let key = p.bytes()?;
        let start = p.at;
        p.value(1)?;
        if key == b"info" {
            return Ok(data[start..p.at].to_vec());
        }
    }
    Err("Torrent : dictionnaire info absent".into())
}
pub fn encode(value: &Value) -> Vec<u8> {
    fn write(v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Int(n) => {
                out.push(b'i');
                out.extend_from_slice(n.to_string().as_bytes());
                out.push(b'e');
            }
            Value::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            Value::List(v) => {
                out.push(b'l');
                for v in v {
                    write(v, out);
                }
                out.push(b'e');
            }
            Value::Dict(v) => {
                out.push(b'd');
                for (k, v) in v {
                    out.extend_from_slice(k.len().to_string().as_bytes());
                    out.push(b':');
                    out.extend_from_slice(k);
                    write(v, out);
                }
                out.push(b'e');
            }
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_info_is_preserved() {
        let data = b"d4:infod4:name4:test12:piece lengthi16384e6:pieces20:12345678901234567890ee";
        let v = parse(data).unwrap();
        assert_eq!(encode(&v), data);
        assert_eq!(
            parse_info_raw(data).unwrap(),
            b"d4:name4:test12:piece lengthi16384e6:pieces20:12345678901234567890e"
        );
    }
    #[test]
    fn malformed_is_rejected() {
        for data in [
            b"i-0e".as_slice(),
            b"i01e",
            b"i+1e",
            b"01:a",
            b"999:a",
            b"d1:ai1e1:ai2ee",
            b"d1:bi1e1:ai2ee",
            b"l",
            b"i1eextra",
        ] {
            assert!(parse(data).is_err(), "{:?}", data);
        }
    }
    #[test]
    fn prefix_separates_metadata_bytes() {
        assert_eq!(parse_prefix(b"d1:ai1eePAYLOAD").unwrap().1, 8);
    }
}
