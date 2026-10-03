#![forbid(unsafe_code)]
use mynou::{
    Result,
    config::{self, Config},
    crypto,
    engine::{Engine, public_job},
    integrations,
    json::{self, Value},
    media,
    net::HttpClient,
    server::Api,
    store::{Request, Store},
};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

const HELP: &str = "Mynou — media automation using Rust std only

  init [--config mynou.json]
  analyze FILE [--json]
  doctor [--config mynou.json]
  serve [--config mynou.json]
  submit --title TITLE [--kind movie|episode|series|file] [--year YEAR]
         [--season N --episode N] [--path FILE | --url MAGNET_OR_TORRENT]
         [--tmdb-id N] [--config mynou.json]
  search --title TITLE [--kind movie|episode] [--year YEAR]
         [--season N --episode N] [--tmdb-id N] [--config mynou.json]
  library [--config mynou.json]
  upgrades [--apply] [--config mynou.json]
  monitor | unmonitor ID [--config mynou.json]
  baseline ID --release-title TITLE [--config mynou.json]
  jobs | status | sync [--config mynou.json]
  show | events | retry | cancel ID [--config mynou.json]
  healthcheck [--config mynou.json]
  setup-docker [--dir mynou-docker]
  demo [--dir mynou-demo]
  version

Search previews profile decisions without submitting or downloading media.
Management commands use the API while the service is running, otherwise the local journal.
The demo uses only synthetic media and local services.
";
struct Args {
    command: String,
    positions: Vec<String>,
    options: BTreeMap<String, String>,
}
impl Args {
    fn parse() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let command = args.next().unwrap_or("help".into());
        let mut positions = Vec::new();
        let mut options: BTreeMap<String, String> = BTreeMap::new();
        while let Some(arg) = args.next() {
            if let Some(key) = arg.strip_prefix("--") {
                let value = if ["json", "help", "apply"].contains(&key) {
                    "true".into()
                } else {
                    args.next()
                        .filter(|v| !v.starts_with("--"))
                        .ok_or_else(|| format!("Missing value for --{key}"))?
                };
                if options.insert(key.into(), value).is_some() {
                    return Err(format!("Duplicate option --{key}"));
                }
            } else {
                positions.push(arg);
            }
        }
        let allowed: &[&str] = match command.as_str() {
            "init" | "doctor" | "serve" | "jobs" | "status" | "sync" | "show" | "events"
            | "retry" | "cancel" | "healthcheck" | "library" | "monitor" | "unmonitor" => &["config", "help"],
            "upgrades" => &["config", "help", "apply"],
            "baseline" => &["config", "help", "release-title"],
            "submit" => &[
                "config", "title", "kind", "year", "season", "episode", "path", "url", "tmdb-id",
                "help",
            ],
            "search" => &[
                "config", "title", "kind", "year", "season", "episode", "tmdb-id", "help",
            ],
            "analyze" => &["json", "help"],
            "demo" | "setup-docker" => &["dir", "help"],
            "help" | "--help" | "version" | "--version" => &[],
            _ => return Err(format!("Unknown command: {command}")),
        };
        for key in options.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("Unknown option: --{key}"));
            }
        }
        let required = usize::from(
            ["analyze", "show", "events", "retry", "cancel", "monitor", "unmonitor", "baseline"].contains(&command.as_str()),
        );
        if !options.contains_key("help") && positions.len() != required {
            return Err(format!("{command} requires {required} argument(s)"));
        }
        Ok(Self {
            command,
            positions,
            options,
        })
    }
    fn value<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.options.get(key).map_or(default, String::as_str)
    }
    fn number(&self, key: &str) -> Result<u32> {
        self.value(key, "0")
            .parse()
            .map_err(|_| format!("--{key} requires a nonnegative integer"))
    }
    fn config_path(&self) -> PathBuf {
        PathBuf::from(self.value("config", "mynou.json"))
    }
}
fn output(value: &Value) {
    println!("{}", json::stringify(value));
}

