//! Atomic import preserves the download and refuses to overwrite existing files.

use crate::Result;
use crate::store::{Request, private_options, reject_symlinks, sync_directory};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
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

fn validate_revision(revision: &str) -> Result<()> {
    if revision.len() != 32 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("import revision must contain exactly 32 hexadecimal characters".to_owned());
    }
    Ok(())
}

fn target(
    source: &Path,
    library: &Path,
    request: &Request,
    revision: Option<&str>,
) -> Result<PathBuf> {
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
    let suffix = revision.map_or_else(String::new, |value| format!(" [mynou-{value}]"));
    if request.kind == "episode" || (request.kind == "series" && request.episode > 0) {
        let season = format!("Season {:02}", request.season);
        let filename = format!(
            "{title} - S{:02}E{:02}{suffix}.{extension}",
            request.season, request.episode
        );
        Ok(library.join(title).join(season).join(filename))
    } else {
        let name = if request.year > 0 {
            format!("{title} ({})", request.year)
        } else {
            title
        };
        Ok(library
            .join(&name)
            .join(format!("{name}{suffix}.{extension}")))
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
    import_destination(source, library, destination, active, true)
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
    import_to_target(source, library, request, None, active)
}

/// Imports an upgrade beside the existing media without changing its title or
/// directory. The revision must be a 32-character hexadecimal job identifier.
/// Reusing a revision is safe only when the destination contains identical bytes.
pub fn import_versioned_file(
    source: &Path,
    library: &Path,
    request: &Request,
    revision: &str,
) -> Result<PathBuf> {
    import_versioned_file_cancellable(source, library, request, revision, &AtomicBool::new(true))
}

pub fn import_versioned_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    revision: &str,
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, Some(revision), active)
}

fn import_to_target(
    source: &Path,
    library: &Path,
    request: &Request,
    revision: Option<&str>,
    active: &AtomicBool,
) -> Result<PathBuf> {
    if let Some(revision) = revision {
        validate_revision(revision)?;
    }
    let destination = target(source, library, request, revision)?;
    import_destination(source, library, destination, active, false)
}

fn import_destination(
    source: &Path,
    library: &Path,
    destination: PathBuf,
    active: &AtomicBool,
    copy_source: bool,
) -> Result<PathBuf> {
    #[cfg(not(unix))]
    return Err("atomic import currently requires a Unix system".to_owned());

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
        lock.try_lock()
            .map_err(|error| format!("another import is in progress: {error}"))?;
        let directory = destination
            .parent()
            .ok_or_else(|| "missing destination directory".to_owned())?;
        create_directory(directory)?;
        if destination
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            return if same_bytes(&mut input, &destination, active)? {
                Ok(destination)
            } else {
                Err("library already contains a different file".to_owned())
            };
        }
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
        let result = (|| {
            if !copy_source && fs::hard_link(source, &temporary).is_ok() {
                owns_temporary = true;
                if !same_bytes(&mut input, &temporary, active)? {
                    return Err("source changed during import".to_owned());
                }
            } else {
                let mut output = private_options()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .map_err(|error| format!("cannot access temporary file: {error}"))?;
                owns_temporary = true;
                input
                    .seek(SeekFrom::Start(0))
                    .map_err(|error| error.to_string())?;
                let mut copied = 0_u64;
                let mut buffer = [0_u8; 131_072];
                loop {
                    active_import(active)?;
                    let count = input
                        .read(&mut buffer)
                        .map_err(|error| format!("cannot read media: {error}"))?;
                    if count == 0 {
                        break;
                    }
                    output
                        .write_all(&buffer[..count])
                        .map_err(|error| format!("cannot copy media: {error}"))?;
                    copied = copied
                        .checked_add(count as u64)
                        .ok_or_else(|| "media too large".to_owned())?;
                }
                if copied != input.metadata().map_err(|error| error.to_string())?.len() {
                    return Err("media size changed during import".to_owned());
                }
                output
                    .flush()
                    .and_then(|_| output.sync_all())
                    .map_err(|error| error.to_string())?;
                if !same_bytes(&mut input, &temporary, active)? {
                    return Err("source changed during copying".to_owned());
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
                        return Err("library already contains a different file".to_owned());
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
