//! Atomic import preserves the download and refuses to overwrite existing files.

use crate::Result;
use crate::crypto::Sha256;
use crate::store::{Request, private_options, reject_symlinks, sync_directory, try_lock_file};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

fn filename_part(value: &str) -> Result<String> {
    let mut result = String::with_capacity(value.len().min(180));
    for character in value.chars() {
        let character = if character.is_control() || "/\\:*?\"<>|".contains(character) {
            '_'
        } else {
            character
        };
        if result.len() + character.len_utf8() > 180 {
            break;
        }
        result.push(character);
    }
    let result = result.trim().trim_matches('.').trim().to_owned();
    if result.is_empty() || result == "." || result == ".." {
        return Err("title cannot produce a filename".to_owned());
    }
    Ok(result)
}

fn validate_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("empty path or directory traversal not allowed".to_owned());
    }
    reject_symlinks(path)
}

/// Runtime availability proof only: journal replay must not depend on disk state.
/// Metadata checks cannot prevent another process changing a path afterward.
pub(crate) fn check_retained_imports(
    job: &crate::store::Job,
    config: &crate::config::Config,
) -> Result<()> {
    for file in &job.imports {
        let path = Path::new(file);
        let root = import_root(job, config);
        let under_root = path != root && path.starts_with(root);
        let confined = if let Some(shared) = &job.shared_file {
            // A shared import keeps its recorded destination across config edits.
            file == &shared.import_path && (job.requester.is_none() || under_root)
        } else {
            under_root
        };
        if !path.is_absolute()
            || !confined
            || validate_path(path).is_err()
            || !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        {
            return Err("Recorded import is missing, unsafe or outside its destination; restore the original regular file and retry".into());
        }
    }
    Ok(())
}

/// The import root a job's files must stay under.
fn import_root<'a>(job: &'a crate::store::Job, config: &'a crate::config::Config) -> &'a Path {
    match (&job.requester, job.request.kind == "movie") {
        (Some(requester), true) => Path::new(&requester.capture.movies_root),
        (Some(requester), false) => Path::new(&requester.capture.series_root),
        (None, true) => Path::new(&config.movies_root),
        (None, false) => Path::new(&config.series_root),
    }
}

/// The files of an earlier version that are still on disk, with their size:
/// regular files under the job's import root, reached without symbolic links,
/// that are neither named by nor the same file as an import of `current`.
pub(crate) fn earlier_files(
    job: &crate::store::Job,
    current: &crate::store::Job,
    config: &crate::config::Config,
) -> Vec<(String, u64)> {
    let root = import_root(job, config);
    let kept: Vec<(u64, u64)> = current
        .imports
        .iter()
        .filter_map(|file| fs::metadata(file).ok())
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .collect();
    job.imports
        .iter()
        .filter(|file| !current.imports.contains(file))
        .filter_map(|file| {
            let path = Path::new(file);
            if !path.is_absolute()
                || path == root
                || !path.starts_with(root)
                || validate_path(path).is_err()
            {
                return None;
            }
            let metadata = fs::symlink_metadata(path).ok()?;
            (metadata.is_file() && !kept.contains(&(metadata.dev(), metadata.ino())))
                .then(|| (file.clone(), metadata.len()))
        })
        .collect()
}

/// Deletes the files `earlier_files` lists, only while every file of
/// `current` is on disk, and returns the bytes they held.
pub(crate) fn delete_earlier_files(
    job: &crate::store::Job,
    current: &crate::store::Job,
    config: &crate::config::Config,
) -> Result<u64> {
    // A listing may be stale: never delete what may now be the only copy.
    if !crate::library::import_exists(current) {
        return Err("the version that replaced this one is missing from disk".to_owned());
    }
    let mut freed = 0_u64;
    for (file, length) in earlier_files(job, current, config) {
        fs::remove_file(&file).map_err(|error| format!("cannot delete {file}: {error}"))?;
        freed += length;
        if let Some(directory) = Path::new(&file).parent() {
            sync_directory(directory)?;
        }
    }
    Ok(freed)
}

