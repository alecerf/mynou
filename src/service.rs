//! Account-owned launchd registration. No shell or companion runtime process.
use crate::{Result, config::Config, json::Value, store};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Component, Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const MAX_LOG: u64 = 65_536;
static LOG_CONFIG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Replace the foreground migration reporter with secret-free service events.
pub fn report_progress(config: &Path) {
    let _ = LOG_CONFIG.set(config.to_owned());
    crate::progress::set_reporter(|_| {
        if let Some(path) = LOG_CONFIG.get() {
            let _ = log(path, "storage_migration");
        }
    });
}

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

fn environment(text: &str) -> Result<BTreeMap<String, String>> {
    let mut values = BTreeMap::new();
    for line in text
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
    {
        let (key, value) = line
            .split_once('=')
            .ok_or("Service: .env requires unquoted NAME=value lines")?;
        if key.is_empty()
            || key.len() > 128
            || value.len() > 4096
            || !key
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_uppercase() || i > 0 && b.is_ascii_digit())
            || value.chars().any(char::is_control)
            || key.starts_with("DYLD_")
            || key.starts_with("LD_")
            || matches!(key, "HOME" | "PATH" | "TMPDIR")
            || values.insert(key.into(), value.into()).is_some()
        {
            return Err("Service: invalid, duplicate or reserved .env entry".into());
        }
    }
    Ok(values)
}

fn plist(
    label: &str,
    binary: &Path,
    config: &Path,
    home: &Path,
    values: &BTreeMap<String, String>,
) -> Result<String> {
    let text = |p: &Path| escape(p.to_str().ok_or("Service: paths must be UTF-8")?);
    let mut env = String::new();
    for (k, v) in values {
        env.push_str(&format!(
            "<key>{}</key><string>{}</string>",
            escape(k)?,
            escape(v)?
        ));
    }
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <plist version=\"1.0\"><dict>\n\
        <key>Label</key><string>{label}</string>\n\
        <key>ProgramArguments</key><array><string>{}</string><string>service-run</string><string>--config</string><string>{}</string></array>\n\
        <key>WorkingDirectory</key><string>{}</string>\n\
        <key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string>{env}</dict>\n\
        <key>RunAtLoad</key><true/>\n\
        <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n\
        <key>ThrottleInterval</key><integer>60</integer>\n\
        <key>ExitTimeOut</key><integer>120</integer>\n\
        <key>Umask</key><integer>63</integer>\n\
        <key>StandardOutPath</key><string>/dev/null</string>\n\
        <key>StandardErrorPath</key><string>/dev/null</string>\n\
        </dict></plist>\n",
        text(binary)?,
        text(config)?,
        text(
            config
                .parent()
                .ok_or("Service: missing configuration directory")?
        )?,
        text(home)?
    ))
}

/// Refuse missing volumes before any engine opens or creates storage. Missing
/// mounts must never become newly created local directories under /Volumes.
pub fn preflight(path: &Path, config: &Config) -> Result<()> {
    private(path)?;
    for (code, root) in [
        ("media_volume_unavailable", &config.movies_root),
        ("media_volume_unavailable", &config.series_root),
        ("download_volume_unavailable", &config.downloads.data_dir),
        ("state_directory_unavailable", &config.store_dir),
        ("state_directory_unavailable", &config.downloads.state_dir),
    ] {
        if absolute(root).is_err() || !root.is_dir() {
            let _ = log(path, code);
            return Err(format!(
                "Service: {code}; mount the configured volumes and check account permissions"
            ));
        }
    }
    Ok(())
}

/// A fixed-code operational journal, at most two 64 KiB files. Raw error text,
/// paths, credentials and provider responses never enter this journal.
pub fn log(config: &Path, code: &str) -> Result<()> {
    if !code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return Err("Service: invalid log code".into());
    }
    let root = config
        .parent()
        .ok_or("Service: missing configuration directory")?
        .join(".mynou-service");
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
    file.try_lock().map_err(|_| "Service: event log busy")?;
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

fn owns(document: &str, label: &str, config: &Path) -> Result<bool> {
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
        && args.children[2].text == "--config"
        && Some(args.children[3].text.as_str()) == config.to_str())
}

/// Registration commands run only in the owning account's GUI domain. They
/// never invoke a shell, print launchctl's secret-bearing output or delete data.
pub fn manage(
    label: &str,
    action: &str,
    path: &Path,
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
    if !label.starts_with("org.mynou.")
        || label.len() > 96
        || !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err("Service: label must start with org.mynou. and contain only letters, digits, dots and hyphens".into());
    }
    private(path)?;
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
    // The configuration must belong to the account that owns HOME. A temporary
    // create_new file yields our effective UID without unsafe code or id(1).
    let root = path
        .parent()
        .ok_or("Service: missing configuration directory")?
        .join(".mynou-service");
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
    if private(path)?.uid() != uid
        || fs::metadata(&home)
            .map_err(|_| "Service: HOME unavailable")?
            .uid()
            != uid
    {
        return Err("Service: configuration and HOME must belong to the current account".into());
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
        && (!owns(&read(&registration, 524_288)?, label, path)?
            || private(&registration)?.uid() != uid)
    {
        return Err("Service: this registration belongs to another configuration".into());
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
        preflight(path, config)?;
        let env_path = path
            .parent()
            .ok_or("Service: missing configuration directory")?
            .join(".env");
        if private(&env_path)?.uid() != uid {
            return Err("Service: .env belongs to another account".into());
        }
        let values = environment(&read(&env_path, 65_536)?)?;
        if !values.get(&config.api_token_env).is_some_and(|v| {
            (32..=4096).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic())
        }) {
            return Err("Service: .env must contain the API token (32–4096 graphic bytes)".into());
        }
        let binary = std::env::current_exe().map_err(|_| "Service: executable unavailable")?;
        absolute(&binary)?;
        let document = plist(label, &binary, path, &home, &values)?;
        if registered {
            if read(&registration, 524_288)? != document {
                return Err("Service: existing registration differs; stop and uninstall it before reinstalling".into());
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
        preflight(path, config)?;
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
    report.insert("logs", "private_fixed_codes_two_64k_files");
    Ok(report)
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
