//! Account-owned launchd registration. No shell or companion runtime process.
use crate::{Result, config::Config, json::Value, store};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Component, Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const MAX_LOG: u64 = 65_536;

fn absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("Service: use an absolute path without dot components".into());
    }
    store::reject_symlinks(path).map_err(|_| "Service: symlinked paths are unsupported".into())
}

fn private(path: &Path) -> Result<fs::Metadata> {
    absolute(path)?;
    let m = fs::symlink_metadata(path).map_err(|_| "Service: private file unavailable")?;
    if !m.is_file() || m.nlink() != 1 || m.mode() & 0o7777 != 0o600 {
        return Err("Service: files require mode 0600 without links".into());
    }
    Ok(m)
}

fn read(path: &Path, limit: u64) -> Result<String> {
    let m = private(path)?;
    let file = store::open_inspected(path, &m)?;
    let mut text = String::new();
    file.take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|_| "Service: private file cannot be read")?;
    if text.len() as u64 > limit {
        return Err("Service: private file exceeds size limit".into());
    }
    Ok(text)
}

fn create(path: &Path, bytes: &[u8]) -> Result<()> {
    absolute(path)?;
    let mut file = store::private_options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "Service: cannot create private file; no existing file was replaced")?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| "Service: private file write failed".into())
}

/// The data directory identifies the installation: a real, private directory.
fn installation(path: &Path) -> Result<fs::Metadata> {
    absolute(path)?;
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && m.mode() & 0o077 == 0 => Ok(m),
        _ => Err("Service: the data directory must be an existing private (0700) directory; run `mynou serve` once to create it".into()),
    }
}

fn directory(path: &Path) -> Result<()> {
    absolute(path)?;
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && m.mode() & 0o077 == 0 => Ok(()),
        Ok(_) => Err("Service: registration directory must be private (0700)".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| "Service: cannot create registration directory".into()),
        Err(_) => Err("Service: registration directory unavailable".into()),
    }
}

fn escape(text: &str) -> Result<String> {
    if text.chars().any(char::is_control) {
        return Err("Service: control characters are unsupported".into());
    }
    Ok(text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

/// Where launchd sends the service's stdout and stderr. Registrations made
/// before the log existed sent them to `/dev/null`.
fn output_keys(path: &str) -> String {
    format!(
        "<key>StandardOutPath</key><string>{path}</string>\n\
        <key>StandardErrorPath</key><string>{path}</string>\n"
    )
}

fn output_path(data_dir: &Path) -> Result<String> {
    escape(
        crate::logs::directory(data_dir)
            .join(crate::logs::OUTPUT)
            .to_str()
            .ok_or("Service: paths must be UTF-8")?,
    )
}

/// An older registration of this data directory, made to send the service's
/// output to the log; `None` when it needs no change or is not one.
fn upgraded(document: &str, data_dir: &Path) -> Result<Option<String>> {
    let older = output_keys("/dev/null");
    Ok(document
        .contains(&older)
        .then(|| output_path(data_dir))
        .transpose()?
        .map(|path| document.replacen(&older, &output_keys(&path), 1)))
}

/// Replaces a registration at once, never leaving a partial one.
fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Service: invalid registration")?;
    let next = path.with_file_name(format!("{name}.mynou-new"));
    let _ = fs::remove_file(&next);
    create(&next, bytes)?;
    fs::rename(&next, path).map_err(|_| "Service: cannot replace the registration".into())
}

fn plist(label: &str, binary: &Path, data_dir: &Path, home: &Path) -> Result<String> {
    let text = |p: &Path| escape(p.to_str().ok_or("Service: paths must be UTF-8")?);
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <plist version=\"1.0\"><dict>\n\
        <key>Label</key><string>{label}</string>\n\
        <key>ProgramArguments</key><array><string>{}</string><string>service-run</string><string>--dir</string><string>{}</string></array>\n\
        <key>WorkingDirectory</key><string>{}</string>\n\
        <key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string></dict>\n\
        <key>RunAtLoad</key><true/>\n\
        <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
        <key>ThrottleInterval</key><integer>60</integer>\n\
        <key>ExitTimeOut</key><integer>120</integer>\n\
        <key>Umask</key><integer>63</integer>\n\
        <key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>10240</integer></dict>\n\
        {}\
        </dict></plist>\n",
        text(binary)?,
        text(data_dir)?,
        text(data_dir)?,
        text(home)?,
        output_keys(&output_path(data_dir)?)
    ))
}

