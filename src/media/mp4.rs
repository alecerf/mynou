use super::*;

#[derive(Clone, Copy)]
struct BoxHeader {
    kind: [u8; 4],
    data: u64,
    end: u64,
}

fn boxes(input: &mut Input, start: u64, end: u64) -> Result<Vec<BoxHeader>> {
    let mut at = start;
    let mut result = Vec::new();
    while at < end {
        input.count()?;
        if end - at < 8 {
            return Err("En-tête MP4 tronqué".into());
        }
        let head = input.read(at, 8)?;
        let short_size = u32be(&head, 0)?;
        let mut header_size = 8;
        let size = match short_size {
            0 => end - at,
            1 => {
                if end - at < 16 {
                    return Err("Taille étendue MP4 tronquée".into());
                }
                header_size = 16;
                u64be(&input.read(at + 8, 8)?, 0)?
            }
            other => u64::from(other),
        };
        if head.get(4..8) == Some(b"uuid") {
            header_size += 16;
        }
        if size < header_size {
            return Err("Taille de boîte MP4 invalide".into());
        }
        let next = at.checked_add(size).ok_or("Taille MP4 débordante")?;
        if next > end {
            return Err("Boîte MP4 au-delà de son conteneur".into());
        }
        result.push(BoxHeader {
            kind: head[4..8].try_into().map_err(|_| "Type MP4 invalide")?,
            data: at + header_size,
            end: next,
        });
        at = next;
    }
    Ok(result)
}

