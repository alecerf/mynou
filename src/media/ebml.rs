use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
struct Element {
    id: u32,
    data: u64,
    end: u64,
    unknown: bool,
}

fn header(input: &mut Input, at: u64, end: u64) -> Result<Element> {
    input.count()?;
    if at >= end {
        return Err("Missing EBML header".into());
    }
    let bytes = input.read(at, (end - at).min(12) as usize)?;
    let (id, id_len, _) = vint(&bytes, 0, true)?;
    if id_len > 4 {
        return Err("EBML identifier is too long".into());
    }
    let (size, size_len, unknown) = vint(&bytes, id_len, false)?;
    let data = at
        .checked_add((id_len + size_len) as u64)
        .ok_or("EBML offset overflow")?;
    let element_end = if unknown {
        end
    } else {
        data.checked_add(size).ok_or("EBML size overflow")?
    };
    if data > end || element_end > end {
        return Err("EBML element extends beyond its container".into());
    }
    Ok(Element {
        id: id as u32,
        data,
        end: element_end,
        unknown,
    })
}

fn vint(data: &[u8], at: usize, identifier: bool) -> Result<(u64, usize, bool)> {
    let first = *data.get(at).ok_or("Truncated EBML integer")?;
    if first == 0 {
        return Err("EBML integer has no length marker".into());
    }
    let len = first.leading_zeros() as usize + 1;
    let bytes = data.get(at..at + len).ok_or("Truncated EBML integer")?;
    let marker = 1_u8 << (8 - len);
    let mut value = u64::from(if identifier {
        first
    } else {
        first & (marker - 1)
    });
    for byte in &bytes[1..] {
        value = (value << 8) | u64::from(*byte);
    }
    let unknown = !identifier && value == ((1_u64 << (len * 7)) - 1);
    Ok((value, len, unknown))
}

fn children(input: &mut Input, parent: Element) -> Result<Vec<Element>> {
    let mut at = parent.data;
    let mut result = Vec::new();
    while at < parent.end {
        let child = header(input, at, parent.end)?;
        if child.unknown {
            return Err("Unknown EBML size in metadata".into());
        }
        at = child.end;
        result.push(child);
    }
    Ok(result)
}
fn unsigned(input: &mut Input, element: Element) -> Result<u64> {
    let size = element.end - element.data;
    if size > 8 {
        return Err("EBML metadata integer is too long".into());
    }
    let data = input.read(element.data, size as usize)?;
    Ok(data
        .iter()
        .fold(0, |number, byte| (number << 8) | u64::from(*byte)))
}
fn float(input: &mut Input, element: Element) -> Result<f64> {
    let data = input.read(element.data, (element.end - element.data) as usize)?;
    let value = match data.len() {
        4 => f64::from(f32::from_bits(u32be(&data, 0)?)),
        8 => f64::from_bits(u64be(&data, 0)?),
        _ => return Err("Invalid EBML floating-point length".into()),
    };
    if !value.is_finite() {
        return Err("Non-finite EBML floating-point value".into());
    }
    Ok(value)
}
fn string_value(input: &mut Input, element: Element) -> Result<String> {
    Ok(text(&input.read(
        element.data,
        usize::try_from(element.end - element.data).map_err(|_| "EBML text exceeds the size limit")?,
    )?))
}

pub(super) fn parse(input: &mut Input, media: &mut MediaFile) -> Result<()> {
    let ebml = header(input, 0, input.len)?;
    if ebml.id != 0x1a45dfa3 || ebml.unknown {
        return Err("Invalid EBML header".into());
    }
    let mut doc_type = None;
    for item in children(input, ebml)? {
        match item.id {
            0x4282 => doc_type = Some(string_value(input, item)?),
            0x42f2 if unsigned(input, item)? > 4 => {
                return Err(
                    "EBML identifiers longer than four bytes are unsupported".into(),
                );
            }
            0x42f3 if unsigned(input, item)? > 8 => {
                return Err("EBML sizes longer than eight bytes are unsupported".into());
            }
            _ => {}
        }
    }
    media.container = match doc_type.as_deref() {
        Some("matroska") => "matroska".into(),
        Some("webm") => "webm".into(),
        _ => return Err("EBML document is neither Matroska nor WebM".into()),
    };
    let mut at = ebml.end;
    let segment = loop {
        if at >= input.len {
            return Err("Missing Matroska segment".into());
        }
        let element = header(input, at, input.len)?;
        if element.id == 0x18538067 {
            break element;
        }
        if element.unknown {
            return Err("Unknown EBML size before the segment".into());
        }
        at = element.end;
    };
    at = segment.data;
    let mut parsed = BTreeSet::new();
    let mut seeks = Vec::new();
    while at < segment.end {
        let element = header(input, at, segment.end)?;
        match element.id {
            0x114d9b74 => seek_head(input, element, &mut seeks)?,
            0x1549a966 | 0x1654ae6b | 0x1254c367 => {
                parse_metadata(input, element, media)?;
                parsed.insert((element.id, at));
            }
            _ => {}
        }
        if element.unknown {
            break;
        }
        at = element.end;
    }
    for (id, offset) in seeks {
        if !matches!(id, 0x1549a966 | 0x1654ae6b | 0x1254c367) {
            continue;
        }
        let at = segment
            .data
            .checked_add(offset)
            .ok_or("SeekHead offset overflow")?;
        if parsed.contains(&(id, at)) {
            continue;
        }
        let target = header(input, at, segment.end)?;
        if target.id != id || target.unknown {
            return Err("Invalid Matroska SeekHead target".into());
        }
        parse_metadata(input, target, media)?;
        parsed.insert((id, at));
    }
    Ok(())
}

