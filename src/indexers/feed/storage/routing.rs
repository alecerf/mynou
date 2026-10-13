//! A generation and observation commit in one index; checkpoints never resave
//! candidate payloads and compare the exact generation before changing progress.
use super::*;
use super::super::generation::{self, Generation, Header};
const GENERATION: u8 = 0x12;
const ENTRY: u8 = 0x13;
fn entry_key(id: &str, ordinal: usize) -> Key {
    let mut key = key(ENTRY, id);
    key[33..35].copy_from_slice(&(ordinal as u16).to_be_bytes()); key
}
pub(super) fn counts(index: &mut Index) -> Result<(u64,u64)> {
    let bytes = index.get(&ROUTING)?.ok_or("Feed routing: missing counters")?;
    if bytes.len() != 16 { return Err("Feed routing: invalid counters".into()); }
    let generations = u64::from_le_bytes(bytes[..8].try_into().map_err(|_| "Feed routing: invalid count")?);
    let entries = u64::from_le_bytes(bytes[8..].try_into().map_err(|_| "Feed routing: invalid entry count")?);
    if generations > super::super::MAX_WATCHED as u64 || entries < generations || entries > generations * generation::MAX_ENTRIES as u64 {
        return Err("Feed routing: counter bounds disagree".into());
    }
    Ok((generations, entries))
}
pub(super) fn schedule(index: &mut Index) -> Result<String> {
    let bytes = index.get(&SCHEDULE)?.ok_or("Feed routing: missing schedule cursor")?;
    let id = String::from_utf8(bytes).map_err(|_| "Feed routing: invalid schedule cursor")?;
    if !id.is_empty() && !super::super::valid_id(&id) { return Err("Feed routing: invalid schedule identity".into()); }
    Ok(id)
}
pub(super) fn upgrade(index: &mut Index) -> Result<()> {
    match index.get(&HEADER)?.as_deref() {
        Some(FORMAT) => Ok(()),
        Some(OLD_FORMAT) => {
            check(index)?;
            index.commit(vec![Change::PutInline(HEADER, FORMAT.to_vec()), Change::PutInline(ROUTING, encode_counts(0,0)), Change::PutInline(SCHEDULE, Vec::new())])?;
            Ok(())
        }
        _ => Err("Feed routing: unsupported index format".into()),
    }
}
pub(super) fn header(index: &mut Index, id: &str) -> Result<Option<Header>> {
    let Some(bytes) = index.get(&key(GENERATION, id))? else { return Ok(None); };
    let header = Header::from_json(&json::parse(std::str::from_utf8(&bytes).map_err(|_| "Feed routing: invalid manifest bytes")?)?)?;
    if header.source != id { return Err("Feed routing: manifest source collision or corruption".into()); }
    let record = get(index,id)?.ok_or("Feed routing: manifest is missing its observation")?;
    if !record.baselined || record.binding != header.binding || record.revision != header.record_revision {
        return Err("Feed routing: manifest observation proof disagrees".into());
    }
    Ok(Some(header))
}
fn load(index: &mut Index, id: &str) -> Result<Option<Generation>> {
    let Some(header) = header(index,id)? else { return Ok(None); };
    let mut entries = Vec::new(); let mut bytes = 0;
    for ordinal in 0..header.count {
        let encoded = index.get(&entry_key(id,ordinal))?.ok_or("Feed routing: missing captured entry")?;
        bytes += encoded.len();
        if bytes > generation::MAX_BYTES { return Err("Feed routing: capture exceeds byte budget".into()); }
        entries.push(generation::decode_entry(&header,ordinal,&json::parse(std::str::from_utf8(&encoded).map_err(|_| "Feed routing: invalid capture bytes")?)?)?);
    }
    let generation = Generation { header, entries };
    generation.encoded_entries()?;
    Ok(Some(generation))
}
pub(super) fn cancel(index: &mut Index, id: &str) -> Result<Vec<Change>> {
    let Some(generation) = load(index,id)? else { return Ok(Vec::new()); };
    let (generations, entries) = counts(index)?;
    let generations = generations.checked_sub(1).ok_or("Feed routing: generation count underflow")?;
    let entries = entries.checked_sub(generation.header.count as u64).ok_or("Feed routing: entry count underflow")?;
    let mut changes = vec![Change::Delete(key(GENERATION,id)), Change::PutInline(ROUTING,encode_counts(generations,entries))];
    for ordinal in 0..generation.header.count { changes.push(Change::Delete(entry_key(id,ordinal))); }
    Ok(changes)
}
impl Storage {
    fn writable_index(&mut self) -> Result<&mut Index> {
        self.available()?;
        if self.read_only || self.upgrade { return Err("Feed routing: writable upgraded storage required".into()); }
        let Backend::Ready(index) = &mut self.backend else { return Err("Feed routing: indexed storage required".into()); };
        let marker = read(&self.directory.join("feeds.bin"))?;
        if marker.len() != 80 || &marker[..8] != MARKER || marker[16..48] != index.identity() {
            return Err("Feed routing: marker changed; restart required".into());
        }
        Ok(index)
    }
    pub(crate) fn capture(&mut self, record: Record, generation: Generation) -> Result<()> {
        let encoded = generation.encoded_entries()?;
        Header::from_json(&generation.header.to_json())?;
        if !record.baselined || record.id != generation.header.source || record.binding != generation.header.binding || record.revision != generation.header.record_revision {
            return Err("Feed routing: capture does not bind observation".into());
        }
        let index = self.writable_index()?;
        if header(index,&record.id)?.is_some() { return Err("Feed routing: pending generation backpressures polling".into()); }
        let (generations, entries) = counts(index)?;
        let generations = generations.checked_add(1).ok_or("Feed routing: generation count overflow")?;
        let entries = entries.checked_add(encoded.len() as u64).ok_or("Feed routing: entry count overflow")?;
        if generations > super::super::MAX_WATCHED as u64 { return Err("Feed routing: concurrent source bound exceeded".into()); }
        let mut changes = vec![Change::Put(key(GENERATION,&record.id),json::stringify(&generation.header.to_json()).into_bytes()), Change::PutInline(ROUTING,encode_counts(generations,entries))];
        for (ordinal, bytes) in encoded.into_iter().enumerate() { changes.push(Change::Put(entry_key(&record.id,ordinal),bytes)); }
        put_with(index,&[record],None,changes)
    }
    pub(crate) fn pending(&mut self, id: &str) -> Result<Option<Generation>> {
        self.available()?;
        match &mut self.backend {
            Backend::Ready(index) => load(index,id),
            Backend::Unpublished(..) | Backend::Legacy(_) | Backend::Absent => Ok(None),
        }
    }
    pub(crate) fn pending_header(&mut self,id: &str) -> Result<Option<Header>> {
        self.available()?;
        match &mut self.backend { Backend::Ready(index) => header(index,id), _ => Ok(None) }
    }
    pub(crate) fn checkpoint(&mut self, expected: &Header, next: Option<&Header>) -> Result<bool> {
        let index = self.writable_index()?;
        let Some(current) = header(index,&expected.source)? else { return Ok(false); };
        if &current != expected { return Err("Feed routing: stale generation checkpoint".into()); }
        let changes = match next {
            None => cancel(index,&expected.source)?,
            Some(next) => {
                let mut progress = expected.clone(); progress.library = next.library; progress.after.clone_from(&next.after);
                Header::from_json(&next.to_json())?;
                if &progress != next || (expected.library && !next.library) { return Err("Feed routing: invalid checkpoint transition".into()); }
                if expected.library != next.library {
                    if next.after.is_some() { return Err("Feed routing: new phase must start at its first row".into()); }
                } else {
                    let after = next.after.as_ref().ok_or("Feed routing: checkpoint must advance a row")?;
                    if let Some(previous) = &expected.after {
                        let ordering = |cursor: &str| -> Result<(u64,String)> { Ok((!u64::from_str_radix(&cursor[..16],16).map_err(|_| "Feed routing: invalid cursor time")?,cursor[16..].to_owned())) };
                        if ordering(after)? <= ordering(previous)? { return Err("Feed routing: checkpoint did not advance".into()); }
                    }
                }
                vec![Change::Put(key(GENERATION,&expected.source),json::stringify(&next.to_json()).into_bytes())]
            }
        };
        index.commit(changes)?; Ok(true)
    }
    pub(crate) fn routing_counts(&mut self) -> Result<(u64,u64)> {
        self.available()?;
        match &mut self.backend { Backend::Ready(index) if !self.upgrade => counts(index), _ => Ok((0,0)) }
    }
    pub(crate) fn schedule_cursor(&mut self) -> Result<String> {
        self.available()?;
        match &mut self.backend { Backend::Ready(index) if !self.upgrade => schedule(index), _ => Ok(String::new()) }
    }
    pub(crate) fn scheduled(&mut self,id: &str) -> Result<()> {
        if !super::super::valid_id(id) { return Err("Feed routing: invalid scheduled identity".into()); }
        self.writable_index()?.commit(vec![Change::Put(SCHEDULE,id.as_bytes().to_vec())])?; Ok(())
    }
}