fn create_directory(path: &Path) -> Result<()> {
    validate_path(path)?;
    if path.exists() {
        if !fs::metadata(path)
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            return Err("library directory is a file".to_owned());
        }
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_directory(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_path(path)?;
            if !fs::metadata(path)
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                return Err("library directory is a file".to_owned());
            }
        }
        Err(error) => {
            return Err(format!("cannot create library directory: {error}"));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    sync_directory(path)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        sync_directory(parent)?;
    }
    Ok(())
}

fn target(source: &Path, library: &Path, request: &Request) -> Result<PathBuf> {
    request.validate()?;
    let title = filename_part(&request.title)?;
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| "media extension missing or not UTF-8".to_owned())?;
    if extension.is_empty()
        || extension.len() > 16
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err("invalid media extension".to_owned());
    }
    if request.kind == "episode" || (request.kind == "series" && request.episode > 0) {
        let season = format!("Season {:02}", request.season);
        let filename = format!(
            "{title} - S{:02}E{:02}.{extension}",
            request.season, request.episode
        );
        Ok(library.join(title).join(season).join(filename))
    } else {
        let name = if request.year > 0 {
            format!("{title} ({})", request.year)
        } else {
            title
        };
        Ok(library.join(&name).join(format!("{name}.{extension}")))
    }
}

fn active_import(active: &AtomicBool) -> Result<()> {
    if active.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("import cancelled or processing lease invalidated".to_owned())
    }
}

/// Plex's consecutive-episode range names one physical file for every owner.
pub(crate) fn shared_target(library: &Path, file: &crate::pack::SharedFile) -> Result<PathBuf> {
    let title = filename_part(&file.title)?;
    let extension = Path::new(&file.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or("Shared media extension is missing")?;
    let suffix = format!(
        " - S{:02}E{:02}-E{:02} [mynou-{}].{extension}",
        file.season,
        file.first_episode,
        file.last_episode,
        file.id()
    );
    let mut name = title.clone();
    while name.len() + suffix.len() > 255 {
        name.pop();
    }
    Ok(library
        .join(title)
        .join(format!("Season {:02}", file.season))
        .join(format!("{name}{suffix}")))
}

pub(crate) fn import_shared_file_cancellable(
    source: &Path,
    library: &Path,
    file: &crate::pack::SharedFile,
    active: &AtomicBool,
) -> Result<PathBuf> {
    file.validate()?;
    let destination = shared_target(library, file)?;
    if destination != Path::new(&file.import_path) {
        return Err(
            "Shared library root changed; the recorded destination cannot be reassigned".into(),
        );
    }
    // Keep the native payload inode private. Only the temporary import and
    // final library name share a link during atomic publication.
    import_destination(source, library, destination, &[], active, true)
}

/// Device, inode, length and modification time of an open file: what tells
/// that it is still the same file, unchanged.
type Fingerprint = (u64, u64, u64, i64, i64);

fn fingerprint(file: &File) -> Result<Fingerprint> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

/// Whether `directory` is on the volume of `file`.
fn same_device(file: &File, directory: &Path) -> Result<bool> {
    let file = file.metadata().map_err(|error| error.to_string())?;
    let directory = fs::metadata(directory).map_err(|error| error.to_string())?;
    Ok(file.dev() == directory.dev())
}

/// Whether `file` lives on an APFS volume, the only one whose copies are
/// clones. Other volumes that take hard links (HFS+, NFS) have no clones:
/// `fs::copy` would copy every byte there, without cancellation or a check.
/// Each APFS mount point listed by `mount` is compared by device, which
/// also finds the data volume behind firmlinks such as /Users.
fn on_apfs(file: &File) -> Result<bool> {
    let device = file.metadata().map_err(|error| error.to_string())?.dev();
    let Ok(output) = std::process::Command::new("/sbin/mount").output() else {
        return Ok(false);
    };
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(apfs_mount_point)
        .any(|point| fs::metadata(point).is_ok_and(|metadata| metadata.dev() == device)))
}

/// The mount point of a `mount` line such as
/// `/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled)`, when the
/// volume is APFS.
fn apfs_mount_point(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(" on ")?;
    let (point, options) = rest.rsplit_once(" (")?;
    (options.split([',', ')']).next()? == "apfs").then_some(point)
}

