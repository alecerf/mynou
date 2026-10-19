use super::*;
use std::{
    fs::{OpenOptions, Permissions},
    io::Write,
    os::unix::{fs::symlink, net::UnixListener},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

const PROFILE: &str = "[Interface]\nPrivateKey = AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=\nAddress = 10.23.0.2/24\nDNS = 10.23.0.1\n[Peer]\nPublicKey = CQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\nAllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.10:51820\n";

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "mynou-profile-file-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    fn profile(&self) -> PathBuf {
        let path = self.0.join("private-profile.conf");
        write(&path, PROFILE.as_bytes());
        path
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn write(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

fn failure(result: Result<Profile, ImportError>) -> ImportError {
    match result {
        Ok(_) => panic!("unsafe import was accepted"),
        Err(error) => error,
    }
}

#[test]
fn reads_private_profiles_without_modifying_source_or_permissions() {
    let directory = Directory::new();
    let path = directory.profile();
    for mode in [0o400, 0o600] {
        fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        let before = fs::metadata(&path).unwrap();
        let config = read_private_file(&path).unwrap().into_tunnel_config();
        assert_eq!(config.secret, [1; 32]);
        assert_eq!(
            config.endpoint,
            crate::tunnel::Endpoint::Address("192.0.2.10:51820".parse().unwrap())
        );
        unchanged(&before, &fs::metadata(&path).unwrap()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), PROFILE.as_bytes());
    }
    assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
}

#[test]
fn rejects_unsafe_modes_links_directories_sockets_and_missing_files() {
    let directory = Directory::new();
    let path = directory.profile();
    for mode in [0o644, 0o640, 0o604, 0o660, 0o700, 0o4600] {
        fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
        assert_eq!(failure(read_private_file(&path)), ImportError::UnsafeFile);
    }
    fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    let link = directory.0.join("linked");
    symlink(&path, &link).unwrap();
    assert_eq!(failure(read_private_file(&link)), ImportError::UnsafeFile);
    fs::remove_file(&link).unwrap();
    fs::hard_link(&path, &link).unwrap();
    for path in [&path, &link] {
        assert_eq!(failure(read_private_file(path)), ImportError::UnsafeFile);
    }
    assert_eq!(
        failure(read_private_file(&directory.0)),
        ImportError::UnsafeFile
    );
    let socket = directory.0.join("socket");
    let _listener = UnixListener::bind(&socket).unwrap();
    assert_eq!(failure(read_private_file(&socket)), ImportError::UnsafeFile);
    assert_eq!(
        failure(read_private_file(&directory.0.join("missing"))),
        ImportError::Io
    );
}

#[test]
fn rejects_ancestor_symlinks_and_parent_traversal() {
    let directory = Directory::new();
    let path = directory.profile();
    let alias = directory.0.join("alias");
    symlink(&directory.0, &alias).unwrap();
    assert_eq!(
        failure(read_private_file(&alias.join(path.file_name().unwrap()))),
        ImportError::UnsafeFile
    );
    assert_eq!(
        failure(read_private_file(
            &directory.0.join("alias/../private-profile.conf")
        )),
        ImportError::UnsafeFile
    );
    fs::remove_file(alias).unwrap();
}

#[test]
fn enforces_file_size_encoding_and_existing_parser_rules() {
    let directory = Directory::new();
    let path = directory.0.join("input");
    let mut exact = PROFILE.as_bytes().to_vec();
    while exact.len() < MAX_PROFILE_BYTES {
        let remaining = MAX_PROFILE_BYTES - exact.len();
        let row = remaining.min(1000);
        exact.push(b'#');
        exact.extend(std::iter::repeat_n(b' ', row.saturating_sub(2)));
        if row > 1 {
            exact.push(b'\n');
        }
    }
    write(&path, &exact);
    assert!(read_private_file(&path).is_ok());
    fs::remove_file(&path).unwrap();
    exact.push(b' ');
    write(&path, &exact);
    assert_eq!(failure(read_private_file(&path)), ImportError::ProfileLimit);
    fs::remove_file(&path).unwrap();
    write(&path, b"\xffsecret");
    assert_eq!(failure(read_private_file(&path)), ImportError::Encoding);
    fs::remove_file(&path).unwrap();
    write(&path, b"[Interface]\nPostUp = private-payload\n");
    assert_eq!(
        failure(read_private_file(&path)),
        ImportError::Parse(ProfileError {
            line: 2,
            kind: super::super::ErrorKind::UnsupportedDirective
        })
    );
}

#[test]
fn detects_replacement_before_open_after_open_and_after_read() {
    for phase in [Checkpoint::Inspected, Checkpoint::Opened, Checkpoint::Read] {
        let directory = Directory::new();
        let path = directory.profile();
        let result = read_checked(&path, |at| {
            if at == phase {
                fs::rename(&path, directory.0.join("original")).unwrap();
                write(&path, PROFILE.as_bytes());
            }
        });
        assert_eq!(failure(result), ImportError::Changed);
    }
}

#[test]
fn refuses_symlink_and_nonregular_substitution_before_open() {
    for socket in [false, true] {
        let directory = Directory::new();
        let path = directory.profile();
        let mut listener = None;
        let result = read_checked(&path, |at| {
            if at == Checkpoint::Inspected {
                let original = directory.0.join("original");
                fs::rename(&path, &original).unwrap();
                if socket {
                    listener = Some(UnixListener::bind(&path).unwrap());
                } else {
                    symlink(original, &path).unwrap();
                }
            }
        });
        assert_eq!(failure(result), ImportError::Io);
    }
}

#[test]
fn fifo_substitution_is_rejected_without_waiting_for_a_writer() {
    let directory = Directory::new();
    let path = directory.profile();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let path = &path;
        scope.spawn(move || {
            let result = read_checked(path, |at| {
                if at == Checkpoint::Inspected {
                    fs::remove_file(path).unwrap();
                    crate::store::open_tests::fifo(path);
                }
            });
            sender.send(failure(result)).unwrap();
        });
        match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(error) => assert_eq!(error, ImportError::UnsafeFile),
            Err(_) => {
                // Release a regressed blocking reader before the scoped join.
                let _ = crate::store::private_options().write(true).open(path);
                panic!("profile open blocked on a substituted FIFO");
            }
        }
    });
}

