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
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const HELP: &str = "Mynou — media automation using Rust std only

  init [--config mynou.json]
  analyze FILE [--json]
  nzb-inspect FILE
  doctor [--config mynou.json]
  serve [--config mynou.json]
  submit --title TITLE [--kind movie|episode|series|file] [--year YEAR]
         [--season N --episode N] [--path FILE | --url MAGNET_OR_TORRENT]
         [--tmdb-id N] [--source-numbering JSON] [--config mynou.json]
  search --title TITLE [--kind movie|episode] [--year YEAR]
         [--season N --episode N] [--tmdb-id N] [--source-numbering JSON] [--config mynou.json]
  track-series --title TITLE [--year YEAR --tmdb-id N --season N]
               [--future-only] [--include-specials] [--unmonitored] [--config mynou.json]
  series-pack ID --url MAGNET_OR_TORRENT --mapping FILE [--config mynou.json]
  series-shared-file ID --url MAGNET_OR_TORRENT --mapping FILE
      [--apply --plan-id ID] [--config mynou.json]
  library-group ID --mapping FILE [--apply --plan-id ID] [--config mynou.json]
  series-pack-search ID --season N [--apply] [--candidate-id ID --scope-id ID]
                     [--config mynou.json]
  pack-remap JOB_ID --file-path PATH [--config mynou.json]
  series [ID] [--config mynou.json]
  series-numbering ID [--mapping FILE] [--apply --plan-id ID] [--config mynou.json]
  series-monitor | series-unmonitor | series-refresh ID [--config mynou.json]
  episode-monitor | episode-unmonitor ID --season N --episode N [--config mynou.json]
  calendar [--from YYYY-MM-DD --to YYYY-MM-DD --series-id ID]
           [--offset N --limit N] [--config mynou.json]
  library [--config mynou.json]
  upgrades [--apply] [--config mynou.json]
  monitor | unmonitor ID [--config mynou.json]
  baseline ID --release-title TITLE [--config mynou.json]
  torrents | torrent ID [--config mynou.json]
  pause | resume ID [--config mynou.json]
  torrent-priority ID --priority N [--config mynou.json]
  file-priority ID --file N --priority low|normal|high [--config mynou.json]
  torrent-select ID --selection '{\"indices\":[0,2]}' [--config mynou.json]
  torrent-policy ID --policy JSON_OR_NULL [--config mynou.json]
  requesters [--config mynou.json]
  requester ACCOUNT_ID [--offset N --limit N] [--config mynou.json]
  requester-control ACCOUNT_ID --mapping FILE [--apply --plan-id ID] [--config mynou.json]
  requester-sync [--config mynou.json]
  indexers [--config mynou.json]
  indexer-control SOURCE_ID --action enable|pause|reset_session|probe [--apply --plan-id ID] [--config mynou.json]
  notifications [--offset N --limit N] [--config mynou.json]
  notifications-dispatch [--config mynou.json]
  notification-control EVENT_ID --kind requester|irc --action retry|discard [--apply --plan-id ID] [--config mynou.json]
  irc [--config mynou.json]
  announcements [--offset N --limit N] [--config mynou.json]
  announcement ANNOUNCEMENT_ID [--config mynou.json]
  irc-preview SOURCE_ID --announcement FILE | --text FILE [--config mynou.json]
  irc-control ANNOUNCEMENT_ID --action acknowledge|dismiss|request [--apply --plan-id ID] [--config mynou.json]
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
                let value = if [
                    "json",
                    "help",
                    "apply",
                    "future-only",
                    "include-specials",
                    "unmonitored",
                ]
                .contains(&key)
                {
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
            | "retry" | "cancel" | "healthcheck" | "library" | "monitor" | "unmonitor" => {
                &["config", "help"]
            }
            "requesters" | "requester-sync" => &["config", "help"],
            "indexers" => &["config", "help"],
            "indexer-control" => &["config", "help", "action", "apply", "plan-id"],
            "notifications" => &["config", "help", "offset", "limit"],
            "notifications-dispatch" => &["config", "help"],
            "notification-control" => &["config", "help", "kind", "action", "apply", "plan-id"],
            "irc" | "announcement" => &["config", "help"],
            "announcements" => &["config", "help", "offset", "limit"],
            "irc-preview" => &["config", "help", "announcement", "text"],
            "irc-control" => &["config", "help", "action", "apply", "plan-id"],
            "requester" => &["config", "help", "offset", "limit"],
            "requester-control" => &["config", "help", "mapping", "apply", "plan-id"],
            "torrents" | "torrent" | "pause" | "resume" => &["config", "help"],
            "torrent-priority" => &["config", "help", "priority"],
            "file-priority" => &["config", "help", "file", "priority"],
            "torrent-policy" => &["config", "help", "policy"],
            "torrent-select" => &["config", "help", "selection"],
            "upgrades" => &["config", "help", "apply"],
            "baseline" => &["config", "help", "release-title"],
            "submit" => &[
                "config",
                "title",
                "kind",
                "year",
                "season",
                "episode",
                "path",
                "url",
                "tmdb-id",
                "help",
                "source-numbering",
            ],
            "search" => &[
                "config",
                "title",
                "kind",
                "year",
                "season",
                "episode",
                "tmdb-id",
                "help",
                "source-numbering",
            ],
            "track-series" => &[
                "config",
                "help",
                "title",
                "year",
                "season",
                "episode",
                "tmdb-id",
                "future-only",
                "include-specials",
                "unmonitored",
            ],
            "series-pack" => &["config", "help", "url", "mapping"],
            "series-shared-file" => &["config", "help", "url", "mapping", "apply", "plan-id"],
            "library-group" => &["config", "help", "mapping", "apply", "plan-id"],
            "series-numbering" => &["config", "help", "mapping", "apply", "plan-id"],
            "series-pack-search" => &[
                "config",
                "help",
                "season",
                "apply",
                "candidate-id",
                "scope-id",
            ],
            "pack-remap" => &["config", "help", "file-path"],
            "series" | "series-monitor" | "series-unmonitor" | "series-refresh" => {
                &["config", "help"]
            }
            "episode-monitor" | "episode-unmonitor" => &["config", "help", "season", "episode"],
            "calendar" => &[
                "config",
                "help",
                "from",
                "to",
                "series-id",
                "offset",
                "limit",
            ],
            "analyze" => &["json", "help"],
            "nzb-inspect" => &["help"],
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
            [
                "requester",
                "requester-control",
                "announcement",
                "irc-preview",
                "irc-control",
                "notification-control",
                "indexer-control",
                "analyze",
                "nzb-inspect",
                "show",
                "events",
                "retry",
                "cancel",
                "monitor",
                "unmonitor",
                "baseline",
                "torrent",
                "pause",
                "resume",
                "torrent-priority",
                "file-priority",
                "torrent-policy",
                "torrent-select",
                "series-monitor",
                "series-unmonitor",
                "series-refresh",
                "series-pack",
                "series-shared-file",
                "library-group",
                "series-pack-search",
                "series-numbering",
                "pack-remap",
                "episode-monitor",
                "episode-unmonitor",
            ]
            .contains(&command.as_str()),
        );
        if !options.contains_key("help")
            && (if command == "series" {
                positions.len() > 1
            } else {
                positions.len() != required
            })
        {
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
        source_numbering: args
            .options
            .get("source-numbering")
            .map(|text| mynou::numbering::SourceNumber::from_json(&json::parse(text)?))
            .transpose()?,
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
        "nzb-inspect" => {
            let nzb = mynou::usenet::nzb::Nzb::read_file(Path::new(&args.positions[0]))?;
            output(&nzb.report());
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
        v.insert("transfer_policy", config.download_policy.to_json());
        v.insert("max_peers", config.downloads.max_peers as u32);
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
    let online = args.command != "irc-preview" && running(&config);
    match args.command.as_str() {
        "torrents" => {
            if !online {
                return Err("Native transfer management requires the running Mynou service".into());
            }
            output(&call(&config, &path, "GET", "/api/transfers", None)?);
        }
        "torrent" | "pause" | "resume" | "torrent-priority" | "file-priority"
        | "torrent-policy" | "torrent-select" => {
            let id = &args.positions[0];
            if ![40, 64].contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Invalid native transfer ID".into());
            }
            let id = id.to_ascii_lowercase();
            let mut body = Value::object();
            let action = match args.command.as_str() {
                "torrent" => "",
                "pause" => "pause",
                "resume" => "resume",
                "torrent-priority" => {
                    let priority = args
                        .options
                        .get("priority")
                        .ok_or("--priority is required")?
                        .parse::<i32>()
                        .map_err(|_| "Priority must be an integer between -1000 and 1000")?;
                    if !(-1000..=1000).contains(&priority) {
                        return Err("Priority must be an integer between -1000 and 1000".into());
                    }
                    body.insert("priority", Value::Number(f64::from(priority)));
                    "priority"
                }
                "file-priority" => {
                    let index = args
                        .options
                        .get("file")
                        .ok_or("--file is required")?
                        .parse::<u32>()
                        .map_err(|_| "File index must be a nonnegative integer")?;
                    let priority = args
                        .options
                        .get("priority")
                        .ok_or("--priority is required")?;
                    if !["low", "normal", "high"].contains(&priority.as_str()) {
                        return Err("File priority must be low, normal or high".into());
                    }
                    body.insert("index", index);
                    body.insert("priority", priority.clone());
                    "files"
                }
                "torrent-policy" => {
                    let policy =
                        json::parse(args.options.get("policy").ok_or("--policy is required")?)?;
                    if policy != Value::Null {
                        mynou::torrent::TransferPolicy::from_json(&policy)?;
                    }
                    body.insert("policy", policy);
                    "policy"
                }
                "torrent-select" => {
                    let update = json::parse(
                        args.options
                            .get("selection")
                            .ok_or("--selection is required")?,
                    )?;
                    mynou::torrent::SelectionUpdate::from_json(&update)?;
                    body = update;
                    "selection"
                }
                _ => unreachable!(),
            };
            if !online {
                return Err("Native transfer management requires the running Mynou service".into());
            }
            let route = if action.is_empty() {
                format!("/api/transfers/{id}")
            } else {
                format!("/api/transfers/{id}/{action}")
            };
            output(&call(
                &config,
                &path,
                if action.is_empty() { "GET" } else { "POST" },
                &route,
                if action.is_empty() { None } else { Some(&body) },
            )?);
        }
        "library" => {
            if online {
                output(&call(&config, &path, "GET", "/api/library", None)?);
            } else if Store::directory_exists(&config.store_dir)? {
                let store = Store::open_read_only(&config.store_dir)?;
                output(&mynou::library::describe(
                    &store.library_jobs(),
                    &store.list(),
                ));
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
            } else if Store::directory_exists(&config.store_dir)? {
                output(&Engine::open_for_preview(config)?.check_upgrades(false)?);
            } else {
                output(&mynou::library::empty_preview());
            }
        }
        "pack-remap" => {
            if !online {
                return Err("Pack mapping correction requires a running Mynou service".into());
            }
            let id = args.positions[0].to_ascii_lowercase();
            if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Invalid pack episode request ID".into());
            }
            let file_path = args
                .options
                .get("file-path")
                .ok_or("Specify --file-path PATH")?;
            mynou::pack::validate_file_path(file_path)?;
            let mut body = Value::object();
            body.insert("file_path", file_path.clone());
            output(&call(
                &config,
                &path,
                "POST",
                &format!("/api/jobs/{id}/pack-mapping"),
                Some(&body),
            )?);
        }
        "requesters" => {
            if online {
                output(&call(&config, &path, "GET", "/api/requesters", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.requesters()?);
            }
        }
        "indexers" => {
            if online {
                output(&call(&config, &path, "GET", "/api/indexers", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.indexers()?);
            }
        }
        "indexer-control" => {
            let mut v = Value::object();
            v.insert("action", args.value("action", ""));
            v.insert("apply", args.options.contains_key("apply"));
            v.insert(
                "plan_id",
                args.options
                    .get("plan-id")
                    .cloned()
                    .map_or(Value::Null, Value::from),
            );
            let q = mynou::indexers::ControlRequest::from_json(&v)?;
            let id = &args.positions[0];
            if !mynou::requesters::valid_id(id) {
                return Err("Indexer policy: invalid source ID".into());
            }
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    &format!("/api/indexers/{id}/control"),
                    Some(&q.to_json()),
                )?);
            } else {
                if q.apply {
                    return Err("Indexer policy: application requires the running service".into());
                }
                output(&Engine::open_for_preview(config)?.indexer_control(id, &q)?);
            }
        }
        "notifications" => {
            let query = format!(
                "offset={}&limit={}",
                args.value("offset", "0"),
                args.value("limit", "100")
            );
            let (offset, limit) = mynou::requesters::page(&query)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "GET",
                    &format!("/api/notifications?{query}"),
                    None,
                )?);
            } else {
                output(&Engine::open_for_preview(config)?.notifications(offset, limit)?);
            }
        }
        "notifications-dispatch" => {
            if !online {
                return Err("Notifications: dispatch requires the running service".into());
            }
            output(&call(
                &config,
                &path,
                "POST",
                "/api/notifications/dispatch",
                Some(&Value::object()),
            )?);
        }
        "notification-control" => {
            let mut v = Value::object();
            v.insert("kind", args.value("kind", ""));
            v.insert("event_id", args.positions[0].clone());
            v.insert("action", args.value("action", ""));
            v.insert("apply", args.options.contains_key("apply"));
            v.insert(
                "plan_id",
                args.options
                    .get("plan-id")
                    .cloned()
                    .map_or(Value::Null, Value::from),
            );
            let q = mynou::notifications::ControlRequest::from_json(&v)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    "/api/notifications/control",
                    Some(&q.to_json()),
                )?);
            } else {
                if q.apply {
                    return Err("Notifications: application requires the running service".into());
                }
                output(&Engine::open_for_preview(config)?.notification_control(&q)?);
            }
        }
        "irc" => {
            if online {
                output(&call(&config, &path, "GET", "/api/irc", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.irc_sources()?);
            }
        }
        "announcements" => {
            let query = format!(
                "offset={}&limit={}",
                args.value("offset", "0"),
                args.value("limit", "100")
            );
            let (offset, limit) = mynou::irc::page(&query)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "GET",
                    &format!("/api/irc/announcements?{query}"),
                    None,
                )?);
            } else {
                output(&Engine::open_for_preview(config)?.irc_announcements(offset, limit)?);
            }
        }
        "announcement" => {
            let id = &args.positions[0];
            if !mynou::irc::valid_announcement_id(id) {
                return Err("IRC: invalid announcement ID".into());
            }
            if online {
                output(&call(
                    &config,
                    &path,
                    "GET",
                    &format!("/api/irc/announcements/{id}"),
                    None,
                )?);
            } else {
                output(&Engine::open_for_preview(config)?.irc_announcement(id)?);
            }
        }
        "irc-preview" => {
            let source = &args.positions[0];
            if !mynou::irc::valid_id(source) {
                return Err("IRC: invalid source ID".into());
            }
            let (file, text) = match (args.options.get("announcement"), args.options.get("text")) {
                (Some(file), None) => (file, false),
                (None, Some(file)) => (file, true),
                _ => return Err("Specify exactly one of --announcement FILE or --text FILE".into()),
            };
            let mut bytes = Vec::new();
            std::fs::File::open(file)
                .map_err(|_| "IRC: cannot open announcement file")?
                .take(8193)
                .read_to_end(&mut bytes)
                .map_err(|_| "IRC: cannot read announcement file")?;
            if bytes.len() > 8192 {
                return Err("IRC: announcement file exceeds 8 KiB".into());
            }
            let body =
                std::str::from_utf8(&bytes).map_err(|_| "IRC: announcement file is not UTF-8")?;
            // This operation stays pure even when the service is running.
            output(&if text {
                mynou::irc::preview_text(&config, source, body)?
            } else {
                mynou::irc::preview(&config, source, &json::parse(body)?)?
            });
        }
        "irc-control" => {
            let id = &args.positions[0];
            if !mynou::irc::valid_announcement_id(id) {
                return Err("IRC: invalid announcement ID".into());
            }
            let mut value = Value::object();
            value.insert(
                "action",
                args.options
                    .get("action")
                    .ok_or("Specify --action acknowledge, dismiss or request")?
                    .clone(),
            );
            value.insert("apply", args.options.contains_key("apply"));
            if let Some(plan) = args.options.get("plan-id") {
                value.insert("plan_id", plan.clone());
            }
            let query = mynou::irc::ControlRequest::from_json(&value)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    &format!("/api/irc/announcements/{id}/control"),
                    Some(&query.to_json()),
                )?);
            } else if query.apply {
                return Err("IRC: application requires a running Mynou service".into());
            } else {
                output(&Engine::open_for_preview(config)?.irc_control(id, &query)?);
            }
        }
        "requester" => {
            let id = &args.positions[0];
            if !mynou::requesters::valid_id(id) {
                return Err("Invalid requester account ID".into());
            }
            let query = format!(
                "offset={}&limit={}",
                args.value("offset", "0"),
                args.value("limit", "100")
            );
            let (offset, limit) = mynou::requesters::page(&query)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "GET",
                    &format!("/api/requesters/{id}?{query}"),
                    None,
                )?);
            } else {
                output(&Engine::open_for_preview(config)?.requester(id, offset, limit)?);
            }
        }
        "requester-control" => {
            let id = &args.positions[0];
            if !mynou::requesters::valid_id(id) {
                return Err("Invalid requester account ID".into());
            }
            let file = args
                .options
                .get("mapping")
                .ok_or("Specify --mapping FILE")?;
            let mut bytes = Vec::new();
            std::fs::File::open(file)
                .map_err(|_| "Cannot open requester mapping")?
                .take(65537)
                .read_to_end(&mut bytes)
                .map_err(|_| "Cannot read requester mapping")?;
            if bytes.len() > 65536 {
                return Err("Requester mapping exceeds 64 KiB".into());
            }
            let mut value = json::parse(
                std::str::from_utf8(&bytes).map_err(|_| "Requester mapping is not UTF-8")?,
            )?;
            if value.as_object().is_none_or(|m| {
                m.keys()
                    .any(|k| !["action", "policy", "demand_id"].contains(&k.as_str()))
            }) {
                return Err("Requester mapping contains an unknown field".into());
            }
            value.insert("apply", args.options.contains_key("apply"));
            if let Some(plan) = args.options.get("plan-id") {
                value.insert("plan_id", plan.clone());
            }
            let query = mynou::requesters::ControlRequest::from_json(&value)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    &format!("/api/requesters/{id}/control"),
                    Some(&query.to_json()),
                )?);
            } else if query.apply {
                return Err("Requester application requires a running Mynou service".into());
            } else {
                output(&Engine::open_for_preview(config)?.requester_control(id, &query)?);
            }
        }
        "requester-sync" => {
            if !online {
                return Err("Requester polling requires a running Mynou service".into());
            }
            output(&call(
                &config,
                &path,
                "POST",
                "/api/requesters/sync",
                Some(&Value::object()),
            )?);
        }
        "library-group" => {
            let id = args.positions[0].to_ascii_lowercase();
            if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Invalid shared library owner ID".into());
            }
            let mapping = args
                .options
                .get("mapping")
                .ok_or("Specify --mapping FILE")?;
            let mut bytes = Vec::new();
            std::fs::File::open(mapping)
                .map_err(|_| "Cannot open group mapping")?
                .take(512 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "Cannot read group mapping")?;
            if bytes.len() > 512 * 1024 {
                return Err("Group mapping exceeds 512 KiB".into());
            }
            let mut value = json::parse(
                std::str::from_utf8(&bytes).map_err(|_| "Group mapping is not UTF-8")?,
            )?;
            if value.as_object().is_none_or(|fields| {
                fields.keys().any(|key| {
                    !["action", "release_title", "source_url", "file_path"].contains(&key.as_str())
                })
            }) {
                return Err("Group mapping contains an unknown field".into());
            }
            value.insert("apply", args.options.contains_key("apply"));
            if let Some(plan) = args.options.get("plan-id") {
                value.insert("plan_id", plan.clone());
            }
            let query = mynou::library::GroupRequest::from_json(&value)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    &format!("/api/library/{id}/group"),
                    Some(&query.to_json()),
                )?);
            } else if query.apply {
                return Err("Group application requires a running Mynou service".into());
            } else {
                output(&Engine::open_for_preview(config)?.library_group(&id, &query)?);
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
                body.insert(
                    "release_title",
                    args.options
                        .get("release-title")
                        .ok_or("--release-title is required")?
                        .clone(),
                );
            } else {
                body.insert("enabled", args.command == "monitor");
            }
            if online {
                let route = format!(
                    "/api/library/{id}/{}",
                    if baseline { "baseline" } else { "monitor" }
                );
                output(&call(&config, &path, "POST", &route, Some(&body))?);
            } else {
                let mut store = Store::open(&config.store_dir)?;
                let job = if baseline {
                    let job = store.get(id).ok_or("Unknown library entry")?;
                    let title = args
                        .options
                        .get("release-title")
                        .ok_or("--release-title is required")?;
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
        "series-numbering" => {
            let id = args.positions[0].to_ascii_lowercase();
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Invalid series ID".into());
            }
            let mut body = if let Some(file) = args.options.get("mapping") {
                let mut bytes = Vec::new();
                fs::File::open(file)
                    .map_err(|error| format!("Numbering mapping file: {error}"))?
                    .take(512 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                if bytes.len() > 512 * 1024 {
                    return Err("Numbering mapping file exceeds 512 KiB".into());
                }
                let value = json::parse(
                    std::str::from_utf8(&bytes).map_err(|_| "Numbering mapping is not UTF-8")?,
                )?;
                if value
                    .as_object()
                    .is_none_or(|fields| fields.keys().any(|key| key != "changes"))
                {
                    return Err("Numbering mapping files contain only the changes array".into());
                }
                value
            } else {
                let mut value = Value::object();
                value.insert("changes", Value::Array(Vec::new()));
                value
            };
            body.insert("apply", args.options.contains_key("apply"));
            if let Some(plan) = args.options.get("plan-id") {
                body.insert("plan_id", plan.clone());
            }
            let query = mynou::series::NumberingRequest::from_json(&body)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    &format!("/api/series/{id}/numbering"),
                    Some(&body),
                )?);
            } else {
                if query.apply {
                    return Err("Numbering apply requires a running Mynou service".into());
                }
                output(&Engine::open_for_preview(config)?.series_numbering(&id, &query)?);
            }
        }
        "series-shared-file" => {
            let id = args.positions[0].to_ascii_lowercase();
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Invalid series ID".into());
            }
            let file = args
                .options
                .get("mapping")
                .ok_or("Specify --mapping FILE")?;
            let source = args
                .options
                .get("url")
                .ok_or("Specify --url MAGNET_OR_TORRENT")?;
            let mut bytes = Vec::new();
            fs::File::open(file)
                .map_err(|_| "Cannot open shared-file mapping")?
                .take(512 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "Cannot read shared-file mapping")?;
            if bytes.len() > 512 * 1024 {
                return Err("Shared-file mapping exceeds 512 KiB".into());
            }
            let mut body = json::parse(
                std::str::from_utf8(&bytes).map_err(|_| "Shared-file mapping is not UTF-8")?,
            )?;
            if body.as_object().is_none_or(|fields| {
                fields
                    .keys()
                    .any(|key| !["file_path", "season", "episodes"].contains(&key.as_str()))
            }) {
                return Err(
                    "Shared-file mapping contains only file_path, season and episodes".into(),
                );
            }
            body.insert("source_url", source.clone());
            body.insert("apply", args.options.contains_key("apply"));
            if let Some(id) = args.options.get("plan-id") {
                body.insert("plan_id", id.clone());
            }
            let query = mynou::pack::SharedFileRequest::from_json(&body)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    &format!("/api/series/{id}/shared-file"),
                    Some(&body),
                )?);
            } else {
                if query.apply {
                    return Err("Shared-file acquisition requires a running Mynou service".into());
                }
                output(&Engine::open_for_preview(config)?.shared_file(&id, &query)?);
            }
        }
        "series-pack-search" => {
            let id = args.positions[0].to_ascii_lowercase();
            if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Invalid series ID".into());
            }
            if !args.options.contains_key("season") {
                return Err("Specify --season N".into());
            }
            let mut body = Value::object();
            body.insert("season", args.number("season")?);
            body.insert("apply", args.options.contains_key("apply"));
            for (option, key) in [("scope-id", "scope_id"), ("candidate-id", "candidate_id")] {
                if let Some(value) = args.options.get(option) {
                    body.insert(key, value.clone());
                }
            }
            let query = mynou::pack::AutoPackRequest::from_json(&body)?;
            if online {
                output(&call(
                    &config,
                    &path,
                    "POST",
                    &format!("/api/series/{id}/pack-search"),
                    Some(&body),
                )?);
            } else {
                if query.apply {
                    return Err("Pack acquisition requires a running Mynou service".into());
                }
                output(&Engine::open_for_preview(config)?.search_packs(&id, &query)?);
            }
        }
        "series" | "track-series" | "series-pack" | "series-monitor" | "series-unmonitor"
        | "series-refresh" | "episode-monitor" | "episode-unmonitor" | "calendar" => {
            if !online {
                return Err("Series management requires a running Mynou service".into());
            }
            if args.command == "track-series" {
                let mut request = request(&args)?;
                request.kind = "series".into();
                request.validate()?;
                let mut body = Value::object();
                body.insert("request", request.to_json());
                body.insert(
                    "include_specials",
                    args.options.contains_key("include-specials"),
                );
                body.insert("future_only", args.options.contains_key("future-only"));
                body.insert("enabled", !args.options.contains_key("unmonitored"));
                output(&call(&config, &path, "POST", "/api/series", Some(&body))?);
            } else if args.command == "calendar" {
                let mut checked = mynou::series::CalendarQuery::new(
                    args.options.get("from").map(String::as_str),
                    args.options.get("to").map(String::as_str),
                )?;
                checked.series_id = args.options.get("series-id").cloned();
                for (name, target) in [
                    ("offset", &mut checked.offset),
                    ("limit", &mut checked.limit),
                ] {
                    if let Some(value) = args.options.get(name) {
                        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                            return Err("Invalid calendar pagination".into());
                        }
                        *target = value.parse().map_err(|_| "Invalid calendar pagination")?;
                    }
                }
                checked.validate()?;
                let fields: Vec<_> = ["from", "to", "series-id", "offset", "limit"]
                    .iter()
                    .filter_map(|name| {
                        args.options
                            .get(*name)
                            .map(|value| format!("{}={value}", name.replace('-', "_")))
                    })
                    .collect();
                let query = fields.join("&");
                mynou::series::CalendarQuery::parse(&query)?;
                output(&call(
                    &config,
                    &path,
                    "GET",
                    &format!("/api/calendar?{query}"),
                    None,
                )?);
            } else if args.command == "series" && args.positions.is_empty() {
                output(&call(&config, &path, "GET", "/api/series", None)?);
            } else {
                let id = args.positions[0].to_ascii_lowercase();
                if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err("Invalid series ID".into());
                }
                let mut body = Value::object();
                let (method, route) = match args.command.as_str() {
                    "series-pack" => {
                        let path = args
                            .options
                            .get("mapping")
                            .ok_or("Specify --mapping FILE")?;
                        let source = args
                            .options
                            .get("url")
                            .ok_or("Specify --url MAGNET_OR_TORRENT")?;
                        let mut bytes = Vec::new();
                        fs::File::open(path)
                            .map_err(|_| "Cannot open pack mapping file")?
                            .take(1_048_577)
                            .read_to_end(&mut bytes)
                            .map_err(|_| "Cannot read pack mapping file")?;
                        if bytes.len() > 1_048_576 {
                            return Err("Pack mapping file exceeds 1 MiB".into());
                        }
                        body.insert("source_url", source.clone());
                        body.insert(
                            "episodes",
                            json::parse(
                                std::str::from_utf8(&bytes)
                                    .map_err(|_| "Pack mapping is not valid UTF-8")?,
                            )?,
                        );
                        mynou::pack::PackSubmission::from_json(&body)?;
                        ("POST", format!("/api/series/{id}/packs"))
                    }
                    "series" => ("GET", format!("/api/series/{id}")),
                    "series-refresh" => ("POST", format!("/api/series/{id}/refresh")),
                    "series-monitor" | "series-unmonitor" => {
                        body.insert("enabled", args.command == "series-monitor");
                        ("POST", format!("/api/series/{id}/monitor"))
                    }
                    _ => {
                        if !args.options.contains_key("season")
                            || !args.options.contains_key("episode")
                        {
                            return Err("Specify both --season and --episode".into());
                        }
                        body.insert("season", args.number("season")?);
                        body.insert("episode", args.number("episode")?);
                        body.insert("enabled", args.command == "episode-monitor");
                        ("POST", format!("/api/series/{id}/episodes"))
                    }
                };
                output(&call(
                    &config,
                    &path,
                    method,
                    &route,
                    (method == "POST").then_some(&body),
                )?);
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
                if r.kind == "series" {
                    let engine = Engine::open_for_management(config.clone())?;
                    output(&Value::Array(
                        engine.submit(r)?.iter().map(public_job).collect(),
                    ));
                    return Ok(());
                }
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