/// The length and SHA-256 of a regular file, read without following links.
fn file_digest(path: &Path, active: &AtomicBool) -> Result<Option<(u64, [u8; 32])>> {
    validate_path(path)?;
    let mut file = private_options()
        .read(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        active_import(active)?;
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(Some((length, hasher.finalize())));
        }
        hasher.update(&buffer[..count]);
        length += count as u64;
    }
}

fn same_bytes(source: &mut File, destination: &Path, active: &AtomicBool) -> Result<bool> {
    active_import(active)?;
    validate_path(destination)?;
    if !fs::symlink_metadata(destination)
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Ok(false);
    }
    let mut destination = private_options()
        .read(true)
        .open(destination)
        .map_err(|error| error.to_string())?;
    let source_metadata = source.metadata().map_err(|error| error.to_string())?;
    let destination_metadata = destination.metadata().map_err(|error| error.to_string())?;
    if !destination_metadata.is_file() || source_metadata.len() != destination_metadata.len() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if source_metadata.dev() == destination_metadata.dev()
            && source_metadata.ino() == destination_metadata.ino()
        {
            return Ok(true);
        }
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut source_buffer = [0_u8; 65_536];
    let mut destination_buffer = [0_u8; 65_536];
    loop {
        active_import(active)?;
        let length = source
            .read(&mut source_buffer)
            .map_err(|error| error.to_string())?;
        if length == 0 {
            return Ok(true);
        }
        destination
            .read_exact(&mut destination_buffer[..length])
            .map_err(|error| error.to_string())?;
        if source_buffer[..length] != destination_buffer[..length] {
            return Ok(false);
        }
    }
}

/// Creates a hard link when the filesystems allow it, otherwise copies to a
/// temporary file. Final publication uses `hard_link` to avoid overwriting
/// a file created by a concurrent process.
pub fn import_file(source: &Path, library: &Path, request: &Request) -> Result<PathBuf> {
    import_file_cancellable(source, library, request, &AtomicBool::new(true))
}

pub fn import_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, &[], active, false)
}

/// Imports a native torrent payload by copy. A hard link would give the payload
/// a second name, and the native client rejects any media file whose link count
/// is not one, so it could no longer seed or verify the download.
pub(crate) fn import_native_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, &[], active, true)
}

/// Imports an upgrade in place of the version it replaces: the file takes the
/// usual name, and when one of `replaced` already has that name, the new file
/// takes its place in one step. A file of another name (another extension) is
/// left for the caller to delete. No other existing file is ever overwritten.
pub fn import_upgrade_file(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
) -> Result<PathBuf> {
    import_upgrade_file_cancellable(source, library, request, replaced, &AtomicBool::new(true))
}

pub fn import_upgrade_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, replaced, active, false)
}

/// The upgrade counterpart of [`import_native_file_cancellable`].
pub(crate) fn import_native_upgrade_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, replaced, active, true)
}

fn import_to_target(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
    active: &AtomicBool,
    copy_source: bool,
) -> Result<PathBuf> {
    let destination = target(source, library, request)?;
    import_destination(source, library, destination, replaced, active, copy_source)
}

/// Removes `.mynou-*.tmp` regular files that an interrupted import left in
/// `directory`. The caller holds the library import lock, so no live import can
/// own one. Best effort: a failure here must not fail the import.
#[cfg(unix)]
fn remove_stale_temporaries(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".mynou-") || !name.ends_with(".tmp") {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            let _ = fs::remove_file(&path);
        }
    }
}

