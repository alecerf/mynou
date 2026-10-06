//! Original local receipt/crash fixtures. No acquisition or external helpers.
mod library_support;
use library_support::Directory;
use mynou::{
    crypto::sha256,
    json::{self, Value},
    usenet::{
        workspace::Workspace,
        yenc::{self, Crc32, Part},
    },
};
use std::{fs, path::Path};
const SERVER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn source(count: u32) -> Vec<u8> {
    format!("<nzb><file subject=\"Private original fixture\"><groups><group>alt.binaries.fixture</group></groups><segments>{}</segments></file></nzb>",(1..=count).map(|n|format!("<segment number=\"{n}\" bytes=\"1024\">part{n}@fixture.test</segment>")).collect::<String>()).into_bytes()
}
fn encoded(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut width = 0;
    for b in bytes {
        let e = b.wrapping_add(42);
        let mut v = vec![e];
        if matches!(e, 0 | 9 | 10 | 13 | 32 | 46 | 61) {
            v = vec![b'=', e.wrapping_add(64)];
        }
        if width + v.len() > 128 {
            out.extend_from_slice(b"\r\n");
            width = 0;
        }
        width += v.len();
        out.extend_from_slice(&v);
    }
    out.extend_from_slice(b"\r\n");
    out
}
fn part(bytes: &[u8], number: u32, total: u32, begin: usize, end: usize) -> Part {
    let data = &bytes[begin..end];
    let mut article=format!("=ybegin part={number} total={total} line=128 size={} name=original fixture.mp4\r\n=ypart begin={} end={end}\r\n",bytes.len(),begin+1).into_bytes();
    article.extend_from_slice(&encoded(data));
    article.extend_from_slice(
        format!(
            "=yend size={} part={number} pcrc32={:08x}{}\r\n",
            data.len(),
            yenc::crc32(data),
            if number == total {
                format!(" crc32={:08x}", yenc::crc32(bytes))
            } else {
                String::new()
            }
        )
        .as_bytes(),
    );
    yenc::decode(&article).unwrap()
}
fn create(path: &Path, src: &[u8]) -> Workspace {
    Workspace::create(path, src, 0, SERVER, 1 << 30).unwrap()
}
fn open(path: &Path, src: &[u8], ro: bool) -> Workspace {
    Workspace::open(path, src, 0, SERVER, 1 << 30, ro).unwrap()
}
fn accepted(w: &mut Workspace, p: &Part) {
    w.accept(p, &format!("part{}@fixture.test", p.number()))
        .unwrap();
}
fn header(path: &Path) -> (Value, Vec<u8>) {
    let b = fs::read(path).unwrap();
    let n = u64::from_le_bytes(b[8..16].try_into().unwrap()) as usize;
    (
        json::parse(std::str::from_utf8(&b[24..24 + n]).unwrap()).unwrap(),
        b[24 + n..b.len() - 32].to_vec(),
    )
}
fn write_frame(path: &Path, magic: &[u8; 8], v: &Value, data: &[u8]) {
    let h = json::stringify(v).into_bytes();
    let mut b = magic.to_vec();
    b.extend_from_slice(&(h.len() as u64).to_le_bytes());
    b.extend_from_slice(&(data.len() as u64).to_le_bytes());
    b.extend_from_slice(&h);
    b.extend_from_slice(data);
    b.extend_from_slice(&sha256(&b));
    fs::write(path, b).unwrap();
}
#[test]
fn verified_receipts_resume_without_replay_and_assemble_exact_private_bytes() {
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(3);
    let data = b"Original synthetic media multipart fixture";
    let mut w = create(&root, &src);
    accepted(&mut w, &part(data, 2, 3, 10, 20));
    accepted(&mut w, &part(data, 1, 3, 0, 10));
    assert!(w.available_file().is_none());
    assert!(w.assemble().is_err());
    drop(w);
    let mut w = open(&root, &src, false);
    assert_eq!(w.report().get("verified_parts").unwrap().as_u64(), Some(2));
    accepted(&mut w, &part(data, 3, 3, 20, data.len()));
    let output = w.assemble().unwrap();
    assert_eq!(fs::read(&output).unwrap(), data);
    assert!(output.starts_with(root.join("output")));
    assert_eq!(w.assemble().unwrap(), output);
    drop(w);
    let w = open(&root, &src, true);
    assert_eq!(w.available_file(), Some(output));
    let public = json::stringify(&w.report());
    for private in [
        "fixture.test",
        "Private original",
        "original fixture.mp4",
        "server_binding",
    ] {
        assert!(!public.contains(private));
    }
}
#[test]
fn receipt_identity_is_bound_to_inventory_and_conflicts_never_overwrite() {
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(2);
    let data = b"Original fixture content";
    let mut w = create(&root, &src);
    let p = part(data, 1, 2, 0, 10);
    assert!(w.accept(&p, "foreign@fixture.test").is_err());
    assert!(!root.join("part-00001.bin").exists());
    accepted(&mut w, &p);
    let before = fs::read(root.join("part-00001.bin")).unwrap();
    accepted(&mut w, &p);
    assert_eq!(fs::read(root.join("part-00001.bin")).unwrap(), before);
    let mut bad = data.to_vec();
    bad[0] ^= 1;
    assert!(
        w.accept(&part(&bad, 1, 2, 0, 10), "part1@fixture.test")
            .is_err()
    );
    assert_eq!(fs::read(root.join("part-00001.bin")).unwrap(), before);
    assert!(
        w.accept(&part(data, 1, 1, 0, data.len()), "part1@fixture.test")
            .is_err()
    );
}
#[test]
fn source_provider_file_index_and_size_limits_cannot_be_rebound() {
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(2);
    drop(create(&root, &src));
    let before = fs::read(root.join("workspace.bin")).unwrap();
    let mut different = src.clone();
    different.extend_from_slice(b" ");
    assert!(Workspace::open(&root, &different, 0, SERVER, 1 << 30, true).is_err());
    assert!(Workspace::open(&root, &src, 0, &"b".repeat(64), 1 << 30, true).is_err());
    assert!(Workspace::open(&root, &src, 1, SERVER, 1 << 30, true).is_err());
    assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 29, true).is_err());
    assert_eq!(fs::read(root.join("workspace.bin")).unwrap(), before);
    let small = d.0.join("small");
    let mut w = Workspace::create(&small, &src, 0, SERVER, 5).unwrap();
    assert!(
        w.accept(&part(b"Original fixture", 1, 2, 0, 5), "part1@fixture.test")
            .is_err()
    );
}
#[test]
fn competing_owners_and_read_only_operations_do_not_mutate_state() {
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(1);
    let mut w = create(&root, &src);
    assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
    assert!(Workspace::create(&root, &src, 0, SERVER, 1 << 30).is_err());
    let p = part(b"Original media", 1, 1, 0, 14);
    accepted(&mut w, &p);
    drop(w);
    let descriptor = fs::read(root.join("workspace.bin")).unwrap();
    let receipt = fs::read(root.join("part-00001.bin")).unwrap();
    let mut ro = open(&root, &src, true);
    assert!(ro.accept(&p, "part1@fixture.test").is_err());
    assert!(ro.assemble().is_err());
    assert_eq!(fs::read(root.join("workspace.bin")).unwrap(), descriptor);
    assert_eq!(fs::read(root.join("part-00001.bin")).unwrap(), receipt);
}
#[test]
fn corrupt_receipt_frames_and_valid_frames_with_wrong_identity_or_crc_fail_closed() {
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(1);
    let mut w = create(&root, &src);
    accepted(&mut w, &part(b"Original media", 1, 1, 0, 14));
    drop(w);
    let path = root.join("part-00001.bin");
    let original = fs::read(&path).unwrap();
    for index in [0, 8, 16, 26, original.len() - 1] {
        let mut bad = original.clone();
        bad[index] ^= 1;
        fs::write(&path, &bad).unwrap();
        assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
    fs::write(&path, &original).unwrap();
    let (v, data) = header(&path);
    for (key, value) in [
        ("binding", Value::from("b".repeat(64))),
        ("article", Value::from("b".repeat(64))),
    ] {
        let mut bad = v.clone();
        bad.insert(key, value);
        write_frame(&path, b"MYNOUP01", &bad, &data);
        assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
    }
    let mut bad = v.clone();
    bad.get_mut("part").unwrap().insert("part_crc", 1_u32);
    write_frame(&path, b"MYNOUP01", &bad, &data);
    assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
}
#[test]
fn corruption_and_unsupported_descriptor_formats_never_repair_read_only_storage() {
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(1);
    drop(create(&root, &src));
    let path = root.join("workspace.bin");
    let original = fs::read(&path).unwrap();
    for n in [0, 8, 16, 30, original.len() - 1] {
        let mut bad = original.clone();
        bad[n] ^= 1;
        fs::write(&path, &bad).unwrap();
        assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
    fs::write(&path, &original).unwrap();
    let (mut h, data) = header(&path);
    h.insert("phase", "ready");
    write_frame(&path, b"MYNOUW01", &h, &data);
    assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
}
#[test]
fn coverage_and_whole_crc_failures_publish_no_ready_output() {
    let d = Directory::new();
    let data = b"Original synthetic fixture data";
    for (end, start) in [(10, 9), (9, 10)] {
        let root = d.0.join(format!("range-{end}"));
        let src = source(2);
        let mut w = create(&root, &src);
        accepted(&mut w, &part(data, 1, 2, 0, end));
        accepted(&mut w, &part(data, 2, 2, start, data.len()));
        assert!(w.assemble().is_err());
        assert!(w.available_file().is_none());
        assert_eq!(fs::read_dir(root.join("output")).unwrap().count(), 0);
    }
    let root = d.0.join("crc");
    let src = source(2);
    let mut w = create(&root, &src);
    let mut corrupt = data.to_vec();
    corrupt[0] ^= 1;
    accepted(&mut w, &part(&corrupt, 1, 2, 0, 10));
    accepted(&mut w, &part(data, 2, 2, 10, data.len()));
    assert!(w.assemble().is_err());
    assert!(w.available_file().is_none());
    assert_eq!(fs::read_dir(root.join("output")).unwrap().count(), 0);
}
#[test]
fn crash_after_prepared_proof_recovers_checked_temp_or_published_output_without_writes_in_preview()
{
    for renamed in [false, true] {
        let d = Directory::new();
        let root = d.0.join("workspace");
        let src = source(1);
        let data = b"Original media";
        let mut w = create(&root, &src);
        accepted(&mut w, &part(data, 1, 1, 0, data.len()));
        let output = w.assemble().unwrap();
        drop(w);
        let descriptor = root.join("workspace.bin");
        let (mut h, _) = header(&descriptor);
        h.insert("phase", "prepared");
        let temp_id = h
            .get("output")
            .unwrap()
            .get("temp")
            .unwrap()
            .as_str()
            .unwrap();
        let temp = root.join(format!(".assembly-{temp_id}.tmp"));
        if !renamed {
            fs::rename(&output, &temp).unwrap();
        }
        write_frame(&descriptor, b"MYNOUW01", &h, &[]);
        let before = fs::read(&descriptor).unwrap();
        let w = open(&root, &src, true);
        assert!(w.available_file().is_none());
        assert_eq!(fs::read(&descriptor).unwrap(), before);
        assert_eq!(output.exists(), renamed);
        drop(w);
        let w = open(&root, &src, false);
        assert_eq!(w.available_file(), Some(output.clone()));
        assert_eq!(fs::read(&output).unwrap(), data);
        assert_eq!(w.report().get("phase").unwrap().as_str(), Some("ready"));
    }
}
#[test]
fn completed_output_corruption_and_foreign_existing_outputs_are_preserved_and_rejected() {
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(1);
    let mut w = create(&root, &src);
    accepted(&mut w, &part(b"Original media", 1, 1, 0, 14));
    let expected = root.join("output/original fixture.mp4");
    fs::write(&expected, b"foreign private data").unwrap();
    assert!(w.assemble().is_err());
    assert_eq!(fs::read(&expected).unwrap(), b"foreign private data");
    fs::remove_file(&expected).unwrap();
    let output = w.assemble().unwrap();
    drop(w);
    fs::write(&output, b"Corrupt media!").unwrap();
    assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"Corrupt media!");
}
#[cfg(unix)]
#[test]
fn private_directories_receipts_and_outputs_reject_public_permissions_symlinks_and_hardlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let d = Directory::new();
    let root = d.0.join("workspace");
    let src = source(1);
    let mut w = create(&root, &src);
    accepted(&mut w, &part(b"Original media", 1, 1, 0, 14));
    w.assemble().unwrap();
    drop(w);
    assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
    for relative in [
        ".owner",
        "workspace.bin",
        "part-00001.bin",
        "output/original fixture.mp4",
    ] {
        let path = root.join(relative);
        let saved = d.0.join("saved");
        let bytes = fs::read(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        fs::rename(&path, &saved).unwrap();
        symlink(&saved, &path).unwrap();
        assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
        assert_eq!(fs::read(&saved).unwrap(), bytes);
        fs::remove_file(&path).unwrap();
        fs::rename(&saved, &path).unwrap();
        fs::hard_link(&path, &saved).unwrap();
        assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
        fs::remove_file(&saved).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Workspace::open(&root, &src, 0, SERVER, 1 << 30, true).is_err());
}
#[test]
fn disk_assembly_exceeds_in_memory_limit_with_one_bounded_part_at_a_time() {
    let d = Directory::new();
    let root = d.0.join("large");
    let count = 65_u32;
    let chunk = vec![0_u8; 1024 * 1024];
    let size = chunk.len() as u64 * u64::from(count);
    assert!(size > yenc::MAX_ASSEMBLED_BYTES as u64);
    let mut whole = Crc32::default();
    for _ in 0..count {
        whole.update(&chunk);
    }
    let expected = whole.finish();
    let src = source(count);
    let mut w = create(&root, &src);
    for n in 1..=count {
        let begin = u64::from(n - 1) * chunk.len() as u64;
        let end = begin + chunk.len() as u64;
        let mut article=format!("=ybegin part={n} total={count} line=128 size={size} name=large.mp4\r\n=ypart begin={} end={end}\r\n",begin+1).into_bytes();
        for _ in 0..chunk.len() / 128 {
            article.extend_from_slice(&[b'*'; 128]);
            article.extend_from_slice(b"\r\n");
        }
        article.extend_from_slice(
            format!(
                "=yend size={} part={n} pcrc32={:08x}{}\r\n",
                chunk.len(),
                yenc::crc32(&chunk),
                if n == count {
                    format!(" crc32={expected:08x}")
                } else {
                    String::new()
                }
            )
            .as_bytes(),
        );
        let p = yenc::decode(&article).unwrap();
        accepted(&mut w, &p);
    }
    let output = w.assemble().unwrap();
    assert_eq!(fs::metadata(&output).unwrap().len(), size);
    drop(w);
    let w = open(&root, &src, true);
    assert_eq!(w.available_file(), Some(output));
}
