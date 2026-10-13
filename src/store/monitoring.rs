//! Derived library scheduling data. The upgrade only writes secondary entries;
//! each resumable page and its counters/cursor share one inventory transaction.
use super::{
    Job, JobPage, Result, Store,
    layout::*,
    ops::{self, Db, Staged},
};
use crate::inventory::{Change, Index, Key, MAX_PAGE_ROWS, Query};

pub(super) const FORMAT: &[u8] = b"mynou-jobs/2";
pub(super) const PENDING: &[u8] = b"mynou-jobs/2-building";

fn class(job: &Job) -> usize {
    if !job.monitored {
        1
    } else if job.shared_file.is_some() {
        4
    } else if job.release.is_none() {
        2
    } else if !matches!(job.request.kind.as_str(), "movie" | "episode") {
        3
    } else {
        0
    }
}

pub(super) fn encode(counts: [u64; 5]) -> Vec<u8> {
    counts.into_iter().flat_map(u64::to_le_bytes).collect()
}

pub(super) fn counts(index: &mut Index) -> Result<[u64; 5]> {
    let bytes = index
        .get(&MONITOR_COUNTS_KEY)?
        .ok_or("Missing library monitoring counters")?;
    if bytes.len() != 40 {
        return Err("Invalid library monitoring counters".into());
    }
    let mut counts = [0; 5];
    for (slot, bytes) in counts.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *slot = u64::from_le_bytes(ops::array(bytes)?);
    }
    Ok(counts)
}

fn reference(id: &[u8; 16]) -> Key {
    key(&[&[T_MONITOR_REF], id])
}
fn due(at: u64, round: u64, id: &[u8; 16]) -> Key {
    key(&[
        &[T_MONITOR_DUE],
        &at.to_be_bytes(),
        &round.to_be_bytes(),
        id,
    ])
}
fn schedule(job: &Job, round: u64) -> Result<Key> {
    let id = id_bytes(&job.id)?;
    if job.monitor_checked_at == 0 {
        Ok(key(&[
            &[T_MONITOR_UNCHECKED],
            &job.created_at.to_be_bytes(),
            &round.to_be_bytes(),
            &id,
        ]))
    } else {
        Ok(due(job.monitor_checked_at, round, &id))
    }
}
fn shared(id: &[u8; 16]) -> Key {
    key(&[&[T_MONITOR_SHARED], id])
}

