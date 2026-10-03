//! Analyse locale, sans décodeur ni programme externe. Les gros blocs de données
//! sont ignorés par déplacement dans le fichier ; seules les métadonnées sont lues.
mod avi;
mod ebml;
mod mp3;
mod mp4;
mod riff;

use crate::Result;
use crate::json::Value;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_METADATA: usize = 8 * 1024 * 1024;
const MAX_METADATA_TOTAL: usize = 64 * 1024 * 1024;
const MAX_ELEMENTS: usize = 100_000;

#[derive(Debug, Clone)]
pub struct MediaFile {
    pub path: String,
    pub filename: String,
    pub title: String,
    pub title_source: String,
    pub date: Option<String>,
    pub date_source: Option<String>,
    pub container: String,
    pub size_bytes: u64,
    pub duration_seconds: Option<f64>,
    pub bit_rate: Option<u64>,
    pub video_streams: Vec<VideoStream>,
    pub audio_streams: Vec<AudioStream>,
}

#[derive(Debug, Clone)]
pub struct VideoStream {
    pub index: usize,
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

#[derive(Debug, Clone)]
pub struct AudioStream {
    pub index: usize,
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

pub fn analyze(path: &Path) -> Result<MediaFile> {
    let mut input = Input::open(path)?;
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "Le nom du fichier média doit être un texte UTF-8 valide".to_string())?
        .to_owned();
    let title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&filename)
        .replace(['_', '.'], " ");
    let mut media = MediaFile {
        path: path.to_string_lossy().into_owned(),
        filename,
        title,
        title_source: "filename".into(),
        date: None,
        date_source: None,
        container: String::new(),
        size_bytes: input.len,
        duration_seconds: None,
        bit_rate: None,
        video_streams: Vec::new(),
        audio_streams: Vec::new(),
    };
    let prefix = input.read(0, input.len.min(16) as usize)?;
    if prefix.starts_with(b"\x1a\x45\xdf\xa3") {
        ebml::parse(&mut input, &mut media)?;
    } else if prefix.starts_with(b"RIFF") || prefix.starts_with(b"RF64") {
        if prefix.get(8..12) == Some(b"WAVE") {
            riff::wave(&mut input, &mut media)?;
        } else if prefix.get(8..12) == Some(b"AVI ") {
            avi::parse(&mut input, &mut media)?;
        } else {
            return Err("Conteneur RIFF non pris en charge".into());
        }
    } else if prefix.starts_with(b"fLaC") {
        riff::flac(&mut input, &mut media)?;
    } else if prefix.starts_with(b"ID3") || mp3::is_header(&prefix) {
        mp3::parse(&mut input, &mut media)?;
    } else if prefix.get(4..8).is_some_and(|kind| {
        matches!(
            kind,
            b"ftyp" | b"moov" | b"mdat" | b"free" | b"wide" | b"skip"
        )
    }) {
        mp4::parse(&mut input, &mut media)?;
    } else {
        return Err("Format média non pris en charge : MP4/MOV, Matroska/WebM, WAV, FLAC, MP3 ou AVI attendu".into());
    }
    if media.video_streams.is_empty() && media.audio_streams.is_empty() {
        return Err("Le conteneur ne contient aucun flux audio ou vidéo analysable".into());
    }
    media.duration_seconds = positive(media.duration_seconds);
    if let Some(duration) = media.duration_seconds
        && media.bit_rate.is_none()
    {
        let rate = media.size_bytes as f64 * 8.0 / duration;
        if rate.is_finite() && rate > 0.0 && rate < u64::MAX as f64 {
            media.bit_rate = Some(rate.round() as u64);
        }
    }
    Ok(media)
}

