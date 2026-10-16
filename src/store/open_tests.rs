//! Private store opens refuse a final symbolic link and never block on a FIFO,
//! even when the path is swapped after its inspection. The swap fixture is
//! shared with the inventory and IRC store tests.
use super::{
    BEFORE_PRIVATE_OPEN, O_NOFOLLOW, O_NONBLOCK, SPAWNING, open_inspected, private_options,
    read_only_file, secure_file, sync_directory,
};
use crate::crypto::random_bytes;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{FileTypeExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, PoisonError, mpsc},
    time::Duration,
};

/// `ELOOP` from each target's `<errno.h>`: what `O_NOFOLLOW` reports.
#[cfg(target_os = "macos")]
const ELOOP: i32 = 62;
#[cfg(target_os = "linux")]
const ELOOP: i32 = 40;

/// Only detects a blocked open; a passing open returns at once.
const BLOCKED: Duration = Duration::from_secs(60);

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let suffix: String = random_bytes::<16>()
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = std::env::temp_dir().join(format!("mynou-open-{suffix}"));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Test-only: std has no `mkfifo`. Until the child execs it shares every
/// descriptor, including other tests' store locks, so no lock is taken
/// meanwhile ([`SPAWNING`]).
pub(crate) fn fifo(path: &Path) {
    let status = {
        let _spawning = SPAWNING.write().unwrap_or_else(PoisonError::into_inner);
        Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(path)
            .status()
            .unwrap()
    };
    assert!(status.success(), "mkfifo failed");
}

/// Writes a private copy of `original` beside `path`, then renames it over
/// `path`.
fn replace_with_copy(original: &Path, path: &Path) {
    let mut copy = path.as_os_str().to_owned();
    copy.push(".copy");
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&copy)
        .unwrap();
    file.write_all(&fs::read(original).unwrap()).unwrap();
    fs::rename(&copy, path).unwrap();
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Swap {
    /// A symbolic link to the original, still private regular file.
    Symlink,
    /// A FIFO that no process writes.
    Fifo,
    /// Another private regular file with the same bytes, renamed over the
    /// path: only an inode comparison tells it apart.
    Replaced,
}

/// Runs `work` on its own thread and panics if it has not returned within
/// [`BLOCKED`], after opening `fifo` for writing to release a blocked reader.
fn unblocked<T: Send + 'static>(
    fifo: Arc<Mutex<Option<PathBuf>>>,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.recv_timeout(BLOCKED).unwrap_or_else(|_| {
        if let Some(path) = fifo.lock().unwrap().as_deref() {
            let _ = OpenOptions::new().write(true).open(path);
        }
        panic!("a private open blocked on a FIFO");
    })
}

/// Runs `work` on its own thread. At the `skip`-th private open of a file
/// named `name`, after its inspection, the path is replaced by `swap`. Returns
/// the error `work` reports; panics if it succeeds, if the swap never happens
/// or if the open blocks.
pub(crate) fn swapped(
    name: &str,
    skip: usize,
    swap: Swap,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> String {
    let name = name.to_owned();
    let replaced: Arc<Mutex<Option<PathBuf>>> = Arc::default();
    let seen = Arc::clone(&replaced);
    let outcome = unblocked(Arc::clone(&replaced), move || {
        let mut remaining = skip;
        BEFORE_PRIVATE_OPEN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                if path.file_name().and_then(|name| name.to_str()) != Some(name.as_str())
                    || seen.lock().unwrap().is_some()
                {
                    return;
                }
                if remaining > 0 {
                    remaining -= 1;
                    return;
                }
                let mut moved = path.as_os_str().to_owned();
                moved.push(".moved");
                fs::rename(path, &moved).unwrap();
                match swap {
                    Swap::Symlink => symlink(&moved, path).unwrap(),
                    Swap::Fifo => fifo(path),
                    Swap::Replaced => replace_with_copy(Path::new(&moved), path),
                }
                *seen.lock().unwrap() = Some(path.to_owned());
            }));
        });
        work()
    });
    assert!(
        replaced.lock().unwrap().is_some(),
        "{swap:?} swap never happened"
    );
    match outcome {
        Ok(()) => panic!("a swapped {swap:?} was accepted"),
        Err(error) => error,
    }
}

