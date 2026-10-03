//! Upgrade imports preserve earlier library files and the original downloads.
#![cfg(unix)]

use mynou::organizer::{
    import_file, import_versioned_file, import_versioned_file_cancellable,
};
use mynou::store::Request;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const FIRST_REVISION: &str = "0123456789abcdef0123456789abcdef";
const SECOND_REVISION: &str = "fedcba9876543210fedcba9876543210";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        Self::in_parent(&std::env::temp_dir())
    }

    fn in_parent(parent: &Path) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = parent.join(format!(
            "mynou-versioned-import-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn movie() -> Request {
    Request {
        kind: "movie".to_owned(),
        title: "Example Movie".to_owned(),
        year: 2026,
        season: 0,
        episode: 0,
        source_path: None,
        source_url: None,
        tmdb_id: None,
    }
}

#[test]
fn independent_upgrades_preserve_the_original_and_all_downloads() {
    use std::os::unix::fs::MetadataExt;

    let directory = Directory::new();
    let original = directory.source("original.mkv", b"original 720p media");
    let first = directory.source("first.mkv", b"first 1080p media");
    let second = directory.source("second.mkv", b"second 2160p media");
    let library = directory.0.join("library");
    let request = movie();
    let old_path = import_file(&original, &library, &request).unwrap();
    let first_path = import_versioned_file(&first, &library, &request, FIRST_REVISION).unwrap();
    let second_path = import_versioned_file(&second, &library, &request, SECOND_REVISION).unwrap();

    assert_eq!(old_path.parent(), first_path.parent());
    assert_eq!(old_path.parent(), second_path.parent());
    assert_ne!(old_path, first_path);
    assert_ne!(first_path, second_path);
    assert_eq!(
        first_path.file_name().unwrap().to_str().unwrap(),
        format!("Example Movie (2026) [mynou-{FIRST_REVISION}].mkv")
    );
    assert_eq!(request, movie());
    for (source, destination, bytes) in [
        (&original, &old_path, b"original 720p media".as_slice()),
        (&first, &first_path, b"first 1080p media".as_slice()),
        (&second, &second_path, b"second 2160p media".as_slice()),
    ] {
        assert_eq!(fs::read(source).unwrap(), bytes);
        assert_eq!(fs::read(destination).unwrap(), bytes);
        let source_metadata = fs::metadata(source).unwrap();
        let destination_metadata = fs::metadata(destination).unwrap();
        assert_eq!(source_metadata.dev(), destination_metadata.dev());
        assert_eq!(source_metadata.ino(), destination_metadata.ino());
    }
}

#[test]
fn retry_accepts_identical_bytes_and_rejects_different_bytes_without_overwriting() {
    let directory = Directory::new();
    let first = directory.source("first.mkv", b"verified upgrade bytes");
    let identical = directory.source("identical.mkv", b"verified upgrade bytes");
    let different = directory.source("different.mkv", b"different upgrade data");
    let library = directory.0.join("library");
    let request = movie();
    let destination =
        import_versioned_file(&first, &library, &request, FIRST_REVISION).unwrap();

    assert_eq!(
        import_versioned_file(&first, &library, &request, FIRST_REVISION).unwrap(),
        destination
    );
    assert_eq!(
        import_versioned_file(&identical, &library, &request, FIRST_REVISION).unwrap(),
        destination
    );
    let error =
        import_versioned_file(&different, &library, &request, FIRST_REVISION).unwrap_err();
    assert!(error.contains("different"), "{error}");
    assert_eq!(fs::read(&destination).unwrap(), b"verified upgrade bytes");
    assert_eq!(fs::read(&different).unwrap(), b"different upgrade data");
    assert_eq!(fs::read_dir(destination.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn movie_and_series_upgrade_paths_preserve_existing_library_layouts() {
    let directory = Directory::new();
    let original = directory.source("original.mp4", b"old episode");
    let upgrade = directory.source("upgrade.mp4", b"new episode");
    let library = directory.0.join("library");
    let mut request = movie();
    request.title = "Example Series".to_owned();
    request.season = 2;
    request.episode = 7;
    for kind in ["episode", "series"] {
        request.kind = kind.to_owned();
        let old_path = import_file(&original, &library, &request).unwrap();
        let new_path =
            import_versioned_file(&upgrade, &library, &request, FIRST_REVISION).unwrap();
        assert_eq!(old_path.parent(), new_path.parent());
        assert_eq!(
            new_path,
            library.join("Example Series").join("Season 02").join(format!(
                "Example Series - S02E07 [mynou-{FIRST_REVISION}].mp4"
            ))
        );
        assert_eq!(fs::read(&old_path).unwrap(), b"old episode");
        assert_eq!(fs::read(&new_path).unwrap(), b"new episode");
    }

    let mut request = movie();
    request.year = 0;
    let destination =
        import_versioned_file(&upgrade, &library, &request, SECOND_REVISION).unwrap();
    assert_eq!(
        destination,
        library.join("Example Movie").join(format!(
            "Example Movie [mynou-{SECOND_REVISION}].mp4"
        ))
    );
}

#[test]
fn invalid_revisions_are_rejected_before_creating_library_directories() {
    let directory = Directory::new();
    let source = directory.source("source.mkv", b"intact download");
    let library = directory.0.join("missing-library");
    for revision in [
        "",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef0",
        "0123456789abcdef0123456789abcdeg",
        "../../0123456789abcdef0123456789",
        "0123456789abcdef0123456789abcde\0",
    ] {
        let error = import_versioned_file(&source, &library, &movie(), revision).unwrap_err();
        assert!(error.contains("revision"), "{error}");
        assert!(!library.exists());
    }
    assert_eq!(fs::read(&source).unwrap(), b"intact download");
}

#[test]
fn cancellation_preserves_existing_media_and_does_not_create_a_revision() {
    let directory = Directory::new();
    let original = directory.source("original.mkv", b"original movie");
    let source = directory.source("upgrade.mkv", b"cancelled upgrade");
    let library = directory.0.join("library");
    let old_path = import_file(&original, &library, &movie()).unwrap();
    let cancelled = AtomicBool::new(false);

    let error = import_versioned_file_cancellable(
        &source,
        &library,
        &movie(),
        FIRST_REVISION,
        &cancelled,
    )
    .unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    assert_eq!(fs::read(&old_path).unwrap(), b"original movie");
    assert_eq!(fs::read(&source).unwrap(), b"cancelled upgrade");
    assert_eq!(fs::read_dir(old_path.parent().unwrap()).unwrap().count(), 1);

    let missing = directory.0.join("missing-library");
    assert!(
        import_versioned_file_cancellable(
            &source,
            &missing,
            &movie(),
            FIRST_REVISION,
            &cancelled,
        )
        .is_err()
    );
    assert!(!missing.exists());
}

#[test]
fn traversal_and_symlinks_cannot_redirect_upgrade_imports() {
    use std::os::unix::fs::symlink;

    let directory = Directory::new();
    let source = directory.source("source.mkv", b"download bytes");
    let source_alias = directory.0.join("alias.mkv");
    symlink(&source, &source_alias).unwrap();
    let outside = directory.0.join("outside");
    fs::create_dir(&outside).unwrap();
    let linked_library = directory.0.join("linked-library");
    symlink(&outside, &linked_library).unwrap();
    let library = directory.0.join("library");
    let traversal = directory.0.join("../escaped-library");

    for (source, library) in [
        (source_alias.as_path(), library.as_path()),
        (source.as_path(), linked_library.as_path()),
        (source.as_path(), traversal.as_path()),
    ] {
        assert!(import_versioned_file(source, library, &movie(), FIRST_REVISION).is_err());
    }
    assert!(!library.exists());
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    assert_eq!(fs::read(&source).unwrap(), b"download bytes");

    let destination = library.join("Example Movie (2026)").join(format!(
        "Example Movie (2026) [mynou-{FIRST_REVISION}].mkv"
    ));
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let outside_file = directory.source("outside.mkv", b"external media");
    symlink(&outside_file, &destination).unwrap();
    assert!(import_versioned_file(&source, &library, &movie(), FIRST_REVISION).is_err());
    assert_eq!(fs::read(&outside_file).unwrap(), b"external media");
    assert!(fs::symlink_metadata(&destination).unwrap().is_symlink());
}

#[cfg(target_os = "linux")]
#[test]
fn versioned_import_copies_across_filesystems_and_retries_without_mutation() {
    use std::os::unix::fs::MetadataExt;

    if !Path::new("/dev/shm").is_dir() {
        return;
    }
    let source_directory = Directory::new();
    let library_directory = Directory::in_parent(Path::new("/dev/shm"));
    if fs::metadata(&source_directory.0).unwrap().dev()
        == fs::metadata(&library_directory.0).unwrap().dev()
    {
        return;
    }
    let bytes: Vec<u8> = (0..=255).cycle().take(270_000).collect();
    let source = source_directory.source("upgrade.mkv", &bytes);
    let library = library_directory.0.join("library");
    let destination =
        import_versioned_file(&source, &library, &movie(), FIRST_REVISION).unwrap();

    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(fs::read(&destination).unwrap(), bytes);
    assert_ne!(
        fs::metadata(&source).unwrap().dev(),
        fs::metadata(&destination).unwrap().dev()
    );
    assert_eq!(
        import_versioned_file(&source, &library, &movie(), FIRST_REVISION).unwrap(),
        destination
    );
    assert_eq!(fs::read_dir(destination.parent().unwrap()).unwrap().count(), 1);
}
