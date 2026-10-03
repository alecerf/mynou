//! Dependency-free release packaging. Run this example from the release workflow.
use mynou::crypto::{Sha256, sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;

struct Entry {
    name: String,
    data: Vec<u8>,
    mode: u32,
}

fn fail(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn regular(path: &Path) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(fail(format!("Expected a regular file: {}", path.display())));
    }
    Ok(metadata)
}

fn archive_path(path: &Path) -> io::Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(fail("Archive paths must contain only normal components"));
        };
        let part = part
            .to_str()
            .ok_or_else(|| fail("Archive path is not UTF-8"))?;
        if part.is_empty() || part.contains(['\\', ':']) || part.chars().any(char::is_control) {
            return Err(fail("Invalid archive path component"));
        }
        parts.push(part);
        if parts.len() > 64 {
            return Err(fail("Archive path exceeds the depth limit"));
        }
    }
    if parts.is_empty() {
        return Err(fail("Empty archive path"));
    }
    let name = parts.join("/");
    if name.len() > 4096 {
        return Err(fail("Archive path exceeds the length limit"));
    }
    Ok(name)
}

fn gather(root: &Path, relative: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() {
        return Err(fail(format!(
            "Source symlinks are not allowed: {}",
            path.display()
        )));
    }
    archive_path(relative)?;
    if metadata.is_dir() {
        for child in fs::read_dir(path)? {
            let child = child?;
            let name = child.file_name();
            if matches!(name.to_str(), Some("target" | ".git" | "bin")) {
                continue;
            }
            gather(root, &relative.join(name), paths)?;
        }
    } else if metadata.is_file() {
        if paths.len() >= MAX_ENTRIES {
            return Err(fail("Source archive contains too many files"));
        }
        paths.push(relative.to_owned());
    } else {
        return Err(fail(format!(
            "Special source files are not allowed: {}",
            path.display()
        )));
    }
    Ok(())
}

fn bounded_read(path: &Path) -> io::Result<Vec<u8>> {
    if regular(path)?.len() > MAX_ENTRY_BYTES {
        return Err(fail("Archive entry exceeds the size limit"));
    }
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_ENTRY_BYTES {
        return Err(fail("Archive entry grew beyond the size limit"));
    }
    Ok(data)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    result
}

fn checksum(path: &Path) -> io::Result<String> {
    regular(path)?;
    let mut input = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = input.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    Ok(hex(&hash.finalize()))
}