fn existing_store(config: &Config) -> Result<bool> {
    mynou::store::reject_symlinks(&config.store_dir)?;
    match fs::symlink_metadata(&config.store_dir) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err("Storage must be a directory".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot access storage: {error}")),
    }
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
fn random_token() -> Result<String> {
    Ok(crypto::random_bytes::<32>()?
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect())
}
fn api_token(config: &Config, path: &Path) -> Result<String> {
    if std::env::var_os(&config.api_token_env).is_some_and(|v| !v.is_empty()) {
        return config::secret(&config.api_token_env);
    }
    let file = path.parent().unwrap_or(Path::new(".")).join(".env");
    use std::io::Read;
    let mut data = String::new();
    fs::File::open(file)
        .and_then(|f| f.take(65_537).read_to_string(&mut data))
        .map_err(|_| {
            format!(
                "Missing secret {}; set the environment variable or run init",
                config.api_token_env
            )
        })?;
    if data.len() > 65_536 {
        return Err(".env file is too large".into());
    }
    for line in data.lines() {
        if let Some((key, value)) = line.split_once('=')
            && key == config.api_token_env
            && value.len() >= 32
            && value.len() <= 4096
            && value.bytes().all(|b| b.is_ascii_graphic())
        {
            return Ok(value.into());
        }
    }
    Err(format!("Missing secret {}", config.api_token_env))
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
fn running(config: &Config) -> bool {
    endpoint(config)
        .ok()
        .and_then(|url| {
            HttpClient::new()
                .without_proxy()
                .with_timeout(Duration::from_millis(300))
                .get(&format!("{url}/healthz"))
                .ok()
        })
        .is_some_and(|r| r.status == 200)
}
fn call(
    config: &Config,
    path: &Path,
    method: &str,
    route: &str,
    body: Option<&Value>,
) -> Result<Value> {
    let token = api_token(config, path)?;
    let url = format!("{}{route}", endpoint(config)?);
    let bytes = body.map_or_else(Vec::new, |v| json::stringify(v).into_bytes());
    let response = HttpClient::new()
        .without_proxy()
        .with_timeout(Duration::from_secs(120))
        .request(
            method,
            &url,
            &[
                ("Authorization".into(), format!("Bearer {token}")),
                ("Content-Type".into(), "application/json".into()),
            ],
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
fn request(args: &Args) -> Result<Request> {
    let source_path = match args.options.get("path") {
        Some(p) => Some(
            fs::canonicalize(p)
                .map_err(|e| format!("Source file: {e}"))?
                .to_str()
                .ok_or("Source path is not valid UTF-8")?
                .into(),
        ),
        None => None,
    };
    let request = Request {
        kind: args
            .value(
                "kind",
                if source_path.is_some() {
                    "file"
                } else {
                    "movie"
                },
            )
            .into(),
        title: args
            .options
            .get("title")
            .ok_or("--title is required")?
            .clone(),
        year: args.number("year")?,
        season: args.number("season")?,
        episode: args.number("episode")?,
        source_path,
        source_url: args.options.get("url").cloned(),
        tmdb_id: args
            .options
            .get("tmdb-id")
            .map(|v| v.parse::<u64>().map_err(|_| "Invalid --tmdb-id"))
            .transpose()?,
    };
    request.validate()?;
    Ok(request)
}
fn init(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let env = path.parent().unwrap_or(Path::new(".")).join(".env");
    if path.exists() || env.exists() {
        return Err("Configuration or .env already exists; refusing to overwrite".into());
    }
    let token = random_token()?;
    private_write(&env,format!("MYNOU_API_TOKEN={token}\nMYNOU_PLEX_TOKEN=\nMYNOU_TMDB_TOKEN=\nMYNOU_TMDB_API_KEY=\nMYNOU_INDEXER_API_KEY=\n").as_bytes())?;
    if let Err(e) = private_write(path, format!("{}\n", json::stringify(value)).as_bytes()) {
        let _ = fs::remove_file(&env);
        return Err(e);
    }
    Ok(())
}
fn docker_config() -> Value {
    let mut v = config::default_json();
    v.insert("store_dir", "/data/jobs");
    v.insert("listen", "0.0.0.0:8787");
    if let Some(Value::Object(d)) = v.get_mut("downloads") {
        d.insert("data_dir".into(), "/data/downloads".into());
        d.insert("state_dir".into(), "/data/torrents".into());
    }
    if let Some(Value::Object(l)) = v.get_mut("library") {
        l.insert("movies_root".into(), "/library/movies".into());
        l.insert("series_root".into(), "/library/series".into());
    }
    v
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
        "analyze" => {
            let analyzed = media::analyze(Path::new(&args.positions[0]))?;
            if args.options.contains_key("json") {
                output(&analyzed.to_json());
            } else {
                println!(
                    "{}\nContainer: {}\nSize: {} bytes\nDuration: {} s\nVideo: {} streams; audio: {} streams",
                    analyzed.title,
                    analyzed.container,
                    analyzed.size_bytes,
                    analyzed
                        .duration_seconds
                        .map_or("unknown".into(), |v| format!("{v:.3}")),
                    analyzed.video_streams.len(),
                    analyzed.audio_streams.len()
                );
                for stream in analyzed.video_streams {
                    println!(
                        "  Video {}: {} {}×{}",
                        stream.index,
                        stream.codec,
                        stream.width.unwrap_or(0),
                        stream.height.unwrap_or(0)
                    );
                }
                for stream in analyzed.audio_streams {
                    println!(
                        "  Audio {}: {} {} Hz, {} channel(s)",
                        stream.index,
                        stream.codec,
                        stream.sample_rate.unwrap_or(0),
                        stream.channels.unwrap_or(0)
                    );
                }
            }
            return Ok(());
        }
        "demo" => {
            output(&mynou::demo::run(Path::new(
                args.value("dir", "mynou-demo"),
            ))?);
            return Ok(());
        }
        "init" => {
            init(&args.config_path(), &config::default_json())?;
            println!(
                "Created configuration and .env. Run: mynou serve --config {}",
                args.config_path().display()
            );
            return Ok(());
        }
        "setup-docker" => {
            let dir = Path::new(args.value("dir", "mynou-docker"));
            if dir.exists() {
                return Err("The Docker installation directory must not already exist".into());
            }
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            init(&dir.join("mynou.json"), &docker_config())?;
            private_write(
                &dir.join("compose.yaml"),
                include_bytes!("../deploy-compose.yaml"),
            )?;
            for name in ["data", "library/movies", "library/series"] {
                fs::create_dir_all(dir.join(name)).map_err(|e| e.to_string())?;
            }
            println!(
                "Created installation in {}. Build the image from source: docker build -t mynou:{} . ; then run: cd {} && docker compose up -d",
                dir.display(),
                env!("CARGO_PKG_VERSION"),
                dir.display()
            );
            return Ok(());
        }
        _ => {}
    }
    let path = args.config_path();
    let config = config::load(&path)?;
    if args.command == "healthcheck" {
        let r = HttpClient::new()
            .without_proxy()
            .with_timeout(Duration::from_secs(2))
            .get(&format!("{}/readyz", endpoint(&config)?))?;
        if r.status != 200 {
            return Err("Service unavailable".into());
        }
        return Ok(());
    }
    if args.command == "doctor" {
        let mut v = Value::object();
        v.insert("version", env!("CARGO_PKG_VERSION"));
        v.insert("dependencies", 0_u32);
        v.insert("configuration", "valid");
        v.insert(
            "api_token",
            if api_token(&config, &path).is_ok() {
                "present"
            } else {
                "absent"
            },
        );
        v.insert(
            "service",
            if running(&config) {
                "running"
            } else {
                "stopped"
            },
        );
        v.insert("plex_enabled", config.plex.enabled);
        v.insert("indexers", config.sources.len() as u32);
        v.insert("movie_profile", config.selection.movie_profile.clone());
        v.insert("episode_profile", config.selection.episode_profile.clone());
        v.insert("downloads_enabled", config.downloads_enabled);
        v.insert("monitoring_enabled", config.monitoring.enabled);
        v.insert(
            "native_media_formats",
            "MP4/MOV, Matroska/WebM, AVI, WAV/RF64, FLAC, MP3",
        );
        output(&v);
        return Ok(());
    }
    if args.command == "serve" {
        let token = api_token(&config, &path)?;
        let engine = Engine::open(config)?;
        let api = Api::bind(engine.clone(), token)?;
        eprintln!(
            "mynou {} — API http://{}",
            env!("CARGO_PKG_VERSION"),
            api.address()?
        );
        let _workers = engine.start();
        return api.run();
    }
    let online = running(&config);
    match args.command.as_str() {
        "library" => {
            if online {
                output(&call(&config, &path, "GET", "/api/library", None)?);
            } else if existing_store(&config)? {
                let store = Store::open_read_only(&config.store_dir)?;
                output(&mynou::library::describe(&store.library_jobs(), &store.list()));
            } else {
                output(&Value::Array(Vec::new()));
            }
        }
        "upgrades" => {
            let apply = args.options.contains_key("apply");
            if online {
                let mut body = Value::object();
                body.insert("apply", apply);
                output(&call(&config, &path, "POST", "/api/upgrades", Some(&body))?);
            } else if apply {
                output(&Engine::open_for_management(config)?.check_upgrades(true)?);
            } else if existing_store(&config)? {
                output(&Engine::open_for_preview(config)?.check_upgrades(false)?);
            } else {
                output(&mynou::library::empty_preview());
            }
        }
        "monitor" | "unmonitor" | "baseline" => {
            let id = &args.positions[0];
            if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Invalid library entry ID".into());
            }
            let baseline = args.command == "baseline";
            let mut body = Value::object();
            if baseline {
                body.insert("release_title", args.options.get("release-title")
                    .ok_or("--release-title is required")?.clone());
            } else {
                body.insert("enabled", args.command == "monitor");
            }
            if online {
                let route = format!("/api/library/{id}/{}", if baseline { "baseline" } else { "monitor" });
                output(&call(&config, &path, "POST", &route, Some(&body))?);
            } else {
                let mut store = Store::open(&config.store_dir)?;
                let job = if baseline {
                    let job = store.get(id).ok_or("Unknown library entry")?;
                    let title = args.options.get("release-title").ok_or("--release-title is required")?;
                    let release = mynou::library::validate_baseline(&config, &job, title)?;
                    store.set_baseline(id, release)?
                } else {
                    store.set_monitored(id, args.command == "monitor")?
                };
                output(&public_job(&job));
            }
        }
        "search" => {
            let r = request(&args)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    "/api/search",
                    Some(&r.to_json()),
                )?);
            } else {
                output(&integrations::search_report(&config, &r)?);
            }
        }
        "submit" => {
            let r = request(&args)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    "/api/jobs",
                    Some(&r.to_json()),
                )?);
            } else {
                let requests = integrations::expand(&config, &r)?;
                let mut store = Store::open(&config.store_dir)?;
                let jobs: Vec<_> = requests
                    .into_iter()
                    .map(|r| store.submit(r).map(|j| public_job(&j)))
                    .collect::<Result<_>>()?;
                output(&Value::Array(jobs));
            }
        }
        "jobs" => {
            if online {
                output(&call(&config, &path, "GET", "/api/jobs", None)?);
            } else {
                output(&Value::Array(
                    Store::open(&config.store_dir)?
                        .list()
                        .iter()
                        .map(public_job)
                        .collect(),
                ));
            }
        }
        "status" => {
            if online {
                output(&call(&config, &path, "GET", "/api/status", None)?);
            } else {
                let store = Store::open(&config.store_dir)?;
                let mut v = Value::object();
                v.insert("service", "stopped");
                v.insert("jobs", store.list().len() as u32);
                output(&v);
            }
        }
        "sync" => {
            if online {
                output(&call(&config, &path, "POST", "/api/sync", None)?);
            } else {
                let engine = Engine::open(config)?;
                let mut v = Value::object();
                v.insert("submitted", engine.sync()? as u32);
                output(&v);
            }
        }
        "show" | "events" | "retry" | "cancel" => {
            let id = &args.positions[0];
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Invalid job ID".into());
            }
            if online {
                let route = if args.command == "show" {
                    format!("/api/jobs/{id}")
                } else {
                    format!("/api/jobs/{id}/{}", args.command)
                };
                let method = if ["retry", "cancel"].contains(&args.command.as_str()) {
                    "POST"
                } else {
                    "GET"
                };
                output(&call(&config, &path, method, &route, None)?);
            } else {
                let mut store = Store::open(&config.store_dir)?;
                let v = match args.command.as_str() {
                    "show" => public_job(&store.get(id).ok_or("Unknown job")?),
                    "events" => {
                        Value::Array(store.events(id).iter().map(|e| e.to_json()).collect())
                    }
                    "retry" => {
                        let mut job = store.retry(id)?;
                        if job.imports.is_empty()
                            && job.request.source_path.is_none()
                            && job.request.source_url.is_none()
                        {
                            if let Some(old_id) = &job.download_id {
                                let shared = store.list().iter().any(|other| {
                                    other.id != job.id
                                        && other.download_id.as_ref() == Some(old_id)
                                        && other.state != "cancelled"
                                });
                                if !shared {
                                    mynou::torrent::pause_persisted(&config.downloads, old_id)?;
                                }
                            }
                            job.acquisition_url = None;
                            job.download_id = None;
                            job.files.clear();
                            job.progress = 0.0;
                            store.update(job.clone())?;
                        }
                        public_job(&job)
                    }
                    _ => public_job(&store.cancel(id)?),
                };
                output(&v);
            }
        }
        _ => return Err("Unknown command".into()),
    }
    Ok(())
}
fn main() {
    if let Err(error) = Args::parse().and_then(execute) {
        eprintln!("mynou: {error}");
        std::process::exit(1);
    }
}
