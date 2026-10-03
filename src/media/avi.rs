use super::*;

#[derive(Clone, Copy)]
struct Chunk {
    kind: [u8; 4],
    data: u64,
    end: u64,
}
fn chunks(input: &mut Input, mut at: u64, end: u64) -> Result<Vec<Chunk>> {
    let mut result = Vec::new();
    while at < end {
        input.count()?;
        if end - at < 8 {
            return Err("Truncated AVI chunk".into());
        }
        let head = input.read(at, 8)?;
        let size = u64::from(u32le(&head, 4)?);
        let body = at + 8;
        let next = body.checked_add(size).ok_or("AVI size overflow")?;
        if next > end {
            return Err("AVI chunk is out of bounds".into());
        }
        result.push(Chunk {
            kind: head[..4].try_into().map_err(|_| "Invalid AVI chunk type")?,
            data: body,
            end: next,
        });
        at = next
            .checked_add(size & 1)
            .ok_or("AVI alignment overflow")?;
        if at > end {
            return Err("Truncated AVI alignment".into());
        }
    }
    Ok(result)
}

pub(super) fn parse(input: &mut Input, media: &mut MediaFile) -> Result<()> {
    media.container = "avi".into();
    let head = input.read(0, 12)?;
    let end = u64::from(u32le(&head, 4)?)
        .checked_add(8)
        .ok_or("AVI size overflow")?;
    if end > input.len || end < 12 {
        return Err("Invalid AVI RIFF size".into());
    }
    let mut index = 0;
    for item in chunks(input, 12, end)? {
        if &item.kind != b"LIST" {
            continue;
        }
        if item.end - item.data < 4 {
            return Err("Truncated AVI list".into());
        }
        let kind = input.read(item.data, 4)?;
        if kind == b"hdrl" {
            for entry in chunks(input, item.data + 4, item.end)? {
                if &entry.kind == b"avih" {
                    let data = input.read(entry.data, (entry.end - entry.data).min(56) as usize)?;
                    let micros = u32le(&data, 0)?;
                    let frames = u32le(&data, 16)?;
                    if micros > 0 && frames > 0 {
                        media.duration_seconds =
                            Some(f64::from(micros) * f64::from(frames) / 1_000_000.0);
                    }
                } else if &entry.kind == b"LIST"
                    && entry.end - entry.data >= 4
                    && input.read(entry.data, 4)? == b"strl"
                {
                    stream(input, entry, media, index)?;
                    index += 1;
                }
            }
        } else if kind == b"INFO" {
            for entry in chunks(input, item.data + 4, item.end)? {
                if matches!(&entry.kind, b"INAM" | b"ICRD") {
                    let value = text(
                        &input.read(
                            entry.data,
                            usize::try_from(entry.end - entry.data)
                                .map_err(|_| "AVI metadata exceeds the size limit")?,
                        )?,
                    );
                    if &entry.kind == b"INAM" {
                        set_title(media, value);
                    } else {
                        set_date(media, value);
                    }
                }
            }
        }
    }
    Ok(())
}

fn stream(input: &mut Input, parent: Chunk, media: &mut MediaFile, index: usize) -> Result<()> {
    let mut header = None;
    let mut format = None;
    let mut title = None;
    for entry in chunks(input, parent.data + 4, parent.end)? {
        match &entry.kind {
            b"strh" => {
                header = Some(input.read(entry.data, (entry.end - entry.data).min(64) as usize)?)
            }
            b"strf" => {
                format = Some(input.read(entry.data, (entry.end - entry.data).min(64) as usize)?)
            }
            b"strn" => {
                title = Some(text(&input.read(
                    entry.data,
                    usize::try_from(entry.end - entry.data).map_err(|_| "AVI title exceeds the size limit")?,
                )?))
            }
            _ => {}
        }
    }
    let header = header.ok_or("Missing AVI track description")?;
    if header.len() < 36 {
        return Err("Truncated AVI track description".into());
    }
    let default = u32le(&header, 8)? & 1 == 0;
    if &header[..4] == b"vids" {
        let scale = u32le(&header, 20)?;
        let rate = u32le(&header, 24)?;
        let frame_rate = if scale > 0 && rate > 0 {
            Some(f64::from(rate) / f64::from(scale))
        } else {
            None
        };
        let format = format.ok_or("Missing AVI video format")?;
        if format.len() < 20 {
            return Err("Truncated AVI video format".into());
        }
        let width = i32::from_le_bytes(
            format[4..8]
                .try_into()
                .map_err(|_| "Invalid AVI width")?,
        );
        let height = i32::from_le_bytes(
            format[8..12]
                .try_into()
                .map_err(|_| "Invalid AVI height")?,
        );
        let codec_bytes = if format[16..20] == [0, 0, 0, 0] {
            &header[4..8]
        } else {
            &format[16..20]
        };
        let codec = match codec_bytes {
            b"H264" | b"h264" | b"avc1" | b"X264" => "h264".into(),
            b"HEVC" | b"H265" | b"h265" => "hevc".into(),
            b"XVID" | b"DIVX" | b"DX50" | b"FMP4" | b"MP4V" => "mpeg4".into(),
            b"MJPG" | b"mjpg" => "mjpeg".into(),
            b"VP90" => "vp9".into(),
            b"VP80" => "vp8".into(),
            b"AV01" => "av1".into(),
            [0, 0, 0, 0] | b"DIB " => "rawvideo".into(),
            _ => text(codec_bytes),
        };
        media.video_streams.push(VideoStream {
            index,
            codec,
            width: u32::try_from(width).ok().filter(|n| *n > 0),
            height: Some(height.unsigned_abs()).filter(|n| *n > 0),
            frame_rate,
            language: None,
            title,
            default,
        });
    } else if &header[..4] == b"auds" {
        let format = format.ok_or("Missing AVI audio format")?;
        let id = u16le(&format, 0)?;
        let channels = u16le(&format, 2)?;
        let sample_rate = u32le(&format, 4)?;
        if channels == 0 || sample_rate == 0 {
            return Err("Invalid AVI audio parameters".into());
        }
        let codec = match id {
            1 => format!("pcm_s{}le", u16le(&format, 14)?),
            3 => format!("pcm_f{}le", u16le(&format, 14)?),
            0x55 => "mp3".into(),
            0xff => "aac".into(),
            0x2000 => "ac3".into(),
            0x2001 => "dts".into(),
            6 => "pcm_alaw".into(),
            7 => "pcm_mulaw".into(),
            _ => format!("wav_0x{id:04x}"),
        };
        media.audio_streams.push(AudioStream {
            index,
            codec,
            sample_rate: Some(sample_rate),
            channels: Some(channels),
            language: None,
            title,
            default,
        });
    }
    Ok(())
}
