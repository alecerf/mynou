//! Torrents added by hand: a magnet link, a torrent URL or the bytes of a
//! .torrent file. The release name is matched against the library with the
//! identity rules of automatic selection; a film or an episode takes the
//! torrent as its chosen release, a season pack fills the missing episodes of
//! a followed series, and anything else downloads as a plain transfer.
use crate::{
    Result,
    crypto::sha256,
    engine::{Engine, lock, public_job},
    integrations,
    json::Value,
    pack::season_title_matches,
    store::{RecordedRelease, Request},
    torrent::{TorrentMetadata, inspect_metadata, url_decode},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    time::{Duration, Instant},
};

/// Bounds the bytes of an uploaded .torrent file (the API body holds its Base64).
const MAX_TORRENT: usize = 768 * 1024;
const METADATA_BUDGET: Duration = Duration::from_secs(30);

pub enum ManualTorrent {
    /// A magnet link or an http(s) URL of a .torrent file.
    Link(String),
    /// The contents of a .torrent file.
    File(Vec<u8>),
}

impl ManualTorrent {
    pub fn from_json(value: &Value) -> Result<Self> {
        match (
            value.get("source").and_then(Value::as_str),
            value.get("torrent").and_then(Value::as_str),
        ) {
            (Some(source), None) => {
                let source = source.trim();
                if !(source.starts_with("magnet:?")
                    || source.starts_with("http://")
                    || source.starts_with("https://"))
                {
                    return Err("Paste a magnet link or the http(s) URL of a .torrent file".into());
                }
                Ok(Self::Link(integrations::acquisition_url(source, "")?))
            }
            (None, Some(torrent)) => Ok(Self::File(decode_base64(torrent)?)),
            _ => {
                Err("Give either a magnet link or URL (source) or a .torrent file (torrent)".into())
            }
        }
    }
}

/// What a release name refers to in the library.
enum Target {
    Media(Request),
    Season {
        series_id: String,
        label: String,
        season: u32,
    },
}

impl Engine {
    pub fn add_torrent(&self, torrent: &ManualTorrent) -> Result<Value> {
        if self.read_only {
            return Err("Read-only preview cannot add a torrent".into());
        }
        let client = self.native_client()?;
        let source = match torrent {
            ManualTorrent::Link(url) => url.clone(),
            ManualTorrent::File(bytes) => self.keep_torrent_file(bytes)?,
        };
        // A magnet names its release; anything else is read for its file names.
        let mut metadata = None;
        let name = match magnet_name(&source)? {
            Some(name) => Some(name),
            None => {
                let inspected = inspect_metadata(
                    &*self.torrent_network,
                    &source,
                    Instant::now() + METADATA_BUDGET,
                );
                let inspected = match (inspected, torrent) {
                    (Ok(inspected), _) => Some(inspected),
                    // A file or URL that cannot be read would not download either.
                    (Err(error), ManualTorrent::File(_)) => return Err(error),
                    (Err(error), _) if !source.starts_with("magnet:") => return Err(error),
                    (Err(_), _) => None,
                };
                let name = inspected.as_ref().and_then(release_name);
                metadata = inspected;
                name
            }
        };
        let mut result = Value::object();
        result.insert("name", name.clone().map_or(Value::Null, Value::String));
        let targets = match &name {
            Some(name) => self.manual_targets(name)?,
            None => Vec::new(),
        };
        let note = match targets.as_slice() {
            [Target::Media(request)] => {
                let job =
                    self.grab_manual(request, &source, name.as_deref().unwrap_or_default())?;
                result.insert("matched", request.kind.clone());
                result.insert("job", public_job(&job));
                return Ok(result);
            }
            [
                Target::Season {
                    series_id,
                    label,
                    season,
                },
            ] => {
                let metadata = match metadata {
                    Some(metadata) => metadata,
                    None => inspect_metadata(
                        &*self.torrent_network,
                        &source,
                        Instant::now() + METADATA_BUDGET,
                    )
                    .map_err(|_| {
                        "The season pack's file list could not be read from its peers; try again or add the .torrent file"
                    })?,
                };
                match self.submit_manual_pack(series_id, *season, &source, &metadata)? {
                    Some(pack) => {
                        result.insert("matched", "season");
                        result.insert("pack", pack);
                        return Ok(result);
                    }
                    None => format!(
                        "{label} season {season} has no missing episode left to fill from a pack"
                    ),
                }
            }
            [] => "Nothing in your library matches this release".into(),
            _ => "This release matches several titles of your library".into(),
        };
        let status = client.ensure(&source)?;
        result.insert("matched", Value::Null);
        result.insert("note", note);
        result.insert("transfer_id", status.id);
        Ok(result)
    }

