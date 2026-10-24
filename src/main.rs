#![forbid(unsafe_code)]
use mynou::{
    Result,
    config::{self, Config},
    engine::{Engine, public_job},
    integrations,
    json::{self, Value},
    media,
    net::HttpClient,
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

  vpn [--action status|import|enable|disable|revoke|reconnect|enable-forwarding|disable-forwarding]
      [--profile PRIVATE.conf | --settings PRIVATE.json] [--dir DIR]

  token --reset [--dir DIR]   (while Mynou is stopped: creates a new access token and prints it once)
  settings [--save] [--dir DIR]   (--save reads the settings document from standard input)
  secrets [--set NAME | --delete NAME] [--dir DIR]   (--set reads the value from standard input)
  analyze FILE [--json]
  doctor [--dir DIR]
  serve [--dir DIR]
  service --action install|status|start|stop|uninstall --dir /absolute/data-directory
          [--label org.mynou.automation]
  submit --title TITLE [--kind movie|episode|series|file] [--year YEAR]
         [--season N --episode N] [--path FILE | --url MAGNET_OR_TORRENT]
         [--tmdb-id N] [--source-numbering JSON] [--dir DIR]
  search --title TITLE [--kind movie|episode] [--year YEAR]
         [--season N --episode N] [--tmdb-id N] [--source-numbering JSON]
         [--limit N --after CURSOR] [--dir DIR]   (a page of 100 candidates by default)
  track-series --title TITLE [--year YEAR --tmdb-id N --season N]
               [--future-only] [--include-specials] [--unmonitored] [--dir DIR]
  series-pack ID --url MAGNET_OR_TORRENT --mapping FILE [--dir DIR]
  series-shared-file ID --url MAGNET_OR_TORRENT --mapping FILE
      [--apply --plan-id ID] [--dir DIR]
  library-group ID --mapping FILE [--apply --plan-id ID] [--dir DIR]
  series-pack-search ID --season N [--apply] [--candidate-id ID --scope-id ID]
                     [--dir DIR]
  pack-remap JOB_ID --file-path PATH [--dir DIR]
  series [ID | --limit N --after SERIES_ID] [--dir DIR]   (a page of 50 by default)
  series-numbering ID [--mapping FILE] [--apply --plan-id ID] [--dir DIR]
  series-monitor | series-unmonitor | series-refresh ID [--dir DIR]
  episode-monitor | episode-unmonitor ID --season N --episode N [--dir DIR]
  calendar [--from YYYY-MM-DD --to YYYY-MM-DD --series-id ID]
           [--offset N | --after CURSOR] [--limit N] [--dir DIR]
  library [--limit N --after CURSOR] [--query TEXT --state STATE] [--dir DIR]
      (a page of 50 by default; a filtered page examines at most 1,000 entries)
  upgrades [--apply] [--dir DIR]
  monitor | unmonitor ID [--dir DIR]
  baseline ID --release-title TITLE [--dir DIR]
  torrents [--limit N --after CURSOR] [--dir DIR]   (a page of 50 by default)
  torrent ID [--dir DIR]
  cross-seed SOURCE_ID --torrent FILE [--mapping '[0,2]'] [--dir DIR]
  cross-seed-discovery [--run [--dry-run] [--force] [--indexer ID] [--transfer ID]] [--dir DIR]
  pause | resume ID [--dir DIR]
  torrent-priority ID --priority N [--dir DIR]
  file-priority ID --file N --priority low|normal|high [--dir DIR]
  torrent-select ID --selection '{\"indices\":[0,2]}' [--dir DIR]
  torrent-policy ID --policy JSON_OR_NULL [--dir DIR]
  requesters [--dir DIR]
  requester ACCOUNT_ID [--offset N | --after CURSOR] [--limit N] [--dir DIR]
  requester-control ACCOUNT_ID --mapping FILE [--apply --plan-id ID] [--dir DIR]
  requester-sync [--dir DIR]
  title-profiles [--limit N --after CURSOR] [--dir DIR]   (a page of 100 by default)
  title-profile --kind movie|series --tmdb-id N (--profile NAME | --clear)
                [--apply --plan-id ID] [--dir DIR]
  indexers [--dir DIR]
  indexer-control SOURCE_ID --action enable|pause|reset_session|probe [--apply --plan-id ID] [--dir DIR]
  feeds [--retained --limit N --after CURSOR] [--dir DIR]
  irc [--dir DIR]
  announcements [--offset N | --after CURSOR] [--limit N] [--dir DIR]
  announcement ANNOUNCEMENT_ID [--dir DIR]
  irc-preview SOURCE_ID --announcement FILE | --text FILE [--dir DIR]
  irc-control ANNOUNCEMENT_ID --action acknowledge|dismiss|request [--apply --plan-id ID] [--dir DIR]
  jobs [--limit N --after CURSOR] [--query TEXT --state STATE] [--dir DIR]
      (a page of 50, newest first; a filtered page examines at most 1,000 jobs)
  status | sync [--dir DIR]
  show | events | retry | cancel ID [--dir DIR]
  healthcheck [--dir DIR]
  update [--dir DIR]   (installs the latest release now; Mynou also does it once a day)
  demo [--dir mynou-demo]
  version

DIR is the data directory: --dir, else MYNOU_HOME, else ~/Library/Application Support/Mynou.
Its storage keeps every setting and saved secret: run `mynou serve`, open the web interface
to create the access token, and configure Mynou there, through the API or `mynou settings`.
Commands that reach the running service read the access token from MYNOU_API_TOKEN.
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
                    "retained",
                    "clear",
                    "run",
                    "dry-run",
                    "force",
                    "save",
                    "reset",
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
            "service" => &["dir", "help", "action", "label"],
            "doctor" | "serve" | "service-run" | "status" | "sync" | "show" | "events"
            | "update" | "retry" | "cancel" | "healthcheck" | "monitor" | "unmonitor" => {
                &["dir", "help"]
            }
            "jobs" | "library" => &["dir", "help", "after", "limit", "query", "state"],
            "requesters" | "requester-sync" => &["dir", "help"],
            "vpn" => &["dir", "help", "action", "profile", "settings"],
            "indexers" => &["dir", "help"],
            "feeds" => &["dir", "help", "retained", "after", "limit"],
            "indexer-control" => &["dir", "help", "action", "apply", "plan-id"],
            "irc" | "announcement" => &["dir", "help"],
            "announcements" => &["dir", "help", "offset", "after", "limit"],
            "irc-preview" => &["dir", "help", "announcement", "text"],
            "irc-control" => &["dir", "help", "action", "apply", "plan-id"],
            "requester" => &["dir", "help", "offset", "after", "limit"],
            "requester-control" => &["dir", "help", "mapping", "apply", "plan-id"],
            "title-profiles" => &["dir", "help", "after", "limit"],
            "title-profile" => &[
                "dir", "help", "kind", "tmdb-id", "profile", "clear", "apply", "plan-id",
            ],
            "torrents" => &["dir", "help", "limit", "after"],
            "torrent" | "pause" | "resume" => &["dir", "help"],
            "torrent-priority" => &["dir", "help", "priority"],
            "file-priority" => &["dir", "help", "file", "priority"],
            "torrent-policy" => &["dir", "help", "policy"],
            "torrent-select" => &["dir", "help", "selection"],
            "cross-seed" => &["dir", "help", "torrent", "mapping"],
            "cross-seed-discovery" => &[
                "dir", "help", "run", "dry-run", "force", "indexer", "transfer",
            ],
            "upgrades" => &["dir", "help", "apply"],
            "baseline" => &["dir", "help", "release-title"],
            "submit" => &[
                "dir",
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
                "dir",
                "after",
                "limit",
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
                "dir",
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
            "series-pack" => &["dir", "help", "url", "mapping"],
            "series-shared-file" => &["dir", "help", "url", "mapping", "apply", "plan-id"],
            "library-group" => &["dir", "help", "mapping", "apply", "plan-id"],
            "series-numbering" => &["dir", "help", "mapping", "apply", "plan-id"],
            "series-pack-search" => &["dir", "help", "season", "apply", "candidate-id", "scope-id"],
            "pack-remap" => &["dir", "help", "file-path"],
            "series" => &["dir", "help", "after", "limit"],
            "series-monitor" | "series-unmonitor" | "series-refresh" => &["dir", "help"],
            "episode-monitor" | "episode-unmonitor" => &["dir", "help", "season", "episode"],
            "calendar" => &[
                "dir",
                "help",
                "from",
                "to",
                "series-id",
                "offset",
                "limit",
                "after",
            ],
            "settings" => &["dir", "help", "save"],
            "token" => &["dir", "help", "reset"],
            "secrets" => &["dir", "help", "set", "delete"],
            "analyze" => &["json", "help"],
            "demo" => &["dir", "help"],
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
                "indexer-control",
                "analyze",
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
                "cross-seed",
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
/// The access token of the running service, which the user provides in
/// `MYNOU_API_TOKEN`; Mynou keeps only its digest.
fn api_token() -> Result<String> {
    std::env::var(config::API_TOKEN_ENV)
        .ok()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            format!(
                "Set {} to the access token shown when Mynou was set up; `mynou token --reset` replaces a lost one",
                config::API_TOKEN_ENV
            )
        })
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
/// A stale address, another service or a refused token count as none.
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
    let token = api_token()?;
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
/// A settings document from standard input, for `mynou settings --save`.
fn read_document() -> Result<Value> {
    let mut data = String::new();
    std::io::stdin()
        .take(1_048_577)
        .read_to_string(&mut data)
        .map_err(|_| "Cannot read the settings from standard input")?;
    if data.len() > 1_048_576 {
        return Err("Settings are too large (maximum 1 MiB)".into());
    }
    json::parse(&data)
}
/// A secret value from standard input, without its final line break.
fn read_secret() -> Result<String> {
    let mut data = String::new();
    std::io::stdin()
        .take(mynou::config::MAX_SECRET_BYTES as u64 + 2)
        .read_to_string(&mut data)
        .map_err(|_| "Cannot read the secret from standard input")?;
    let value = data.strip_suffix('\n').unwrap_or(&data);
    Ok(value.strip_suffix('\r').unwrap_or(value).to_owned())
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
        "token" => {
            if !args.options.contains_key("reset") {
                return Err("Use `mynou token --reset` to replace the access token".into());
            }
            let path = args.data_dir()?;
            ensure_data_dir(&path)?;
            let config = config::load(&path).map_err(|e| {
                if e.contains("already open") {
                    "Stop Mynou before replacing its access token".to_owned()
                } else {
                    e
                }
            })?;
            let (token, digest) = mynou::access::generate()?;
            Store::open_for_management(&config)?.save_access(&digest)?;
            println!(
                "{token}\nThis is the new access token of {}. Keep it safe: Mynou shows it only now.",
                path.display()
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
    if args.command == "settings" {
        if args.options.contains_key("save") {
            let document = read_document()?;
            if online {
                let mut body = Value::object();
                body.insert("settings", document);
                output(&call(&config, "POST", "/api/settings", Some(&body))?);
            } else {
                config::from_json(&document, &path)?;
                Store::open_for_management(&config)?.save_settings(&document)?;
                let mut reply = Value::object();
                reply.insert("state", "saved");
                output(&reply);
            }
        } else if online {
            let reply = call(&config, "GET", "/api/settings", None)?;
            output(reply.get("settings").unwrap_or(&Value::Null));
        } else {
            output(&config.document);
        }
        return Ok(());
    }
    if args.command == "secrets" {
        let update = match (args.options.get("set"), args.options.get("delete")) {
            (Some(_), Some(_)) => return Err("Use either --set or --delete".into()),
            (Some(name), None) => {
                config::check_secret_name(name)?;
                let mut body = Value::object();
                body.insert("name", name.as_str());
                body.insert("value", read_secret()?);
                Some(body)
            }
            (None, Some(name)) => {
                let mut body = Value::object();
                body.insert("name", name.as_str());
                body.insert("value", Value::Null);
                Some(body)
            }
            (None, None) => None,
        };
        if online {
            output(&match &update {
                Some(body) => call(&config, "POST", "/api/secrets", Some(body))?,
                None => call(&config, "GET", "/api/secrets", None)?,
            });
        } else {
            let mut store = if update.is_some() {
                Store::open_for_management(&config)?
            } else if Store::directory_exists(&config.store_dir)? {
                Store::open_read_only(&config.store_dir)?
            } else {
                output(&mynou::settings::secrets_report(
                    &[&config.document],
                    &BTreeMap::new(),
                ));
                return Ok(());
            };
            let mut secrets = store.secrets()?;
            if let Some(body) = &update {
                mynou::settings::apply_secret(&mut secrets, body)?;
                store.save_secrets(&secrets)?;
            }
            output(&mynou::settings::secrets_report(
                &[&config.document],
                &secrets,
            ));
        }
        return Ok(());
    }
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
            if api_token().is_ok() {
                "present"
            } else {
                "absent"
            },
        );
        v.insert("data_dir", path.to_string_lossy().into_owned());
        v.insert("service", if online { "running" } else { "stopped" });
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
    if args.command == "serve" || supervised {
        if supervised {
            mynou::service::preflight(&path, &config)?;
        }
        ensure_data_dir(&path)?;
        let access = mynou::access::Access::new(mynou::access::stored(&config.store_dir)?);
        // The listener answers health and migration progress while storage opens.
        let startup = mynou::server::Startup::bind(&config, access)?;
        record_endpoint(&path, startup.address()?)?;
        if !supervised {
            eprintln!(
                "mynou {} — {} — API http://{}",
                env!("CARGO_PKG_VERSION"),
                path.display(),
                startup.address()?
            );
        }
        let engine = Engine::open_serving(config)?;
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
        return result;
    }
    let online = args.command != "irc-preview" && online;
    if args.command == "vpn" {
        let action = args.value("action", "status");
        if action != "import"
            && (args.options.contains_key("profile") || args.options.contains_key("settings"))
        {
            return Err("Profile files are only accepted by import".into());
        }
        let mut body = Value::object();
        body.insert("action", action);
        if action == "import" {
            let (file, structured) =
                match (args.options.get("profile"), args.options.get("settings")) {
                    (Some(file), None) => (file, false),
                    (None, Some(file)) => (file, true),
                    _ => {
                        return Err(
                            "Import requires exactly one --profile or --settings private file"
                                .into(),
                        );
                    }
                };
            body = mynou::vpn::import_request(Path::new(file), structured)?;
        }
        if online {
            output(&if action == "status" {
                call(&config, "GET", "/api/vpn", None)?
            } else {
                call(&config, "POST", "/api/vpn/control", Some(&body))?
            });
        } else {
            let engine = Engine::open_for_management(config)?;
            output(&if action == "status" {
                engine.vpn_status()
            } else {
                engine.vpn_control(&body)?
            });
        }
        return Ok(());
    }
    match args.command.as_str() {
        "cross-seed" => {
            if !online {
                return Err("Cross-seeding requires the running Mynou service".into());
            }
            let id = &args.positions[0];
            if ![40, 64].contains(&id.len()) || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Invalid native source transfer ID".into());
            }
            let file = args.options.get("torrent").ok_or("--torrent is required")?;
            let mut metadata = Vec::new();
            fs::File::open(file)
                .and_then(|file| {
                    file.take((mynou::torrent::MAX_CROSS_SEED_METADATA + 1) as u64)
                        .read_to_end(&mut metadata)
                })
                .map_err(|_| "Could not read cross-seed torrent metadata")?;
            if metadata.is_empty() || metadata.len() > mynou::torrent::MAX_CROSS_SEED_METADATA {
                return Err("Cross-seed metadata must contain 1 to 262144 bytes".into());
            }
            let mut body = Value::object();
            body.insert(
                "metadata_hex",
                metadata
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>(),
            );
            if let Some(mapping) = args.options.get("mapping") {
                body.insert("mapping", json::parse(mapping)?);
            }
            mynou::torrent::CrossSeedRequest::from_json(&body)?;
            output(&call(
                &config,
                "POST",
                &format!("/api/transfers/{}/cross-seed", id.to_ascii_lowercase()),
                Some(&body),
            )?);
        }
        "cross-seed-discovery" => {
            let mut body = Value::object();
            for (option, field) in [("dry-run", "dry_run"), ("force", "force")] {
                if args.options.contains_key(option) {
                    body.insert(field, true);
                }
            }
            for name in ["indexer", "transfer"] {
                if let Some(value) = args.options.get(name) {
                    body.insert(name, value.as_str());
                }
            }
            let run = args.options.contains_key("run");
            if !run && body.as_object().is_some_and(|fields| !fields.is_empty()) {
                return Err("Discovery options require --run".into());
            }
            if run {
                if !online {
                    return Err("Cross-seed discovery requires the running Mynou service".into());
                }
                mynou::indexers::cross_seed::Run::from_json(&body)?;
                output(&call(
                    &config,
                    "POST",
                    "/api/cross-seed/discovery/run",
                    Some(&body),
                )?);
            } else if online {
                output(&call(&config, "GET", "/api/cross-seed/discovery", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.cross_seed_status()?);
            }
        }
        "torrents" => {
            if !online {
                return Err("Native transfer management requires the running Mynou service".into());
            }
            let query = ["limit", "after"]
                .iter()
                .filter_map(|name| {
                    args.options
                        .get(*name)
                        .map(|value| format!("{name}={value}"))
                })
                .collect::<Vec<_>>()
                .join("&");
            mynou::engine::TransferPageQuery::parse(&query)?;
            let route = if query.is_empty() {
                "/api/transfers".to_owned()
            } else {
                format!("/api/transfers?{query}")
            };
            output(&call(&config, "GET", &route, None)?);
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
                if action.is_empty() { "GET" } else { "POST" },
                &route,
                if action.is_empty() { None } else { Some(&body) },
            )?);
        }
        "library" => {
            let query = page_query(&args)?;
            let page = mynou::store::JobQuery::parse(&query)?;
            if online {
                output(&call(
                    &config,
                    "GET",
                    &format!("/api/library?{query}"),
                    None,
                )?);
            } else if Store::directory_exists(&config.store_dir)? {
                let mut store = Store::open_read_only(&config.store_dir)?;
                output(&mynou::library::describe_query(&mut store, &page)?);
            } else {
                let mut empty = Value::object();
                empty.insert("library", Value::Array(Vec::new()));
                empty.insert("next", Value::Null);
                empty.insert("total", 0_u32);
                empty.insert("sequence", "0");
                output(&empty);
            }
        }
        "upgrades" => {
            let apply = args.options.contains_key("apply");
            if online {
                let mut body = Value::object();
                body.insert("apply", apply);
                output(&call(&config, "POST", "/api/upgrades", Some(&body))?);
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
                "POST",
                &format!("/api/jobs/{id}/pack-mapping"),
                Some(&body),
            )?);
        }
        "requesters" => {
            if online {
                output(&call(&config, "GET", "/api/requesters", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.requesters()?);
            }
        }
        "indexers" => {
            if online {
                output(&call(&config, "GET", "/api/indexers", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.indexers()?);
            }
        }
        "feeds" => {
            if args.options.contains_key("retained") {
                let fields: Vec<_> = ["limit", "after"]
                    .iter()
                    .filter_map(|name| {
                        args.options
                            .get(*name)
                            .map(|value| format!("{name}={value}"))
                    })
                    .collect();
                let query = fields.join("&");
                if online {
                    output(&call(
                        &config,
                        "GET",
                        &format!("/api/feeds/retained?{query}"),
                        None,
                    )?);
                } else {
                    output(&Engine::open_for_preview(config)?.feeds_retained(&query)?);
                }
            } else if args.options.contains_key("limit") || args.options.contains_key("after") {
                return Err("Feed pages require --retained".into());
            } else if online {
                output(&call(&config, "GET", "/api/feeds", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.feeds()?);
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
        "irc" => {
            if online {
                output(&call(&config, "GET", "/api/irc", None)?);
            } else {
                output(&Engine::open_for_preview(config)?.irc_sources()?);
            }
        }
        "announcements" => {
            if args.options.contains_key("after") && args.options.contains_key("offset") {
                return Err("Use either --offset or --after".into());
            }
            let query = if let Some(after) = args.options.get("after") {
                format!("after={after}&limit={}", args.value("limit", "100"))
            } else if args.options.contains_key("offset") {
                format!(
                    "offset={}&limit={}",
                    args.value("offset", "0"),
                    args.value("limit", "100")
                )
            } else {
                format!("limit={}", args.value("limit", "100"))
            };
            if args.options.contains_key("offset") {
                mynou::irc::page(&query)?;
            } else {
                mynou::irc::cursor_page(&query)?;
            }
            if online {
                output(&call(
                    &config,
                    "GET",
                    &format!("/api/irc/announcements?{query}"),
                    None,
                )?);
            } else if args.options.contains_key("offset") {
                let (offset, limit) = mynou::irc::page(&query)?;
                output(&Engine::open_for_preview(config)?.irc_announcements(offset, limit)?);
            } else {
                let (after, limit) = mynou::irc::cursor_page(&query)?;
                output(
                    &Engine::open_for_preview(config)?
                        .irc_announcements_page(after.as_deref(), limit)?,
                );
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
            if args.options.contains_key("after") && args.options.contains_key("offset") {
                return Err("Use either --offset or --after".into());
            }
            let query = if let Some(after) = args.options.get("after") {
                format!("after={after}&limit={}", args.value("limit", "100"))
            } else {
                format!(
                    "offset={}&limit={}",
                    args.value("offset", "0"),
                    args.value("limit", "100")
                )
            };
            if online {
                output(&call(
                    &config,
                    "GET",
                    &format!("/api/requesters/{id}?{query}"),
                    None,
                )?);
            } else if args.options.contains_key("after") {
                let (after, limit) = mynou::requesters::cursor_page(&query)?;
                output(&Engine::open_for_preview(config)?.requester_page(
                    id,
                    after.as_deref(),
                    limit,
                )?);
            } else {
                let (offset, limit) = mynou::requesters::page(&query)?;
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
        "title-profiles" => {
            let limit = args.value("limit", "100");
            let mut query = format!("limit={limit}");
            if let Some(after) = args.options.get("after") {
                query.push_str(&format!("&after={after}"));
            }
            if online {
                output(&call(
                    &config,
                    "GET",
                    &format!("/api/title-profiles?{query}"),
                    None,
                )?);
            } else {
                let (after, limit) = mynou::profiles::page(&query)?;
                let mut store = mynou::profiles::TitleProfiles::open(&config.store_dir, true)?;
                output(&mynou::profiles::list(
                    &config,
                    &mut store,
                    after.as_ref(),
                    limit,
                )?);
            }
        }
        "title-profile" => {
            let mut value = Value::object();
            let clear = args.options.contains_key("clear");
            value.insert("action", if clear { "clear" } else { "set" });
            value.insert("kind", args.value("kind", ""));
            value.insert(
                "tmdb_id",
                Value::Number(
                    args.options
                        .get("tmdb-id")
                        .ok_or("--tmdb-id is required")?
                        .parse::<u64>()
                        .map_err(|_| "Invalid --tmdb-id")? as f64,
                ),
            );
            if let Some(profile) = args.options.get("profile") {
                value.insert("profile", profile.clone());
            }
            value.insert("apply", args.options.contains_key("apply"));
            if let Some(plan) = args.options.get("plan-id") {
                value.insert("plan_id", plan.clone());
            }
            let query = mynou::profiles::ControlRequest::from_json(&value)?;
            if online {
                output(&call(
                    &config,
                    "POST",
                    "/api/title-profiles/control",
                    Some(&query.to_json()),
                )?);
            } else if query.apply {
                return Err("Title profile changes require a running Mynou service".into());
            } else {
                let mut store = mynou::profiles::TitleProfiles::open(&config.store_dir, true)?;
                output(&mynou::profiles::control(&config, &mut store, &query)?);
            }
        }
        "requester-sync" => {
            if !online {
                return Err("Requester polling requires a running Mynou service".into());
            }
            output(&call(
                &config,
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
                output(&call(&config, "POST", &route, Some(&body))?);
            } else {
                let mut store = Store::open_for_management(&config)?;
                let job = if baseline {
                    let job = store.get(id)?.ok_or("Unknown library entry")?;
                    let title = args
                        .options
                        .get("release-title")
                        .ok_or("--release-title is required")?;
                    let mut config = config.clone();
                    if let Some(name) = mynou::profiles::stored_binding(&config, &job.request)? {
                        mynou::profiles::bind(&mut config, &job.request.kind, &name);
                    }
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
            let query: Vec<_> = ["after", "limit"]
                .iter()
                .filter_map(|name| {
                    args.options
                        .get(*name)
                        .map(|value| format!("{name}={value}"))
                })
                .collect();
            let query = query.join("&");
            let page = integrations::SearchPage::parse(&query)?;
            if online {
                output(&call(
                    &config,
                    "POST",
                    &if query.is_empty() {
                        "/api/search".to_owned()
                    } else {
                        format!("/api/search?{query}")
                    },
                    Some(&r.to_json()),
                )?);
            } else {
                // Like the API preview, this opens no journal: only bindings, read-only.
                let binding = mynou::profiles::stored_binding(&config, &r)?;
                output(&mynou::profiles::preview(&config, &r, binding, &page)?);
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
                output(&call(&config, "POST", "/api/series", Some(&body))?);
            } else if args.command == "calendar" {
                let mut checked = mynou::series::CalendarQuery::new(
                    args.options.get("from").map(String::as_str),
                    args.options.get("to").map(String::as_str),
                )?;
                checked.series_id = args.options.get("series-id").cloned();
                checked.after = args.options.get("after").cloned();
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
                let fields: Vec<_> = ["from", "to", "series-id", "offset", "limit", "after"]
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
                    "GET",
                    &format!("/api/calendar?{query}"),
                    None,
                )?);
            } else if args.command == "series" && args.positions.is_empty() {
                let fields: Vec<_> = ["limit", "after"]
                    .iter()
                    .filter_map(|name| {
                        args.options
                            .get(*name)
                            .map(|value| format!("{name}={value}"))
                    })
                    .collect();
                let query = fields.join("&");
                mynou::series::SeriesPageQuery::parse(&query)?;
                output(&call(
                    &config,
                    "GET",
                    &format!("/api/series?{query}"),
                    None,
                )?);
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
                            .take(1_000_001)
                            .read_to_end(&mut bytes)
                            .map_err(|_| "Cannot read pack mapping file")?;
                        if bytes.len() > 1_000_000 {
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
                    method,
                    &route,
                    (method == "POST").then_some(&body),
                )?);
            }
        }
        "submit" => {
            let r = request(&args)?;
            if online {
                output(&call(&config, "POST", "/api/jobs", Some(&r.to_json()))?);
            } else {
                if r.kind == "series" {
                    let engine = Engine::open_for_management(config.clone())?;
                    output(&Value::Array(
                        engine.submit(r)?.iter().map(public_job).collect(),
                    ));
                    return Ok(());
                }
                let requests = integrations::expand(&config, &r)?;
                let mut store = Store::open_for_management(&config)?;
                let jobs: Vec<_> = requests
                    .into_iter()
                    .map(|r| store.submit(r).map(|j| public_job(&j)))
                    .collect::<Result<_>>()?;
                output(&Value::Array(jobs));
            }
        }
        "jobs" => {
            let query = page_query(&args)?;
            let page = mynou::store::JobQuery::parse(&query)?;
            if online {
                output(&call(&config, "GET", &format!("/api/jobs?{query}"), None)?);
            } else if Store::directory_exists(&config.store_dir)? {
                let mut store = Store::open_read_only(&config.store_dir)?;
                output(&mynou::engine::jobs_query_value(&mut store, &page)?);
            } else {
                output(&mynou::engine::jobs_value(mynou::store::JobPage {
                    sequence: 0,
                    jobs: Vec::new(),
                    next: None,
                    total: 0,
                }));
            }
        }
        "status" => {
            if online {
                output(&call(&config, "GET", "/api/status", None)?);
            } else {
                let jobs = if Store::directory_exists(&config.store_dir)? {
                    Store::open_read_only(&config.store_dir)?.counts()?.jobs
                } else {
                    0
                };
                let mut v = Value::object();
                v.insert("service", "stopped");
                v.insert("jobs", Value::Number(jobs as f64));
                output(&v);
            }
        }
        "sync" => {
            if online {
                output(&call(&config, "POST", "/api/sync", None)?);
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
                output(&call(&config, method, &route, None)?);
            } else {
                let mut store = if matches!(args.command.as_str(), "show" | "events") {
                    Store::open_read_only(&config.store_dir)?
                } else {
                    Store::open_for_management(&config)?
                };
                let v = match args.command.as_str() {
                    "show" => public_job(&store.get(id)?.ok_or("Unknown job")?),
                    "events" => {
                        Value::Array(store.events(id)?.iter().map(|e| e.to_json()).collect())
                    }
                    "retry" => {
                        let mut job = store.retry(id)?;
                        if job.imports.is_empty()
                            && job.request.source_path.is_none()
                            && job.request.source_url.is_none()
                        {
                            if let Some(old_id) = &job.download_id {
                                let shared = store.has_co_owner(&job)?;
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
/// The `after` and `limit` options as a validated page query string.
fn page_query(args: &Args) -> Result<String> {
    let mut fields: Vec<_> = ["limit", "after", "state"]
        .iter()
        .filter_map(|name| {
            args.options
                .get(*name)
                .map(|value| format!("{name}={value}"))
        })
        .collect();
    if let Some(text) = args.options.get("query") {
        let mut encoded = String::from("q=");
        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                encoded.push(char::from(byte));
            } else {
                encoded.push_str(&format!("%{byte:02X}"));
            }
        }
        fields.push(encoded);
    }
    let query = fields.join("&");
    mynou::store::JobQuery::parse(&query)?;
    Ok(query)
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
