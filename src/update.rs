//! Daily self-update from the latest published release, the way Plex Media
//! Server updates itself. A newer executable is downloaded, verified against
//! the release's `SHA256SUMS` and staged next to the running one. It is
//! installed when Mynou is idle: Mynou stops cleanly, backs up `state/` and
//! the previous executable under `updates/` in the data directory, swaps the
//! executable and restarts on it. If the new version cannot open the storage,
//! the backup and the previous executable are restored.
use crate::{
    Result,
    crypto::sha256,
    json::{self, Value},
    net::HttpClient,
};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const LATEST: &str = "https://api.github.com/repos/alecerf/mynou/releases/latest";
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 3600);
const FIRST_CHECK: Duration = Duration::from_secs(300);
const MAX_EXECUTABLE: usize = 256 * 1024 * 1024;
const KEPT_BACKUPS: usize = 2;
const NOT_RELEASE: &str = "Only published release executables update themselves";

pub type Version = (u32, u32, u32);

pub fn parse_version(text: &str) -> Option<Version> {
    let mut parts = text.strip_prefix('v').unwrap_or(text).split('.');
    let mut next = || parts.next()?.parse().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

pub fn display((major, minor, patch): Version) -> String {
    format!("{major}.{minor}.{patch}")
}

pub fn current() -> Version {
    parse_version(env!("CARGO_PKG_VERSION")).expect("the package version is x.y.z")
}

/// The executable and checksum manifest of a published release.
#[derive(Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    pub executable: String,
    pub sums: String,
}

pub fn asset_name(version: Version) -> String {
    format!("mynou-v{}-macos-arm64", display(version))
}

/// Reads a GitHub `releases/latest` document.
pub fn release(latest: &Value) -> Result<Release> {
    if latest.get("draft").and_then(Value::as_bool) != Some(false)
        || latest.get("prerelease").and_then(Value::as_bool) != Some(false)
    {
        return Err("Update: the latest release is not published".into());
    }
    let version = latest
        .get("tag_name")
        .and_then(Value::as_str)
        .and_then(parse_version)
        .ok_or("Update: the latest release has no version")?;
    let assets = latest
        .get("assets")
        .and_then(Value::as_array)
        .unwrap_or(&[]);
    let url = |name: &str| {
        assets
            .iter()
            .find(|a| a.get("name").and_then(Value::as_str) == Some(name))
            // The API asset URL: github.com download pages send headers
            // longer than the HTTP client accepts.
            .and_then(|a| a.get("url"))
            .and_then(Value::as_str)
            .filter(|url| url.starts_with("https://api.github.com/"))
            .map(str::to_owned)
            .ok_or_else(|| format!("Update: release {} lacks {name}", display(version)))
    };
    Ok(Release {
        version,
        executable: url(&asset_name(version))?,
        sums: url("SHA256SUMS")?,
    })
}

/// The digest `SHA256SUMS` lists for `name`.
pub fn checksum(sums: &str, name: &str) -> Option<[u8; 32]> {
    sums.lines().find_map(|line| {
        let (digest, file) = line.split_once(' ')?;
        let file = file.trim_start_matches(' ').trim_start_matches('*');
        (file == name).then(|| digest_from_hex(digest))?
    })
}

fn digest_from_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut digest = [0; 32];
    for (n, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * n..2 * n + 2], 16).ok()?;
    }
    Some(digest)
}

/// A verified executable waiting next to the one it replaces.
#[derive(Clone, Debug)]
pub struct Staged {
    pub version: Version,
    pub file: PathBuf,
    pub target: PathBuf,
}

#[derive(Default)]
struct Status {
    checked: Option<Instant>,
    checked_at: Option<u64>,
    latest: Option<Version>,
    staged: Option<Staged>,
    error: Option<String>,
}

static STATUS: Mutex<Status> = Mutex::new(Status {
    checked: None,
    checked_at: None,
    latest: None,
    staged: None,
    error: None,
});
static STARTED: OnceLock<Instant> = OnceLock::new();
static RESTART: AtomicBool = AtomicBool::new(false);
static CHECKING: Mutex<()> = Mutex::new(());

