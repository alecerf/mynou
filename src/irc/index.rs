//! Row, chronological and routing tables share one atomic inventory commit.
use super::{Record, State, hash};
use crate::{
    Result,
    inventory::{Change, Index, Key, Query},
    json,
};

pub(super) const HEADER: Key = [0; 64];
pub(super) const SOURCE: Key = constant(1);
pub(super) const CURSOR: Key = constant(2);
pub(super) const COUNT: Key = constant(3);
const fn constant(n: u8) -> Key {
    let mut k = [0; 64];
    k[1] = n;
    k
}
pub(super) fn prefix(n: u8) -> Key {
    let mut k = [0; 64];
    k[0] = n;
    k
}
pub(super) fn primary(id: &str) -> Result<Key> {
    if !hash(id) {
        return Err("IRC: invalid announcement identity".into());
    }
    let mut k = prefix(1);
    for (i, pair) in id.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hex = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        k[i + 1] = hex(pair[0]) * 16 + hex(pair[1]);
    }
    Ok(k)
}
fn chronological(r: &Record) -> Result<Key> {
    let p = primary(&r.id)?;
    let mut k = prefix(2);
    k[1..9].copy_from_slice(&(!r.first_seen).to_be_bytes());
    for (dst, src) in k[9..41].iter_mut().zip(&p[1..33]) {
        *dst = !src;
    }
    Ok(k)
}
fn secondary(r: &Record, table: u8) -> Result<Key> {
    let mut k = primary(&r.id)?;
    k[0] = table;
    Ok(k)
}
fn pending(r: &Record) -> bool {
    (r.decision == "pending" && r.admission.is_none()
        || r.decision == "requested"
            && r.admission.as_ref().is_some_and(|a| a.phase == "committed"))
        && r.route.is_none()
}
fn recovery(r: &Record) -> bool {
    r.route.is_some() || r.admission.is_some()
}
pub(super) fn entries(r: &Record) -> Result<Vec<(Key, Vec<u8>)>> {
    Record::from_json(&r.to_json())?;
    let p = primary(&r.id)?;
    let mut entries = vec![
        (p, json::stringify(&r.to_json()).into_bytes()),
        (chronological(r)?, p.to_vec()),
    ];
    if pending(r) {
        entries.push((secondary(r, 3)?, p.to_vec()));
    }
    if recovery(r) {
        entries.push((secondary(r, 4)?, p.to_vec()));
    }
    Ok(entries)
}
pub(super) fn decode(bytes: &[u8]) -> Result<Record> {
    Record::from_json(&json::parse(
        std::str::from_utf8(bytes).map_err(|_| "IRC: invalid record text")?,
    )?)
}
pub(super) fn get(index: &mut Index, id: &str) -> Result<Option<Record>> {
    index
        .get(&primary(id)?)?
        .map(|bytes| {
            let r = decode(&bytes)?;
            if r.id != id {
                return Err("IRC: record key differs from identity".into());
            }
            Ok(r)
        })
        .transpose()
}
pub(super) fn count(index: &mut Index) -> Result<u64> {
    let bytes = index.get(&COUNT)?;
    if bytes.is_none() && index.get(&HEADER)?.is_some() {
        return Err("IRC: history count is missing".into());
    }
    bytes.map_or(Ok(0), |b| {
        Ok(u64::from_le_bytes(
            b.as_slice()
                .try_into()
                .map_err(|_| "IRC: invalid history count")?,
        ))
    })
}
pub(super) fn commit(
    index: &mut Index,
    header: &State,
    rows: &[Record],
    cursor: Option<&str>,
) -> Result<()> {
    let mut changes = vec![Change::Put(
        HEADER,
        json::stringify(&header.to_json()).into_bytes(),
    )];
    let mut total = count(index)?;
    let mut ids = std::collections::BTreeSet::new();
    for r in rows {
        if !ids.insert(&r.id) {
            return Err("IRC: duplicate identity in history patch".into());
        }
        if header.bindings.get(&r.source_id) != Some(&r.binding) {
            return Err("IRC: unbound announcement".into());
        }
        if let Some(old) = get(index, &r.id)? {
            if old.source_id != r.source_id
                || old.binding != r.binding
                || old.first_seen != r.first_seen
                || old.announcement != r.announcement
            {
                return Err("IRC: immutable announcement changed".into());
            }
            for (key, _) in entries(&old)? {
                changes.push(Change::Delete(key));
            }
        } else {
            total = total.checked_add(1).ok_or("IRC: history count overflow")?;
        }
        for (key, value) in entries(r)? {
            changes.push(Change::Put(key, value));
        }
    }
    changes.push(Change::PutInline(COUNT, total.to_le_bytes().to_vec()));
    if let Some(cursor) = cursor {
        changes.push(Change::Put(CURSOR, primary(cursor)?.to_vec()));
    }
    let mut unique = std::collections::BTreeMap::new();
    for change in changes {
        let key = match &change {
            Change::Put(k, _) | Change::PutInline(k, _) | Change::Delete(k) => *k,
        };
        unique.insert(key, change);
    }
    index.commit(unique.into_values().collect())?;
    Ok(())
}
pub(super) fn page(
    index: &mut Index,
    table: u8,
    after: Option<Key>,
    limit: usize,
) -> Result<(Vec<Record>, Option<Key>)> {
    let page = index.page(&Query {
        start: prefix(table),
        end: Some(prefix(table + 1)),
        after,
        limit,
        sequence: None,
    })?;
    let mut rows = Vec::with_capacity(page.entries.len());
    let mut size = 0;
    let mut next = page.next;
    for (key, value) in page.entries {
        let r = if table == 1 {
            decode(&value)?
        } else {
            let primary: Key = value
                .as_slice()
                .try_into()
                .map_err(|_| "IRC: invalid secondary reference")?;
            decode(
                &index
                    .get(&primary)?
                    .ok_or("IRC: missing referenced record")?,
            )?
        };
        let expected = match table {
            1 => primary(&r.id)?,
            2 => chronological(&r)?,
            3 if pending(&r) => secondary(&r, 3)?,
            4 if recovery(&r) => secondary(&r, 4)?,
            _ => return Err("IRC: inconsistent history table".into()),
        };
        if key != expected {
            return Err("IRC: history page key differs from record".into());
        }
        size += json::stringify(&r.to_json()).len();
        if size > crate::inventory::MAX_PAGE_BYTES && !rows.is_empty() {
            next = Some(after_for_rows(&rows, table)?);
            break;
        }
        rows.push(r);
    }
    Ok((rows, next))
}
fn after_for_rows(rows: &[Record], table: u8) -> Result<Key> {
    let r = rows.last().ok_or("IRC: empty history page")?;
    if table == 2 {
        chronological(r)
    } else {
        secondary(r, table)
    }
}
pub(super) fn cursor(key: &Key) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}
pub(super) fn parse_cursor(text: &str) -> Result<Key> {
    if text.len() != 128
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("IRC: invalid history cursor".into());
    }
    let mut k = [0; 64];
    for (i, pair) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hex = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        k[i] = hex(pair[0]) * 16 + hex(pair[1]);
    }
    if k[0] != 2 || k[41..].iter().any(|b| *b != 0) {
        return Err("IRC: invalid chronological cursor".into());
    }
    Ok(k)
}

pub(super) fn pending_cursor(r: &Record) -> Result<Key> {
    secondary(r, 3)
}
