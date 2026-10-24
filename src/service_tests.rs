use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "mynou-service-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn plist_escapes_paths_and_confines_process_arguments() {
    let config = Path::new("/fixture/a&b/data");
    let document = plist(
        "org.mynou.fixture",
        Path::new("/fixture/mynou"),
        config,
        Path::new("/fixture/home"),
    )
    .unwrap();
    assert!(owns(&document, "org.mynou.fixture", config).unwrap());
    assert!(!owns(&document, "org.mynou.fixture", Path::new("/other/data")).unwrap());
    assert!(!owns(&document, "org.mynou.other", config).unwrap());
    let args = document
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .unwrap()
        .split("</array>")
        .next()
        .unwrap();
    assert!(!args.contains("/bin/sh"));
    assert!(args.contains("/fixture/a&amp;b/data"));
    // Secrets live in the store, never in the registration.
    assert!(!document.contains("MYNOU_API_TOKEN"));
    #[cfg(target_os = "macos")]
    {
        use std::process::Stdio;
        // Until exec, the child shares every descriptor in this test process.
        // Use the same gate as the store fixtures so another test cannot take
        // a new lock that survives its parent's close in the child.
        let _spawning = store::SPAWNING
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut child = Command::new("/usr/bin/plutil")
            .args(["-lint", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(document.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn fixed_code_logs_rotate_and_refuse_links_without_touching_the_target() {
    let dir = Directory::new();
    let config = dir.0.clone();
    log(&config, "starting").unwrap();
    let root = dir.0.join(".mynou-service");
    let path = root.join("events.log");
    fs::write(&path, vec![b'x'; MAX_LOG as usize - 1]).unwrap();
    log(&config, "runtime_failed").unwrap();
    assert!(
        fs::metadata(root.join("events.previous.log"))
            .unwrap()
            .len()
            <= MAX_LOG
    );
    assert!(fs::metadata(&path).unwrap().len() <= MAX_LOG);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(log(&config, "SECRET: https://private.example").is_err());
    let untouched = dir.0.join("untouched");
    create(&untouched, b"keep").unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&untouched, &path).unwrap();
    assert!(log(&config, "starting").is_err());
    assert_eq!(fs::read(&untouched).unwrap(), b"keep");
}

#[test]
fn a_missing_volume_prevents_storage_creation_and_reports_a_fixed_code() {
    let dir = Directory::new();
    fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
    let config = crate::config::from_json(&crate::config::default_json(), &dir.0).unwrap();
    assert!(
        preflight(&dir.0, &config)
            .unwrap_err()
            .contains("media_volume_unavailable")
    );
    assert!(!config.store_dir.exists());
    assert!(!config.movies_root.exists());
    assert!(
        fs::read_to_string(dir.0.join(".mynou-service/events.log"))
            .unwrap()
            .contains("media_volume_unavailable")
    );
}

#[test]
fn shutdown_fences_the_launchd_process_identity_before_stopping() {
    use std::sync::atomic::Ordering;
    let dir = Directory::new();
    let config = crate::config::from_json(&crate::config::default_json(), &dir.0).unwrap();
    let engine = crate::engine::Engine::open(config).unwrap();
    let wrong = format!("{{\"expected_pid\":{}}}", u64::from(std::process::id()) + 1);
    assert!(crate::server::route(&engine, "POST", "/api/shutdown", wrong.as_bytes()).is_err());
    assert!(!engine.stopped.load(Ordering::Acquire));
    let correct = format!("{{\"expected_pid\":{}}}", std::process::id());
    assert!(crate::server::route(&engine, "POST", "/api/shutdown", correct.as_bytes()).is_ok());
    assert!(engine.stopped.load(Ordering::Acquire));
}
