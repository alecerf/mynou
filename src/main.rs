#![forbid(unsafe_code)]
use mynou::{
    Result,
    config::{self, Config},
    engine::Engine,
    json::{self, Value},
    net::HttpClient,
    store::Store,
};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const HELP: &str = "Mynou — media automation using Rust std only

  serve [--dir DIR]
  service --action install|status|start|stop|uninstall --dir /absolute/data-directory
          [--label org.mynou.automation]
  update [--dir DIR]   (installs the latest release now; Mynou also does it once a day)
  version

DIR is the data directory: --dir, else MYNOU_HOME, else ~/Library/Application Support/Mynou.
Run `mynou serve`, then open the web interface and set Mynou up there.
";
struct Args {
    command: String,
    options: BTreeMap<String, String>,
}
impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let command = args.next().unwrap_or("help".into());
        let mut options: BTreeMap<String, String> = BTreeMap::new();
        while let Some(arg) = args.next() {
            let Some(key) = arg.strip_prefix("--") else {
                return Err(format!("{command} takes no argument: {arg}"));
            };
            let value = if key == "help" {
                "true".into()
            } else {
                args.next()
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("Missing value for --{key}"))?
            };
            if options.insert(key.into(), value).is_some() {
                return Err(format!("Duplicate option --{key}"));
            }
        }
        let allowed: &[&str] = match command.as_str() {
            "service" => &["dir", "help", "action", "label"],
            "serve" | "service-run" | "update" => &["dir", "help"],
            "help" | "--help" | "version" | "--version" => &[],
            _ => return Err(format!("Unknown command: {command}")),
        };
        for key in options.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("Unknown option: --{key}"));
            }
        }
        Ok(Self { command, options })
    }
    fn value<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.options.get(key).map_or(default, String::as_str)
    }
    fn data_dir(&self) -> Result<PathBuf> {
        config::data_dir(self.options.get("dir").map(String::as_str))
    }
}
fn output(value: &Value) {
    println!("{}", json::stringify(value));
}

fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())
}
fn endpoint(config: &Config) -> Result<String> {
    let addr = config
        .listen
        .parse::<std::net::SocketAddr>()
        .map_err(|_| "Invalid API address")?;
    let host = if addr.ip().is_unspecified() {
        if addr.is_ipv4() {
            "127.0.0.1".into()
        } else {
            "[::1]".into()
        }
    } else if addr.is_ipv6() {
        format!("[{}]", addr.ip())
    } else {
        addr.ip().to_string()
    };
    Ok(format!("http://{host}:{}", addr.port()))
}
/// Where a running service records the address it listens on. Commands find
/// the service there, since its settings are inside the storage it holds.
fn endpoint_file(path: &Path) -> PathBuf {
    path.join("state/endpoint")
}
fn record_endpoint(path: &Path, address: std::net::SocketAddr) -> Result<()> {
    let file = endpoint_file(path);
    let _ = fs::remove_file(&file);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    }
    private_write(&file, format!("{address}\n").as_bytes())
}
/// The address and settings of the service running for this data directory.
/// A stale address or another service count as none.
fn served(path: &Path) -> Option<(String, Value)> {
    let mut text = String::new();
    fs::File::open(endpoint_file(path))
        .and_then(|f| f.take(256).read_to_string(&mut text))
        .ok()?;
    let address = text
        .trim()
        .parse::<std::net::SocketAddr>()
        .ok()?
        .to_string();
    let mut probe = config::from_json(&config::default_json(), path).ok()?;
    probe.listen = address.clone();
    let url = endpoint(&probe).ok()?;
    HttpClient::new()
        .without_proxy()
        .with_timeout(Duration::from_millis(300))
        .get(&format!("{url}/healthz"))
        .ok()
        .filter(|r| r.status == 200)?;
    let reply = call(&probe, "GET", "/api/settings", None).ok()?;
    (reply.get("data_dir").and_then(Value::as_str) == path.to_str()).then_some((address, reply))
}
/// The settings the running service uses, else the saved ones, and whether
/// commands should go through the service's API.
fn context(path: &Path) -> Result<(Config, bool)> {
    let Some((address, reply)) = served(path) else {
        return Ok((config::load(path)?, false));
    };
    let mut config = config::from_json(
        reply
            .get("active")
            .ok_or("The service reported no settings")?,
        path,
    )?;
    config.listen = address;
    Ok((config, true))
}
fn call(config: &Config, method: &str, route: &str, body: Option<&Value>) -> Result<Value> {
    let url = format!("{}{route}", endpoint(config)?);
    let bytes = body.map_or_else(Vec::new, |v| json::stringify(v).into_bytes());
    let response = HttpClient::new()
        .without_proxy()
        .with_timeout(Duration::from_secs(120))
        .request(
            method,
            &url,
            &[("Content-Type".into(), "application/json".into())],
            &bytes,
        )?;
    let value = json::parse(
        std::str::from_utf8(&response.body).map_err(|_| "Response is not valid UTF-8")?,
    )?;
    if !(200..300).contains(&response.status) {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("API error")
            .into());
    }
    Ok(value)
}
/// Creates the data directory, private to its owner, when it is missing.
fn ensure_data_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("Cannot create {}: {e}", path.display()))
}
fn execute(args: Args) -> Result<()> {
    if args.options.contains_key("help") {
        print!("{HELP}");
        return Ok(());
    }
    match args.command.as_str() {
        "help" | "--help" => {
            print!("{HELP}");
            return Ok(());
        }
        "version" | "--version" => {
            println!(
                "mynou {} — Rust std, 0 dependencies",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(());
        }
        _ => {}
    }
    let path = args.data_dir()?;
    let supervised = args.command == "service-run";
    if supervised {
        mynou::service::log(&path, "starting")?;
        mynou::service::report_progress(&path);
    }
    let (config, online) = if args.command == "serve" || supervised {
        (config::load(&path)?, false)
    } else {
        context(&path)?
    };
    if args.command == "service" {
        output(&mynou::service::manage(
            args.value("label", "org.mynou.automation"),
            args.value("action", "status"),
            &path,
            &config,
            |process_id| {
                let mut expected = Value::object();
                expected.insert("expected_pid", process_id);
                call(&config, "POST", "/api/shutdown", Some(&expected)).map(|_| ())
            },
        )?);
        return Ok(());
    }
    if args.command == "update" {
        if online {
            let mut reply = call(&config, "POST", "/api/update", None)?;
            if reply.get("staged").and_then(Value::as_str).is_some() {
                reply = call(&config, "POST", "/api/update/install", None)?;
            }
            output(&reply);
        } else {
            mynou::update::check_now();
            let installed = match mynou::update::staged() {
                Some(staged) => {
                    // Stopped: nothing runs while the state is backed up.
                    let _lock = Store::open_for_management(&config)?;
                    mynou::update::apply(&path, &staged)?;
                    Some(mynou::update::display(staged.version))
                }
                None => None,
            };
            let mut reply = mynou::update::report();
            reply.insert("installed", installed.map_or(Value::Null, Value::String));
            output(&reply);
        }
        return Ok(());
    }
    // `serve` or `service-run`.
    if supervised {
        mynou::service::preflight(&path, &config)?;
    }
    ensure_data_dir(&path)?;
    // The listener answers health and migration progress while storage opens.
    let access = mynou::access::stored(&config.store_dir)?;
    let startup = mynou::server::Startup::bind(
        &config,
        std::sync::Arc::new(mynou::access::Access::new(access)),
    )?;
    record_endpoint(&path, startup.address()?)?;
    if !supervised {
        eprintln!(
            "mynou {} — {} — API http://{}",
            env!("CARGO_PKG_VERSION"),
            path.display(),
            startup.address()?
        );
    }
    let engine = Engine::open_for_service(config)?;
    // The storage opened: an update that led here is kept.
    mynou::update::confirm(&path);
    let api = startup.serve(engine)?;
    let result = api.run_supervised();
    let _ = fs::remove_file(endpoint_file(&path));
    if result.is_ok()
        && mynou::update::restart_requested()
        && let Some(staged) = mynou::update::staged()
    {
        match mynou::update::apply(&path, &staged) {
            Ok(()) if supervised => mynou::service::log(&path, "updating")?,
            Ok(()) => eprintln!(
                "mynou: installing {}",
                mynou::update::display(staged.version)
            ),
            // Restart unchanged rather than stay stopped.
            Err(error) => eprintln!("mynou: {error}"),
        }
        return Err(mynou::update::restart());
    }
    if supervised && result.is_ok() {
        mynou::service::log(&path, "stopped_cleanly")?;
    }
    result
}

fn main() {
    mynou::progress::set_reporter(|migration| eprintln!("mynou: {}", migration.describe()));
    let args = Args::parse();
    let service_path = args
        .as_ref()
        .ok()
        .filter(|a| a.command == "service-run")
        .and_then(|args| args.data_dir().ok());
    let serving_path = args
        .as_ref()
        .ok()
        .filter(|a| matches!(a.command.as_str(), "serve" | "service-run"))
        .and_then(|args| args.data_dir().ok());
    if let Err(mut error) = args.and_then(execute) {
        // The version an update just installed could not start: restart on
        // the previous version and the state backed up before the update.
        if let Some(path) = &serving_path
            && mynou::update::recover(path) == Ok(true)
        {
            if service_path.is_some() {
                let _ = mynou::service::log(path, "update_rolled_back");
            }
            eprintln!("mynou: {error}; restoring the previous version");
            error = mynou::update::restart();
        }
        if let Some(path) = service_path {
            let _ = mynou::service::log(&path, "runtime_failed");
        } else {
            eprintln!("mynou: {error}");
        }
        std::process::exit(1);
    }
}