#[test]
fn detects_permission_link_content_and_final_symlink_changes_during_read() {
    for phase in [Checkpoint::Opened, Checkpoint::Read] {
        for change in 0..5 {
            let directory = Directory::new();
            let path = directory.profile();
            let result = read_checked(&path, |at| {
                if at != phase {
                    return;
                }
                match change {
                    0 => fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap(),
                    1 => fs::hard_link(&path, directory.0.join("hard-link")).unwrap(),
                    2 => {
                        OpenOptions::new()
                            .append(true)
                            .open(&path)
                            .unwrap()
                            .write_all(b"#changed\n")
                            .unwrap();
                    }
                    3 => {
                        // Same-size modification, even with the original mtime restored.
                        let file = OpenOptions::new().write(true).open(&path).unwrap();
                        let modified = file.metadata().unwrap().modified().unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        (&file).write_all(b"#").unwrap();
                        file.set_times(fs::FileTimes::new().set_modified(modified))
                            .unwrap();
                    }
                    4 => {
                        let original = directory.0.join("source");
                        fs::create_dir(&original).unwrap();
                        fs::rename(&path, original.join("private-profile.conf")).unwrap();
                        symlink(original.join("private-profile.conf"), &path).unwrap();
                    }
                    _ => unreachable!(),
                }
            });
            assert!(matches!(
                failure(result),
                ImportError::UnsafeFile | ImportError::Changed
            ));
        }
    }
}

#[test]
fn detects_ancestor_substitution_at_every_checkpoint() {
    for phase in [Checkpoint::Inspected, Checkpoint::Opened, Checkpoint::Read] {
        let directory = Directory::new();
        let parent = directory.0.join("parent");
        let renamed = directory.0.join("renamed");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("profile.conf");
        write(&path, PROFILE.as_bytes());
        let result = read_checked(&path, |at| {
            if at == phase {
                fs::rename(&parent, &renamed).unwrap();
                symlink(&renamed, &parent).unwrap();
            }
        });
        assert_eq!(failure(result), ImportError::UnsafeFile);
        fs::remove_file(&parent).unwrap();
    }
}

#[test]
fn detects_growth_past_the_read_limit_after_open() {
    let directory = Directory::new();
    let path = directory.profile();
    let result = read_checked(&path, |at| {
        if at == Checkpoint::Opened {
            OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len((MAX_PROFILE_BYTES * 2) as u64)
                .unwrap();
        }
    });
    assert_eq!(failure(result), ImportError::Changed);
}

#[test]
fn redacts_paths_os_payloads_and_profile_values() {
    let directory = Directory::new();
    let path = directory.0.join("sensitive-account-and-key");
    write(&path, b"[Interface]\nPrivateKey = sensitive-key\n[Peer]\n");
    let errors = [
        failure(read_private_file(&path)),
        failure(read_private_file(&path.join("sensitive-child"))),
        failure(read_private_file(&directory.0.join("sensitive-missing"))),
    ];
    for error in errors {
        let message = format!("{error} {error:?}");
        for secret in ["sensitive", "mynou-profile-file", "192.0.2.10", PROFILE] {
            assert!(!message.contains(secret), "diagnostic exposed input");
        }
        assert!(std::error::Error::source(&error).is_none());
    }
}
