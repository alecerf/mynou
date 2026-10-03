use super::*;

pub(super) fn wave(input: &mut Input, media: &mut MediaFile) -> Result<()> {
    if input.len < 12 {
        return Err("En-tête WAV tronqué".into());
    }
    media.container = "wav".into();
    let head = input.read(0, 12)?;
    let rf64 = head.starts_with(b"RF64");
    let riff_length = u64::from(u32le(&head, 4)?);
    let mut end = if rf64 {
        input.len
    } else {
        riff_length.checked_add(8).ok_or("Taille WAV débordante")?
    };
    if end > input.len || end < 12 {
        return Err("Taille du conteneur WAV invalide".into());
    }
    let mut data_size = 0_u64;
    let mut rf64_data = None;
    let mut byte_rate = 0;
    let mut stream = None;
    let mut at = 12;
    while at < end {
        input.count()?;
        if end - at < 8 {
            return Err("Bloc WAV tronqué".into());
        }
        let chunk = input.read(at, 8)?;
        let advertised = u32le(&chunk, 4)?;
        let size = if rf64 && &chunk[0..4] == b"data" && advertised == u32::MAX {
            rf64_data.ok_or("Le WAV RF64 doit fournir ds64 avant data")?
        } else {
            u64::from(advertised)
        };
        let body = at + 8;
        let next = body.checked_add(size).ok_or("Bloc WAV débordant")?;
        if next > end {
            return Err("Bloc WAV au-delà du conteneur".into());
        }
        match &chunk[0..4] {
            b"ds64" if rf64 => {
                if size < 28 {
                    return Err("Bloc RF64 ds64 tronqué".into());
                }
                let data = input.read(body, 28)?;
                let declared_end = u64le(&data, 0)?
                    .checked_add(8)
                    .ok_or("Taille RF64 débordante")?;
                if declared_end > input.len || declared_end < next {
                    return Err("Taille RF64 invalide".into());
                }
                end = declared_end;
                rf64_data = Some(u64le(&data, 8)?);
            }
            b"fmt " => {
                if size < 16 {
                    return Err("Description audio WAV tronquée".into());
                }
                let data = input.read(body, size.min(64) as usize)?;
                let mut format = u16le(&data, 0)?;
                let channels = u16le(&data, 2)?;
                let sample_rate = u32le(&data, 4)?;
                byte_rate = u32le(&data, 8)?;
                let bits = u16le(&data, 14)?;
                if format == 0xfffe {
                    if size < 40 || u16le(&data, 16)? < 22 {
                        return Err("Format WAV extensible tronqué".into());
                    }
                    format = u16le(&data, 24)?;
                }
                if channels == 0 || sample_rate == 0 || byte_rate == 0 {
                    return Err("Paramètres audio WAV invalides".into());
                }
                let codec = match format {
                    1 => match bits {
                        8 => "pcm_u8".into(),
                        16 => "pcm_s16le".into(),
                        24 => "pcm_s24le".into(),
                        32 => "pcm_s32le".into(),
                        _ => format!("pcm_{bits}bit"),
                    },
                    3 => format!("pcm_f{bits}le"),
                    2 => "adpcm_ms".into(),
                    6 => "pcm_alaw".into(),
                    7 => "pcm_mulaw".into(),
                    0x11 => "adpcm_ima_wav".into(),
                    0x55 => "mp3".into(),
                    0xff => "aac".into(),
                    _ => format!("wav_0x{format:04x}"),
                };
                stream = Some(AudioStream {
                    index: 0,
                    codec,
                    sample_rate: Some(sample_rate),
                    channels: Some(channels),
                    language: None,
                    title: None,
                    default: true,
                });
            }
            b"data" => {
                data_size = data_size
                    .checked_add(size)
                    .ok_or("Taille de données WAV débordante")?
            }
            b"LIST" if size >= 4 && input.read(body, 4)? == b"INFO" => {
                wave_info(input, body + 4, next, media)?;
            }
            _ => {}
        }
        at = next
            .checked_add(size & 1)
            .ok_or("Alignement WAV débordant")?;
        if at > end {
            return Err("Octet d’alignement WAV manquant".into());
        }
    }
    if let Some(stream) = stream {
        media.audio_streams.push(stream);
    } else {
        return Err("Bloc fmt WAV manquant".into());
    }
    if byte_rate > 0 && data_size > 0 {
        media.duration_seconds = Some(data_size as f64 / f64::from(byte_rate));
    }
    media.bit_rate = Some(u64::from(byte_rate) * 8);
    Ok(())
}