fn seek_head(input: &mut Input, parent: Element, seeks: &mut Vec<(u32, u64)>) -> Result<()> {
    for entry in children(input, parent)? {
        if entry.id != 0x4dbb {
            continue;
        }
        let mut id = None;
        let mut offset = None;
        for item in children(input, entry)? {
            match item.id {
                0x53ab => id = u32::try_from(unsigned(input, item)?).ok(),
                0x53ac => offset = Some(unsigned(input, item)?),
                _ => {}
            }
        }
        if let (Some(id), Some(offset)) = (id, offset) {
            seeks.push((id, offset));
        }
    }
    Ok(())
}

fn parse_metadata(input: &mut Input, parent: Element, media: &mut MediaFile) -> Result<()> {
    if parent.unknown {
        return Err("Unknown Matroska metadata size".into());
    }
    match parent.id {
        0x1549a966 => {
            let mut scale = 1_000_000;
            let mut duration = None;
            for item in children(input, parent)? {
                match item.id {
                    0x2ad7b1 => scale = unsigned(input, item)?,
                    0x4489 => duration = Some(float(input, item)?),
                    0x7ba9 => set_title(media, string_value(input, item)?),
                    0x4461 => {
                        if item.end - item.data != 8 {
                            return Err("Invalid Matroska UTC date length".into());
                        }
                        let data = input.read(item.data, 8)?;
                        let nanos = i64::from_be_bytes(
                            data.try_into().map_err(|_| "Invalid Matroska UTC date")?,
                        );
                        if let Some(date) = nanos
                            .div_euclid(1_000_000_000)
                            .checked_add(978_307_200)
                            .and_then(utc_date)
                        {
                            set_date(media, date);
                        }
                    }
                    _ => {}
                }
            }
            if scale == 0 {
                return Err("Matroska time scale is zero".into());
            }
            media.duration_seconds =
                positive(duration.map(|duration| duration * scale as f64 / 1_000_000_000.0));
        }
        0x1654ae6b => {
            if !media.video_streams.is_empty() || !media.audio_streams.is_empty() {
                return Err("Multiple Matroska Tracks sections".into());
            }
            for (index, entry) in children(input, parent)?
                .into_iter()
                .filter(|entry| entry.id == 0xae)
                .enumerate()
            {
                track(input, entry, media, index)?;
            }
        }
        0x1254c367 => tags(input, parent, media)?,
        _ => {}
    }
    Ok(())
}