fn status() -> std::sync::MutexGuard<'static, Status> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The installed executable, if it is one that updates itself.
pub fn executable() -> Result<PathBuf> {
    if cfg!(debug_assertions) {
        return Err(NOT_RELEASE.into());
    }
    let path = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|_| "Update: cannot locate the executable")?;
    if path.components().any(|c| c.as_os_str() == "target") {
        return Err(NOT_RELEASE.into());
    }
    Ok(path)
}

pub fn report() -> Value {
    let status = status();
    let mut v = Value::object();
    v.insert("version", display(current()));
    v.insert(
        "eligible",
        executable().map_or_else(Value::String, |_| Value::Bool(true)),
    );
    v.insert(
        "checked_at",
        status
            .checked_at
            .map_or(Value::Null, |at| Value::Number(at as f64)),
    );
    v.insert(
        "latest",
        status.latest.map_or(Value::Null, |v| display(v).into()),
    );
    v.insert(
        "staged",
        status
            .staged
            .as_ref()
            .map_or(Value::Null, |s| display(s.version).into()),
    );
    v.insert(
        "error",
        status.error.clone().map_or(Value::Null, Value::String),
    );
    v
}

pub fn staged() -> Option<Staged> {
    status().staged.clone()
}

/// Whether the daily check is due; the first one runs a few minutes after
/// the first start of the engine.
pub fn due() -> bool {
    let started = *STARTED.get_or_init(Instant::now);
    match status().checked {
        None => started.elapsed() >= FIRST_CHECK,
        Some(at) => at.elapsed() >= CHECK_INTERVAL,
    }
}

/// Starts the due check without holding up the caller, which may be asked
/// to stop meanwhile.
pub fn check_in_background() {
    status().checked = Some(Instant::now());
    std::thread::spawn(check_now);
}

/// Checks the latest release now and stages it when it is newer.
pub fn check_now() -> Value {
    let _one_at_a_time = CHECKING.lock().unwrap_or_else(|e| e.into_inner());
    status().checked = Some(Instant::now());
    let result = check();
    {
        let mut status = status();
        status.checked_at = Some(crate::store::now());
        match result {
            Ok((latest, staged)) => {
                status.latest = Some(latest);
                status.error = None;
                if staged.is_some() {
                    status.staged = staged;
                }
            }
            Err(error) => status.error = Some(error),
        }
    }
    report()
}

fn latest() -> Result<Release> {
    let headers = [("Accept".into(), "application/vnd.github+json".into())];
    let response = HttpClient::new()
        .with_timeout(Duration::from_secs(30))
        .request("GET", LATEST, &headers, &[])
        .map_err(|_| "Update: cannot reach GitHub releases")?;
    if response.status != 200 {
        return Err(format!("Update: GitHub answered {}", response.status));
    }
    let text = std::str::from_utf8(&response.body).map_err(|_| "Update: invalid release")?;
    release(&json::parse(text)?)
}

fn check() -> Result<(Version, Option<Staged>)> {
    let target = executable()?;
    let release = latest()?;
    status().latest = Some(release.version);
    if release.version <= current() {
        return Ok((release.version, None));
    }
    if let Some(staged) = staged().filter(|s| s.version == release.version && s.file.is_file()) {
        return Ok((release.version, Some(staged)));
    }
    let download = |url: &str, max: usize| {
        let response = HttpClient::new()
            .with_timeout(Duration::from_secs(600))
            .with_max_body(max)
            .request(
                "GET",
                url,
                &[("Accept".into(), "application/octet-stream".into())],
                &[],
            )
            .map_err(|_| "Update: download failed")?;
        if response.status != 200 {
            return Err(format!("Update: download answered {}", response.status));
        }
        Ok(response.body)
    };
    let sums = download(&release.sums, 64 * 1024)?;
    let name = asset_name(release.version);
    let digest = checksum(&String::from_utf8_lossy(&sums), &name)
        .ok_or("Update: SHA256SUMS does not list the executable")?;
    let bytes = download(&release.executable, MAX_EXECUTABLE)?;
    let file = stage(&target, &bytes, &digest, release.version)?;
    Ok((
        release.version,
        Some(Staged {
            version: release.version,
            file,
            target,
        }),
    ))
}