fn window(
    index: &mut Index,
    start: Key,
    end: Option<Key>,
    limit: usize,
) -> Result<Vec<(Key, Vec<u8>)>> {
    let mut rows = Vec::new();
    let mut after = None;
    while rows.len() < limit {
        let page = index.page(&Query {
            start,
            end,
            after,
            limit: (limit - rows.len()).min(MAX_PAGE_ROWS),
            sequence: None,
        })?;
        rows.extend(page.entries);
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    Ok(rows)
}

fn decode(bytes: &[u8]) -> Result<(usize, u64, u64)> {
    if bytes.len() != 17 || bytes[0] > 4 {
        return Err("Invalid library monitoring reference".into());
    }
    Ok((
        bytes[0] as usize,
        u64::from_be_bytes(ops::array(&bytes[1..9])?),
        u64::from_be_bytes(ops::array(&bytes[9..])?),
    ))
}

fn entries(job: &Job, round: u64) -> Result<Vec<Change>> {
    let id = id_bytes(&job.id)?;
    let class = class(job);
    let mut value = vec![class as u8];
    value.extend_from_slice(&job.monitor_checked_at.to_be_bytes());
    value.extend_from_slice(&round.to_be_bytes());
    let mut changes = vec![Change::PutInline(reference(&id), value)];
    if class == 0 {
        changes.push(Change::PutInline(schedule(job, round)?, Vec::new()));
    }
    if class == 4 {
        changes.push(Change::PutInline(shared(&id), Vec::new()));
    }
    Ok(changes)
}

/// Called only after primary format/provenance checks and journal settlement.
pub(super) fn upgrade(index: &mut Index, read_only: bool) -> Result<()> {
    let format = index.get(&FORMAT_KEY)?.ok_or("Missing job index format")?;
    if format == FORMAT || format == super::children::FORMAT || format == super::children::PENDING {
        return verify_counts(index);
    }
    if format != super::layout::FORMAT && format != PENDING {
        return Err("Unsupported job monitoring format".into());
    }
    if read_only {
        return Err(
            "Library scheduling index needs a writable restart to complete its upgrade".into(),
        );
    }
    if format != PENDING {
        index.commit(vec![
            Change::PutInline(FORMAT_KEY, PENDING.to_vec()),
            Change::Put(MONITOR_COUNTS_KEY, encode([0; 5])),
            Change::Delete(MONITOR_CURSOR_KEY),
        ])?;
    }
    let mut cursor = index
        .get(&MONITOR_CURSOR_KEY)?
        .map(|bytes| ops::array::<64>(&bytes))
        .transpose()?;
    if let Some(cursor) = &cursor {
        let (start, end) = prefix_range(&[T_LIBRARY]);
        if *cursor < start
            || end.is_some_and(|end| *cursor >= end)
            || cursor[25..].iter().any(|byte| *byte != 0)
        {
            return Err("Invalid library scheduling upgrade cursor".into());
        }
    }
    let mut totals = counts(index)?;
    loop {
        let page = Db(index).scan(&[T_LIBRARY], cursor, MAX_PAGE_ROWS)?;
        if page.entries.is_empty() {
            break;
        }
        let mut changes = Vec::new();
        for (key, value) in &page.entries {
            if !value.is_empty() || key[25..].iter().any(|byte| *byte != 0) {
                return Err("Invalid library entry during scheduling upgrade".into());
            }
            let id = ops::key_part(key, 9)?;
            let job = Db(index).require(&id)?;
            let created = !u64::from_be_bytes(ops::key_part(key, 1)?);
            if job.created_at != created || !imported(&job) || !Db(index).is_tip(&job.id)? {
                return Err(
                    "Library scheduling upgrade found an inconsistent library entry".into(),
                );
            }
            totals[class(&job)] = totals[class(&job)]
                .checked_add(1)
                .ok_or("Library monitoring count overflow")?;
            changes.extend(entries(&job, 0)?);
            cursor = Some(*key);
        }
        changes.push(Change::Put(MONITOR_COUNTS_KEY, encode(totals)));
        changes.push(Change::Put(
            MONITOR_CURSOR_KEY,
            cursor.ok_or("Missing scheduling cursor")?.to_vec(),
        ));
        index.commit(changes)?;
        if page.next.is_none() {
            break;
        }
    }
    verify_counts(index)?;
    index.commit(vec![
        Change::PutInline(FORMAT_KEY, FORMAT.to_vec()),
        Change::Delete(MONITOR_CURSOR_KEY),
    ])?;
    Ok(())
}

fn verify_counts(index: &mut Index) -> Result<()> {
    let total = counts(index)?.into_iter().try_fold(0_u64, |total, n| {
        total
            .checked_add(n)
            .ok_or("Library monitoring count overflow")
    })?;
    if total != Db(index).counts()?.library {
        return Err("Library monitoring counts disagree with the library".into());
    }
    Ok(())
}

/// Replace the old tip's scheduling proof with the authoritative new tip.
pub(super) fn stage(
    db: &mut Db<'_>,
    staged: &mut Staged,
    totals: &mut [u64; 5],
    before: Option<[u8; 16]>,
    next: Option<&Job>,
    rotate: bool,
) -> Result<()> {
    let previous = before
        .map(|id| -> Result<_> {
            let bytes =
                db.0.get(&reference(&id))?
                    .ok_or("Missing library monitoring reference")?;
            Ok((id, decode(&bytes)?))
        })
        .transpose()?;
    let next_id = next.map(|job| id_bytes(&job.id)).transpose()?;
    let mut round =
        db.0.sequence()?
            .checked_add(1)
            .ok_or("Library monitoring round overflow")?;
    if let Some((id, (category, at, previous_round))) = previous {
        totals[category] = totals[category]
            .checked_sub(1)
            .ok_or("Library monitoring count underflow")?;
        staged.insert(reference(&id), None);
        if category == 0 {
            let job = db.require(&id)?;
            if class(&job) != 0 || job.monitor_checked_at != at {
                return Err("Inconsistent previous library scheduling reference".into());
            }
            let key = schedule(&job, previous_round)?;
            if db.0.get(&key)?.as_deref() != Some(&[][..]) {
                return Err("Missing previous library scheduling entry".into());
            }
            staged.insert(key, None);
        }
        if category == 4 {
            staged.insert(shared(&id), None);
        }
        if !rotate && next_id == Some(id) && next.is_some_and(|job| job.monitor_checked_at == at) {
            round = previous_round;
        }
    }
    if let Some(job) = next {
        totals[class(job)] = totals[class(job)]
            .checked_add(1)
            .ok_or("Library monitoring count overflow")?;
        for change in entries(job, round)? {
            match change {
                Change::PutInline(key, bytes) => {
                    staged.insert(key, Some(Entry::Inline(bytes)));
                }
                _ => return Err("Invalid scheduling change".into()),
            }
        }
    }
    Ok(())
}

impl Store {
    pub(crate) fn monitoring_counts(&mut self) -> Result<[u64; 5]> {
        self.read([0; 5], |db| counts(db.0))
    }

    /// An oldest-check window; at most `limit` jobs and the usual byte budget.
    /// A separate future-time range preserves clock-rollback checks.
    pub(crate) fn monitoring_due(
        &mut self,
        at: u64,
        interval: u64,
        force: bool,
        limit: usize,
    ) -> Result<JobPage> {
        if !(1..=257).contains(&limit) {
            return Err("Invalid monitoring window".into());
        }
        self.read(
            JobPage {
                jobs: Vec::new(),
                next: None,
                total: 0,
                sequence: 0,
            },
            |db| {
                let (start, end) = prefix_range(&[T_MONITOR_DUE]);
                // Future check times are serviced first so correcting one
                // cannot hold back the remaining rollback work on forced passes.
                let mut rows = if at < u64::MAX {
                    window(
                        db.0,
                        key(&[&[T_MONITOR_DUE], &(at + 1).to_be_bytes()]),
                        end,
                        limit,
                    )?
                } else {
                    Vec::new()
                };
                let (unchecked_start, unchecked_end) = prefix_range(&[T_MONITOR_UNCHECKED]);
                if rows.len() < limit && at < u64::MAX {
                    rows.extend(window(
                        db.0,
                        key(&[&[T_MONITOR_UNCHECKED], &(at + 1).to_be_bytes()]),
                        unchecked_end,
                        limit - rows.len(),
                    )?);
                }
                let remaining = limit - rows.len();
                if remaining > 0 {
                    let cutoff = if force {
                        Some(at)
                    } else {
                        at.checked_sub(interval)
                    };
                    let mut normal = if let Some(cutoff) = cutoff {
                        let due_end = cutoff
                            .checked_add(1)
                            .map(|next| key(&[&[T_MONITOR_DUE], &next.to_be_bytes()]))
                            .or(end);
                        window(db.0, start, due_end, remaining)?
                    } else {
                        Vec::new()
                    };
                    let normal_unchecked_end = at
                        .checked_add(1)
                        .map(|next| key(&[&[T_MONITOR_UNCHECKED], &next.to_be_bytes()]))
                        .or(unchecked_end);
                    normal.extend(window(
                        db.0,
                        unchecked_start,
                        normal_unchecked_end,
                        remaining,
                    )?);
                    normal.sort_by_key(|(key, _)| {
                        let mut time = [0; 8];
                        time.copy_from_slice(&key[1..9]);
                        let time = u64::from_be_bytes(time);
                        let priority = if key[0] == T_MONITOR_UNCHECKED && !force {
                            time.saturating_sub(interval)
                        } else {
                            time
                        };
                        let mut order = [0; 24];
                        order.copy_from_slice(&key[9..33]);
                        (priority, order)
                    });
                    rows.extend(normal.into_iter().take(remaining));
                }
                let mut jobs = Vec::new();
                let mut bytes = 0_usize;
                let mut limited = false;
                for (found, value) in rows {
                    let id = ops::key_part(&found, 17)?;
                    let job = db.require(&id)?;
                    let binding =
                        db.0.get(&reference(&id))?
                            .ok_or("Missing monitoring reference")?;
                    let (category, checked, round) = decode(&binding)?;
                    if !value.is_empty()
                        || category != 0
                        || class(&job) != 0
                        || checked != job.monitor_checked_at
                        || found != schedule(&job, round)?
                        || !db.is_tip(&job.id)?
                    {
                        return Err("Inconsistent library monitoring entry".into());
                    }
                    let length = encode_job(&job).len();
                    if !jobs.is_empty() && bytes + length > crate::inventory::MAX_PAGE_BYTES {
                        limited = true;
                        break;
                    }
                    bytes += length;
                    jobs.push(job);
                }
                Ok(JobPage {
                    next: limited.then(|| "more".into()),
                    jobs,
                    total: counts(db.0)?[0],
                    sequence: db.0.sequence()?,
                })
            },
        )
    }

    pub(crate) fn monitored_shared(&mut self, limit: usize) -> Result<Vec<Job>> {
        self.read(Vec::new(), |db| {
            let rows = db.scan(&[T_MONITOR_SHARED], None, limit)?.entries;
            let mut jobs = Vec::new();
            let mut bytes = 0_usize;
            for (key, value) in rows {
                let id = ops::key_part(&key, 1)?;
                let job = db.require(&id)?;
                let binding =
                    db.0.get(&reference(&id))?
                        .ok_or("Missing shared monitoring reference")?;
                let (category, checked, _) = decode(&binding)?;
                if !value.is_empty()
                    || key != shared(&id)
                    || category != 4
                    || checked != job.monitor_checked_at
                    || class(&job) != 4
                    || !db.is_tip(&job.id)?
                {
                    return Err("Inconsistent shared library monitoring entry".into());
                }
                let length = encode_job(&job).len();
                if !jobs.is_empty() && bytes + length > crate::inventory::MAX_PAGE_BYTES {
                    break;
                }
                bytes += length;
                jobs.push(job);
            }
            Ok(jobs)
        })
    }
}
