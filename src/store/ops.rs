//! Index operations behind the job store: point reads, bounded scans, the
//! staging of one transaction (record, entries, counters, history, library
//! tips) and the claim queue. Everything here reads one tree path per lookup
//! and writes through a single index commit.
use super::{
    Change, Event, Index, Job, Key, MAX_JOB_EVENTS, MAX_PAGE_ROWS, Query, Result, bounded_message,
    layout::*,
};
use crate::json;
use std::collections::BTreeMap;

/// The history message of a lease renewal.
pub(super) const RENEWED: &str = "processing lease renewed";

/// Totals kept in one inline entry so status needs no scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub jobs: u64,
    /// Jobs that are not ready, failed or cancelled.
    pub active: u64,
    pub ready: u64,
    /// The newest history sequence number.
    pub events: u64,
    /// Media identities that have a current library version.
    pub library: u64,
}

impl Counts {
    pub(super) fn encode(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(32);
        for value in [self.jobs, self.active, self.ready, self.events] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        let mut values = [0_u64; 4];
        if bytes.len() != 32 {
            return Err("Invalid job index counters".into());
        }
        for (slot, chunk) in values.iter_mut().zip(bytes.as_chunks::<8>().0) {
            let mut word = [0_u8; 8];
            word.copy_from_slice(chunk);
            *slot = u64::from_le_bytes(word);
        }
        let [jobs, active, ready, events] = values;
        if active > jobs || ready > jobs || active.saturating_add(ready) > jobs {
            return Err("Job index counters disagree with their totals".into());
        }
        Ok(Self {
            jobs,
            active,
            ready,
            events,
            library: 0,
        })
    }
}

/// A job before and after one transaction.
pub(super) struct Pair {
    pub previous: Option<Job>,
    pub job: Job,
    pub root: Root,
}

/// The index with the lookups the store needs.
pub(super) struct Db<'a>(pub &'a mut Index);

pub(super) type Staged = BTreeMap<Key, Option<Entry>>;

fn stage_put(staged: &mut Staged, key: Key, entry: Entry) {
    staged.insert(key, Some(entry));
}

fn stage_delete(staged: &mut Staged, key: Key) {
    // A put staged for the same key by another job of the transaction wins.
    staged.entry(key).or_insert(None);
}

pub(super) fn array<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| "Invalid job index entry".to_owned())
}

pub(super) fn key_part<const N: usize>(key: &Key, at: usize) -> Result<[u8; N]> {
    let end = at.checked_add(N).ok_or("Invalid job index key")?;
    array(key.get(at..end).ok_or("Invalid job index key")?)
}

/// A cursor over jobs ordered by creation: sixteen hex digits of the creation
/// time followed by the job identifier.
pub(super) fn cursor_text(created: u64, id: &[u8; 16]) -> String {
    format!("{created:016x}{}", id_text(id))
}

pub(super) fn parse_cursor(text: &str) -> Result<(u64, [u8; 16])> {
    let invalid = || "Invalid job page cursor".to_owned();
    if text.len() != 48 || !text.is_char_boundary(16) {
        return Err(invalid());
    }
    let (created, id) = text.split_at(16);
    if !created
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid());
    }
    let created = u64::from_str_radix(created, 16).map_err(|_| invalid())?;
    Ok((created, id_bytes(id).map_err(|_| invalid())?))
}

impl super::JobLookup for Db<'_> {
    fn lookup(&mut self, id: &str) -> Result<Option<Job>> {
        self.get(id)
    }
}