/// Set once this process ran itself again with its output in the log.
const ADOPTED: &str = "MYNOU_SERVICE_OUTPUT";

/// A service started by an older registration has its stdout and stderr on
/// `/dev/null`: it runs itself again with them in the log's output file,
/// keeping its process identity so that launchd keeps supervising it. Returns
/// when there is nothing to do, or with the reason it could not.
pub fn adopt_output(data_dir: &Path) -> Result<()> {
    use std::os::{fd::AsFd, unix::process::CommandExt};
    // Only a launchd job of Mynou, which launchd names in XPC_SERVICE_NAME.
    if std::env::var_os(ADOPTED).is_some() || launchd_label().is_none() {
        return Ok(());
    }
    let null = fs::metadata("/dev/null").map_err(|_| "Service: /dev/null unavailable")?;
    let stderr = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(fs::File::from)
        .and_then(|file| file.metadata())
        .map_err(|_| "Service: cannot inspect stderr")?;
    if !store::same_inode(&null, &stderr) {
        return Ok(());
    }
    let dir = crate::logs::directory(data_dir);
    crate::logs::prepare(&dir)?;
    let output = store::private_options()
        .append(true)
        .create(true)
        .open(dir.join(crate::logs::OUTPUT))
        .map_err(|_| "Service: cannot open the output file")?;
    if !output.metadata().is_ok_and(|m| m.is_file()) {
        return Err("Service: the output file is not a file".into());
    }
    let copy = output
        .try_clone()
        .map_err(|_| "Service: cannot open the output file")?;
    let executable = std::env::current_exe().map_err(|_| "Service: executable unavailable")?;
    let error = Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env(ADOPTED, "1")
        .stdout(copy)
        .stderr(output)
        .exec();
    Err(format!(
        "Service: cannot send the output to the log: {error}"
    ))
}

/// The registration launchd started this service from, rewritten when it is
/// an older one of this data directory, so that the next start sends the
/// output to the log too. launchd names the job in `XPC_SERVICE_NAME`.
/// Returns whether it changed.
pub fn refresh_registration(data_dir: &Path) -> Result<bool> {
    let Some(label) = launchd_label() else {
        return Ok(false);
    };
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("Service: HOME is missing")?);
    absolute(&home)?;
    let registration = home.join(format!("Library/LaunchAgents/{label}.plist"));
    if fs::symlink_metadata(&registration).is_err() {
        return Ok(false);
    }
    let document = read(&registration, 524_288)?;
    if !owns(&document, &label, data_dir)? {
        return Ok(false);
    }
    let Some(next) = upgraded(&document, data_dir)? else {
        return Ok(false);
    };
    crate::logs::prepare(&crate::logs::directory(data_dir))?;
    replace(&registration, next.as_bytes())?;
    Ok(true)
}

/// The label of the launchd job this process runs as, if it is Mynou's.
fn launchd_label() -> Option<String> {
    std::env::var("XPC_SERVICE_NAME")
        .ok()
        .filter(|label| valid_label(label))
}