/// Writes a verified executable next to `target`, ready to replace it.
pub fn stage(target: &Path, bytes: &[u8], digest: &[u8; 32], version: Version) -> Result<PathBuf> {
    if !crate::crypto::constant_time_eq(&sha256(bytes), digest) {
        return Err("Update: the download does not match SHA256SUMS".into());
    }
    let dir = target
        .parent()
        .ok_or("Update: the executable has no folder")?;
    // Drop executables staged by an earlier process for any version.
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".mynou-update-")
        {
            let _ = fs::remove_file(entry.path());
        }
    }
    let file = dir.join(format!(".mynou-update-{}", display(version)));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&file)
        .and_then(|mut f| {
            f.write_all(bytes)?;
            f.sync_all()
        });
    if written.is_err() {
        let _ = fs::remove_file(&file);
        return Err(format!(
            "Update: cannot write next to the executable in {}",
            dir.display()
        ));
    }
    // A download for another architecture or a truncated file stops here.
    let reports = std::process::Command::new(&file)
        .arg("version")
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && output
                    .stdout
                    .starts_with(format!("mynou {} ", display(version)).as_bytes())
        });
    if !reports {
        let _ = fs::remove_file(&file);
        return Err(format!(
            "Update: the executable of {} does not run on this Mac",
            display(version)
        ));
    }
    Ok(file)
}

pub fn request_restart() {
    RESTART.store(true, Ordering::Release);
}

pub fn restart_requested() -> bool {
    RESTART.load(Ordering::Acquire)
}

fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(_) => Err(format!("Update: cannot create {}", path.display())),
    }
}

/// Copies a folder of ordinary files and folders, refusing symbolic links.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    private_dir(to)?;
    let entries =
        fs::read_dir(from).map_err(|_| format!("Update: cannot read {}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|_| format!("Update: cannot read {}", from.display()))?;
        let (source, copy) = (entry.path(), to.join(entry.file_name()));
        let kind = entry
            .file_type()
            .map_err(|_| format!("Update: cannot inspect {}", source.display()))?;
        if kind.is_dir() {
            copy_tree(&source, &copy)?;
        } else if kind.is_file() {
            fs::copy(&source, &copy)
                .and_then(|_| fs::File::open(&copy)?.sync_all())
                .map_err(|_| format!("Update: cannot copy {}", source.display()))?;
        } else {
            return Err(format!(
                "Update: {} is not an ordinary file",
                source.display()
            ));
        }
    }
    Ok(())
}

/// Replaces `target` with a copy of `executable`, atomically.
fn replace(target: &Path, executable: &Path) -> Result<()> {
    let dir = target
        .parent()
        .ok_or("Update: the executable has no folder")?;
    let temporary = dir.join(".mynou-restore");
    let _ = fs::remove_file(&temporary);
    fs::copy(executable, &temporary)
        .and_then(|_| fs::File::open(&temporary)?.sync_all())
        .and_then(|_| fs::rename(&temporary, target))
        .map_err(|_| {
            let _ = fs::remove_file(&temporary);
            "Update: cannot restore the previous executable".to_owned()
        })
}

fn sync_dir(path: &Path) {
    if let Ok(dir) = fs::File::open(path) {
        let _ = dir.sync_all();
    }
}

/// Installs a staged executable while Mynou is stopped, after backing up
/// `state/` and the running executable.
pub fn apply(data_dir: &Path, staged: &Staged) -> Result<()> {
    apply_from(data_dir, staged, current())
}

fn apply_from(data_dir: &Path, staged: &Staged, from: Version) -> Result<()> {
    let root = data_dir.join("updates");
    private_dir(&root)?;
    let backup = root.join(format!("{}-to-{}", display(from), display(staged.version)));
    if backup.exists() {
        // An earlier attempt for the same versions stopped before switching.
        fs::remove_dir_all(&backup).map_err(|_| "Update: cannot reuse the backup folder")?;
    }
    private_dir(&backup)?;
    fs::copy(&staged.target, backup.join("mynou"))
        .map_err(|_| "Update: cannot back up the executable")?;
    let state = data_dir.join("state");
    if state.is_dir() {
        copy_tree(&state, &backup.join("state"))?;
    }
    let marker = root.join("pending");
    fs::write(
        &marker,
        backup.file_name().unwrap_or_default().as_encoded_bytes(),
    )
    .map_err(|_| "Update: cannot record the pending update")?;
    sync_dir(&root);
    if fs::rename(&staged.file, &staged.target).is_err() {
        let _ = fs::remove_file(&marker);
        return Err("Update: cannot replace the executable".into());
    }
    if let Some(dir) = staged.target.parent() {
        sync_dir(dir);
    }
    status().staged = None;
    prune(&root);
    Ok(())
}

