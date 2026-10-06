//! Original synthetic NZB/yEnc fixtures. No provider or public article access.
mod library_support;
use library_support::Directory;
use mynou::{
    json,
    usenet::{
        nzb::{self, Nzb},
        yenc::{self, Part},
    },
};
use std::{fs, process::Command};

fn nzb(segments: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"><head><meta type="title">Private fixture</meta></head><file poster="private@example.test" date="1720000000" subject="untrusted title"><groups><group>alt.binaries.fixture</group></groups><segments>{segments}</segments></file></nzb>"#
    )
}
fn segment(number: u32, bytes: u64, id: &str) -> String {
    format!(r#"<segment bytes="{bytes}" number="{number}">{id}</segment>"#)
}
fn reference_crc(bytes: &[u8]) -> u32 {
    let mut n = u32::MAX;
    for byte in bytes {
        n ^= u32::from(*byte);
        for _ in 0..8 {
            n = if n & 1 == 0 {
                n >> 1
            } else {
                (n >> 1) ^ 0xedb8_8320
            };
        }
    }
    !n
}
fn body(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut width = 0;
    for b in bytes {
        let e = b.wrapping_add(42);
        let encoded = if matches!(e, 0 | 9 | 10 | 13 | 32 | 46 | 61) {
            vec![b'=', e.wrapping_add(64)]
        } else {
            vec![e]
        };
        if width + encoded.len() > 128 {
            out.extend_from_slice(b"\r\n");
            width = 0;
        }
        width += encoded.len();
        out.extend_from_slice(&encoded);
    }
    out.extend_from_slice(b"\r\n");
    out
}
fn single(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "Original fixture\r\n=ybegin line=128 size={} name={name}\r\n",
        bytes.len()
    )
    .into_bytes();
    out.extend_from_slice(&body(bytes));
    out.extend_from_slice(
        format!(
            "=yend size={} crc32={:08x}\r\n",
            bytes.len(),
            reference_crc(bytes)
        )
        .as_bytes(),
    );
    out
}
fn multi(name: &str, bytes: &[u8], number: u32, total: u32, begin: usize, end: usize) -> Vec<u8> {
    let data = &bytes[begin..end];
    let mut out = format!("=ybegin part={number} total={total} line=128 size={} name={name}\r\n=ypart begin={} end={end}\r\n",bytes.len(),begin+1).into_bytes();
    out.extend_from_slice(&body(data));
    out.extend_from_slice(
        format!(
            "=yend size={} part={number} pcrc32={:08x}{}\r\n",
            data.len(),
            reference_crc(data),
            if number == total {
                format!(" crc32={:08x}", reference_crc(bytes))
            } else {
                String::new()
            }
        )
        .as_bytes(),
    );
    out
}
fn decoded(bytes: &[u8], number: u32, total: u32, begin: usize, end: usize) -> Part {
    yenc::decode(&multi(
        "original fixture.mp4",
        bytes,
        number,
        total,
        begin,
        end,
    ))
    .unwrap()
}
#[test]
fn nzb_sorts_exact_article_identities_and_reports_no_private_claims() {
    let input =
        nzb(&(segment(2, 40, "second@fixture.test") + &segment(1, 30, "first@fixture.test")));
    let parsed = Nzb::parse(input.as_bytes()).unwrap();
    assert_eq!(parsed.files[0].segments[0].message_id, "first@fixture.test");
    assert_eq!(parsed.files[0].groups, vec!["alt.binaries.fixture"]);
    assert_eq!(
        parsed.id,
        mynou::crypto::sha256(input.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let report = json::stringify(&parsed.report());
    for secret in [
        "private",
        "fixture.test",
        "subject",
        "poster",
        "message_id",
        "untrusted title",
    ] {
        assert!(!report.contains(secret));
    }
    assert_eq!(
        parsed.report().get("advertised_bytes").unwrap().as_str(),
        Some("70")
    );
    assert_eq!(
        parsed.report().get("content_verified").unwrap().as_bool(),
        Some(false)
    );
}
#[test]
fn nzb_rejects_gaps_duplicate_numbers_ids_and_invalid_quantities() {
    for parts in [
        segment(2, 40, "second@fixture.test"),
        segment(1, 40, "second@fixture.test") + &segment(1, 40, "other@fixture.test"),
        segment(1, 40, "same@fixture.test") + &segment(2, 40, "same@fixture.test"),
        segment(0, 40, "bad@fixture.test"),
        segment(1, 0, "bad@fixture.test"),
        segment(1, nzb::MAX_ARTICLE_BYTES + 1, "bad@fixture.test"),
        segment(1, 40, "bad&#13;&#10;@fixture.test"),
        segment(1, 40, "&lt;wrapped@fixture.test&gt;"),
    ] {
        assert!(
            Nzb::parse(nzb(&parts).as_bytes()).is_err(),
            "Accepted invalid article inventory"
        );
    }
    for id in [
        "x@fixture.test\r\nBODY injected",
        "x@",
        "@fixture.test",
        "a@b@c",
        "space here@b",
        "x<y@z",
        "nonasciié@b",
    ] {
        assert!(!nzb::valid_message_id(id));
    }
}
#[test]
fn nzb_rejects_dtd_entities_wrong_structure_and_unbounded_input() {
    let good = nzb(&segment(1, 40, "first@fixture.test"));
    let cases = [
        good.replace(
            "<nzb ",
            "<!DOCTYPE nzb [<!ENTITY external SYSTEM 'file:///fixture'>]><nzb ",
        ),
        good.replace("untrusted title", "&external;"),
        good.replace("<groups>", "<unknown>"),
        good.replace(
            "<group>alt.binaries.fixture</group>",
            "<group>invalid group</group>",
        ),
        good.replace(
            "<group>alt.binaries.fixture</group>",
            "<group>alt.binaries.fixture</group><group>alt.binaries.fixture</group>",
        ),
        good.replace("subject=", "extra='unsupported' subject="),
        good.replace("date=\"1720000000\"", "date=\"-1\""),
        good.replace("http://www.newzbin.com/DTD/2003/nzb", "urn:foreign"),
        good.replace("first@fixture.test", "<inner>first@fixture.test</inner>"),
        "<nzb/>".into(),
        good.clone() + "<nzb/>",
    ];
    for input in cases {
        assert!(Nzb::parse(input.as_bytes()).is_err());
    }
    assert!(Nzb::parse(&vec![b' '; nzb::MAX_BYTES + 1]).is_err());
    assert!(Nzb::parse(&[255, 254]).is_err());
}
#[test]
fn nzb_global_bounds_reject_excess_files_and_cross_file_article_reuse() {
    let good = nzb(&segment(1, 40, "first@fixture.test"));
    let start = good.find("<file ").unwrap();
    let end = good.find("</file>").unwrap() + 7;
    let file = &good[start..end];
    assert!(Nzb::parse(format!("<nzb>{file}{file}</nzb>").as_bytes()).is_err());
    let files = (0..=nzb::MAX_FILES)
        .map(|i| file.replace("first@fixture.test", &format!("id{i}@fixture.test")))
        .collect::<String>();
    assert!(Nzb::parse(format!("<nzb>{files}</nzb>").as_bytes()).is_err());
    let parts = (1..=nzb::MAX_SEGMENTS + 1)
        .map(|i| segment(i as u32, 1, &format!("id{i}@fixture.test")))
        .collect::<String>();
    assert!(Nzb::parse(nzb(&parts).as_bytes()).is_err());
}
#[test]
fn crc_matches_independent_bitwise_reference_and_incremental_known_vectors() {
    assert_eq!(yenc::crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(yenc::crc32(b""), 0);
    let all = (0..=255)
        .cycle()
        .take(8192)
        .map(|n| n as u8)
        .collect::<Vec<_>>();
    assert_eq!(yenc::crc32(&all), reference_crc(&all));
    let mut crc = yenc::Crc32::default();
    for chunk in all.chunks(17) {
        crc.update(chunk);
    }
    assert_eq!(crc.finish(), reference_crc(&all));
}
#[test]
fn yenc_decodes_all_bytes_and_verifies_a_single_original_file() {
    let bytes = (0..=255).map(|n| n as u8).collect::<Vec<_>>();
    let article = single("original fixture.mp4", &bytes);
    let part = yenc::decode(&article).unwrap();
    assert_eq!(part.data(), bytes);
    assert_eq!(part.begin(), 0);
    assert_eq!(part.total_size(), 256);
    let file = yenc::assemble(vec![part]).unwrap();
    assert_eq!(file.name(), "original fixture.mp4");
    assert_eq!(file.crc32(), reference_crc(&bytes));
    assert_eq!(file.into_bytes(), bytes);
}
#[test]
fn multipart_out_of_order_input_requires_exact_coverage_and_whole_file_crc() {
    let bytes = b"Original synthetic multipart media fixture";
    let file = yenc::assemble(vec![
        decoded(bytes, 3, 3, 20, bytes.len()),
        decoded(bytes, 1, 3, 0, 10),
        decoded(bytes, 2, 3, 10, 20),
    ])
    .unwrap();
    assert_eq!(file.data(), bytes);
    assert!(
        yenc::assemble(vec![
            decoded(bytes, 1, 3, 0, 10),
            decoded(bytes, 3, 3, 20, bytes.len())
        ])
        .is_err()
    );
    assert!(
        yenc::assemble(vec![
            decoded(bytes, 1, 2, 0, 20),
            decoded(bytes, 2, 2, 19, bytes.len())
        ])
        .is_err()
    );
    assert!(
        yenc::assemble(vec![
            decoded(bytes, 1, 2, 0, 19),
            decoded(bytes, 2, 2, 20, bytes.len())
        ])
        .is_err()
    );
    assert!(
        yenc::assemble(vec![
            decoded(bytes, 1, 2, 0, 20),
            decoded(bytes, 1, 2, 0, 20)
        ])
        .is_err()
    );
    let foreign = b"A different synthetic fixture of matching length";
    assert!(
        yenc::assemble(vec![
            decoded(bytes, 1, 2, 0, 20),
            decoded(foreign, 2, 2, 20, foreign.len())
        ])
        .is_err()
    );
}
#[test]
fn valid_part_crc_does_not_substitute_for_complete_file_integrity() {
    let bytes = b"Original fixture content";
    let mut corrupted = bytes.to_vec();
    corrupted[0] ^= 1;
    let file = vec![
        decoded(&corrupted, 1, 2, 0, 10),
        decoded(bytes, 2, 2, 10, bytes.len()),
    ];
    assert!(yenc::assemble(file).unwrap_err().contains("CRC mismatch"));
    let mut final_part = multi("fixture.mp4", bytes, 2, 2, 10, bytes.len());
    let at = final_part.windows(7).position(|w| w == b" crc32=").unwrap();
    final_part.drain(at..at + 15);
    assert!(yenc::decode(&final_part).is_err());
}
#[test]
fn yenc_rejects_crc_corruption_truncation_bad_escapes_and_extra_payload() {
    let article = single("fixture.mp4", b"ABCDEF1234567890");
    for n in [0, 1, article.len() - 1, article.len() / 2] {
        assert!(yenc::decode(&article[..n]).is_err());
    }
    let mut corrupt = article.clone();
    let at = corrupt.windows(2).position(|x| x == b"\r\n").unwrap();
    let at = at
        + 2
        + corrupt[at + 2..]
            .windows(2)
            .position(|x| x == b"\r\n")
            .unwrap()
        + 2;
    corrupt[at] ^= 1;
    assert!(yenc::decode(&corrupt).is_err());
    let text = String::from_utf8(article).unwrap();
    for bad in [
        text.replace("crc32=", "unknown="),
        text.replace("line=128", "line=128 line=128"),
        text.replace("size=16", "size=17"),
        text.replace("\r\n", "\n"),
        text.clone() + "unexpected",
        text + "=ybegin line=1 size=1 name=extra\r\n",
    ] {
        assert!(yenc::decode(bad.as_bytes()).is_err());
    }
    let bad = b"=ybegin line=128 size=1 name=fixture.mp4\r\n=\r\n=yend size=1 crc32=00000000\r\n";
    assert!(yenc::decode(bad).is_err());
}
#[test]
fn yenc_rejects_unsafe_names_oversized_parts_and_missing_multipart_identity() {
    for name in [
        "../outside.mp4",
        "/outside.mp4",
        "path\\file.mp4",
        "C:file.mp4",
        "..",
        ".",
        "fixture.mp4 ",
        "bad\nname",
        "bad:stream",
        "file.",
    ] {
        assert!(!yenc::valid_name(name));
        assert!(yenc::decode(&single(name, b"fixture")).is_err());
    }
    let header = format!(
        "=ybegin line=128 size={} name=fixture.mp4\r\n",
        yenc::MAX_PART_BYTES + 1
    );
    assert!(yenc::decode(header.as_bytes()).is_err());
    for header in [
        "=ybegin part=2 total=1 line=128 size=1 name=fixture.mp4\r\n",
        "=ybegin part=1 line=128 size=1 name=fixture.mp4\r\n",
        "=ybegin total=2 line=128 size=1 name=fixture.mp4\r\n",
        "=ybegin part=1 total=2 line=128 size=10 name=fixture.mp4\r\n=ypart begin=5 end=3\r\n",
    ] {
        assert!(yenc::decode(header.as_bytes()).is_err());
    }
}
#[test]
fn cli_nzb_inspection_is_bounded_read_only_and_has_no_network_or_private_output() {
    let d = Directory::new();
    let path = d.0.join("original.nzb");
    let content = nzb(&segment(1, 40, "first@fixture.test"));
    fs::write(&path, &content).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mynou"))
        .args(["nzb-inspect"])
        .arg(&path)
        .env_remove("MYNOU_API_TOKEN")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report = json::parse(std::str::from_utf8(&out.stdout).unwrap()).unwrap();
    assert_eq!(report.get("file_count").unwrap().as_u64(), Some(1));
    assert!(
        !String::from_utf8(out.stdout)
            .unwrap()
            .contains("fixture.test")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), content);
    assert_eq!(fs::read_dir(&d.0).unwrap().count(), 1);
    fs::write(&path, "<!DOCTYPE forbidden><nzb/>").unwrap();
    assert!(
        !Command::new(env!("CARGO_BIN_EXE_mynou"))
            .arg("nzb-inspect")
            .arg(&path)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(Nzb::read_file(&d.0).is_err());
    #[cfg(unix)]
    {
        let alias = d.0.join("alias.nzb");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(Nzb::read_file(&alias).is_err());
    }
}