#[test]
fn open_flags_match_the_target_headers() {
    #[cfg(target_os = "macos")]
    assert_eq!((O_NOFOLLOW, O_NONBLOCK), (0x0100, 0x0004));
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    assert_eq!((O_NOFOLLOW, O_NONBLOCK), (0o400_000, 0o4_000));
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    assert_eq!((O_NOFOLLOW, O_NONBLOCK), (0o100_000, 0o4_000));
}

#[test]
fn private_opens_refuse_a_final_symlink_and_return_at_once_on_a_fifo() {
    let directory = Directory::new();
    let target = directory.0.join("target");
    let mut file = private_options()
        .write(true)
        .create_new(true)
        .open(&target)
        .unwrap();
    file.write_all(b"secret").unwrap();
    let link = directory.0.join("link");
    symlink(&target, &link).unwrap();
    for writable in [false, true] {
        let error = private_options()
            .read(true)
            .write(writable)
            .open(&link)
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(ELOOP), "{error}");
    }
    // Creation never follows a dangling link either.
    let dangling = directory.0.join("dangling");
    symlink(directory.0.join("absent"), &dangling).unwrap();
    assert!(
        private_options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&dangling)
            .is_err()
    );
    assert!(!directory.0.join("absent").exists());
    // Without a writer a plain open would wait forever; this one returns a
    // descriptor whose type callers then refuse.
    let pipe = directory.0.join("pipe");
    fifo(&pipe);
    let shared = Arc::new(Mutex::new(Some(pipe.clone())));
    let error = unblocked(shared, move || {
        let opened = private_options().read(true).open(&pipe).unwrap();
        assert!(opened.metadata().unwrap().file_type().is_fifo());
        sync_directory(&pipe).unwrap_err()
    });
    assert!(error.contains("not a directory"), "{error}");
}

#[test]
fn an_inspected_open_refuses_a_swapped_symlink_or_fifo() {
    let directory = Directory::new();
    let path = directory.0.join("state");
    fs::write(&path, b"state").unwrap();
    let inspected = fs::symlink_metadata(&path).unwrap();
    let mut file = open_inspected(&path, &inspected).unwrap();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes).unwrap();
    assert_eq!(bytes, b"state");
    for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
        let path = directory.0.join(format!("{swap:?}"));
        fs::write(&path, b"state").unwrap();
        let inspected = fs::symlink_metadata(&path).unwrap();
        let name = format!("{swap:?}");
        let error = swapped(&name, 0, swap, move || {
            open_inspected(&path, &inspected).map(drop)
        });
        match swap {
            Swap::Symlink => assert!(error.starts_with("cannot open private file"), "{error}"),
            Swap::Fifo | Swap::Replaced => assert_eq!(error, "private file changed while opening"),
        }
    }
}

#[test]
fn job_store_locks_refuse_a_file_swapped_after_inspection() {
    for swap in [Swap::Symlink, Swap::Fifo, Swap::Replaced] {
        for writable in [true, false] {
            let directory = Directory::new();
            let path = directory.0.join(".lock");
            let mut file = private_options()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            file.write_all(b"lock").unwrap();
            let error = swapped(".lock", 0, swap, move || {
                if writable {
                    secure_file(&path).map(drop)
                } else {
                    read_only_file(&path).map(drop)
                }
            });
            match swap {
                Swap::Symlink => assert!(error.starts_with("cannot open"), "{error}"),
                Swap::Fifo => assert_eq!(error, "storage must be a regular file"),
                Swap::Replaced => assert_eq!(error, "storage changed while opening"),
            }
        }
    }
}

#[test]
fn a_missing_job_store_lock_is_created_exclusively() {
    let directory = Directory::new();
    let path = directory.0.join(".lock");
    secure_file(&path).unwrap();
    let mode = fs::symlink_metadata(&path).unwrap().permissions();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
        0o600
    );
    // A file that appears between inspection and creation is inspected again.
    let racing = Arc::new(Mutex::new(Some(path.clone())));
    let creator = Arc::clone(&racing);
    let path = directory.0.join(".lock-raced");
    BEFORE_PRIVATE_OPEN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |opened: &Path| {
            if creator.lock().unwrap().take().is_some() {
                fs::write(opened, b"").unwrap();
                let mut permissions = fs::symlink_metadata(opened).unwrap().permissions();
                std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o600);
                fs::set_permissions(opened, permissions).unwrap();
            }
        }));
    });
    let opened = secure_file(&path);
    BEFORE_PRIVATE_OPEN.with(|hook| *hook.borrow_mut() = None);
    opened.unwrap();
    assert!(racing.lock().unwrap().is_none(), "the race never happened");
}
