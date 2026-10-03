use crate::Result;

#[derive(Clone, Copy)]
pub(super) struct Element<'a> {
    pub tag: u8,
    pub body: &'a [u8],
    pub encoded: &'a [u8],
}

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }
    pub(super) fn empty(&self) -> bool {
        self.cursor == self.bytes.len()
    }
    pub(super) fn finish(&self) -> Result<()> {
        if self.empty() {
            Ok(())
        } else {
            Err("DER: trailing data".into())
        }
    }
    pub(super) fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }
    pub(super) fn read(&mut self) -> Result<Element<'a>> {
        let start = self.cursor;
        let tag = *self.bytes.get(self.cursor).ok_or("DER: truncated element")?;
        self.cursor += 1;
        if tag & 0x1f == 0x1f {
            return Err("DER: extended tag numbers are unsupported".into());
        }
        let first = *self
            .bytes
            .get(self.cursor)
            .ok_or("DER: missing length")?;
        self.cursor += 1;
        let length = if first & 0x80 == 0 {
            usize::from(first)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 {
                return Err("DER: indefinite or excessive length".into());
            }
            let raw = self
                .bytes
                .get(self.cursor..self.cursor + count)
                .ok_or("DER: truncated length")?;
            if raw[0] == 0 {
                return Err("DER: noncanonical length".into());
            }
            self.cursor += count;
            let value = raw
                .iter()
                .fold(0usize, |n, byte| (n << 8) | usize::from(*byte));
            if value < 128 {
                return Err("DER: nonminimal length".into());
            }
            value
        };
        let end = self
            .cursor
            .checked_add(length)
            .ok_or("DER: length overflow")?;
        let body = self
            .bytes
            .get(self.cursor..end)
            .ok_or("DER: truncated element")?;
        self.cursor = end;
        Ok(Element {
            tag,
            body,
            encoded: &self.bytes[start..end],
        })
    }
    pub(super) fn expect(&mut self, tag: u8) -> Result<Element<'a>> {
        let element = self.read()?;
        if element.tag == tag {
            Ok(element)
        } else {
            Err(format!(
                "DER: expected tag {tag:02x}, received {:02x}",
                element.tag
            ))
        }
    }
}

pub(super) fn sequence(bytes: &[u8]) -> Result<Reader<'_>> {
    let mut outer = Reader::new(bytes);
    let value = outer.expect(0x30)?;
    outer.finish()?;
    Ok(Reader::new(value.body))
}

pub(super) fn positive_integer(bytes: &[u8]) -> Result<&[u8]> {
    if bytes.is_empty() || bytes[0] & 0x80 != 0 {
        return Err("DER: positive integer expected".into());
    }
    if bytes.len() > 1 && bytes[0] == 0 {
        if bytes[1] & 0x80 == 0 {
            return Err("DER: nonminimal integer".into());
        }
        Ok(&bytes[1..])
    } else {
        Ok(bytes)
    }
}

pub(super) fn bit_string(bytes: &[u8]) -> Result<&[u8]> {
    if bytes.first() != Some(&0) {
        return Err("DER: bit string is not byte-aligned".into());
    }
    Ok(&bytes[1..])
}

pub(super) fn boolean(bytes: &[u8]) -> Result<bool> {
    match bytes {
        [0] => Ok(false),
        [0xff] => Ok(true),
        _ => Err("DER: noncanonical boolean".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_ber_and_truncation() {
        for invalid in [
            &[0x30, 0x80, 0, 0][..],
            &[0x30, 0x81, 0],
            &[0x30, 0x82, 0, 128],
            &[0x30, 1],
            &[0x1f, 0],
        ] {
            assert!(Reader::new(invalid).read().is_err());
        }
        assert!(positive_integer(&[0, 1]).is_err());
        assert!(positive_integer(&[0x80]).is_err());
        assert_eq!(positive_integer(&[0, 0x80]).unwrap(), &[0x80]);
    }
}