impl Db<'_> {
    pub(super) fn scan(
        &mut self,
        prefix: &[u8],
        after: Option<Key>,
        limit: usize,
    ) -> Result<crate::inventory::Page> {
        let (start, end) = prefix_range(prefix);
        self.0.page(&Query {
            start,
            end,
            after,
            limit: limit.clamp(1, MAX_PAGE_ROWS),
            sequence: None,
        })
    }

    /// Keys under a prefix, in order, at most `limit`, without reading values.
    pub(super) fn keys(&mut self, prefix: &[u8], limit: usize) -> Result<Vec<Key>> {
        Ok(self
            .scan(prefix, None, limit)?
            .entries
            .into_iter()
            .map(|(key, _)| key)
            .collect())
    }

    pub(super) fn has_prefix(&mut self, prefix: &[u8]) -> Result<bool> {
        Ok(!self.scan(prefix, None, 1)?.entries.is_empty())
    }

    pub(super) fn exists(&mut self, key: &Key) -> Result<bool> {
        Ok(self.0.get(key)?.is_some())
    }

    pub(super) fn get(&mut self, id: &str) -> Result<Option<Job>> {
        let Ok(raw) = id_bytes(id) else {
            return Ok(None);
        };
        self.job(&raw)
    }

    pub(super) fn job(&mut self, id: &[u8; 16]) -> Result<Option<Job>> {
        let Some(mut bytes) = self.0.get(&job_key(id))? else {
            return Ok(None);
        };
        let value = json::parse(std::str::from_utf8(&bytes).map_err(|_| "Invalid job record")?)?;
        if let Some(chunks) = value.get("job_chunks") {
            let count = chunks.as_u64().ok_or("Invalid job chunk count")?;
            let length = value
                .get("job_bytes")
                .and_then(crate::json::Value::as_u64)
                .ok_or("Missing job chunk length")?;
            if !(2..=16).contains(&count)
                || length > super::legacy::MAX_RECORD as u64
                || length <= crate::inventory::MAX_VALUE_BYTES as u64
                || count != length.div_ceil(crate::inventory::MAX_VALUE_BYTES as u64)
            {
                return Err("Invalid job chunk bounds".into());
            }
            bytes = Vec::with_capacity(length as usize);
            for n in 0..count {
                let chunk = self
                    .0
                    .get(&key(&[&[T_JOB_CHUNK], id, &(n as u32).to_be_bytes()]))?
                    .ok_or("Missing job record chunk")?;
                let expected =
                    (length as usize - bytes.len()).min(crate::inventory::MAX_VALUE_BYTES);
                if chunk.len() != expected {
                    return Err("Invalid job record chunk length".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            if value.get("job_digest").and_then(crate::json::Value::as_str)
                != Some(super::hex(&crate::crypto::sha256(&bytes)).as_str())
            {
                return Err("Invalid job record chunk digest".into());
            }
        }
        decode_job(&bytes, &id_text(id)).map(Some)
    }

    pub(super) fn require(&mut self, id: &[u8; 16]) -> Result<Job> {
        self.job(id)?
            .ok_or_else(|| "Job index refers to a missing job".to_owned())
    }

    pub(super) fn root(&mut self, id: &[u8; 16]) -> Result<Option<Root>> {
        self.0
            .get(&root_key(id))?
            .map(|bytes| Root::decode(&bytes))
            .transpose()
    }

    pub(super) fn counts(&mut self) -> Result<Counts> {
        let mut counts = Counts::decode(
            &self
                .0
                .get(&COUNTS_KEY)?
                .ok_or("Job index is missing its counters")?,
        )?;
        let library = self
            .0
            .get(&LIBRARY_COUNT_KEY)?
            .ok_or("Job index is missing its library counter")?;
        counts.library = u64::from_le_bytes(array(&library)?);
        if counts.library > counts.jobs {
            return Err("Job index counters disagree with their totals".into());
        }
        Ok(counts)
    }

    /// The job owning a canonical request key.
    pub(super) fn by_key(&mut self, key: &str) -> Result<Option<Job>> {
        let digest = digest_bytes(key)?;
        let Some(holder) = self.0.get(&canonical_key(&digest))? else {
            return Ok(None);
        };
        let job = self.require(&array(&holder)?)?;
        if job.key != key {
            return Err("Job key index disagrees with its record".into());
        }
        Ok(Some(job))
    }

    pub(super) fn find_job(
        &mut self,
        prefix: &[u8],
        id_at: usize,
        predicate: impl Fn(&Job) -> bool,
    ) -> Result<Option<Job>> {
        let mut after = None;
        loop {
            let page = self.scan(prefix, after, MAX_PAGE_ROWS)?;
            for (key, _) in &page.entries {
                let job = self.require(&key_part(key, id_at)?)?;
                if predicate(&job) {
                    return Ok(Some(job));
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(None);
            }
        }
    }

    pub(super) fn media_state(&mut self, media: &str) -> Result<Option<Job>> {
        let digest = digest_bytes(media)?;
        let prefix = [&[T_MEDIA][..], &digest].concat();
        let mut after = None;
        let mut chosen = None;
        loop {
            let page = self.scan(&prefix, after, MAX_PAGE_ROWS)?;
            for (key, _) in &page.entries {
                let job = self.require(&key_part(key, 41)?)?;
                if job.request.media_key() != media {
                    return Err("Job media index disagrees with its record".into());
                }
                if job.state == "ready" {
                    return Ok(Some(job));
                }
                chosen = Some(job);
            }
            after = page.next;
            if after.is_none() {
                return Ok(chosen);
            }
        }
    }

    pub(super) fn transfer_matches(
        &mut self,
        transfer: &str,
        predicate: impl Fn(&Job) -> bool,
    ) -> Result<bool> {
        let prefix = [&[T_TRANSFER][..], &short("transfer", &[transfer])].concat();
        Ok(self
            .find_job(&prefix, 17, |job| {
                transfers(job).contains(&transfer) && predicate(job)
            })?
            .is_some())
    }

    /// Jobs of one media identity in creation order, reading every page of this identity.
    pub(super) fn media_jobs(&mut self, media: &str) -> Result<Vec<Job>> {
        let digest = digest_bytes(media)?;
        let mut jobs = Vec::new();
        for key in self.all_keys(&[&[T_MEDIA][..], &digest].concat())? {
            let job = self.require(&key_part(&key, 41)?)?;
            if job.request.media_key() != media {
                return Err("Job media index disagrees with its record".into());
            }
            jobs.push(job);
        }
        Ok(jobs)
    }

    pub(super) fn media_exists(&mut self, media: &str) -> Result<bool> {
        let digest = digest_bytes(media)?;
        self.has_prefix(&[&[T_MEDIA][..], &digest].concat())
    }

    /// Whether some job other than `except` has a pending upgrade for this media.
    pub(super) fn other_pending(&mut self, media: &str, except: Option<&str>) -> Result<bool> {
        let digest = digest_bytes(media)?;
        let except = except.map(id_bytes).transpose()?;
        let mut after = None;
        loop {
            let page = self.scan(&[&[T_PENDING][..], &digest].concat(), after, MAX_PAGE_ROWS)?;
            for (key, _) in &page.entries {
                if Some(key_part::<16>(key, 33)?) != except {
                    return Ok(true);
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(false);
            }
        }
    }

    /// Flags of a job that is the current library version of its media:
    /// `(monitored, has a release baseline)`.
    pub(super) fn tip(&mut self, id: &[u8; 16]) -> Result<Option<(bool, bool)>> {
        let Some(bytes) = self.0.get(&tip_key(id))? else {
            return Ok(None);
        };
        match bytes.as_slice() {
            [monitored, release] if *monitored <= 1 && *release <= 1 => {
                Ok(Some((*monitored == 1, *release == 1)))
            }
            _ => Err("Invalid library tip entry".into()),
        }
    }

    pub(super) fn is_tip(&mut self, id: &str) -> Result<bool> {
        let Ok(raw) = id_bytes(id) else {
            return Ok(false);
        };
        Ok(self.tip(&raw)?.is_some())
    }

    /// Members of a shared-file group, in canonical episode order. Only
    /// entries whose record carries exactly this binding are returned.
    pub(super) fn group_members(&mut self, file: &crate::pack::SharedFile) -> Result<Vec<Job>> {
        let group = short("group", &[&file.id()]);
        let mut jobs = Vec::new();
        let mut after = None;
        loop {
            let page = self.scan(&[&[T_GROUP][..], &group].concat(), after, MAX_PAGE_ROWS)?;
            for (_, value) in &page.entries {
                let job = self.require(&array(value)?)?;
                if job.shared_file.as_ref() == Some(file) {
                    jobs.push(job);
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        jobs.sort_by_key(|job| job.request.episode);
        Ok(jobs)
    }

    /// Jobs that name a transfer through any of their bindings.
    pub(super) fn transfer_jobs(&mut self, transfer: &str) -> Result<Vec<Job>> {
        let prefix = [&[T_TRANSFER][..], &short("transfer", &[transfer])].concat();
        let mut jobs = Vec::new();
        for key in self.all_keys(&prefix)? {
            let job = self.require(&key_part(&key, 17)?)?;
            if transfers(&job).contains(&transfer) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    /// Whether a job that is not cancelled still depends on the transfer.
    pub(super) fn transfer_has_owner(&mut self, transfer: &str) -> Result<bool> {
        let prefix = [&[T_TRANSFER][..], &short("transfer", &[transfer])].concat();
        let mut after = None;
        loop {
            let page = self.scan(&prefix, after, MAX_PAGE_ROWS)?;
            for (key, _) in &page.entries {
                let job = self.require(&key_part(key, 17)?)?;
                if job.state != "cancelled" && transfers(&job).contains(&transfer) {
                    return Ok(true);
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(false);
            }
        }
    }

    /// Jobs mapped to a pack file through a source URL or a transfer.
    pub(super) fn pack_holders(&mut self, path: &str, kind: &str, value: &str) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        for key in self.all_keys(&packref_prefix(path, kind, value))? {
            let job = self.require(&key_part(&key, 33)?)?;
            if job.pack_file.as_deref() == Some(path) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    pub(super) fn children(&mut self, parent: &[u8; 16]) -> Result<Vec<([u8; 16], bool)>> {
        let mut found = Vec::new();
        let mut after = None;
        loop {
            let page = self.scan(&[&[T_CHILD][..], parent].concat(), after, MAX_PAGE_ROWS)?;
            for (key, value) in &page.entries {
                found.push((key_part::<16>(key, 17)?, value.first() == Some(&1)));
            }
            after = page.next;
            if after.is_none() {
                return Ok(found);
            }
        }
    }

    // -- staging ---------------------------------------------------------

    /// Everything one transaction changes: the jobs and their entries, the
    /// library tips, the counters and the history of the first job.
    pub(super) fn stage(&mut self, jobs: &[Job], message: &str) -> Result<Vec<Change>> {
        let mut staged = Staged::new();
        let mut pairs = Vec::with_capacity(jobs.len());
        let mut counts = self.counts()?;
        for job in jobs {
            let id = id_bytes(&job.id)?;
            let previous = self.job(&id)?;
            let root = match &previous {
                Some(_) => self.root(&id)?.ok_or("Job lineage entry is missing")?,
                None => match &job.upgrade_parent {
                    Some(parent) => self
                        .root(&id_bytes(parent)?)?
                        .ok_or("Missing upgrade parent")?,
                    None => Root {
                        id,
                        created_at: job.created_at,
                    },
                },
            };
            pairs.push(Pair {
                previous,
                job: job.clone(),
                root,
            });
        }
        for pair in &pairs {
            self.stage_job(pair, &mut staged)?;
            counts = adjust(counts, pair)?;
        }
        let library = counts.library;
        counts.library = self.stage_tips(&pairs, &mut staged, library)?;
        let first = jobs.first().ok_or("Empty job transaction")?;
        counts.events = counts
            .events
            .checked_add(1)
            .ok_or("Job history sequence overflow")?;
        let event = Event {
            id: counts.events,
            job_id: first.id.clone(),
            state: first.state.clone(),
            message: bounded_message(message),
            at: first.updated_at,
        };
        self.stage_event(&event, &mut staged)?;
        staged.insert(COUNTS_KEY, Some(Entry::Inline(counts.encode())));
        staged.insert(
            LIBRARY_COUNT_KEY,
            Some(Entry::Inline(counts.library.to_le_bytes().to_vec())),
        );
        Ok(staged
            .into_iter()
            .map(|(key, entry)| match entry {
                Some(entry) => entry.change(key),
                None => Change::Delete(key),
            })
            .collect())
    }

    fn stage_job(&mut self, pair: &Pair, staged: &mut Staged) -> Result<()> {
        let id = id_bytes(&pair.job.id)?;
        let mut desired = owned(&pair.job, &pair.root)?;
        let mut current = match &pair.previous {
            Some(previous) => owned(previous, &pair.root)?,
            None => BTreeMap::new(),
        };
        if let Some(previous) = &pair.previous
            && open(previous)
        {
            // A job that was promoted to the due queue has no wait entry.
            let queued = queue_key(previous.created_at, &id);
            if self.exists(&queued)? {
                current.remove(&wait_key(wake(previous), previous.created_at, &id));
                if open(&pair.job) && wake(&pair.job) == wake(previous) {
                    desired.remove(&wait_key(wake(&pair.job), pair.job.created_at, &id));
                } else {
                    stage_delete(staged, queued);
                }
            }
        }
        for (key, entry) in &desired {
            if current.get(key) != Some(entry) {
                stage_put(staged, *key, entry.clone());
            }
        }
        for key in current.keys() {
            if !desired.contains_key(key) {
                stage_delete(staged, *key);
            }
        }
        Ok(())
    }

    fn stage_event(&mut self, event: &Event, staged: &mut Staged) -> Result<()> {
        let id = id_bytes(&event.job_id)?;
        let metadata = self.0.get(&events_key(&id, 0))?;
        let (mut count, last) = match metadata {
            Some(bytes) if bytes.len() == 16 => (
                u64::from_le_bytes(array(&bytes[..8])?),
                u64::from_le_bytes(array(&bytes[8..])?),
            ),
            Some(_) => return Err("Invalid job history counters".into()),
            None => (0, 0),
        };
        if count > MAX_JOB_EVENTS as u64 || (count == 0) != (last == 0) {
            return Err("Invalid job history counters".into());
        }
        // Consecutive heartbeats replace only the previous heartbeat.
        if event.message == RENEWED
            && last != 0
            && self
                .event(&id, last)?
                .is_some_and(|old| old.message == RENEWED)
        {
            staged.insert(events_key(&id, last), None);
            staged.insert(sequence_key(last), None);
            count -= 1;
        }
        if count == MAX_JOB_EVENTS as u64 {
            let first = self
                .0
                .page(&Query {
                    start: events_key(&id, 1),
                    end: prefix_range(&[&[T_EVENTS][..], &id].concat()).1,
                    after: None,
                    limit: 1,
                    sequence: None,
                })?
                .entries
                .into_iter()
                .next()
                .ok_or("Missing job history row")?
                .0;
            staged.insert(first, None);
            staged.insert(
                sequence_key(u64::from_be_bytes(key_part(&first, 17)?)),
                None,
            );
            count -= 1;
        }
        let mut metadata = (count + 1).to_le_bytes().to_vec();
        metadata.extend_from_slice(&event.id.to_le_bytes());
        staged.insert(events_key(&id, 0), Some(Entry::Inline(metadata)));
        staged.insert(
            events_key(&id, event.id),
            Some(Entry::File(encode_event(event))),
        );
        staged.insert(sequence_key(event.id), Some(Entry::Inline(id.to_vec())));
        Ok(())
    }

    pub(super) fn event(&mut self, id: &[u8; 16], sequence: u64) -> Result<Option<Event>> {
        self.0
            .get(&events_key(id, sequence))?
            .map(|bytes| decode_event(&bytes, &id_text(id), sequence))
            .transpose()
    }

    pub(super) fn events(&mut self, id: &[u8; 16]) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        let mut after = None;
        loop {
            let page = self.scan(&[&[T_EVENTS][..], id].concat(), after, MAX_PAGE_ROWS)?;
            for (key, bytes) in page.entries {
                let sequence = u64::from_be_bytes(key_part(&key, 17)?);
                if sequence != 0 {
                    events.push(decode_event(&bytes, &id_text(id), sequence)?);
                    if events.len() > MAX_JOB_EVENTS {
                        return Err("Oversized job history".into());
                    }
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(events);
            }
        }
    }

    /// Updates which job is the library's current version of each media
    /// identity touched by the transaction, with its flags and list entry.
    fn stage_tips(&mut self, pairs: &[Pair], staged: &mut Staged, library: u64) -> Result<u64> {
        let mut library = library;
        let mut by_media: BTreeMap<[u8; 32], Vec<&Pair>> = BTreeMap::new();
        for pair in pairs {
            by_media
                .entry(digest_bytes(&pair.job.request.media_key())?)
                .or_default()
                .push(pair);
        }
        let in_transaction: BTreeMap<[u8; 16], bool> = pairs
            .iter()
            .filter_map(|pair| {
                let child = id_bytes(&pair.job.id).ok()?;
                Some((child, imported(&pair.job)))
            })
            .collect();
        for (media, members) in by_media {
            // The current winner is a point lookup. Updating an unrelated
            // non-imported job never scans all ready lineages of this media.
            let before = self
                .0
                .get(&media_tip_key(&media))?
                .map(|bytes| array::<24>(&bytes))
                .transpose()?;
            let mut candidates: BTreeMap<Key, [u8; 24]> = BTreeMap::new();
            if let Some(value) = before {
                let id = array(&value[..16])?;
                let root = self.root(&id)?.ok_or("Missing library lineage root")?;
                candidates.insert(candidate_key(&media, root.created_at, &root.id), value);
            }
            for pair in &members {
                if !imported(&pair.job) {
                    continue;
                }
                let id = id_bytes(&pair.job.id)?;
                // A job that already has a ready import as a child is replaced.
                let mut replaced = false;
                for (child, ready) in self.children(&id)? {
                    replaced |= in_transaction.get(&child).copied().unwrap_or(ready);
                }
                if replaced {
                    continue;
                }
                let key = candidate_key(&media, pair.root.created_at, &pair.root.id);
                let mut value = [0_u8; 24];
                value[..16].copy_from_slice(&id);
                value[16..].copy_from_slice(&pair.job.created_at.to_be_bytes());
                candidates.insert(key, value);
                stage_put(staged, key, Entry::Inline(value.to_vec()));
            }
            let after_tip = candidates.iter().next_back().map(|(_, value)| *value);
            if before != after_tip {
                staged.insert(
                    media_tip_key(&media),
                    after_tip.map(|value| Entry::Inline(value.to_vec())),
                );
            }
            let holder = |value: &[u8; 24]| -> Result<([u8; 16], u64)> {
                Ok((
                    array(&value[..16])?,
                    u64::from_be_bytes(array(&value[16..])?),
                ))
            };
            let before = before.as_ref().map(holder).transpose()?;
            let after_tip = after_tip.as_ref().map(holder).transpose()?;
            if before != after_tip {
                if let Some((id, created)) = before {
                    staged.insert(library_key(created, &id), None);
                    staged.insert(tip_key(&id), None);
                    library = library
                        .checked_sub(1)
                        .ok_or("Job index library counter disagrees with its entries")?;
                }
                if let Some((id, created)) = after_tip {
                    staged.insert(library_key(created, &id), Some(Entry::Inline(Vec::new())));
                    library = library.checked_add(1).ok_or("Library counter overflow")?;
                }
            }
            if let Some((id, _)) = after_tip {
                // Flags follow the record while it is the current version.
                let member = members
                    .iter()
                    .find(|pair| id_bytes(&pair.job.id).is_ok_and(|raw| raw == id));
                let flags = match member {
                    Some(pair) => Some((pair.job.monitored, pair.job.release.is_some())),
                    None if before != after_tip => {
                        let job = self.require(&id)?;
                        Some((job.monitored, job.release.is_some()))
                    }
                    None => None,
                };
                if let Some((monitored, release)) = flags {
                    staged.insert(
                        tip_key(&id),
                        Some(Entry::Inline(vec![u8::from(monitored), u8::from(release)])),
                    );
                }
            }
        }
        Ok(library)
    }

    // -- claim queue ---------------------------------------------------------

    /// Moves jobs whose wait ended into the due queue: at most one page.
    pub(super) fn promote(&mut self, at: u64) -> Result<usize> {
        let end = match at.checked_add(1) {
            Some(next) => key(&[&[T_WAIT], &next.to_be_bytes()]),
            None => prefix_range(&[T_WAIT]).1.ok_or("Invalid wait range")?,
        };
        let page = self.0.page(&Query {
            start: key(&[&[T_WAIT]]),
            end: Some(end),
            after: None,
            limit: MAX_PAGE_ROWS,
            sequence: None,
        })?;
        let mut changes = Vec::with_capacity(page.entries.len() * 2);
        for (found, _) in &page.entries {
            let created = u64::from_be_bytes(key_part(found, 9)?);
            let id: [u8; 16] = key_part(found, 17)?;
            changes.push(Change::Delete(*found));
            changes.push(Change::PutInline(queue_key(created, &id), Vec::new()));
        }
        let moved = page.entries.len();
        if !changes.is_empty() {
            self.0.commit(changes)?;
        }
        Ok(moved)
    }

    /// Up to `limit` due-queue keys after `after`, oldest creation first.
    pub(super) fn due(&mut self, after: Option<Key>, limit: usize) -> Result<(Vec<Key>, bool)> {
        let page = self.scan(&[T_QUEUE], after, limit)?;
        Ok((
            page.entries.into_iter().map(|(key, _)| key).collect(),
            page.next.is_some(),
        ))
    }
}

/// Counter changes one job transition causes.
fn adjust(mut counts: Counts, pair: &Pair) -> Result<Counts> {
    let underflow = || "Job index counters disagree with its records".to_owned();
    let add = |value: u64| {
        value
            .checked_add(1)
            .ok_or("Job counter overflow".to_owned())
    };
    let sub = |value: u64| value.checked_sub(1).ok_or_else(underflow);
    if let Some(previous) = &pair.previous {
        if !terminal(&previous.state) {
            counts.active = sub(counts.active)?;
        }
        if previous.state == "ready" {
            counts.ready = sub(counts.ready)?;
        }
    } else {
        counts.jobs = add(counts.jobs)?;
    }
    if !terminal(&pair.job.state) {
        counts.active = add(counts.active)?;
    }
    if pair.job.state == "ready" {
        counts.ready = add(counts.ready)?;
    }
    Ok(counts)
}

pub(super) fn encode_event(event: &Event) -> Vec<u8> {
    json::stringify(&event.to_json()).into_bytes()
}

pub(super) fn decode_event(bytes: &[u8], job: &str, sequence: u64) -> Result<Event> {
    let value = json::parse(std::str::from_utf8(bytes).map_err(|_| "Invalid job history")?)?;
    let event = Event::from_json(&value)?;
    if event.job_id != job || event.id != sequence {
        return Err("Job history disagrees with its key".into());
    }
    Ok(event)
}