fn import_destination(
    source: &Path,
    library: &Path,
    destination: PathBuf,
    replaced: &[String],
    active: &AtomicBool,
    copy_source: bool,
) -> Result<PathBuf> {
    // Only a regular file the replaced version recorded may be overwritten.
    let replaceable = |destination: &Path| {
        replaced.iter().any(|file| Path::new(file) == destination)
            && fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.is_file())
    };
    #[cfg(unix)]
    {
        active_import(active)?;
        validate_path(source)?;
        validate_path(library)?;
        if !fs::symlink_metadata(source)
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("media source must be a regular file".to_owned());
        }
        let mut input = private_options()
            .read(true)
            .open(source)
            .map_err(|error| format!("cannot access media source: {error}"))?;
        if !input
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("media source must be a regular file".to_owned());
        }
        create_directory(library)?;
        let lock_path = library.join(".organizer.lock");
        validate_path(&lock_path)?;
        if let Ok(metadata) = fs::symlink_metadata(&lock_path)
            && !metadata.is_file()
        {
            return Err("import lock must be a regular file".to_owned());
        }
        let lock = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| error.to_string())?;
        if !lock
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("import lock must be a regular file".to_owned());
        }
        try_lock_file(&lock).map_err(|error| format!("another import is in progress: {error}"))?;
        let directory = destination
            .parent()
            .ok_or_else(|| "missing destination directory".to_owned())?;
        create_directory(directory)?;
        if destination
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            if same_bytes(&mut input, &destination, active)? {
                return Ok(destination);
            }
            if !replaceable(&destination) {
                return Err("library already contains a different file".to_owned());
            }
        }
        remove_stale_temporaries(directory);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = directory.join(format!(
            ".mynou-{}-{timestamp}-{}.tmp",
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let mut owns_temporary = false;
        // A copy on the source's own APFS volume is a copy-on-write clone:
        // instant, and sharing the source's blocks until either file changes.
        // Any other volume keeps the cancellable, verified copy below.
        let clone = copy_source && same_device(&input, directory)? && on_apfs(&input)?;
        let result = (|| {
            if !copy_source && fs::hard_link(source, &temporary).is_ok() {
                owns_temporary = true;
                if !same_bytes(&mut input, &temporary, active)? {
                    return Err("source changed during import".to_owned());
                }
            } else if clone {
                let before = fingerprint(&input)?;
                owns_temporary = true;
                // `fs::copy` clones with `fclonefileat` on macOS.
                fs::copy(source, &temporary)
                    .map_err(|error| format!("cannot copy media: {error}"))?;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                    .map_err(|error| error.to_string())?;
                // The path still names the file that was opened and checked,
                // unchanged, and the copy has its length.
                let named = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
                let copied = fs::symlink_metadata(&temporary).map_err(|error| error.to_string())?;
                if !named.is_file()
                    || (named.dev(), named.ino()) != (before.0, before.1)
                    || fingerprint(&input)? != before
                    || !copied.is_file()
                    || copied.len() != before.2
                {
                    return Err("source changed during copying".to_owned());
                }
                private_options()
                    .read(true)
                    .open(&temporary)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| error.to_string())?;
            } else {
                let mut output = private_options()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .map_err(|error| format!("cannot access temporary file: {error}"))?;
                owns_temporary = true;
                let before = fingerprint(&input)?;
                input
                    .seek(SeekFrom::Start(0))
                    .map_err(|error| error.to_string())?;
                // The bytes are hashed as they are copied, then the copy is
                // read back once: the source is not read a second time.
                let mut copied = 0_u64;
                let mut hasher = Sha256::new();
                let mut buffer = vec![0_u8; 1 << 20];
                loop {
                    active_import(active)?;
                    let count = input
                        .read(&mut buffer)
                        .map_err(|error| format!("cannot read media: {error}"))?;
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                    output
                        .write_all(&buffer[..count])
                        .map_err(|error| format!("cannot copy media: {error}"))?;
                    copied = copied
                        .checked_add(count as u64)
                        .ok_or_else(|| "media too large".to_owned())?;
                }
                if copied != before.2 {
                    return Err("media size changed during import".to_owned());
                }
                output
                    .flush()
                    .and_then(|_| output.sync_all())
                    .map_err(|error| error.to_string())?;
                drop(output);
                if fingerprint(&input)? != before {
                    return Err("source changed during copying".to_owned());
                }
                if file_digest(&temporary, active)? != Some((copied, hasher.finalize())) {
                    return Err("the copy differs from the source".to_owned());
                }
            }
            input
                .sync_all()
                .map_err(|error| format!("cannot synchronize media: {error}"))?;
            active_import(active)?;
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !same_bytes(&mut input, &destination, active)? {
                        if !replaceable(&destination) {
                            return Err("library already contains a different file".to_owned());
                        }
                        // The earlier version's file becomes the new one at once.
                        fs::rename(&temporary, &destination)
                            .map_err(|error| format!("cannot replace the earlier file: {error}"))?;
                        owns_temporary = false;
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "atomic publication without overwriting is unsupported on this filesystem: {error}"
                    ));
                }
            }
            sync_directory(directory)?;
            Ok(destination.clone())
        })();
        let cleanup = if owns_temporary {
            fs::remove_file(&temporary)
        } else {
            Ok(())
        };
        if result.is_ok() {
            cleanup.map_err(|error| format!("cannot clean up import: {error}"))?;
            sync_directory(directory)?;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mynou-organizer-{}-{}",
                std::process::id(),
                TEMPORARY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn request() -> Request {
        Request {
            kind: "movie".to_owned(),
            title: "Movie".to_owned(),
            year: 2026,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            runtime_minutes: None,
            source_numbering: None,
            tmdb_id: None,
        }
    }

    #[test]
    fn shared_import_keeps_payload_private_and_refuses_to_overwrite_a_different_copy() {
        use std::os::unix::fs::MetadataExt;
        let directory = Directory::new();
        let source = directory.0.join("source.mp4");
        fs::write(&source, b"original bytes").unwrap();
        let library = directory.0.join("library");
        let mut file = crate::pack::SharedFile {
            torrent_id: "a".repeat(40),
            file_path: "Pack/shared.mp4".into(),
            tmdb_id: 42,
            title: "Fixture Series".into(),
            year: 2024,
            season: 1,
            first_episode: 1,
            last_episode: 2,
            import_path: String::new(),
        };
        file.import_path = shared_target(&library, &file)
            .unwrap()
            .to_str()
            .unwrap()
            .into();
        let active = AtomicBool::new(true);
        let destination =
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap();
        assert_eq!(fs::metadata(&source).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(&destination).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&destination).unwrap().ino()
        );
        assert_eq!(
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap(),
            destination
        );
        fs::write(&source, b"modified bytes").unwrap();
        assert!(
            import_shared_file_cancellable(&source, &library, &file, &active)
                .unwrap_err()
                .contains("different file")
        );
        assert_eq!(fs::read(&destination).unwrap(), b"original bytes");
    }

    #[test]
    fn shared_import_copies_payloads_larger_than_the_copy_buffer() {
        let directory = Directory::new();
        let source = directory.0.join("source.mp4");
        let bytes: Vec<u8> = (0..=255).cycle().take(150_000).collect();
        fs::write(&source, &bytes).unwrap();
        let library = directory.0.join("library");
        let mut file = crate::pack::SharedFile {
            torrent_id: "b".repeat(40),
            file_path: "Pack/large.mp4".into(),
            tmdb_id: 43,
            title: "Large Series".into(),
            year: 2024,
            season: 1,
            first_episode: 1,
            last_episode: 2,
            import_path: String::new(),
        };
        file.import_path = shared_target(&library, &file)
            .unwrap()
            .to_str()
            .unwrap()
            .into();
        let active = AtomicBool::new(true);
        let destination =
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap();
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert_eq!(
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap(),
            destination
        );
    }

    #[test]
    fn import_is_idempotent_and_never_overwrites_different_content() {
        let directory = Directory::new();
        let source = directory.0.join("source.mkv");
        fs::write(&source, b"original content").unwrap();
        let library = directory.0.join("library");
        let destination = import_file(&source, &library, &request()).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"original content");
        assert_eq!(fs::read(&destination).unwrap(), b"original content");
        assert_eq!(
            import_file(&source, &library, &request()).unwrap(),
            destination
        );
        let different = directory.0.join("other.mkv");
        fs::write(&different, b"different content").unwrap();
        assert!(
            import_file(&different, &library, &request())
                .unwrap_err()
                .contains("different")
        );
        assert_eq!(fs::read(&destination).unwrap(), b"original content");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&source).unwrap().ino(),
                fs::metadata(&destination).unwrap().ino()
            );
        }
    }

    #[test]
    fn native_payload_imports_copy_and_keep_the_payload_inode_private() {
        use std::os::unix::fs::MetadataExt;
        let directory = Directory::new();
        let source = directory.0.join("payload.mkv");
        fs::write(&source, b"payload bytes").unwrap();
        let library = directory.0.join("library");
        let active = AtomicBool::new(true);
        let destination =
            import_native_file_cancellable(&source, &library, &request(), &active).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"payload bytes");
        assert_eq!(fs::metadata(&source).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(&destination).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&destination).unwrap().ino()
        );
        assert_eq!(
            import_native_file_cancellable(&source, &library, &request(), &active).unwrap(),
            destination
        );
        let better = directory.0.join("better.mkv");
        fs::write(&better, b"better payload").unwrap();
        let replaced = [destination.to_string_lossy().into_owned()];
        let upgrade = import_native_upgrade_file_cancellable(
            &better,
            &library,
            &request(),
            &replaced,
            &active,
        )
        .unwrap();
        assert_eq!(upgrade, destination);
        assert_eq!(fs::read(&upgrade).unwrap(), b"better payload");
        assert_eq!(fs::metadata(&better).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(&upgrade).unwrap().nlink(), 1);
    }

    #[test]
    fn import_reclaims_stale_temporaries_and_keeps_everything_else() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let source = directory.0.join("payload.mkv");
        fs::write(&source, b"payload bytes").unwrap();
        let library = directory.0.join("library");
        let destination_directory = target(&source, &library, &request())
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        fs::create_dir_all(&destination_directory).unwrap();
        let stale = destination_directory.join(".mynou-1-2-3.tmp");
        let other_tmp = destination_directory.join("notes.tmp");
        let other_hidden = destination_directory.join(".mynou-keep.txt");
        let link = destination_directory.join(".mynou-9-9-9.tmp.link.tmp");
        fs::write(&stale, vec![7_u8; 4096]).unwrap();
        fs::write(&other_tmp, b"keep").unwrap();
        fs::write(&other_hidden, b"keep").unwrap();
        symlink(&source, &link).unwrap();
        let active = AtomicBool::new(true);
        let destination =
            import_native_file_cancellable(&source, &library, &request(), &active).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"payload bytes");
        assert!(!stale.exists());
        assert!(other_tmp.exists());
        assert!(other_hidden.exists());
        let link_type = fs::symlink_metadata(&link).unwrap().file_type();
        assert!(link_type.is_symlink());
        assert_eq!(fs::read(&source).unwrap(), b"payload bytes");
    }

    #[test]
    fn episode_layout_and_title_sanitization_stay_inside_library() {
        let directory = Directory::new();
        let source = directory.0.join("source.mp4");
        fs::write(&source, b"media").unwrap();
        let library = directory.0.join("library");
        let mut request = request();
        request.kind = "episode".to_owned();
        request.title = "../../ Series / danger".to_owned();
        request.season = 2;
        request.episode = 7;
        let destination = import_file(&source, &library, &request).unwrap();
        assert!(destination.starts_with(&library));
        assert_eq!(
            destination.parent().unwrap().file_name().unwrap(),
            "Season 02"
        );
        assert!(
            destination
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("S02E07.mp4")
        );
        assert!(
            destination
                .components()
                .all(|part| part != Component::ParentDir)
        );
    }

    #[test]
    fn cancelled_import_has_no_filesystem_effect_and_preserves_source() {
        let directory = Directory::new();
        let source = directory.0.join("source.mkv");
        fs::write(&source, b"intact source").unwrap();
        let library = directory.0.join("library");
        let error = import_file_cancellable(&source, &library, &request(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(!library.exists());
        assert_eq!(fs::read(source).unwrap(), b"intact source");
    }

    #[cfg(unix)]
    #[test]
    fn source_and_destination_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let source = directory.0.join("source.mkv");
        fs::write(&source, b"media").unwrap();
        let alias = directory.0.join("alias.mkv");
        symlink(&source, &alias).unwrap();
        assert!(import_file(&alias, &directory.0.join("library"), &request()).is_err());
        let outside = directory.0.join("outside");
        fs::create_dir(&outside).unwrap();
        let library = directory.0.join("library");
        symlink(&outside, &library).unwrap();
        assert!(import_file(&source, &library, &request()).is_err());
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }
}
