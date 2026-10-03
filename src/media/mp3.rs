use super::*;

#[derive(Clone, Copy)]
struct Header {
    sample_rate: u32,
    bit_rate: u32,
    channels: u16,
    samples: u32,
    frame_size: usize,
    version: u8,
    layer: u8,
    crc: bool,
}

pub(super) fn is_header(data: &[u8]) -> bool {
    decode_header(data).is_some()
}

fn decode_header(data: &[u8]) -> Option<Header> {
    let bytes: [u8; 4] = data.get(..4)?.try_into().ok()?;
    let bits = u32::from_be_bytes(bytes);
    if bits >> 21 != 0x7ff {
        return None;
    }
    let version = ((bits >> 19) & 3) as u8;
    let layer = ((bits >> 17) & 3) as u8;
    let rate_index = ((bits >> 12) & 15) as usize;
    let frequency = ((bits >> 10) & 3) as usize;
    if version == 1 || layer == 0 || rate_index == 0 || rate_index == 15 || frequency == 3 {
        return None;
    }
    let bit_rate = match (version, layer) {
        (3, 3) => [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ][rate_index],
        (3, 2) => [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ][rate_index],
        (3, 1) => [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ][rate_index],
        (_, 3) => [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ][rate_index],
        _ => [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160][rate_index],
    } * 1_000;
    let sample_rate = [44_100, 48_000, 32_000][frequency]
        / match version {
            3 => 1,
            2 => 2,
            _ => 4,
        };
    let padding = (bits >> 9) & 1;
    let channels = if (bits >> 6) & 3 == 3 { 1 } else { 2 };
    let (samples, frame_size) = if layer == 3 {
        (384, ((12 * bit_rate / sample_rate) + padding) * 4)
    } else if layer == 1 && version != 3 {
        (576, 72 * bit_rate / sample_rate + padding)
    } else {
        (1_152, 144 * bit_rate / sample_rate + padding)
    };
    Some(Header {
        sample_rate,
        bit_rate,
        channels,
        samples,
        frame_size: frame_size as usize,
        version,
        layer,
        crc: bits & 0x10000 == 0,
    })
}

pub(super) fn parse(input: &mut Input, media: &mut MediaFile) -> Result<()> {
    media.container = "mp3".into();
    let mut audio_start = 0;
    if input
        .read(0, input.len.min(10) as usize)?
        .starts_with(b"ID3")
    {
        let head = input.read(0, 10)?;
        let version = head[3];
        if !(2..=4).contains(&version) {
            return Err("Unsupported ID3 version".into());
        }
        let size = synchsafe(&head[6..10])?;
        let footer = version == 4 && head[5] & 0x10 != 0;
        let data = input.read(10, size as usize)?;
        id3(&data, version, head[5], media)?;
        audio_start = 10 + u64::from(size) + if footer { 10 } else { 0 };
    }
    let search_size = input.len.saturating_sub(audio_start).min(128 * 1_024) as usize;
    let scan = input.read(audio_start, search_size)?;
    let mut found = None;
    for at in 0..scan.len().saturating_sub(3) {
        let Some(header) = decode_header(&scan[at..]) else {
            continue;
        };
        let next = at + header.frame_size;
        if next.checked_add(4).is_some_and(|end| end <= scan.len())
            && let Some(other) = decode_header(&scan[next..])
            && other.sample_rate == header.sample_rate
            && other.layer == header.layer
            && other.version == header.version
        {
            found = Some((at, header));
            break;
        }
    }
    let (offset, header) = found.ok_or("No valid pair of MPEG audio frames found")?;
    audio_start += offset as u64;
    let frame = input.read(audio_start, header.frame_size)?;
    let mut frames = None;
    if header.layer == 1 {
        let side_size = match (header.version, header.channels) {
            (3, 1) => 17,
            (3, _) => 32,
            (_, 1) => 9,
            _ => 17,
        };
        let xing = 4 + if header.crc { 2 } else { 0 } + side_size;
        if matches!(frame.get(xing..xing + 4), Some(b"Xing" | b"Info"))
            && u32be(&frame, xing + 4)? & 1 != 0
        {
            frames = Some(u32be(&frame, xing + 8)?);
        } else if frame.get(36..40) == Some(b"VBRI") {
            frames = Some(u32be(&frame, 50)?);
        }
    }
    media.duration_seconds = frames
        .filter(|n| *n > 0)
        .map(|n| f64::from(n) * f64::from(header.samples) / f64::from(header.sample_rate));
    // The first frame's instantaneous bit rate does not establish VBR duration.
    // Without a duration index, keep None to avoid a misleading estimate.
    media.bit_rate = if frames.is_none() {
        None
    } else {
        media
            .duration_seconds
            .map(|s| ((input.len - audio_start) as f64 * 8.0 / s).round() as u64)
    };
    media.audio_streams.push(AudioStream {
        index: 0,
        codec: match header.layer {
            1 => "mp3",
            2 => "mp2",
            _ => "mp1",
        }
        .into(),
        sample_rate: Some(header.sample_rate),
        channels: Some(header.channels),
        language: None,
        title: None,
        default: true,
    });
    if frames.is_none() {
        media.bit_rate = Some(u64::from(header.bit_rate));
    }
    Ok(())
}