    /// The uploaded file is kept for as long as its transfer: the native
    /// client and the worker read it again by its path.
    fn keep_torrent_file(&self, bytes: &[u8]) -> Result<String> {
        let directory = self.config.data_dir.join("state/manual");
        fs::create_dir_all(&directory)
            .map_err(|e| format!("Cannot create {}: {e}", directory.display()))?;
        let digest: String = sha256(bytes).iter().map(|b| format!("{b:02x}")).collect();
        let path = directory.join(format!("{digest}.torrent"));
        if !path.exists() {
            let partial = directory.join(format!("{digest}.partial"));
            fs::write(&partial, bytes)
                .and_then(|()| fs::rename(&partial, &path))
                .map_err(|e| format!("Cannot keep the torrent file: {e}"))?;
        }
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| "The data directory is not valid UTF-8".into())
    }

    /// Films and episodes the name identifies, then season packs. Each media
    /// identity counts once, whether it is followed, requested or both.
    fn manual_targets(&self, name: &str) -> Result<Vec<Target>> {
        let mut media = BTreeMap::new();
        let mut seasons = Vec::new();
        {
            let mut series = lock(&self.series_store)?;
            let mut after: Option<String> = None;
            loop {
                let page = series.page(after.as_deref(), 200)?;
                for summary in &page.series {
                    let Some(id) = summary.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(record) = series.get(id)? else {
                        continue;
                    };
                    let mut numbers = BTreeSet::new();
                    for episode in &record.plan.episodes {
                        numbers.insert(episode.season);
                        let request = record.episode_request(episode);
                        if integrations::release_identity_matches(&request, name) {
                            media.insert(request.media_key(), request);
                        }
                    }
                    for season in numbers {
                        let mut request = record.plan.request.clone();
                        request.season = season;
                        if season_title_matches(&request, name) {
                            seasons.push(Target::Season {
                                series_id: record.id.clone(),
                                label: record.plan.request.title.clone(),
                                season,
                            });
                        }
                    }
                }
                after = page.next;
                if after.is_none() {
                    break;
                }
            }
        }
        let mut after: Option<String> = None;
        loop {
            let page = lock(&self.store)?.page(after.as_deref(), 200)?;
            for job in page.jobs {
                let request = job.request;
                if matches!(request.kind.as_str(), "movie" | "episode")
                    && request.source_path.is_none()
                    && request.source_url.is_none()
                    && integrations::release_identity_matches(&request, name)
                {
                    media.entry(request.media_key()).or_insert(request);
                }
            }
            after = page.next;
            if after.is_none() {
                break;
            }
        }
        if media.is_empty() {
            return Ok(seasons);
        }
        Ok(media.into_values().map(Target::Media).collect())
    }

    /// As choosing a release from a search report does, with the torrent as
    /// that release.
    fn grab_manual(
        &self,
        request: &Request,
        source: &str,
        name: &str,
    ) -> Result<crate::store::Job> {
        let config = self.configuration_for_request(request)?;
        let (profile, _) = config.selection.profile(&request.kind)?;
        let job = {
            let _publication = self.torrent_publication()?;
            let mut store = lock(&self.store)?;
            let job = store.grab(
                request.clone(),
                source.into(),
                RecordedRelease {
                    title: name.into(),
                    profile: profile.into(),
                },
            )?;
            self.retire_previous_transfers(&mut store)?;
            job
        };
        self.requester_operator_interest(std::slice::from_ref(&job))?;
        Ok(job)
    }
}

/// The display name of a magnet, if it has one.
fn magnet_name(source: &str) -> Result<Option<String>> {
    let Some(query) = source.strip_prefix("magnet:?") else {
        return Ok(None);
    };
    for part in query.split('&') {
        if let Some(value) = part.strip_prefix("dn=") {
            let name = url_decode(value)?;
            if !name.trim().is_empty() {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

/// A torrent's name: its directory, or its only file.
fn release_name(metadata: &TorrentMetadata) -> Option<String> {
    let first = metadata.files.iter().find(|file| !file.padding)?;
    first.path.split('/').next().map(str::to_owned)
}

fn decode_base64(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    let text = text
        .strip_suffix("==")
        .or_else(|| text.strip_suffix('='))
        .unwrap_or(text);
    if text.is_empty() || text.len() > MAX_TORRENT.div_ceil(3) * 4 {
        return Err("The .torrent file is empty or larger than 768 KiB".into());
    }
    let mut bytes = Vec::with_capacity(text.len() / 4 * 3 + 2);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("The .torrent file is not valid Base64".into()),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnets_name_their_release_and_base64_round_trips() {
        assert_eq!(
            magnet_name("magnet:?xt=urn:btih:abc&dn=Dune.Part.Two.2024.2160p").unwrap(),
            Some("Dune.Part.Two.2024.2160p".into())
        );
        assert_eq!(magnet_name("magnet:?xt=urn:btih:abc").unwrap(), None);
        assert_eq!(magnet_name("https://example.test/a.torrent").unwrap(), None);
        for data in [&b"d4:infod"[..], b"x", b"xy", b"xyz"] {
            assert_eq!(decode_base64(&crate::ws::base64(data)).unwrap(), data);
        }
        assert!(decode_base64("not base64!").is_err());
    }
}