fn create(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn save(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut output = create(path)?;
    output.write_all(bytes)?;
    output.sync_all()
}

fn copy(source: &Path, destination: &Path, executable: bool) -> io::Result<()> {
    let expected = regular(source)?.len();
    let input = File::open(source)?;
    let mut output = create(destination)?;
    let maximum = expected
        .checked_add(1)
        .ok_or_else(|| fail("Release input is too large"))?;
    if io::copy(&mut input.take(maximum), &mut output)? != expected {
        return Err(fail("Release input changed while being copied"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(fs::Permissions::from_mode(if executable {
            0o755
        } else {
            0o644
        }))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    output.sync_all()
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn u16le(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn u32le(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn zip(path: &Path, entries: &[Entry]) -> io::Result<()> {
    let count = u16::try_from(entries.len()).map_err(|_| fail("ZIP32 entry count exceeded"))?;
    let mut output = create(path)?;
    let mut central = Vec::new();
    let mut offset = 0u32;
    for entry in entries {
        let name = entry.name.as_bytes();
        let name_length =
            u16::try_from(name.len()).map_err(|_| fail("ZIP32 name length exceeded"))?;
        let length =
            u32::try_from(entry.data.len()).map_err(|_| fail("ZIP32 file size exceeded"))?;
        let crc = crc32(&entry.data);
        let mut local = Vec::with_capacity(30 + name.len());
        u32le(&mut local, 0x0403_4b50);
        u16le(&mut local, 20);
        u16le(&mut local, 0x0800);
        u16le(&mut local, 0);
        u16le(&mut local, 0);
        u16le(&mut local, 0x0021);
        u32le(&mut local, crc);
        u32le(&mut local, length);
        u32le(&mut local, length);
        u16le(&mut local, name_length);
        u16le(&mut local, 0);
        local.extend_from_slice(name);
        u32le(&mut central, 0x0201_4b50);
        u16le(&mut central, (3 << 8) | 20);
        // Both headers share the fields from the required version through name length.
        central.extend_from_slice(&local[4..28]);
        u16le(&mut central, 0);
        u16le(&mut central, 0);
        u16le(&mut central, 0);
        u16le(&mut central, 0);
        u32le(&mut central, (0o100000 | entry.mode) << 16);
        u32le(&mut central, offset);
        central.extend_from_slice(name);
        let local_length =
            u32::try_from(local.len()).map_err(|_| fail("ZIP32 header size exceeded"))?;
        offset = offset
            .checked_add(local_length)
            .and_then(|n| n.checked_add(length))
            .ok_or_else(|| fail("ZIP32 archive size exceeded"))?;
        output.write_all(&local)?;
        output.write_all(&entry.data)?;
    }
    let central_length =
        u32::try_from(central.len()).map_err(|_| fail("ZIP32 directory size exceeded"))?;
    offset
        .checked_add(central_length)
        .and_then(|n| n.checked_add(22))
        .ok_or_else(|| fail("ZIP32 archive size exceeded"))?;
    output.write_all(&central)?;
    let mut end = Vec::with_capacity(22);
    u32le(&mut end, 0x0605_4b50);
    u16le(&mut end, 0);
    u16le(&mut end, 0);
    u16le(&mut end, count);
    u16le(&mut end, count);
    u32le(&mut end, central_length);
    u32le(&mut end, offset);
    u16le(&mut end, 0);
    output.write_all(&end)?;
    output.sync_all()
}

fn run() -> io::Result<()> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 4 {
        return Err(fail(
            "Usage: package_release OUTPUT_DIR VERSION STATIC_BINARY DOCKER_IMAGE_TAR_GZ",
        ));
    }
    let version = arguments[1]
        .to_str()
        .ok_or_else(|| fail("Version is not UTF-8"))?;
    if version.len() > 32
        || version.split('.').count() != 3
        || version
            .split('.')
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(fail(
            "Version must contain three numeric components separated by periods",
        ));
    }
    let output = Path::new(&arguments[0]);
    let binary = Path::new(&arguments[2]);
    let image = Path::new(&arguments[3]);
    let binary_data = bounded_read(binary)?;
    regular(image)?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    for name in [
        "Cargo.toml",
        "Cargo.lock",
        "README.md",
        "AGENTS.md",
        "Dockerfile",
        "compose.yaml",
        "deploy-compose.yaml",
        "rust-toolchain.toml",
        ".gitignore",
        ".dockerignore",
        ".github",
        "config",
        "docs",
        "src",
        "tests",
        "examples",
    ] {
        gather(root, Path::new(name), &mut paths)?;
    }
    paths.sort();
    let mut entries = Vec::new();
    let mut inner_checksums = String::new();
    let mut total = binary_data.len();
    for relative in paths {
        let name = archive_path(&relative)?;
        let data = bounded_read(&root.join(relative))?;
        total = total
            .checked_add(data.len())
            .ok_or_else(|| fail("Source archive size overflow"))?;
        if total > MAX_SOURCE_BYTES {
            return Err(fail("Source archive exceeds the size limit"));
        }
        inner_checksums.push_str(&format!("{}  {name}\n", hex(&sha256(&data))));
        entries.push(Entry {
            name: format!("mynou/{name}"),
            data,
            mode: 0o644,
        });
    }
    inner_checksums.push_str(&format!("{}  bin/mynou\n", hex(&sha256(&binary_data))));
    entries.push(Entry {
        name: "mynou/bin/mynou".into(),
        data: binary_data,
        mode: 0o755,
    });
    entries.push(Entry {
        name: "mynou/SHA256SUMS".into(),
        data: inner_checksums.into_bytes(),
        mode: 0o644,
    });
    match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            if fs::read_dir(output)?.next().is_some() {
                return Err(fail(
                    "Output directory must be empty; existing files will not be overwritten",
                ));
            }
        }
        Ok(_) => return Err(fail("Output path must be an ordinary directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(output)?,
        Err(error) => return Err(error),
    }
    let prefix = format!("mynou-v{version}");
    let mut artifacts = [
        format!("{prefix}-linux-x86_64"),
        format!("{prefix}-linux-amd64-image.tar.gz"),
        format!("{prefix}-source.zip"),
    ];
    copy(binary, &output.join(&artifacts[0]), true)?;
    copy(image, &output.join(&artifacts[1]), false)?;
    zip(&output.join(&artifacts[2]), &entries)?;
    artifacts.sort();
    let mut checksums = String::new();
    for name in &artifacts {
        let line = format!("{}  {name}\n", checksum(&output.join(name))?);
        save(&output.join(format!("{name}.sha256")), line.as_bytes())?;
        checksums.push_str(&line);
    }
    save(&output.join("SHA256SUMS"), checksums.as_bytes())?;
    println!("Release artifacts written to {}", output.display());
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Release packaging failed: {error}");
        std::process::exit(1);
    }
}