fn synchsafe(bytes: &[u8]) -> Result<u32> {
    if bytes.len() != 4 || bytes.iter().any(|byte| byte & 0x80 != 0) {
        return Err("Invalid ID3 synchsafe integer".into());
    }
    Ok(bytes
        .iter()
        .fold(0, |value, byte| (value << 7) | u32::from(*byte)))
}

fn id3(data: &[u8], version: u8, flags: u8, media: &mut MediaFile) -> Result<()> {
    // Skip text metadata in compressed, encrypted or unsynchronized tags
    // instead of interpreting transformed data as an ordinary text format.
    if flags & 0x80 != 0 || (version == 2 && flags & 0x40 != 0) {
        return Ok(());
    }
    let mut at = 0;
    if version >= 3 && flags & 0x40 != 0 {
        let size = if version == 4 {
            synchsafe(data.get(..4).ok_or("Truncated ID3 extended header")?)? as usize
        } else {
            u32be(data, 0)? as usize + 4
        };
        if size < 4 || size > data.len() {
            return Err("Invalid ID3 extended header".into());
        }
        at = size;
    }
    let mut count = 0;
    while at < data.len() {
        if data[at] == 0 {
            break;
        }
        count += 1;
        if count > MAX_ELEMENTS {
            return Err("Too many ID3 frames".into());
        }
        let header_len = if version == 2 { 6 } else { 10 };
        let head = data.get(at..at + header_len).ok_or("Truncated ID3 frame")?;
        let id_len = if version == 2 { 3 } else { 4 };
        if !head[..id_len]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return Err("Invalid ID3 identifier".into());
        }
        let length = if version == 2 {
            u32::from_be_bytes([0, head[3], head[4], head[5]])
        } else if version == 4 {
            synchsafe(&head[4..8])?
        } else {
            u32be(head, 4)?
        } as usize;
        let start = at + header_len;
        let end = start.checked_add(length).ok_or("ID3 frame overflow")?;
        let body = data.get(start..end).ok_or("ID3 frame is out of bounds")?;
        let transformed = version >= 3 && head[9] != 0;
        if !transformed
            && matches!(
                &head[..id_len],
                b"TIT2" | b"TT2" | b"TDRC" | b"TYER" | b"TYE"
            )
        {
            let value = id3_text(body)?;
            if matches!(&head[..id_len], b"TIT2" | b"TT2") {
                set_title(media, value);
            } else {
                set_date(media, value);
            }
        }
        at = end;
    }
    Ok(())
}

fn id3_text(data: &[u8]) -> Result<String> {
    let encoding = *data.first().ok_or("Empty ID3 text")?;
    let payload = &data[1..];
    match encoding {
        0 => Ok(payload
            .iter()
            .map(|byte| char::from(*byte))
            .collect::<String>()
            .trim_matches('\0')
            .into()),
        3 => Ok(text(payload)),
        1 | 2 => {
            let (little, payload) = if encoding == 1 {
                if payload.starts_with(&[0xff, 0xfe]) {
                    (true, &payload[2..])
                } else if payload.starts_with(&[0xfe, 0xff]) {
                    (false, &payload[2..])
                } else {
                    return Err("ID3 UTF-16 text has no byte-order mark".into());
                }
            } else {
                (false, payload)
            };
            if !payload.len().is_multiple_of(2) {
                return Err("Truncated ID3 UTF-16 text".into());
            }
            let words = payload.as_chunks::<2>().0.iter().map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            });
            Ok(String::from_utf16_lossy(&words.collect::<Vec<_>>())
                .trim_matches('\0')
                .into())
        }
        _ => Err("Unknown ID3 encoding".into()),
    }
}