/// Keeps the most recent backups only.
fn prune(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut backups: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|k| k.is_dir()))
        .filter(|e| e.file_name().to_string_lossy().contains("-to-"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    backups.sort();
    let excess = backups.len().saturating_sub(KEPT_BACKUPS);
    for (_, path) in backups.into_iter().take(excess) {
        let _ = fs::remove_dir_all(path);
    }
}

/// The new version opened the storage: the update is confirmed.
pub fn confirm(data_dir: &Path) {
    let _ = fs::remove_file(data_dir.join("updates/pending"));
}

/// After an update this version could not open the storage: restores the
/// backup and the previous executable, keeping the failed state in the
/// backup folder. True when the caller should restart on the previous one.
pub fn recover(data_dir: &Path) -> Result<bool> {
    recover_to(data_dir, &executable()?, current())
}

fn recover_to(data_dir: &Path, target: &Path, running: Version) -> Result<bool> {
    let root = data_dir.join("updates");
    let marker = root.join("pending");
    let Ok(name) = fs::read_to_string(&marker) else {
        return Ok(false);
    };
    let backup = root.join(name.trim());
    if !name.trim().ends_with(&format!("-to-{}", display(running)))
        || !backup.join("mynou").is_file()
    {
        let _ = fs::remove_file(&marker);
        return Ok(false);
    }
    let state = data_dir.join("state");
    if state.exists() {
        let failed = backup.join("state.failed");
        let _ = fs::remove_dir_all(&failed);
        fs::rename(&state, &failed).map_err(|_| "Update: cannot set the failed state aside")?;
    }
    if backup.join("state").is_dir() {
        copy_tree(&backup.join("state"), &state)?;
    }
    replace(target, &backup.join("mynou"))?;
    fs::remove_file(&marker).map_err(|_| "Update: cannot clear the pending update")?;
    sync_dir(&root);
    Ok(true)
}

/// Runs the installed executable again with the same arguments, keeping the
/// process identity, so launchd keeps supervising it. Returns only on failure.
pub fn restart() -> String {
    use std::os::unix::process::CommandExt;
    let Ok(executable) = std::env::current_exe() else {
        return "Update: cannot locate the executable".into();
    };
    let error = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .exec();
    format!("Update: cannot restart Mynou: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("mynou-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho 'mynou {version} — test'\n").into_bytes()
    }

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(parse_version("v0.24.0"), Some((0, 24, 0)));
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
        for bad in ["", "1.2", "1.2.3.4", "v1.2.x", "1.2.3-beta"] {
            assert_eq!(parse_version(bad), None, "{bad}");
        }
        assert_eq!(display(current()), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_latest_release_names_its_executable_and_checksums() {
        let document = json::parse(
            r#"{"tag_name":"v9.1.2","draft":false,"prerelease":false,"assets":[
              {"name":"SHA256SUMS","url":"https://api.github.com/assets/1"},
              {"name":"mynou-v9.1.2-macos-arm64","url":"https://api.github.com/assets/2"}]}"#,
        )
        .unwrap();
        assert_eq!(
            release(&document).unwrap(),
            Release {
                version: (9, 1, 2),
                executable: "https://api.github.com/assets/2".into(),
                sums: "https://api.github.com/assets/1".into(),
            }
        );
        let draft =
            json::parse(r#"{"tag_name":"v9.1.2","draft":true,"prerelease":false}"#).unwrap();
        assert!(release(&draft).is_err());
        let missing =
            json::parse(r#"{"tag_name":"v9.1.2","draft":false,"prerelease":false,"assets":[]}"#)
                .unwrap();
        assert!(release(&missing).unwrap_err().contains("lacks"));
    }

    #[test]
    fn checksums_are_read_for_the_named_file_only() {
        let digest = "ab".repeat(32);
        let sums = format!("{digest}  mynou-v1.0.0-macos-arm64\n");
        assert_eq!(
            checksum(&sums, "mynou-v1.0.0-macos-arm64"),
            Some([0xab; 32])
        );
        assert_eq!(checksum(&sums, "mynou-v1.0.1-macos-arm64"), None);
        assert_eq!(
            checksum("zz  mynou-v1.0.0-macos-arm64", "mynou-v1.0.0-macos-arm64"),
            None
        );
    }

    #[test]
    fn only_a_verified_executable_reporting_its_version_is_staged() {
        let dir = directory("stage");
        let target = dir.join("mynou");
        fs::write(&target, script("1.0.0")).unwrap();
        let bytes = script("1.1.0");
        assert!(
            stage(&target, &bytes, &[0; 32], (1, 1, 0))
                .unwrap_err()
                .contains("SHA256SUMS")
        );
        let other = script("1.2.0");
        assert!(
            stage(&target, &other, &sha256(&other), (1, 1, 0))
                .unwrap_err()
                .contains("does not run")
        );
        assert!(!dir.join(".mynou-update-1.1.0").exists());
        fs::write(dir.join(".mynou-update-1.0.5"), b"stale").unwrap();
        let file = stage(&target, &bytes, &sha256(&bytes), (1, 1, 0)).unwrap();
        assert_eq!(fs::read(&file).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_update_backs_up_and_a_failed_start_restores_the_previous_version() {
        let dir = directory("apply");
        let (data, bin) = (dir.join("data"), dir.join("bin"));
        fs::create_dir_all(data.join("state/jobs")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(data.join("state/jobs/log"), b"before").unwrap();
        let target = bin.join("mynou");
        fs::write(&target, script("1.0.0")).unwrap();
        let bytes = script("1.1.0");
        let staged = Staged {
            version: (1, 1, 0),
            file: stage(&target, &bytes, &sha256(&bytes), (1, 1, 0)).unwrap(),
            target: target.clone(),
        };
        apply_from(&data, &staged, (1, 0, 0)).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        let backup = data.join("updates/1.0.0-to-1.1.0");
        assert_eq!(fs::read(backup.join("mynou")).unwrap(), script("1.0.0"));
        assert_eq!(fs::read(backup.join("state/jobs/log")).unwrap(), b"before");

        // The new version migrated the state, then could not open it.
        fs::write(data.join("state/jobs/log"), b"migrated").unwrap();
        assert!(!recover_to(&data, &target, (1, 2, 0)).unwrap());
        fs::write(data.join("updates/pending"), "1.0.0-to-1.1.0").unwrap();
        assert!(recover_to(&data, &target, (1, 1, 0)).unwrap());
        assert_eq!(fs::read(&target).unwrap(), script("1.0.0"));
        assert_eq!(fs::read(data.join("state/jobs/log")).unwrap(), b"before");
        assert_eq!(
            fs::read(backup.join("state.failed/jobs/log")).unwrap(),
            b"migrated"
        );
        assert!(!data.join("updates/pending").exists());
        assert!(!recover_to(&data, &target, (1, 1, 0)).unwrap());

        // A confirmed start keeps the new version.
        let staged = Staged {
            version: (1, 1, 0),
            file: stage(&target, &bytes, &sha256(&bytes), (1, 1, 0)).unwrap(),
            target: target.clone(),
        };
        apply_from(&data, &staged, (1, 0, 0)).unwrap();
        confirm(&data);
        assert!(!recover_to(&data, &target, (1, 1, 0)).unwrap());
        assert_eq!(fs::read(&target).unwrap(), bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_two_most_recent_backups_are_kept() {
        let dir = directory("prune");
        for (n, name) in ["1.0.0-to-1.1.0", "1.1.0-to-1.2.0", "1.2.0-to-1.3.0"]
            .iter()
            .enumerate()
        {
            fs::create_dir(dir.join(name)).unwrap();
            let at = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + n as u64);
            fs::File::open(dir.join(name))
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
        fs::write(dir.join("pending"), "x").unwrap();
        prune(&dir);
        assert!(!dir.join("1.0.0-to-1.1.0").exists());
        assert!(dir.join("1.1.0-to-1.2.0").exists() && dir.join("1.2.0-to-1.3.0").exists());
        assert!(dir.join("pending").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