fn wave_info(input: &mut Input, mut at: u64, end: u64, media: &mut MediaFile) -> Result<()> {
    while at < end {
        input.count()?;
        if end - at < 8 {
            return Err("Métadonnées WAV tronquées".into());
        }
        let head = input.read(at, 8)?;
        let size = u64::from(u32le(&head, 4)?);
        let body = at + 8;
        let next = body
            .checked_add(size)
            .ok_or("Métadonnées WAV débordantes")?;
        if next > end {
            return Err("Métadonnées WAV hors limites".into());
        }
        if matches!(&head[0..4], b"INAM" | b"ICRD") {
            let value = text(&input.read(
                body,
                usize::try_from(size).map_err(|_| "Métadonnées WAV excessives")?,
            )?);
            if &head[0..4] == b"INAM" {
                set_title(media, value);
            } else {
                set_date(media, value);
            }
        }
        at = next
            .checked_add(size & 1)
            .ok_or("Alignement WAV débordant")?;
        if at > end {
            return Err("Alignement de métadonnée WAV tronqué".into());
        }
    }
    Ok(())
}

pub(super) fn flac(input: &mut Input, media: &mut MediaFile) -> Result<()> {
    media.container = "flac".into();
    let mut at = 4;
    let mut first = true;
    let mut saw_stream_info = false;
    loop {
        input.count()?;
        let head = input.read(at, 4)?;
        let kind = head[0] & 0x7f;
        let last = head[0] & 0x80 != 0;
        let size = u32::from_be_bytes([0, head[1], head[2], head[3]]) as u64;
        let body = at + 4;
        let next = body.checked_add(size).ok_or("Bloc FLAC débordant")?;
        if next > input.len {
            return Err("Bloc FLAC tronqué".into());
        }
        if first && kind != 0 {
            return Err("Le premier bloc FLAC doit être STREAMINFO".into());
        }
        match kind {
            0 => {
                if saw_stream_info || size != 34 {
                    return Err("Bloc STREAMINFO FLAC invalide".into());
                }
                saw_stream_info = true;
                let data = input.read(body, 34)?;
                let packed = u64be(&data, 10)?;
                let sample_rate = (packed >> 44) as u32;
                let channels = ((packed >> 41) & 7) as u16 + 1;
                let total_samples = packed & 0x0000_000f_ffff_ffff;
                if sample_rate == 0 {
                    return Err("Fréquence FLAC nulle".into());
                }
                if total_samples > 0 {
                    media.duration_seconds = Some(total_samples as f64 / f64::from(sample_rate));
                }
                media.audio_streams.push(AudioStream {
                    index: 0,
                    codec: "flac".into(),
                    sample_rate: Some(sample_rate),
                    channels: Some(channels),
                    language: None,
                    title: None,
                    default: true,
                });
            }
            4 => vorbis_comments(
                &input.read(
                    body,
                    usize::try_from(size).map_err(|_| "Commentaires FLAC excessifs")?,
                )?,
                media,
            )?,
            127 => return Err("Type de bloc FLAC interdit".into()),
            _ => {}
        }
        at = next;
        first = false;
        if last {
            break;
        }
    }
    Ok(())
}

fn vorbis_comments(data: &[u8], media: &mut MediaFile) -> Result<()> {
    let vendor_len = u32le(data, 0)? as usize;
    let mut at = 4_usize
        .checked_add(vendor_len)
        .ok_or("Commentaire FLAC débordant")?;
    let count = u32le(data, at)? as usize;
    at += 4;
    if count > MAX_ELEMENTS {
        return Err("Trop de commentaires FLAC".into());
    }
    for _ in 0..count {
        let len = u32le(data, at)? as usize;
        at += 4;
        let end = at.checked_add(len).ok_or("Commentaire FLAC débordant")?;
        let comment = text(data.get(at..end).ok_or("Commentaire FLAC tronqué")?);
        if let Some((key, value)) = comment.split_once('=') {
            if key.eq_ignore_ascii_case("TITLE") {
                set_title(media, value.into());
            } else if key.eq_ignore_ascii_case("DATE") {
                set_date(media, value.into());
            }
        }
        at = end;
    }
    if at != data.len() {
        return Err("Longueur des commentaires FLAC invalide".into());
    }
    Ok(())
}
