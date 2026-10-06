//! Original bounded UTF-8 XML shared by RSS/Torznab and NZB. No DTD expansion.
use crate::Result;
use std::collections::BTreeMap;
const MAX_ITEMS: usize = 100_000;
const MAX_XML: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub(crate) struct Element {
    pub(crate) name: String,
    pub(crate) attrs: BTreeMap<String, String>,
    pub(crate) text: String,
    pub(crate) children: Vec<Element>,
}

impl Element {
    pub(crate) fn local(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }
    fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|child| child.local() == name)
    }
}

fn xml_unescape(text: &str) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let end = rest.find(';').ok_or("XML: incomplete entity")?;
        if end > 16 {
            return Err("XML: entity is too long".into());
        }
        let entity = &rest[..end];
        let c = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            text => {
                let number = if let Some(n) = text.strip_prefix("#x") {
                    u32::from_str_radix(n, 16).ok()
                } else {
                    text.strip_prefix('#').and_then(|n| n.parse::<u32>().ok())
                };
                number
                    .and_then(char::from_u32)
                    .filter(|c| matches!(*c, '\t' | '\n' | '\r') || !c.is_control())
                    .ok_or("XML: unknown entity or invalid character")?
            }
        };
        out.push(c);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    if out
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
    {
        return Err("XML: invalid character".into());
    }
    Ok(out)
}

struct Xml<'a> {
    input: &'a str,
    at: usize,
    nodes: usize,
}

impl Xml<'_> {
    fn tail(&self) -> &str {
        &self.input[self.at..]
    }
    fn whitespace(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }
    fn name(&mut self) -> Result<String> {
        let start = self.at;
        while self
            .input
            .as_bytes()
            .get(self.at)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':'))
        {
            self.at += 1;
        }
        let name = &self.input[start..self.at];
        if name.is_empty()
            || name.len() > 256
            || !name.as_bytes()[0].is_ascii_alphabetic() && name.as_bytes()[0] != b'_'
        {
            return Err("XML: invalid name".into());
        }
        Ok(name.into())
    }
    fn comment(&mut self) -> Result<()> {
        self.at += 4;
        let end = self.tail().find("-->").ok_or("XML: incomplete comment")?;
        if self.tail()[..end].contains("--") {
            return Err("XML: invalid comment".into());
        }
        self.at += end + 3;
        Ok(())
    }
    fn element(&mut self, depth: usize) -> Result<Element> {
        if depth > 64 || self.nodes >= MAX_ITEMS {
            return Err("XML: complexity exceeds the limit".into());
        }
        self.nodes += 1;
        if !self.tail().starts_with('<') {
            return Err("XML: expected an element".into());
        }
        self.at += 1;
        let name = self.name()?;
        let mut attrs = BTreeMap::new();
        let closed;
        loop {
            let before = self.at;
            self.whitespace();
            if self.tail().starts_with("/>") {
                self.at += 2;
                closed = true;
                break;
            }
            if self.tail().starts_with('>') {
                self.at += 1;
                closed = false;
                break;
            }
            if before == self.at || attrs.len() >= 64 {
                return Err("XML: invalid attributes".into());
            }
            let key = self.name()?;
            self.whitespace();
            if !self.tail().starts_with('=') {
                return Err("XML: expected an equals sign".into());
            }
            self.at += 1;
            self.whitespace();
            let quote = self
                .input
                .as_bytes()
                .get(self.at)
                .copied()
                .filter(|q| matches!(q, b'\'' | b'"'))
                .ok_or("XML: unquoted attribute")?;
            self.at += 1;
            let end = self
                .tail()
                .find(quote as char)
                .ok_or("XML: incomplete attribute")?;
            if end > 8192 || self.tail()[..end].contains('<') {
                return Err("XML: attribute is too long or invalid".into());
            }
            let value = xml_unescape(&self.tail()[..end])?;
            self.at += end + 1;
            if attrs.insert(key, value).is_some() {
                return Err("XML: duplicate attribute".into());
            }
        }
        let mut out = Element {
            name,
            attrs,
            text: String::new(),
            children: Vec::new(),
        };
        if closed {
            return Ok(out);
        }
        loop {
            if self.tail().starts_with("</") {
                self.at += 2;
                let name = self.name()?;
                self.whitespace();
                if name != out.name || !self.tail().starts_with('>') {
                    return Err("XML: incorrect closing tag".into());
                }
                self.at += 1;
                return Ok(out);
            }
            if self.tail().starts_with("<!--") {
                self.comment()?;
                continue;
            }
            if self.tail().starts_with("<![CDATA[") {
                self.at += 9;
                let end = self.tail().find("]]>").ok_or("XML: incomplete CDATA")?;
                let value = &self.tail()[..end];
                if value
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
                {
                    return Err("XML: invalid CDATA".into());
                }
                out.text.push_str(value);
                self.at += end + 3;
                continue;
            }
            if self.tail().starts_with("<!") || self.tail().starts_with("<?") {
                return Err("XML: DTD and internal processing instructions are forbidden".into());
            }
            if self.tail().starts_with('<') {
                out.children.push(self.element(depth + 1)?);
                continue;
            }
            let end = self.tail().find('<').ok_or("XML: incomplete element")?;
            out.text.push_str(&xml_unescape(&self.tail()[..end])?);
            self.at += end;
        }
    }
}

pub(crate) fn parse_xml(text: &str) -> Result<Element> {
    if text.len() > MAX_XML {
        return Err("XML: document is too large".into());
    }
    let mut parser = Xml {
        input: text.strip_prefix('\u{feff}').unwrap_or(text),
        at: 0,
        nodes: 0,
    };
    parser.whitespace();
    if parser.tail().starts_with("<?xml ") {
        let end = parser
            .tail()
            .find("?>")
            .ok_or("XML: incomplete declaration")?;
        if end > 256
            || parser.tail()[..end].contains('<') && parser.tail()[..end].matches('<').count() > 1
        {
            return Err("XML: invalid declaration".into());
        }
        let declaration = &parser.tail()[..end];
        if declaration.contains("encoding") && !declaration.to_ascii_lowercase().contains("utf-8") {
            return Err("XML: only UTF-8 is supported".into());
        }
        parser.at += end + 2;
    }
    parser.whitespace();
    while parser.tail().starts_with("<!--") {
        parser.comment()?;
        parser.whitespace();
    }
    let element = parser.element(0)?;
    parser.whitespace();
    while parser.tail().starts_with("<!--") {
        parser.comment()?;
        parser.whitespace();
    }
    if !parser.tail().is_empty() {
        return Err("XML: trailing data after the document".into());
    }
    Ok(element)
}