fn valid_label(label: &str) -> bool {
    label.starts_with("org.mynou.")
        && label.len() <= 96
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// Refuse missing volumes before any engine opens or creates storage. Missing
/// mounts must never become newly created local directories under /Volumes.
pub fn preflight(data_dir: &Path, config: &Config) -> Result<()> {
    installation(data_dir)?;
    for (code, root) in [
        ("media_volume_unavailable", &config.movies_root),
        ("media_volume_unavailable", &config.series_root),
        ("download_volume_unavailable", &config.downloads.data_dir),
        ("state_directory_unavailable", &config.store_dir),
        ("state_directory_unavailable", &config.downloads.state_dir),
    ] {
        if absolute(root).is_err() || !root.is_dir() {
            let _ = log(data_dir, code);
            return Err(format!(
                "Service: {code}; mount the configured volumes and check account permissions"
            ));
        }
    }
    Ok(())
}

/// A fixed-code operational journal, at most two 64 KiB files. Raw error text,
/// paths, credentials and provider responses never enter this journal.
pub fn log(data_dir: &Path, code: &str) -> Result<()> {
    if !code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return Err("Service: invalid log code".into());
    }
    let root = data_dir.join(".mynou-service");
    directory(&root)?;
    let path = root.join("events.log");
    let mut file = match fs::symlink_metadata(&path) {
        Ok(_) => {
            let before = private(&path)?;
            let file = store::private_options()
                .append(true)
                .open(&path)
                .map_err(|_| "Service: cannot open event log")?;
            if !store::same_inode(
                &before,
                &file
                    .metadata()
                    .map_err(|_| "Service: cannot inspect event log")?,
            ) {
                return Err("Service: event log changed".into());
            }
            file
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            create(&path, b"")?;
            store::private_options()
                .append(true)
                .open(&path)
                .map_err(|_| "Service: cannot open event log")?
        }
        Err(_) => return Err("Service: event log unavailable".into()),
    };
    store::try_lock_file(&file).map_err(|_| "Service: event log busy")?;
    let line = format!("{} {code}\n", store::now());
    if file
        .metadata()
        .map_err(|_| "Service: event log unavailable")?
        .len()
        + line.len() as u64
        > MAX_LOG
    {
        let previous = root.join("events.previous.log");
        if previous.exists() {
            private(&previous)?;
        }
        // Bound the backup even if an owner manually enlarged the log.
        let _ = fs::remove_file(&previous);
        fs::rename(&path, &previous).map_err(|_| "Service: cannot rotate event log")?;
        drop(file);
        create(&path, line.as_bytes())?;
    } else {
        file.write_all(line.as_bytes())
            .map_err(|_| "Service: event log write failed")?;
    }
    Ok(())
}

fn launch(args: &[&str]) -> Result<std::process::Output> {
    Command::new("/bin/launchctl")
        .args(args)
        .output()
        .map_err(|_| "Service: launchctl unavailable; requires native macOS".into())
}

fn inspect(target: &str) -> Result<Option<String>> {
    let output = launch(&["print", target])?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        // A missing job and an unavailable login session are different states.
        let domain = target.rsplit_once('/').ok_or("Service: invalid domain")?.0;
        if !launch(&["print", domain])?.status.success() {
            return Err("Service: account login session unavailable; log in on macOS first".into());
        }
        Ok(None)
    }
}

fn pid(report: &str) -> Option<u32> {
    report
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = ").and_then(|s| s.parse().ok()))
}

fn owns(document: &str, label: &str, data_dir: &Path) -> Result<bool> {
    let text = document
        .lines()
        .filter(|l| !l.starts_with("<!DOCTYPE"))
        .collect::<Vec<_>>()
        .join("\n");
    let root = crate::xml::parse_xml(&text).map_err(|_| "Service: invalid registration")?;
    let dict = root.child("dict").ok_or("Service: invalid registration")?;
    let value = |key: &str| {
        dict.children
            .windows(2)
            .find(|pair| pair[0].local() == "key" && pair[0].text == key)
            .map(|pair| &pair[1])
    };
    let args = value("ProgramArguments").ok_or("Service: invalid registration")?;
    Ok(value("Label").is_some_and(|v| v.text == label)
        && args.children.len() == 4
        && args.children.iter().all(|e| e.local() == "string")
        && args.children[1].text == "service-run"
        && args.children[2].text == "--dir"
        && Some(args.children[3].text.as_str()) == data_dir.to_str())
}

