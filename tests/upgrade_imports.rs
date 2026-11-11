//! Upgrade imports take the place of the file they replace, never of another.
#![cfg(unix)]

use mynou::organizer::{import_file, import_upgrade_file, import_upgrade_file_cancellable};
use mynou::store::Request;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mynou-upgrade-import-{}-{timestamp}-{}",
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
        runtime_minutes: None,
        source_numbering: None,
        original_language: None,
        tmdb_id: None,
    }
}

fn names(path: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn upgrades_take_the_place_of_the_file_and_keep_the_downloads() {
    let directory = Directory::new();
    let original = directory.source("original.mkv", b"original 720p media");
    let first = directory.source("first.mkv", b"first 1080p media");
    let second = directory.source("second.mkv", b"second 2160p media");
    let library = directory.0.join("library");
    let request = movie();
    let old_path = import_file(&original, &library, &request).unwrap();
    let replaced = [old_path.to_string_lossy().into_owned()];
    let first_path = import_upgrade_file(&first, &library, &request, &replaced).unwrap();
    let second_path = import_upgrade_file(&second, &library, &request, &replaced).unwrap();

    assert_eq!(first_path, old_path);
    assert_eq!(second_path, old_path);
    assert_eq!(names(&old_path), ["Example Movie (2026).mkv"]);
    assert_eq!(fs::read(&old_path).unwrap(), b"second 2160p media");
    // Replacing the library name leaves every download as it was.
    assert_eq!(fs::read(&original).unwrap(), b"original 720p media");
    assert_eq!(fs::read(&first).unwrap(), b"first 1080p media");
    assert_eq!(fs::read(&second).unwrap(), b"second 2160p media");
}

#[test]
fn only_the_replaced_file_may_be_overwritten() {
    let directory = Directory::new();
    let first = directory.source("first.mkv", b"verified upgrade bytes");
    let identical = directory.source("identical.mkv", b"verified upgrade bytes");
    let different = directory.source("different.mkv", b"different upgrade data");
    let library = directory.0.join("library");
    let request = movie();
    let destination = import_upgrade_file(&first, &library, &request, &[]).unwrap();

    // A retry with the same bytes finds its import done.
    assert_eq!(
        import_upgrade_file(&identical, &library, &request, &[]).unwrap(),
        destination
    );
    // A file the earlier version does not own is never overwritten.
    let unrelated = ["/elsewhere/Example Movie (2026).mkv".to_owned()];
    let error = import_upgrade_file(&different, &library, &request, &unrelated).unwrap_err();
    assert!(error.contains("different"), "{error}");
    assert_eq!(fs::read(&destination).unwrap(), b"verified upgrade bytes");
    assert_eq!(fs::read(&different).unwrap(), b"different upgrade data");
    assert_eq!(names(&destination), ["Example Movie (2026).mkv"]);
}

#[test]
fn episodes_keep_their_layout_and_another_extension_is_left_to_delete() {
    let directory = Directory::new();
    let original = directory.source("original.mp4", b"old episode");
    let upgrade = directory.source("upgrade.mp4", b"new episode");
    let other = directory.source("other.mkv", b"newer episode");
    let library = directory.0.join("library");
    let mut request = movie();
    request.title = "Example Series".to_owned();
    request.season = 2;
    request.episode = 7;
    for kind in ["episode", "series"] {
        request.kind = kind.to_owned();
        let old_path = import_file(&original, &library, &request).unwrap();
        let replaced = [old_path.to_string_lossy().into_owned()];
        let new_path = import_upgrade_file(&upgrade, &library, &request, &replaced).unwrap();
        assert_eq!(
            new_path,
            library
                .join("Example Series")
                .join("Season 02")
                .join("Example Series - S02E07.mp4")
        );
        assert_eq!(fs::read(&new_path).unwrap(), b"new episode");
        fs::remove_file(&new_path).unwrap();
    }

    // Another extension has its own name: the earlier file stays until the
    // engine deletes it.
    let old_path = import_file(&original, &library, &request).unwrap();
    let replaced = [old_path.to_string_lossy().into_owned()];
    let new_path = import_upgrade_file(&other, &library, &request, &replaced).unwrap();
    assert_eq!(
        names(&new_path),
        ["Example Series - S02E07.mkv", "Example Series - S02E07.mp4"]
    );
    assert_eq!(fs::read(&old_path).unwrap(), b"old episode");
    assert_eq!(fs::read(&new_path).unwrap(), b"newer episode");
}

#[test]
fn cancellation_keeps_the_earlier_file() {
    let directory = Directory::new();
    let original = directory.source("original.mkv", b"original movie");
    let source = directory.source("upgrade.mkv", b"cancelled upgrade");
    let library = directory.0.join("library");
    let old_path = import_file(&original, &library, &movie()).unwrap();
    let replaced = [old_path.to_string_lossy().into_owned()];
    let cancelled = AtomicBool::new(false);

    let error = import_upgrade_file_cancellable(&source, &library, &movie(), &replaced, &cancelled)
        .unwrap_err();
    assert!(error.contains("cancelled"), "{error}");
    assert_eq!(fs::read(&old_path).unwrap(), b"original movie");
    assert_eq!(fs::read(&source).unwrap(), b"cancelled upgrade");
    assert_eq!(names(&old_path), ["Example Movie (2026).mkv"]);
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
        assert!(import_upgrade_file(source, library, &movie(), &[]).is_err());
    }
    assert!(!library.exists());
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    assert_eq!(fs::read(&source).unwrap(), b"download bytes");

    // A symbolic link is never replaced, even under a recorded name.
    let destination = library
        .join("Example Movie (2026)")
        .join("Example Movie (2026).mkv");
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    let outside_file = directory.source("outside.mkv", b"external media");
    symlink(&outside_file, &destination).unwrap();
    let replaced = [destination.to_string_lossy().into_owned()];
    assert!(import_upgrade_file(&source, &library, &movie(), &replaced).is_err());
    assert_eq!(fs::read(&outside_file).unwrap(), b"external media");
    assert!(fs::symlink_metadata(&destination).unwrap().is_symlink());
}