fn track(input: &mut Input, parent: Element, media: &mut MediaFile, index: usize) -> Result<()> {
    let mut kind = 0;
    let mut codec_id = String::new();
    let mut default = true;
    let mut enabled = true;
    let mut language = Some("eng".into());
    let mut ietf_language = None;
    let mut title = None;
    let mut frame_rate = None;
    let mut width = None;
    let mut height = None;
    let mut sample_rate = Some(8_000);
    let mut channels = Some(1);
    for item in children(input, parent)? {
        match item.id {
            0x83 => kind = unsigned(input, item)?,
            0x86 => codec_id = string_value(input, item)?,
            0x88 => default = unsigned(input, item)? != 0,
            0xb9 => enabled = unsigned(input, item)? != 0,
            0x22b59c => {
                language = Some(string_value(input, item)?).filter(|s| !s.is_empty() && s != "und")
            }
            0x22b59d => {
                ietf_language =
                    Some(string_value(input, item)?).filter(|s| !s.is_empty() && s != "und")
            }
            0x536e => title = Some(string_value(input, item)?).filter(|s| !s.is_empty()),
            0x23e383 => {
                let ns = unsigned(input, item)?;
                if ns > 0 {
                    frame_rate = Some(1_000_000_000.0 / ns as f64);
                }
            }
            0xe0 => {
                for setting in children(input, item)? {
                    match setting.id {
                        0xb0 => {
                            width = Some(
                                u32::try_from(unsigned(input, setting)?)
                                    .map_err(|_| "Matroska width exceeds the limit")?,
                            )
                            .filter(|n| *n > 0)
                        }
                        0xba => {
                            height = Some(
                                u32::try_from(unsigned(input, setting)?)
                                    .map_err(|_| "Matroska height exceeds the limit")?,
                            )
                            .filter(|n| *n > 0)
                        }
                        0x2383e3 => frame_rate = positive(Some(float(input, setting)?)),
                        _ => {}
                    }
                }
            }
            0xe1 => {
                for setting in children(input, item)? {
                    match setting.id {
                        0xb5 | 0x78b5 => {
                            let rate = float(input, setting)?;
                            if !(1.0..=f64::from(u32::MAX)).contains(&rate) {
                                return Err("Invalid Matroska sample rate".into());
                            }
                            sample_rate = Some(rate.round() as u32);
                        }
                        0x9f => {
                            channels = Some(
                                u16::try_from(unsigned(input, setting)?)
                                    .map_err(|_| "Matroska channel count exceeds the limit")?,
                            )
                            .filter(|n| *n > 0)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if codec_id.is_empty() && matches!(kind, 1 | 2) {
        return Err("Missing Matroska codec".into());
    }
    let codec = codec(&codec_id);
    if ietf_language.is_some() {
        language = ietf_language;
    }
    if kind == 1 {
        media.video_streams.push(VideoStream {
            index,
            codec,
            width,
            height,
            frame_rate,
            language,
            title,
            default: default && enabled,
        });
    } else if kind == 2 {
        media.audio_streams.push(AudioStream {
            index,
            codec,
            sample_rate,
            channels,
            language,
            title,
            default: default && enabled,
        });
    }
    Ok(())
}

fn codec(id: &str) -> String {
    match id {
        "V_MPEG4/ISO/AVC" => "h264",
        "V_MPEGH/ISO/HEVC" => "hevc",
        "V_AV1" => "av1",
        "V_VP9" => "vp9",
        "V_VP8" => "vp8",
        "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" => "mpeg4",
        "V_MPEG2" => "mpeg2video",
        "V_MPEG1" => "mpeg1video",
        "V_MJPEG" => "mjpeg",
        "A_AAC" | "A_AAC/MPEG4/LC" | "A_AAC/MPEG2/LC" => "aac",
        "A_OPUS" => "opus",
        "A_VORBIS" => "vorbis",
        "A_FLAC" => "flac",
        "A_MPEG/L3" => "mp3",
        "A_MPEG/L2" => "mp2",
        "A_AC3" => "ac3",
        "A_EAC3" => "eac3",
        "A_DTS" => "dts",
        "A_TRUEHD" => "truehd",
        "A_PCM/INT/LIT" => "pcm_le",
        "A_PCM/INT/BIG" => "pcm_be",
        "A_PCM/FLOAT/IEEE" => "pcm_float",
        _ => id,
    }
    .into()
}

fn tags(input: &mut Input, parent: Element, media: &mut MediaFile) -> Result<()> {
    for tag in children(input, parent)? {
        if tag.id != 0x7373 {
            continue;
        }
        let entries = children(input, tag)?;
        let mut global = true;
        for target in entries.iter().filter(|element| element.id == 0x63c0) {
            for item in children(input, *target)? {
                if matches!(item.id, 0x63c5 | 0x63c4 | 0x63c6 | 0x63c9)
                    && unsigned(input, item)? != 0
                {
                    global = false;
                }
            }
        }
        if !global {
            continue;
        }
        for simple in entries.into_iter().filter(|element| element.id == 0x67c8) {
            simple_tag(input, simple, media, 0)?;
        }
    }
    Ok(())
}
fn simple_tag(
    input: &mut Input,
    parent: Element,
    media: &mut MediaFile,
    depth: usize,
) -> Result<()> {
    if depth >= 12 {
        return Err("Matroska tags are nested too deeply".into());
    }
    let mut name = String::new();
    let mut value = None;
    for item in children(input, parent)? {
        match item.id {
            0x45a3 => name = string_value(input, item)?,
            0x4487 => value = Some(string_value(input, item)?),
            0x67c8 => simple_tag(input, item, media, depth + 1)?,
            _ => {}
        }
    }
    if let Some(value) = value {
        if name.eq_ignore_ascii_case("TITLE") {
            set_title(media, value);
        } else if ["DATE_RELEASED", "DATE_RECORDED", "DATE"]
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known))
        {
            set_date(media, value);
        }
    }
    Ok(())
}