impl MediaFile {
    pub fn to_json(&self) -> Value {
        object([
            ("path", string(&self.path)),
            ("filename", string(&self.filename)),
            ("title", string(&self.title)),
            ("title_source", string(&self.title_source)),
            ("date", optional_string(&self.date)),
            ("date_source", optional_string(&self.date_source)),
            ("container", string(&self.container)),
            ("size_bytes", number(self.size_bytes as f64)),
            ("duration_seconds", optional_number(self.duration_seconds)),
            ("bit_rate", optional_number(self.bit_rate.map(|n| n as f64))),
            (
                "video_streams",
                Value::Array(
                    self.video_streams
                        .iter()
                        .map(|stream| {
                            object([
                                ("index", number(stream.index as f64)),
                                ("codec", string(&stream.codec)),
                                ("width", optional_number(stream.width.map(f64::from))),
                                ("height", optional_number(stream.height.map(f64::from))),
                                ("frame_rate", optional_number(stream.frame_rate)),
                                ("language", optional_string(&stream.language)),
                                ("title", optional_string(&stream.title)),
                                ("default", Value::Bool(stream.default)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "audio_streams",
                Value::Array(
                    self.audio_streams
                        .iter()
                        .map(|stream| {
                            object([
                                ("index", number(stream.index as f64)),
                                ("codec", string(&stream.codec)),
                                (
                                    "sample_rate",
                                    optional_number(stream.sample_rate.map(f64::from)),
                                ),
                                ("channels", optional_number(stream.channels.map(f64::from))),
                                ("language", optional_string(&stream.language)),
                                ("title", optional_string(&stream.title)),
                                ("default", Value::Bool(stream.default)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }
}

fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}
fn string(value: &str) -> Value {
    Value::String(value.into())
}
fn number(value: f64) -> Value {
    Value::Number(value)
}
fn optional_string(value: &Option<String>) -> Value {
    value.as_deref().map(string).unwrap_or(Value::Null)
}
fn optional_number(value: Option<f64>) -> Value {
    value.map(number).unwrap_or(Value::Null)
}
fn positive(value: Option<f64>) -> Option<f64> {
    value.filter(|n| n.is_finite() && *n > 0.0)
}

struct Input {
    file: BufReader<File>,
    len: u64,
    elements: usize,
    position: u64,
    metadata_bytes: usize,
}
impl Input {
    fn open(path: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(0o400000 | 0o4000); // O_NOFOLLOW | O_NONBLOCK.
        }
        let file = options
            .open(path)
            .map_err(|e| format!("Impossible d’ouvrir le média : {e}"))?;
        let meta = file
            .metadata()
            .map_err(|e| format!("Métadonnées du fichier : {e}"))?;
        if !meta.is_file() {
            return Err("Le média doit être un fichier ordinaire".into());
        }
        Ok(Self {
            file: BufReader::with_capacity(16 * 1024, file),
            len: meta.len(),
            elements: 0,
            position: 0,
            metadata_bytes: 0,
        })
    }
    fn read(&mut self, offset: u64, size: usize) -> Result<Vec<u8>> {
        if size > MAX_METADATA {
            return Err("Bloc de métadonnées trop volumineux".into());
        }
        self.metadata_bytes = self
            .metadata_bytes
            .checked_add(size)
            .ok_or("Volume de métadonnées débordant")?;
        if self.metadata_bytes > MAX_METADATA_TOTAL {
            return Err("Volume total des métadonnées excessif".into());
        }
        let end = offset
            .checked_add(size as u64)
            .ok_or("Débordement de position média")?;
        if end > self.len {
            return Err("Fichier média tronqué".into());
        }
        let relative = i128::from(offset) - i128::from(self.position);
        if let Ok(relative) = i64::try_from(relative) {
            self.file
                .seek_relative(relative)
                .map_err(|e| format!("Déplacement média : {e}"))?;
        } else {
            self.file
                .seek(SeekFrom::Start(offset))
                .map_err(|e| format!("Déplacement média : {e}"))?;
        }
        let mut data = vec![0; size];
        self.file
            .read_exact(&mut data)
            .map_err(|e| format!("Lecture média : {e}"))?;
        self.position = end;
        Ok(data)
    }
    fn count(&mut self) -> Result<()> {
        self.elements += 1;
        if self.elements > MAX_ELEMENTS {
            return Err("Trop d’éléments dans le conteneur média".into());
        }
        Ok(())
    }
}

fn u16be(data: &[u8], at: usize) -> Result<u16> {
    let bytes = data.get(at..at + 2).ok_or("Métadonnées tronquées")?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}
fn u32be(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data.get(at..at + 4).ok_or("Métadonnées tronquées")?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
fn u64be(data: &[u8], at: usize) -> Result<u64> {
    let bytes: [u8; 8] = data
        .get(at..at + 8)
        .ok_or("Métadonnées tronquées")?
        .try_into()
        .map_err(|_| "Métadonnées tronquées")?;
    Ok(u64::from_be_bytes(bytes))
}
fn u16le(data: &[u8], at: usize) -> Result<u16> {
    let bytes = data.get(at..at + 2).ok_or("Métadonnées tronquées")?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}
fn u32le(data: &[u8], at: usize) -> Result<u32> {
    let bytes = data.get(at..at + 4).ok_or("Métadonnées tronquées")?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
fn u64le(data: &[u8], at: usize) -> Result<u64> {
    let bytes: [u8; 8] = data
        .get(at..at + 8)
        .ok_or("Métadonnées tronquées")?
        .try_into()
        .map_err(|_| "Métadonnées tronquées")?;
    Ok(u64::from_le_bytes(bytes))
}
fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .trim_matches(['\0', ' ', '\r', '\n'])
        .to_string()
}
fn set_title(media: &mut MediaFile, title: String) {
    if !title.is_empty() {
        media.title = title;
        media.title_source = "metadata".into();
    }
}
fn set_date(media: &mut MediaFile, date: String) {
    if !date.is_empty() {
        media.date = Some(date);
        media.date_source = Some("metadata".into());
    }
}

// Algorithme calendaire arithmétique ; aucun appel à la timezone du système.
fn utc_date(unix_seconds: i64) -> Option<String> {
    let z = unix_seconds.div_euclid(86_400).checked_add(719_468)?;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}
