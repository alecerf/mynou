//! Explicit source labels, independent of immutable library episode numbers.
use crate::{Result, json::Value, selection::tokens};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EpisodeNumber {
    pub season: u32,
    pub episode: u32,
}

impl EpisodeNumber {
    pub fn validate(self) -> Result<()> {
        if self.season > 9999 || self.episode == 0 || self.episode > 99999 {
            return Err("Episode numbering requires season 0–9999 and episode 1–99999".into());
        }
        Ok(())
    }
    pub fn to_json(self) -> Value {
        let mut value = Value::object();
        value.insert("season", self.season);
        value.insert("episode", self.episode);
        value
    }
    pub fn from_json(value: &Value) -> Result<Self> {
        only(value, &["season", "episode"])?;
        let result = Self {
            season: number(value, "season")?,
            episode: number(value, "episode")?,
        };
        result.validate()?;
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceNumber {
    SeasonEpisode(EpisodeNumber),
    Absolute(u32),
}

impl SourceNumber {
    pub fn validate(self) -> Result<()> {
        match self {
            Self::SeasonEpisode(number) => number.validate(),
            Self::Absolute(number) if (1..=99999).contains(&number) => Ok(()),
            Self::Absolute(_) => Err("Absolute numbering requires episode 1–99999".into()),
        }
    }
    pub fn to_json(self) -> Value {
        match self {
            Self::SeasonEpisode(number) => number.to_json(),
            Self::Absolute(number) => {
                let mut value = Value::object();
                value.insert("absolute", number);
                value
            }
        }
    }
    pub fn from_json(value: &Value) -> Result<Self> {
        let result = if value.get("absolute").is_some() {
            only(value, &["absolute"])?;
            Self::Absolute(number(value, "absolute")?)
        } else {
            Self::SeasonEpisode(EpisodeNumber::from_json(value)?)
        };
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn matches(self, labels: &[String], at: usize) -> bool {
        match self {
            Self::SeasonEpisode(number) => labels
                .get(at)
                .is_some_and(|token| episode_marker(token) == Some(number)),
            Self::Absolute(number) => number_label(labels, at) == Some(number),
        }
    }
    pub(crate) fn matches_file(self, path: &Path, year: u32, title: &str) -> bool {
        let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
            return false;
        };
        let labels = tokens(stem);
        let title = tokens(title);
        let labels = if labels.starts_with(&title) {
            &labels[title.len()..]
        } else {
            &labels[..]
        };
        let labels =
            if year != 0 && labels.first().and_then(|label| decimal_token(label)) == Some(year) {
                &labels[1..]
            } else {
                labels
            };
        (0..labels.len())
            .filter(|&at| self.matches(labels, at))
            .count()
            == 1
            && !(0..labels.len()).any(|at| conflicting_marker(self, labels, at, 0))
    }
}

pub(crate) fn conflicting_marker(
    expected: SourceNumber,
    labels: &[String],
    at: usize,
    year: u32,
) -> bool {
    let token = &labels[at];
    if let Some(number) = episode_marker(token) {
        return expected != SourceNumber::SeasonEpisode(number);
    }
    if token
        .strip_prefix('e')
        .or_else(|| token.strip_prefix("ep"))
        .and_then(decimal_token)
        .is_some()
    {
        return true;
    }
    // Malformed or chained explicit labels are never treated as an ordinary suffix.
    if token.starts_with('s')
        && token.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
        && token.contains('e')
    {
        return true;
    }
    if let SourceNumber::Absolute(number) = expected
        && let Some(other) = number_label(labels, at)
    {
        return other != number && other != year;
    }
    false
}

/// Decimal parts of video codecs and audio channel counts are not episode labels.
pub(crate) fn number_label(labels: &[String], at: usize) -> Option<u32> {
    let token = labels.get(at)?;
    let number = decimal_token(token)?;
    let before = &labels[..at];
    let Some(previous) = before.last() else {
        return Some(number);
    };
    if matches!(token.as_str(), "264" | "265") && matches!(previous.as_str(), "h" | "x") {
        return None;
    }
    let audio_codec = |token: &str| {
        [
            "aac", "ac3", "eac3", "ddp", "dd", "dts", "truehd", "atmos", "opus", "flac", "pcm",
            "lpcm",
        ]
        .iter()
        .any(|codec| {
            token
                .strip_prefix(codec)
                .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
        })
    };
    if token.len() == 1
        && (audio_codec(previous)
            || previous.len() == 1
                && decimal_token(previous).is_some()
                && before.len() >= 2
                && audio_codec(&before[before.len() - 2]))
    {
        return None;
    }
    Some(number)
}

pub(crate) fn decimal_token(text: &str) -> Option<u32> {
    (!text.is_empty() && text.len() <= 5 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

pub(crate) fn episode_marker(text: &str) -> Option<EpisodeNumber> {
    let (season, episode) = if let Some(rest) = text.strip_prefix('s') {
        rest.split_once('e')?
    } else {
        let pair = text.split_once('x')?;
        if decimal_token(pair.0)? >= 320 {
            return None;
        }
        pair
    };
    let number = EpisodeNumber {
        season: decimal_token(season)?,
        episode: decimal_token(episode)?,
    };
    number.validate().ok()?;
    Some(number)
}

pub(crate) fn only(value: &Value, allowed: &[&str]) -> Result<()> {
    let fields = value.as_object().ok_or("Numbering must be an object")?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unknown numbering field".into());
    }
    Ok(())
}
fn number(value: &Value, key: &str) -> Result<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("Invalid numbering integer: {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_files_require_one_exact_label_and_reject_multi_episode_markers() {
        let number = SourceNumber::SeasonEpisode(EpisodeNumber {
            season: 2,
            episode: 3,
        });
        for name in ["Show.S02E03.1080p.mp4", "2x03.mkv"] {
            assert!(number.matches_file(Path::new(name), 2024, "Show"), "{name}");
        }
        for name in [
            "S02E030.mp4",
            "S02E03E04.mp4",
            "S02E03-E04.mkv",
            "S02E03.S02E04.mp4",
            "S02E03-S02E03.mp4",
            "03.mp4",
        ] {
            assert!(
                !number.matches_file(Path::new(name), 2024, "Show"),
                "{name}"
            );
        }
    }

    #[test]
    fn explicit_absolute_files_reject_ranges_other_numbers_and_numbered_labels() {
        let number = SourceNumber::Absolute(13);
        assert!(number.matches_file(Path::new("The.100.013.1080p.mkv"), 2024, "The 100"));
        for name in [
            "013.mp4",
            "Show.2024.013.1080p.mkv",
            "Show.013.1080p.WEB.H.264.AAC2.0-GRP.mkv",
            "Show.013.1080p.DDP5.1.H.265.mkv",
            "Show.013.1080p.X.265.DD+7.1.mkv",
            "Show.013.1080p.AAC.2.0.mkv",
        ] {
            assert!(number.matches_file(Path::new(name), 2024, "Show"), "{name}");
        }
        for name in [
            "013-014.mp4",
            "013-S01E13.mp4",
            "013-E14.mp4",
            "013.013.mp4",
            "0013-2200.mp4",
            "Show.013.H.264.AAC2.0.014.mkv",
            "Show.013.H.264.AAC2.0.013.mkv",
            "Show.013.AAC.2.0.1.mkv",
            "Show.013.H.263.mkv",
        ] {
            assert!(
                !number.matches_file(Path::new(name), 2024, "Show"),
                "{name}"
            );
        }
        // Codec and channel numbers must not supply a missing episode label.
        for (episode, name) in [
            (264, "Show.H.264.mkv"),
            (265, "Show.X.265.mkv"),
            (1, "Show.DDP5.1.mkv"),
            (2, "Show.AAC.2.0.mkv"),
        ] {
            assert!(
                !SourceNumber::Absolute(episode).matches_file(Path::new(name), 2024, "Show"),
                "{name}"
            );
        }
    }
}