/// Registration commands run only in the owning account's GUI domain. They
/// never invoke a shell, print launchctl's secret-bearing output or delete data.
pub fn manage(
    label: &str,
    action: &str,
    data_dir: &Path,
    config: &Config,
    stop: impl Fn(u32) -> Result<()>,
) -> Result<Value> {
    if !cfg!(target_os = "macos") {
        return Err("Service: native macOS is required".into());
    }
    if !matches!(
        action,
        "install" | "status" | "start" | "stop" | "uninstall"
    ) {
        return Err("Service: action requires install, status, start, stop or uninstall".into());
    }
    if !valid_label(label) {
        return Err("Service: label must start with org.mynou. and contain only letters, digits, dots and hyphens".into());
    }
    let identity = installation(data_dir)?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("Service: HOME is missing")?);
    absolute(&home)?;
    let agents = home.join("Library/LaunchAgents");
    // Existing LaunchAgents may legitimately be 0755; never change its mode.
    absolute(&agents)?;
    if !agents.exists() && matches!(action, "install") {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&agents)
            .map_err(|_| "Service: cannot create LaunchAgents")?;
    }
    let registration = agents.join(format!("{label}.plist"));
    // The installation must belong to the account that owns HOME. A temporary
    // create_new file yields our effective UID without unsafe code or id(1).
    let root = data_dir.join(".mynou-service");
    directory(&root)?;
    let probe = root.join(format!(
        "owner-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "Service: account clock unavailable")?
            .as_nanos()
    ));
    create(&probe, b"")?;
    let uid = fs::metadata(&probe)
        .map_err(|_| "Service: cannot inspect account")?
        .uid();
    fs::remove_file(&probe).map_err(|_| "Service: cannot remove account probe")?;
    if identity.uid() != uid
        || fs::metadata(&home)
            .map_err(|_| "Service: HOME unavailable")?
            .uid()
            != uid
    {
        return Err("Service: data directory and HOME must belong to the current account".into());
    }
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{label}");
    let registered = match fs::symlink_metadata(&registration) {
        Ok(_) => {
            private(&registration)?;
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err("Service: registration unavailable".into()),
    };
    let mut loaded = inspect(&target)?;
    if registered
        && (!owns(&read(&registration, 524_288)?, label, data_dir)?
            || private(&registration)?.uid() != uid)
    {
        return Err("Service: this registration belongs to another data directory".into());
    }
    if let Some(report) = &loaded {
        let expected = registration
            .to_str()
            .ok_or("Service: non-UTF-8 registration")?;
        if !registered
            || !report
                .lines()
                .any(|l| l.trim().strip_prefix("path = ") == Some(expected))
        {
            return Err("Service: this launchd job belongs to another registration".into());
        }
    }
    if action == "install" {
        preflight(data_dir, config)?;
        let binary = std::env::current_exe().map_err(|_| "Service: executable unavailable")?;
        absolute(&binary)?;
        let document = plist(label, &binary, data_dir, &home)?;
        // launchd opens the output file before Mynou starts.
        crate::logs::prepare(&crate::logs::directory(data_dir))?;
        if registered {
            let existing = read(&registration, 524_288)?;
            if existing != document {
                // One made before the log only gains its output file; it
                // applies the next time launchd loads it.
                if upgraded(&existing, data_dir)?.as_deref() != Some(document.as_str()) {
                    return Err("Service: existing registration differs; stop and uninstall it before reinstalling".into());
                }
                replace(&registration, document.as_bytes())?;
            }
        } else {
            if loaded.is_some() {
                return Err("Service: a different job already owns this label".into());
            }
            create(&registration, document.as_bytes())?;
        }
    }
    if matches!(action, "install" | "start") {
        if action == "start" && !registered {
            return Err("Service: install the registration first".into());
        }
        preflight(data_dir, config)?;
        if loaded.is_none() {
            let file = registration.to_str().ok_or("Service: non-UTF-8 path")?;
            if !launch(&["bootstrap", &domain, file])?.status.success() {
                return Err(
                    "Service: launchd bootstrap failed; check login session and macOS permissions"
                        .into(),
                );
            }
        } else if loaded.as_deref().and_then(pid).is_none()
            && !launch(&["kickstart", &target])?.status.success()
        {
            return Err("Service: launchd start failed".into());
        }
        loaded = inspect(&target)?;
    }
    if matches!(action, "stop" | "uninstall") {
        if let Some(process_id) = loaded.as_deref().and_then(pid) {
            stop(process_id)?;
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                loaded = inspect(&target)?;
                if loaded.as_deref().and_then(pid).is_none() {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(
                        "Service: graceful stop timed out; registration and data preserved".into(),
                    );
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        if loaded.is_some() && !launch(&["bootout", &target])?.status.success() {
            return Err("Service: unload failed; registration and data preserved".into());
        }
        loaded = None;
        if action == "uninstall" && registered {
            fs::remove_file(&registration).map_err(|_| "Service: cannot remove registration")?;
        }
    }
    let mut report = Value::object();
    report.insert("label", label);
    report.insert("scope", "account_login");
    report.insert(
        "installed",
        if action == "uninstall" {
            false
        } else {
            registered || action == "install"
        },
    );
    report.insert("loaded", loaded.is_some());
    report.insert("running", loaded.as_deref().and_then(pid).is_some());
    report.insert(
        "logs",
        crate::logs::directory(data_dir)
            .to_str()
            .ok_or("Service: non-UTF-8 path")?,
    );
    Ok(report)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