pub(super) fn parse(input: &mut Input, media: &mut MediaFile) -> Result<()> {
    media.container = "mp4".into();
    let mut found_movie = false;
    let mut track_index = 0;
    for top in boxes(input, 0, input.len)? {
        match &top.kind {
            b"ftyp" => {
                if top.end - top.data < 8 {
                    return Err("Marque MP4 tronquée".into());
                }
                if input.read(top.data, 4)? == b"qt  " {
                    media.container = "mov".into();
                }
            }
            b"moov" => {
                found_movie = true;
                for movie in boxes(input, top.data, top.end)? {
                    match &movie.kind {
                        b"mvhd" => {
                            let data = input
                                .read(movie.data, (movie.end - movie.data).min(32) as usize)?;
                            let (scale, duration, creation) = timing(&data)?;
                            if scale > 0 {
                                media.duration_seconds =
                                    positive(Some(duration as f64 / f64::from(scale)));
                            }
                            if media.date.is_none() && creation > 0 {
                                let date = i64::try_from(creation)
                                    .ok()
                                    .and_then(|n| n.checked_sub(2_082_844_800))
                                    .and_then(utc_date);
                                if let Some(date) = date {
                                    set_date(media, date);
                                }
                            }
                        }
                        b"trak" => {
                            track(input, movie, media, track_index)?;
                            track_index += 1;
                        }
                        b"udta" | b"meta" => metadata(input, movie, media, 0)?,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !found_movie {
        return Err("Le MP4 ne contient pas de boîte moov ; les métadonnées fragmentées isolées ne sont pas prises en charge".into());
    }
    Ok(())
}

fn timing(data: &[u8]) -> Result<(u32, u64, u64)> {
    match data.first() {
        Some(0) => {
            let duration = u32be(data, 16)?;
            Ok((
                u32be(data, 12)?,
                if duration == u32::MAX {
                    0
                } else {
                    u64::from(duration)
                },
                u64::from(u32be(data, 4)?),
            ))
        }
        Some(1) => {
            let duration = u64be(data, 24)?;
            Ok((
                u32be(data, 20)?,
                if duration == u64::MAX { 0 } else { duration },
                u64be(data, 4)?,
            ))
        }
        _ => Err("Version de chronologie MP4 non prise en charge".into()),
    }
}

#[derive(Default)]
struct Track {
    kind: [u8; 4],
    codec: String,
    width: Option<u32>,
    height: Option<u32>,
    sample_rate: Option<u32>,
    channels: Option<u16>,
    language: Option<String>,
    timescale: u32,
    frames: u64,
    sample_duration: u64,
    default: bool,
}

fn track(input: &mut Input, header: BoxHeader, media: &mut MediaFile, index: usize) -> Result<()> {
    let mut parsed = Track::default();
    let mut mdia = None;
    for item in boxes(input, header.data, header.end)? {
        match &item.kind {
            b"tkhd" => {
                let data = input.read(item.data, (item.end - item.data).min(96) as usize)?;
                if data.len() < 4 {
                    return Err("En-tête de piste MP4 tronqué".into());
                }
                parsed.default = data[3] & 1 != 0;
            }
            b"mdia" => mdia = Some(item),
            _ => {}
        }
    }
    let Some(mdia) = mdia else {
        return Ok(());
    };
    let mut minf = None;
    for item in boxes(input, mdia.data, mdia.end)? {
        match &item.kind {
            b"hdlr" => {
                let data = input.read(item.data, (item.end - item.data).min(12) as usize)?;
                parsed.kind = data
                    .get(8..12)
                    .ok_or("Type de piste MP4 tronqué")?
                    .try_into()
                    .map_err(|_| "Type de piste MP4 invalide")?;
            }
            b"mdhd" => {
                let data = input.read(item.data, (item.end - item.data).min(36) as usize)?;
                (parsed.timescale, _, _) = timing(&data)?;
                let lang_at = if data[0] == 1 { 32 } else { 20 };
                let language = u16be(&data, lang_at)?;
                let bytes = [
                    ((language >> 10) & 31) as u8 + 0x60,
                    ((language >> 5) & 31) as u8 + 0x60,
                    (language & 31) as u8 + 0x60,
                ];
                if bytes.iter().all(u8::is_ascii_lowercase) && &bytes != b"und" {
                    parsed.language = Some(String::from_utf8_lossy(&bytes).into_owned());
                }
            }
            b"minf" => minf = Some(item),
            _ => {}
        }
    }
    if let Some(minf) = minf {
        for item in boxes(input, minf.data, minf.end)? {
            if &item.kind != b"stbl" {
                continue;
            }
            for table in boxes(input, item.data, item.end)? {
                match &table.kind {
                    b"stsd" => sample_description(input, table, &mut parsed)?,
                    b"stts" => {
                        let head =
                            input.read(table.data, (table.end - table.data).min(8) as usize)?;
                        let count = u32be(&head, 4)?;
                        let length = u64::from(count)
                            .checked_mul(8)
                            .and_then(|n| n.checked_add(8))
                            .ok_or("Table temporelle MP4 débordante")?;
                        if length != table.end - table.data || count as usize > MAX_ELEMENTS {
                            return Err("Table temporelle MP4 invalide ou excessive".into());
                        }
                        // Lecture par blocs : une table très longue ne devient pas une grosse allocation.
                        let mut cursor = table.data + 8;
                        for chunk in 0..count.div_ceil(512) {
                            let entries = (count - chunk * 512).min(512);
                            let data = input.read(cursor, entries as usize * 8)?;
                            for at in (0..data.len()).step_by(8) {
                                parsed.frames = parsed
                                    .frames
                                    .checked_add(u64::from(u32be(&data, at)?))
                                    .ok_or("Nombre d’images MP4 débordant")?;
                                let span = u64::from(u32be(&data, at)?)
                                    .checked_mul(u64::from(u32be(&data, at + 4)?))
                                    .ok_or("Durée de table MP4 débordante")?;
                                parsed.sample_duration = parsed
                                    .sample_duration
                                    .checked_add(span)
                                    .ok_or("Durée de table MP4 débordante")?;
                            }
                            cursor += data.len() as u64;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if parsed.codec.is_empty() {
        parsed.codec = "unknown".into();
    }
    if &parsed.kind == b"vide" {
        let frame_rate = if parsed.sample_duration > 0 && parsed.timescale > 0 && parsed.frames > 0
        {
            positive(Some(
                parsed.frames as f64 * f64::from(parsed.timescale) / parsed.sample_duration as f64,
            ))
        } else {
            None
        };
        media.video_streams.push(VideoStream {
            index,
            codec: parsed.codec,
            width: parsed.width,
            height: parsed.height,
            frame_rate,
            language: parsed.language,
            title: None,
            default: parsed.default,
        });
    } else if &parsed.kind == b"soun" {
        media.audio_streams.push(AudioStream {
            index,
            codec: parsed.codec,
            sample_rate: parsed.sample_rate,
            channels: parsed.channels,
            language: parsed.language,
            title: None,
            default: parsed.default,
        });
    }
    Ok(())
}

fn sample_description(input: &mut Input, table: BoxHeader, track: &mut Track) -> Result<()> {
    if table.end - table.data < 8 {
        return Err("Description de flux MP4 tronquée".into());
    }
    let head = input.read(table.data, 8)?;
    let count = u32be(&head, 4)? as usize;
    let entries = boxes(input, table.data + 8, table.end)?;
    if entries.len() != count {
        return Err("Nombre de descriptions MP4 invalide".into());
    }
    let Some(entry) = entries.first() else {
        return Ok(());
    };
    track.codec = codec(&entry.kind);
    if &track.kind == b"vide" {
        let data = input.read(entry.data, (entry.end - entry.data).min(78) as usize)?;
        track.width = Some(u32::from(u16be(&data, 24)?)).filter(|n| *n > 0);
        track.height = Some(u32::from(u16be(&data, 26)?)).filter(|n| *n > 0);
    } else if &track.kind == b"soun" {
        let data = input.read(entry.data, (entry.end - entry.data).min(64) as usize)?;
        let version = u16be(&data, 8)?;
        if version == 2 {
            let rate = f64::from_bits(u64be(&data, 32)?);
            if rate.is_finite() && rate >= 1.0 && rate <= f64::from(u32::MAX) {
                track.sample_rate = Some(rate.round() as u32);
            }
            track.channels = u16::try_from(u32be(&data, 40)?).ok().filter(|n| *n > 0);
        } else if version <= 1 {
            track.channels = Some(u16be(&data, 16)?).filter(|n| *n > 0);
            track.sample_rate = Some(u32be(&data, 24)? >> 16).filter(|n| *n > 0);
        } else {
            return Err("Version audio MOV non prise en charge".into());
        }
        let child_offset = match version {
            0 => 28,
            1 => 44,
            2 => 64,
            _ => 0,
        };
        if entry.end - entry.data > child_offset {
            for child in boxes(input, entry.data + child_offset, entry.end)? {
                if &child.kind == b"esds" {
                    let data = input.read(
                        child.data,
                        usize::try_from(child.end - child.data)
                            .map_err(|_| "Description MPEG-4 excessive")?,
                    )?;
                    if data.len() < 4 {
                        return Err("Description MPEG-4 tronquée".into());
                    }
                    descriptors(&data[4..], track, 0)?;
                }
            }
        }
    }
    Ok(())
}

fn descriptors(mut data: &[u8], track: &mut Track, depth: usize) -> Result<()> {
    if depth >= 8 {
        return Err("Descriptions MPEG-4 trop imbriquées".into());
    }
    while !data.is_empty() {
        let kind = data[0];
        let mut at = 1;
        let mut length = 0_usize;
        loop {
            if at > 4 {
                return Err("Longueur de description MPEG-4 invalide".into());
            }
            let byte = *data
                .get(at)
                .ok_or("Longueur de description MPEG-4 tronquée")?;
            length = (length << 7) | usize::from(byte & 0x7f);
            at += 1;
            if byte & 0x80 == 0 {
                break;
            }
        }
        let end = at
            .checked_add(length)
            .ok_or("Description MPEG-4 débordante")?;
        let body = data.get(at..end).ok_or("Description MPEG-4 hors limites")?;
        match kind {
            3 => {
                let flags = *body.get(2).ok_or("Descripteur ES tronqué")?;
                let mut skip = 3;
                if flags & 0x80 != 0 {
                    skip += 2;
                }
                if flags & 0x40 != 0 {
                    let size = *body.get(skip).ok_or("URL ES tronquée")? as usize;
                    skip += size + 1;
                }
                if flags & 0x20 != 0 {
                    skip += 2;
                }
                descriptors(
                    body.get(skip..).ok_or("Descripteur ES tronqué")?,
                    track,
                    depth + 1,
                )?;
            }
            4 => {
                let object_type = *body.first().ok_or("Configuration MPEG-4 vide")?;
                if matches!(object_type, 0x69 | 0x6b) {
                    track.codec = "mp3".into();
                } else if matches!(object_type, 0x40 | 0x66..=0x68) {
                    track.codec = "aac".into();
                }
                descriptors(
                    body.get(13..).ok_or("Configuration MPEG-4 tronquée")?,
                    track,
                    depth + 1,
                )?;
            }
            5 if track.codec == "aac" => audio_specific_config(body, track)?,
            _ => {}
        }
        data = &data[end..];
    }
    Ok(())
}

fn audio_specific_config(data: &[u8], track: &mut Track) -> Result<()> {
    let mut at = 0;
    let object = bits(data, &mut at, 5)?;
    if object == 31 {
        bits(data, &mut at, 6)?;
    }
    let frequency = bits(data, &mut at, 4)?;
    let rate = if frequency == 15 {
        bits(data, &mut at, 24)?
    } else {
        *[
            96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
            8_000, 7_350,
        ]
        .get(frequency as usize)
        .ok_or("Fréquence AAC réservée")?
    };
    let channel_config = bits(data, &mut at, 4)?;
    track.sample_rate = Some(rate).filter(|n| *n > 0);
    track.channels = match channel_config {
        0 => None,
        1..=6 => Some(channel_config as u16),
        7 => Some(8),
        _ => return Err("Configuration de canaux AAC réservée".into()),
    };
    if matches!(object, 5 | 29) {
        let extension_frequency = bits(data, &mut at, 4)?;
        let rate = if extension_frequency == 15 {
            bits(data, &mut at, 24)?
        } else {
            *[
                96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000,
                11_025, 8_000, 7_350,
            ]
            .get(extension_frequency as usize)
            .ok_or("Fréquence AAC réservée")?
        };
        track.sample_rate = Some(rate).filter(|n| *n > 0);
    }
    Ok(())
}
fn bits(data: &[u8], at: &mut usize, count: usize) -> Result<u32> {
    let end = at.checked_add(count).ok_or("Position AAC débordante")?;
    if end > data.len() * 8 {
        return Err("Configuration AAC tronquée".into());
    }
    let mut value = 0;
    while *at < end {
        value = (value << 1) | u32::from((data[*at / 8] >> (7 - *at % 8)) & 1);
        *at += 1;
    }
    Ok(value)
}

fn codec(kind: &[u8; 4]) -> String {
    match kind {
        b"avc1" | b"avc3" => "h264",
        b"hvc1" | b"hev1" => "hevc",
        b"av01" => "av1",
        b"vp09" => "vp9",
        b"vp08" => "vp8",
        b"mp4v" => "mpeg4",
        b"jpeg" | b"mjpg" => "mjpeg",
        b"mp4a" => "aac",
        b"ac-3" => "ac3",
        b"ec-3" => "eac3",
        b"Opus" => "opus",
        b"fLaC" => "flac",
        b"alac" => "alac",
        b".mp3" | b"mp3 " => "mp3",
        b"sowt" => "pcm_s16le",
        b"twos" => "pcm_s16be",
        b"lpcm" => "pcm",
        b"encv" | b"enca" => "encrypted",
        _ => return text(kind),
    }
    .into()
}

fn metadata(
    input: &mut Input,
    header: BoxHeader,
    media: &mut MediaFile,
    depth: usize,
) -> Result<()> {
    if depth >= 12 {
        return Err("Métadonnées MP4 trop imbriquées".into());
    }
    let start = if &header.kind == b"meta" {
        if header.end - header.data < 4 {
            return Err("Métadonnées MP4 tronquées".into());
        }
        // QuickTime historique peut omettre version/flags. La variante ISO est dominante.
        let data = input.read(header.data, 4)?;
        if data == [0, 0, 0, 0] {
            header.data + 4
        } else {
            header.data
        }
    } else {
        header.data
    };
    for item in boxes(input, start, header.end)? {
        match &item.kind {
            b"meta" | b"ilst" | b"udta" => metadata(input, item, media, depth + 1)?,
            b"\xa9nam" | b"\xa9day" => {
                for value in boxes(input, item.data, item.end)? {
                    if &value.kind != b"data" {
                        continue;
                    }
                    if value.end - value.data < 8 {
                        return Err("Valeur de métadonnée MP4 tronquée".into());
                    }
                    let data = input.read(value.data, (value.end - value.data) as usize)?;
                    let kind = u32be(&data, 0)? & 0x00ff_ffff;
                    let value = if kind == 2 {
                        let chars = data[8..]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|b| u16::from_be_bytes([b[0], b[1]]));
                        String::from_utf16_lossy(&chars.collect::<Vec<_>>())
                            .trim_matches('\0')
                            .to_owned()
                    } else {
                        text(&data[8..])
                    };
                    if &item.kind == b"\xa9nam" {
                        set_title(media, value);
                    } else {
                        set_date(media, value);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
