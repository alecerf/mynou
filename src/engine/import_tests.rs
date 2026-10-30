//! Choice of the file to import among a download's files.
use super::import_source;
use crate::{crypto::random_bytes, store::Request};
use std::{fs, path::PathBuf, sync::atomic::AtomicBool};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix: String = random_bytes::<8>()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = std::env::temp_dir().join(format!("mynou-engine-import-{suffix}"));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    /// Writes `name` as the demo MP4, or as `size` bytes that are not media.
    fn file(&self, name: &str, damaged_size: Option<usize>) -> String {
        let path = self.0.join(name);
        match damaged_size {
            Some(size) => fs::write(&path, vec![0x1a; size]).unwrap(),
            None => fs::write(&path, include_bytes!("../../tests/fixtures/demo.mp4")).unwrap(),
        }
        path.to_str().unwrap().into()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(kind: &str, episode: u32) -> Request {
    Request {
        kind: kind.into(),
        title: "Show".into(),
        year: 2026,
        season: 1,
        episode,
        source_path: None,
        source_url: None,
        tmdb_id: None,
        source_numbering: None,
    }
}

#[test]
fn a_damaged_extra_does_not_fail_the_import() {
    let directory = Directory::new();
    // Larger than the film, so it is analyzed first.
    let damaged = directory.file("Movie.mkv", Some(200_000));
    let film = directory.file("Movie.mp4", None);
    let notes = directory.file("Movie.nfo", Some(10));
    let files = [damaged.clone(), film.clone(), notes];
    let active = AtomicBool::new(true);
    let chosen = import_source(&files, &request("movie", 0), false, &active).unwrap();
    assert_eq!(chosen.to_str(), Some(film.as_str()));

    let error = import_source(&[damaged], &request("movie", 0), false, &active).unwrap_err();
    assert!(error.starts_with("No supported media found for this request ("));
    assert!(error.contains("Movie.mkv"), "{error}");
}

#[test]
fn a_pack_analyzes_only_the_episode_named_for_the_request() {
    let directory = Directory::new();
    let wanted = directory.file("Show.S01E02.mp4", None);
    let files = [
        directory.file("Show.S01E01.mp4", None),
        wanted.clone(),
        directory.file("Show.S01E03.mkv", Some(200_000)),
    ];
    let active = AtomicBool::new(true);
    let chosen = import_source(&files, &request("episode", 2), true, &active).unwrap();
    assert_eq!(chosen.to_str(), Some(wanted.as_str()));

    let twice = [wanted, directory.file("Show.S01E02.Repack.mp4", None)];
    let error = import_source(&twice, &request("episode", 2), true, &active).unwrap_err();
    assert!(error.starts_with("Ambiguous pack"), "{error}");
}
