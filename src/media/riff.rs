use super::*;

pub(super) fn wave(input: &mut Input, media: &mut MediaFile) -> Result<()> {
    if input.len < 12 {
        return Err("Truncated WAV header".into());
    }
    media.container = "wav".into();
    let head = input.read(0, 12)?;
    let rf64 = head.starts_with(b"RF64");
    let riff_length = u64::from(u32le(&head, 4)?);
    let mut end = if rf64 {
        input.len
    } else {
        riff_length.checked_add(8).ok_or("WAV size overflow")?
    };
    if end > input.len || end < 12 {
        return Err("Invalid WAV container size".into());
    }
    let mut data_size = 0_u64;
    let mut rf64_data = None;
    let mut byte_rate = 0;
    let mut stream = None;
    let mut at = 12;
    while at < end {
        input.count()?;
        if end - at < 8 {
            return Err("Truncated WAV chunk".into());
        }
        let chunk = input.read(at, 8)?;
        let advertised = u32le(&chunk, 4)?;
        let size = if rf64 && &chunk[0..4] == b"data" && advertised == u32::MAX {
            rf64_data.ok_or("RF64 WAV requires ds64 before data")?
        } else {
            u64::from(advertised)
        };
        let body = at + 8;
        let next = body.checked_add(size).ok_or("WAV chunk overflow")?;
        if next > end {
            return Err("WAV chunk extends beyond its container".into());
        }
        match &chunk[0..4] {
            b"ds64" if rf64 => {
                if size < 28 {
                    return Err("Truncated RF64 ds64 chunk".into());
                }
                let data = input.read(body, 28)?;
                let declared_end = u64le(&data, 0)?
                    .checked_add(8)
                    .ok_or("RF64 size overflow")?;
                if declared_end > input.len || declared_end < next {
                    return Err("Invalid RF64 size".into());
                }
                end = declared_end;
                rf64_data = Some(u64le(&data, 8)?);
            }
            b"fmt " => {
                if size < 16 {
                    return Err("Truncated WAV audio description".into());
                }
                let data = input.read(body, size.min(64) as usize)?;
                let mut format = u16le(&data, 0)?;
                let channels = u16le(&data, 2)?;
                let sample_rate = u32le(&data, 4)?;
                byte_rate = u32le(&data, 8)?;
                let bits = u16le(&data, 14)?;
                if format == 0xfffe {
                    if size < 40 || u16le(&data, 16)? < 22 {
                        return Err("Truncated WAV extensible format".into());
                    }
                    format = u16le(&data, 24)?;
                }
                if channels == 0 || sample_rate == 0 || byte_rate == 0 {
                    return Err("Invalid WAV audio parameters".into());
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
                    .ok_or("WAV data size overflow")?
            }
            b"LIST" if size >= 4 && input.read(body, 4)? == b"INFO" => {
                wave_info(input, body + 4, next, media)?;
            }
            _ => {}
        }
        at = next.checked_add(size & 1).ok_or("WAV alignment overflow")?;
        if at > end {
            return Err("Missing WAV padding byte".into());
        }
    }
    if let Some(stream) = stream {
        media.audio_streams.push(stream);
    } else {
        return Err("Missing WAV fmt chunk".into());
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
            return Err("Truncated WAV metadata".into());
        }
        let head = input.read(at, 8)?;
        let size = u64::from(u32le(&head, 4)?);
        let body = at + 8;
        let next = body.checked_add(size).ok_or("WAV metadata overflow")?;
        if next > end {
            return Err("WAV metadata is out of bounds".into());
        }
        if matches!(&head[0..4], b"INAM" | b"ICRD") {
            let value = text(&input.read(
                body,
                usize::try_from(size).map_err(|_| "WAV metadata exceeds the size limit")?,
            )?);
            if &head[0..4] == b"INAM" {
                set_title(media, value);
            } else {
                set_date(media, value);
            }
        }
        at = next.checked_add(size & 1).ok_or("WAV alignment overflow")?;
        if at > end {
            return Err("Truncated WAV metadata alignment".into());
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
        let next = body.checked_add(size).ok_or("FLAC block overflow")?;
        if next > input.len {
            return Err("Truncated FLAC block".into());
        }
        if first && kind != 0 {
            return Err("The first FLAC block must be STREAMINFO".into());
        }
        match kind {
            0 => {
                if saw_stream_info || size != 34 {
                    return Err("Invalid FLAC STREAMINFO block".into());
                }
                saw_stream_info = true;
                let data = input.read(body, 34)?;
                let packed = u64be(&data, 10)?;
                let sample_rate = (packed >> 44) as u32;
                let channels = ((packed >> 41) & 7) as u16 + 1;
                let total_samples = packed & 0x0000_000f_ffff_ffff;
                if sample_rate == 0 {
                    return Err("FLAC sample rate is zero".into());
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
                    usize::try_from(size).map_err(|_| "FLAC comments exceed the size limit")?,
                )?,
                media,
            )?,
            127 => return Err("Forbidden FLAC block type".into()),
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
        .ok_or("FLAC comment overflow")?;
    let count = u32le(data, at)? as usize;
    at += 4;
    if count > MAX_ELEMENTS {
        return Err("Too many FLAC comments".into());
    }
    for _ in 0..count {
        let len = u32le(data, at)? as usize;
        at += 4;
        let end = at.checked_add(len).ok_or("FLAC comment overflow")?;
        let comment = text(data.get(at..end).ok_or("Truncated FLAC comment")?);
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
        return Err("Invalid FLAC comment length".into());
    }
    Ok(())
}
