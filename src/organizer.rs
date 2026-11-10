//! Atomic import preserves the download and refuses to overwrite existing files.
//!
//! An import goes where Plex already keeps the title when that folder is
//! unambiguous and inside the library folder, otherwise into Mynou's own
//! layout (see [`placement`]). Subtitles of the release follow their video
//! under Plex's sidecar names, and a file whose duration cannot be the
//! requested title's is refused before anything is written
//! ([`check_runtime`]).

use crate::Result;
use crate::crypto::Sha256;
use crate::store::{Request, private_options, reject_symlinks, sync_directory, try_lock_file};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// A title as a file or folder name Plex reads cleanly: a colon becomes a
/// dash (`Mission: Impossible` → `Mission - Impossible`), and the other
/// characters a file name cannot hold become `_`.
fn filename_part(value: &str) -> Result<String> {
    sanitized(value, true)
}

/// The names Mynou gave before colons became dashes: every forbidden
/// character became `_`. Folders it created that way are kept, and shared
/// multi-episode files keep the names their records were made with.
fn legacy_filename_part(value: &str) -> Result<String> {
    sanitized(value, false)
}

fn sanitized(value: &str, dashes: bool) -> Result<String> {
    let mut result = String::with_capacity(value.len().min(180));
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        let replacement = if dashes && character == ':' {
            if characters.peek().is_none() {
                // A trailing colon has nothing to separate.
                continue;
            }
            if characters.peek().is_some_and(|next| next.is_whitespace()) {
                // `Title: Subtitle` and `Title : Subtitle` both read `Title - Subtitle`.
                result.truncate(result.trim_end().len());
                " -"
            } else {
                "-"
            }
        } else if character.is_control() || "/\\:*?\"<>|".contains(character) {
            "_"
        } else {
            ""
        };
        let added = if replacement.is_empty() {
            character.len_utf8()
        } else {
            replacement.len()
        };
        if result.len() + added > 180 {
            break;
        }
        if replacement.is_empty() {
            result.push(character);
        } else {
            result.push_str(replacement);
        }
    }
    let result = result.trim().trim_matches('.').trim().to_owned();
    if result.is_empty() || result == "." || result == ".." {
        return Err("title cannot produce a filename".to_owned());
    }
    Ok(result)
}

fn validate_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("empty path or directory traversal not allowed".to_owned());
    }
    reject_symlinks(path)
}

/// Runtime availability proof only: journal replay must not depend on disk state.
/// Metadata checks cannot prevent another process changing a path afterward.
pub(crate) fn check_retained_imports(
    job: &crate::store::Job,
    config: &crate::config::Config,
) -> Result<()> {
    for file in &job.imports {
        let path = Path::new(file);
        let root = import_root(job, config);
        let under_root = path != root && path.starts_with(root);
        let confined = if let Some(shared) = &job.shared_file {
            // A shared import keeps its recorded destination across config edits.
            file == &shared.import_path && (job.requester.is_none() || under_root)
        } else {
            under_root
        };
        if !path.is_absolute()
            || !confined
            || validate_path(path).is_err()
            || !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        {
            return Err("Recorded import is missing, unsafe or outside its destination; restore the original regular file and retry".into());
        }
    }
    Ok(())
}

/// The import root a job's files must stay under.
fn import_root<'a>(job: &'a crate::store::Job, config: &'a crate::config::Config) -> &'a Path {
    match (&job.requester, job.request.kind == "movie") {
        (Some(requester), true) => Path::new(&requester.capture.movies_root),
        (Some(requester), false) => Path::new(&requester.capture.series_root),
        (None, true) => Path::new(&config.movies_root),
        (None, false) => Path::new(&config.series_root),
    }
}

/// The files of an earlier version that are still on disk, with their size:
/// regular files under the job's import root, reached without symbolic links,
/// that are neither named by nor the same file as an import of `current`.
pub(crate) fn earlier_files(
    job: &crate::store::Job,
    current: &crate::store::Job,
    config: &crate::config::Config,
) -> Vec<(String, u64)> {
    let root = import_root(job, config);
    let kept: Vec<(u64, u64)> = current
        .imports
        .iter()
        .filter_map(|file| fs::metadata(file).ok())
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .collect();
    job.imports
        .iter()
        .filter(|file| !current.imports.contains(file))
        .filter_map(|file| {
            let path = Path::new(file);
            if !path.is_absolute()
                || path == root
                || !path.starts_with(root)
                || validate_path(path).is_err()
            {
                return None;
            }
            let metadata = fs::symlink_metadata(path).ok()?;
            (metadata.is_file() && !kept.contains(&(metadata.dev(), metadata.ino())))
                .then(|| (file.clone(), metadata.len()))
        })
        .collect()
}

/// Deletes the files `earlier_files` lists, only while every file of
/// `current` is on disk, and returns the bytes they held.
pub(crate) fn delete_earlier_files(
    job: &crate::store::Job,
    current: &crate::store::Job,
    config: &crate::config::Config,
) -> Result<u64> {
    // A listing may be stale: never delete what may now be the only copy.
    if !crate::library::import_exists(current) {
        return Err("the version that replaced this one is missing from disk".to_owned());
    }
    let mut freed = 0_u64;
    for (file, length) in earlier_files(job, current, config) {
        fs::remove_file(&file).map_err(|error| format!("cannot delete {file}: {error}"))?;
        freed += length;
        if let Some(directory) = Path::new(&file).parent() {
            sync_directory(directory)?;
        }
    }
    Ok(freed)
}

fn create_directory(path: &Path) -> Result<()> {
    validate_path(path)?;
    if path.exists() {
        if !fs::metadata(path)
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            return Err("library directory is a file".to_owned());
        }
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_directory(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_path(path)?;
            if !fs::metadata(path)
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                return Err("library directory is a file".to_owned());
            }
        }
        Err(error) => {
            return Err(format!("cannot create library directory: {error}"));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    sync_directory(path)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        sync_directory(parent)?;
    }
    Ok(())
}

fn media_extension(source: &Path) -> Result<&str> {
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| "media extension missing or not UTF-8".to_owned())?;
    if extension.is_empty()
        || extension.len() > 16
        || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err("invalid media extension".to_owned());
    }
    Ok(extension)
}

/// The destination of `source` in Mynou's own layout, without asking Plex.
#[cfg(test)]
fn target(source: &Path, library: &Path, request: &Request) -> Result<PathBuf> {
    let placement = placement(library, request, &[], &mut || Ok(None))?;
    placement.destination(library, source)
}

/// Where an import goes: its folder and its file name without the
/// extension, with how the folder was chosen when it is not Mynou's own
/// layout. Every note starts with a code (`plex_folder`, `folder_kept`,
/// `folder_fallback`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Placement {
    pub(crate) directory: PathBuf,
    pub(crate) stem: String,
    pub(crate) notes: Vec<String>,
}

impl Placement {
    /// The file `source` becomes: its extension on this placement's name,
    /// checked once more to stay below `library`.
    pub(crate) fn destination(&self, library: &Path, source: &Path) -> Result<PathBuf> {
        let extension = media_extension(source)?;
        if self.stem.is_empty()
            || self.stem.contains(['/', '\0'])
            || matches!(self.stem.as_str(), "." | "..")
        {
            return Err("invalid library file name".to_owned());
        }
        let destination = self.directory.join(format!("{}.{extension}", self.stem));
        if !destination.starts_with(library)
            || self.directory == library
            || destination
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err("library destination outside the library folder".to_owned());
        }
        Ok(destination)
    }
}

/// A file Plex lists for the requested film or series, at the path Mynou
/// sees it (through `plex.path_mappings`).
#[derive(Clone, Debug)]
pub(crate) struct ListedFile {
    /// The Plex film or series that lists the file: two of them for one
    /// title are two folders.
    pub(crate) item: usize,
    /// An episode's season.
    pub(crate) season: Option<u32>,
    pub(crate) path: String,
}

fn is_episode(request: &Request) -> bool {
    request.kind == "episode" || (request.kind == "series" && request.episode > 0)
}

/// Chooses the folder and name of an import:
///
/// 1. An upgrade takes the folder and name of the file it replaces
///    (`replaced`, the earlier version's files), so it replaces it in place.
/// 2. Otherwise, the title's folder is the one Plex already lists it in
///    (`plex` is asked only then; `None` when Plex is off). It must be one
///    folder, below `library`, a real folder reached without symbolic links.
///    Anything else falls back to Mynou's layout, with the reason noted.
/// 3. Mynou's layout is `Title (Year)` for a film and `Title` for a series,
///    named by [`filename_part`]; a folder Mynou created under the earlier
///    names ([`legacy_filename_part`]) is kept when the new one is absent.
/// 4. An episode goes into the folder that holds its season: the one Plex
///    lists the season's episodes in, else the only folder of the series
///    named for the season (`Season 2`, `Season 02`, `Specials`), else
///    `Season NN`.
pub(crate) fn placement(
    library: &Path,
    request: &Request,
    replaced: &[String],
    plex: &mut dyn FnMut() -> Result<Option<Vec<ListedFile>>>,
) -> Result<Placement> {
    request.validate()?;
    let episode = is_episode(request);
    let name = |title: String| {
        if !episode && request.year > 0 {
            format!("{title} ({})", request.year)
        } else {
            title
        }
    };
    let title = filename_part(&request.title)?;
    let stem = if episode {
        format!("{title} - S{:02}E{:02}", request.season, request.episode)
    } else {
        name(title.clone())
    };
    let mut notes = Vec::new();
    if let Some(earlier) = replaced.first() {
        match replaced_location(library, Path::new(earlier)) {
            Ok((directory, earlier_stem)) => {
                return Ok(Placement {
                    directory,
                    stem: earlier_stem.unwrap_or(stem),
                    notes,
                });
            }
            Err(reason) => notes.push(format!("folder_fallback: {reason}")),
        }
    }
    let mut plex_seasons = BTreeSet::new();
    let mut folder = None;
    if let Some(files) = plex()?.filter(|files| !files.is_empty()) {
        match listed_folder(library, request, &files) {
            Ok((found, seasons)) => {
                notes.push(format!(
                    "plex_folder: Plex already keeps this title in {}",
                    found.display()
                ));
                folder = Some(found);
                plex_seasons = seasons;
            }
            Err(reason) => notes.push(format!(
                "folder_fallback: {reason}; Mynou used its own layout"
            )),
        }
    }
    let folder = match folder {
        Some(folder) => folder,
        None => {
            let default = library.join(name(title));
            let legacy = library.join(name(legacy_filename_part(&request.title)?));
            if legacy != default
                && fs::symlink_metadata(&default).is_err()
                && confined(library, &legacy).is_ok()
            {
                notes.push(format!(
                    "folder_kept: Mynou already imported this title into {}",
                    legacy.display()
                ));
                legacy
            } else {
                default
            }
        }
    };
    if !episode {
        return Ok(Placement {
            directory: folder,
            stem,
            notes,
        });
    }
    let default = folder.join(format!("Season {:02}", request.season));
    let directory = if plex_seasons.len() > 1 {
        notes.push(format!(
            "folder_fallback: Plex keeps season {} in {} folders ({}); Mynou used {}",
            request.season,
            plex_seasons.len(),
            listing(&plex_seasons),
            default.display()
        ));
        default
    } else if let Some(season) = plex_seasons.pop_first() {
        match confined(&folder, &season) {
            Ok(()) => season,
            Err(reason) => {
                notes.push(format!(
                    "folder_fallback: the season folder Plex lists, {}, {reason}; Mynou used {}",
                    season.display(),
                    default.display()
                ));
                default
            }
        }
    } else {
        let mut found = season_folders(&folder, request.season);
        if found.len() > 1 {
            notes.push(format!(
                "folder_fallback: {} folders hold season {} ({}); Mynou used {}",
                found.len(),
                request.season,
                listing(&found.iter().cloned().collect()),
                default.display()
            ));
            default
        } else {
            found.pop().unwrap_or(default)
        }
    };
    Ok(Placement {
        directory,
        stem,
        notes,
    })
}

/// Up to three paths of a set, for a note.
fn listing(paths: &BTreeSet<PathBuf>) -> String {
    let mut shown: Vec<String> = paths
        .iter()
        .take(3)
        .map(|path| path.display().to_string())
        .collect();
    if paths.len() > 3 {
        shown.push(format!("{} more", paths.len() - 3));
    }
    shown.join(", ")
}

/// Why `folder` cannot receive an import below `root`: it must be strictly
/// below it, by plain components, and be a real folder reached without a
/// symbolic link.
fn confined(root: &Path, folder: &Path) -> std::result::Result<(), String> {
    if folder == root {
        return Err(format!(
            "is {} itself, not a folder inside it",
            root.display()
        ));
    }
    if !folder.is_absolute()
        || !folder.starts_with(root)
        || folder
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(format!("is outside {}", root.display()));
    }
    if reject_symlinks(folder).is_err() {
        return Err("goes through a symbolic link".to_owned());
    }
    match fs::symlink_metadata(folder) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err("is not a folder".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("is missing from the disk".to_owned())
        }
        Err(error) => Err(format!("cannot be read: {error}")),
    }
}

/// The folder and name of the file an upgrade replaces, when that file is a
/// regular file below `library`. A name from before upgrades replaced files
/// (`... [mynou-<id>]`) gives way to the usual name.
fn replaced_location(
    library: &Path,
    file: &Path,
) -> std::result::Result<(PathBuf, Option<String>), String> {
    let directory = file.parent().ok_or("the file it replaces has no folder")?;
    if let Err(reason) = confined(library, directory) {
        return Err(format!("the folder of the file it replaces {reason}"));
    }
    if !fs::symlink_metadata(file).is_ok_and(|metadata| metadata.is_file()) {
        return Err("the file it replaces is missing".to_owned());
    }
    let stem = file
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty() && !(stem.contains(" [mynou-") && stem.ends_with(']')))
        .map(str::to_owned);
    Ok((directory.to_path_buf(), stem))
}

/// The title folder among the files Plex lists, and for an episode the
/// folders below it that hold the requested season. A film's folder is the
/// one holding its files; a series' is the folder its episodes share, above
/// a season folder. More than one folder is ambiguous.
fn listed_folder(
    library: &Path,
    request: &Request,
    files: &[ListedFile],
) -> std::result::Result<(PathBuf, BTreeSet<PathBuf>), String> {
    let parent = |file: &ListedFile| {
        let path = Path::new(&file.path);
        path.parent()
            .filter(|_| path.is_absolute())
            .map(Path::to_path_buf)
            .ok_or_else(|| format!("Plex lists {} outside {}", file.path, library.display()))
    };
    let mut folders = BTreeSet::new();
    let episode = is_episode(request);
    if episode {
        let items: BTreeSet<usize> = files.iter().map(|file| file.item).collect();
        for item in items {
            let mut common: Option<PathBuf> = None;
            for file in files.iter().filter(|file| file.item == item) {
                let directory = parent(file)?;
                common = Some(match common {
                    None => directory,
                    Some(common) => common
                        .ancestors()
                        .find(|ancestor| directory.starts_with(ancestor))
                        .unwrap_or(Path::new("/"))
                        .to_path_buf(),
                });
            }
            if let Some(common) = common {
                let series = match common.file_name().and_then(|name| name.to_str()) {
                    Some(name) if season_number(name).is_some() => {
                        common.parent().map_or(common.clone(), Path::to_path_buf)
                    }
                    _ => common,
                };
                folders.insert(series);
            }
        }
    } else {
        for file in files {
            folders.insert(parent(file)?);
        }
    }
    if folders.len() > 1 {
        return Err(format!(
            "Plex keeps this title in {} folders ({})",
            folders.len(),
            listing(&folders)
        ));
    }
    let folder = folders
        .pop_first()
        .ok_or("Plex lists no file for this title")?;
    if let Err(reason) = confined(library, &folder) {
        return Err(format!(
            "the folder Plex keeps this title in, {}, {reason}",
            folder.display()
        ));
    }
    let mut seasons = BTreeSet::new();
    if episode {
        for file in files
            .iter()
            .filter(|file| file.season == Some(request.season))
        {
            let directory = parent(file)?;
            if directory != folder && directory.starts_with(&folder) {
                seasons.insert(directory);
            }
        }
    }
    Ok((folder, seasons))
}

/// The season a folder's name stands for: `Season 2`, `Season 02`,
/// `Saison 2`, `S02`, `Specials` (season 0).
pub(crate) fn season_number(name: &str) -> Option<u32> {
    let name = name.trim().to_lowercase();
    if matches!(name.as_str(), "specials" | "special") {
        return Some(0);
    }
    let rest = [
        "season",
        "saison",
        "series",
        "staffel",
        "temporada",
        "stagione",
        "s",
    ]
    .iter()
    .find_map(|prefix| name.strip_prefix(prefix))?;
    let digits = rest.trim_start_matches([' ', '_', '.', '-']);
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The folders directly below `series` named for `season`, read without
/// following symbolic links. The listing is bounded.
fn season_folders(series: &Path, season: u32) -> Vec<PathBuf> {
    if confined(Path::new("/"), series).is_err() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(series) else {
        return Vec::new();
    };
    entries
        .take(10_000)
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| entry.file_name().to_str().and_then(season_number) == Some(season))
        .map(|entry| entry.path())
        .take(16)
        .collect()
}

/// The shortest file accepted for a title, as a share of its runtime: a
/// sample, a trailer or another episode lasts far less.
pub(crate) const RUNTIME_SHORTEST: f64 = 0.70;
/// The longest file accepted, as a share of its runtime: an extended cut or
/// long credits stay below it; two episodes or another film do not.
pub(crate) const RUNTIME_LONGEST: f64 = 1.60;
/// The code that starts the error of a refused import.
pub(crate) const RUNTIME_MISMATCH: &str = "runtime_mismatch";

/// Compares the analyzed duration of the chosen file with the title's
/// runtime. A file outside [`RUNTIME_SHORTEST`]–[`RUNTIME_LONGEST`] of the
/// runtime is refused with a `runtime_mismatch` error giving both values;
/// otherwise the result is the note the job keeps, including why the check
/// was skipped (`runtime` holds why no runtime is known).
pub(crate) fn check_runtime(
    request: &Request,
    duration_seconds: Option<f64>,
    runtime: std::result::Result<u32, String>,
) -> Result<String> {
    let what = if is_episode(request) {
        "episode"
    } else {
        "film"
    };
    let runtime = match runtime {
        Ok(minutes) if minutes > 0 => minutes,
        Ok(_) => {
            return Ok(format!(
                "runtime_check_skipped: no runtime is known for this {what}"
            ));
        }
        Err(reason) => return Ok(format!("runtime_check_skipped: {reason}")),
    };
    let Some(duration) = duration_seconds.filter(|seconds| seconds.is_finite() && *seconds > 0.0)
    else {
        return Ok("runtime_check_skipped: the file's duration could not be read".to_owned());
    };
    let expected = f64::from(runtime) * 60.0;
    let minutes = (duration / 60.0).round();
    if duration < expected * RUNTIME_SHORTEST || duration > expected * RUNTIME_LONGEST {
        return Err(format!(
            "{RUNTIME_MISMATCH}: the file lasts {minutes} min but the {what} runs {runtime} min \
             (Mynou accepts {} to {} min)",
            (f64::from(runtime) * RUNTIME_SHORTEST).ceil(),
            (f64::from(runtime) * RUNTIME_LONGEST).floor()
        ));
    }
    Ok(format!(
        "runtime_checked: the file lasts {minutes} min, the {what} runs {runtime} min"
    ))
}

/// Subtitle formats imported with their video. A VobSub `.idx` and `.sub`
/// only go together.
const SUBTITLE_EXTENSIONS: [&str; 6] = ["srt", "ass", "ssa", "vtt", "idx", "sub"];
const VIDEO_EXTENSIONS: [&str; 6] = ["mp4", "m4v", "mov", "mkv", "webm", "avi"];
/// The largest subtitle file imported (a VobSub `.sub` holds images).
const MAX_SUBTITLE_BYTES: u64 = 64 * 1024 * 1024;
/// Subtitle files imported with one video at most.
const MAX_SUBTITLES: usize = 32;

/// Languages read in subtitle file names, with the ISO 639-1 code a Plex
/// sidecar name uses. French release tags name French subtitles.
const LANGUAGES: &[(&str, &[&str])] = &[
    ("en", &["en", "eng", "english", "anglais"]),
    (
        "fr",
        &[
            "fr",
            "fre",
            "fra",
            "french",
            "francais",
            "français",
            "vf",
            "vff",
            "vfq",
            "vfi",
            "vf2",
            "vostfr",
            "truefrench",
            "subfrench",
        ],
    ),
    (
        "es",
        &[
            "es",
            "spa",
            "spanish",
            "espanol",
            "español",
            "castellano",
            "latino",
        ],
    ),
    ("de", &["de", "ger", "deu", "german", "deutsch"]),
    ("it", &["it", "ita", "italian", "italiano"]),
    (
        "pt",
        &[
            "pt",
            "por",
            "portuguese",
            "portugues",
            "português",
            "brazilian",
        ],
    ),
    ("nl", &["nl", "dut", "nld", "dutch", "nederlands"]),
    ("ru", &["ru", "rus", "russian"]),
    ("ja", &["ja", "jpn", "japanese"]),
    ("zh", &["zh", "chi", "zho", "chinese"]),
    ("ko", &["ko", "kor", "korean"]),
    ("ar", &["ar", "ara", "arabic"]),
    ("sv", &["sv", "swe", "swedish"]),
    ("da", &["da", "dan", "danish"]),
    ("no", &["no", "nor", "nob", "norwegian"]),
    ("fi", &["fi", "fin", "finnish"]),
    ("pl", &["pl", "pol", "polish"]),
    ("cs", &["cs", "cze", "ces", "czech"]),
    ("hu", &["hu", "hun", "hungarian"]),
    ("el", &["el", "gre", "ell", "greek"]),
    ("tr", &["tr", "tur", "turkish"]),
    ("he", &["he", "heb", "hebrew"]),
    ("ro", &["ro", "rum", "ron", "romanian"]),
    ("hi", &["hin", "hindi"]),
    ("uk", &["uk", "ukr", "ukrainian"]),
    ("vi", &["vi", "vie", "vietnamese"]),
    ("th", &["th", "tha", "thai"]),
    ("id", &["id", "ind", "indonesian"]),
];
const FORCED: [&str; 2] = ["forced", "foreign"];
/// Hearing-impaired subtitles; `hi` is never read as Hindi.
const SDH: [&str; 4] = ["sdh", "hi", "cc", "hoh"];
const NEUTRAL: [&str; 7] = [
    "default",
    "full",
    "complete",
    "sub",
    "subs",
    "subtitle",
    "subtitles",
];

fn language(token: &str) -> Option<&'static str> {
    let base = token.split('-').next().unwrap_or(token);
    LANGUAGES.iter().find_map(|(code, names)| {
        (names.contains(&token) || (base.len() == 2 && names.contains(&base))).then_some(*code)
    })
}

/// The sidecar tags of a subtitle (`.fr`, `.en.forced`, `.fr.sdh`) read
/// from `words`: the part of its name after the video's name, or with
/// `whole` its whole name, where only the last words count.
fn subtitle_tags(words: &str, whole: bool) -> String {
    let tokens: Vec<String> = words
        .split(['.', '_', ' ', '[', ']', '(', ')', ',', '+'])
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect();
    let word = |token: &String| {
        let token = token.as_str();
        !FORCED.contains(&token)
            && !SDH.contains(&token)
            && !NEUTRAL.contains(&token)
            && !token.bytes().all(|byte| byte.is_ascii_digit())
    };
    // A whole name is a release or a title: only its last word may be a
    // language. After a video's name, any word may be.
    let (found, flags) = if whole {
        let last = tokens.iter().rposition(word);
        (
            last.and_then(|index| language(&tokens[index])),
            last.map_or(0, |index| index + 1),
        )
    } else {
        (
            tokens
                .iter()
                .rev()
                .filter(|token| word(token))
                .find_map(|token| language(token)),
            0,
        )
    };
    let flag = |names: &[&str]| {
        tokens[flags..]
            .iter()
            .any(|token| names.contains(&token.as_str()))
    };
    let (forced, sdh) = (flag(&FORCED), flag(&SDH));
    let mut tags = String::new();
    if let Some(code) = found {
        tags.push('.');
        tags.push_str(code);
    }
    if forced {
        tags.push_str(".forced");
    }
    if sdh {
        tags.push_str(".sdh");
    }
    tags
}

/// A subtitle file of the release, with the tags its library name takes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sidecar {
    pub(crate) source: PathBuf,
    pub(crate) tags: String,
    pub(crate) extension: String,
}

fn lowercase_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
}

/// The subtitles among `files` that belong to `video`: those named after it
/// (`Movie.fr.srt` for `Movie.mkv`), and when `files` is a whole release
/// with this one video (samples aside), every subtitle it ships. VobSub
/// `.idx` and `.sub` are kept only as a pair.
pub(crate) fn release_subtitles(
    video: &Path,
    files: &[PathBuf],
    whole_release: bool,
) -> Vec<Sidecar> {
    let Some(video_stem) = video
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_lowercase)
    else {
        return Vec::new();
    };
    let sample = |path: &Path| {
        path.to_str().is_some_and(|text| {
            text.to_lowercase()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word == "sample")
        })
    };
    let videos = files
        .iter()
        .filter(|file| {
            lowercase_extension(file).is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.as_str()))
                && !sample(file)
        })
        .count();
    let single = whole_release && videos == 1 && files.iter().any(|file| file == video);
    let mut found = Vec::new();
    for file in files {
        let Some(extension) = lowercase_extension(file)
            .filter(|extension| SUBTITLE_EXTENSIONS.contains(&extension.as_str()))
        else {
            continue;
        };
        let Some(stem) = file.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let lower = stem.to_lowercase();
        let tags = if lower == video_stem {
            String::new()
        } else if let Some(rest) = lower
            .strip_prefix(&video_stem)
            .and_then(|rest| rest.strip_prefix('.'))
        {
            subtitle_tags(rest, false)
        } else if single && !sample(file) {
            subtitle_tags(stem, true)
        } else {
            continue;
        };
        found.push(Sidecar {
            source: file.clone(),
            tags,
            extension,
        });
    }
    // A VobSub track is an index and its images: one without the other is useless.
    let pair = |sidecar: &Sidecar, other: &str| {
        found.iter().any(|candidate| {
            candidate.extension == other
                && candidate.source.with_extension("") == sidecar.source.with_extension("")
        })
    };
    let paired: Vec<Sidecar> = found
        .iter()
        .filter(|sidecar| match sidecar.extension.as_str() {
            "idx" => pair(sidecar, "sub"),
            "sub" => pair(sidecar, "idx"),
            _ => true,
        })
        .cloned()
        .collect();
    paired.into_iter().take(MAX_SUBTITLES).collect()
}

/// The regular files beside `video` that are named after it, for a file
/// imported from a path rather than a release. The listing is bounded.
pub(crate) fn neighbours(video: &Path) -> Vec<PathBuf> {
    let Some(directory) = video.parent() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .take(10_000)
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .collect()
}

/// Imports `subtitles` beside the imported `video` under Plex's sidecar
/// names, `<video name>.<language>[.forced][.sdh].<extension>`, the way the
/// video was imported (`copy_source` for a native payload). A name that
/// already holds another file is never overwritten: that subtitle is
/// skipped, and so is one that cannot be read; a note says why. Returns the
/// imported files and the notes.
pub(crate) fn import_subtitles(
    video: &Path,
    library: &Path,
    subtitles: &[Sidecar],
    active: &AtomicBool,
    copy_source: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    let (Some(directory), Some(stem)) = (
        video.parent(),
        video.file_stem().and_then(|stem| stem.to_str()),
    ) else {
        return Err("imported video has no name".to_owned());
    };
    let mut imported = Vec::new();
    let mut notes = Vec::new();
    for subtitle in subtitles {
        active_import(active)?;
        let name = format!("{stem}{}.{}", subtitle.tags, subtitle.extension);
        let shown = subtitle
            .source
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let oversized = fs::symlink_metadata(&subtitle.source)
            .is_ok_and(|metadata| metadata.len() > MAX_SUBTITLE_BYTES);
        if oversized {
            notes.push(format!("subtitle_skipped: {shown} is larger than 64 MiB"));
            continue;
        }
        let destination = directory.join(&name);
        if destination.parent() != Some(directory) || !destination.starts_with(library) {
            notes.push(format!(
                "subtitle_skipped: {shown} has no valid library name"
            ));
            continue;
        }
        match import_destination(
            &subtitle.source,
            library,
            destination.clone(),
            &[],
            active,
            copy_source,
        ) {
            Ok(path) => {
                let path = path.to_string_lossy().into_owned();
                if !imported.contains(&path) {
                    imported.push(path);
                }
            }
            // The whole import is tried again: the video is found done.
            Err(error)
                if !active.load(Ordering::Acquire) || error.starts_with(IMPORT_IN_PROGRESS) =>
            {
                return Err(error);
            }
            Err(error) if error == DIFFERENT_FILE => notes.push(format!(
                "subtitle_kept: {} already exists; {shown} was not imported",
                destination.display()
            )),
            Err(error) => notes.push(format!("subtitle_skipped: {shown}: {error}")),
        }
    }
    Ok((imported, notes))
}

/// The error of an import whose destination holds another file.
const DIFFERENT_FILE: &str = "library already contains a different file";
/// The start of the error of an import that waits for another one.
const IMPORT_IN_PROGRESS: &str = "another import is in progress";

fn active_import(active: &AtomicBool) -> Result<()> {
    if active.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err("import cancelled or processing lease invalidated".to_owned())
    }
}

/// Plex's consecutive-episode range names one physical file for every owner.
/// Shared files keep the names their records were made with
/// ([`legacy_filename_part`]): a recorded destination is never reassigned.
pub(crate) fn shared_target(library: &Path, file: &crate::pack::SharedFile) -> Result<PathBuf> {
    let title = legacy_filename_part(&file.title)?;
    let extension = Path::new(&file.file_path)
        .extension()
        .and_then(|e| e.to_str())
        .ok_or("Shared media extension is missing")?;
    let suffix = format!(
        " - S{:02}E{:02}-E{:02} [mynou-{}].{extension}",
        file.season,
        file.first_episode,
        file.last_episode,
        file.id()
    );
    let mut name = title.clone();
    while name.len() + suffix.len() > 255 {
        name.pop();
    }
    Ok(library
        .join(title)
        .join(format!("Season {:02}", file.season))
        .join(format!("{name}{suffix}")))
}

pub(crate) fn import_shared_file_cancellable(
    source: &Path,
    library: &Path,
    file: &crate::pack::SharedFile,
    active: &AtomicBool,
) -> Result<PathBuf> {
    file.validate()?;
    let destination = shared_target(library, file)?;
    if destination != Path::new(&file.import_path) {
        return Err(
            "Shared library root changed; the recorded destination cannot be reassigned".into(),
        );
    }
    // Keep the native payload inode private. Only the temporary import and
    // final library name share a link during atomic publication.
    import_destination(source, library, destination, &[], active, true)
}

/// Device, inode, length and modification time of an open file: what tells
/// that it is still the same file, unchanged.
type Fingerprint = (u64, u64, u64, i64, i64);

fn fingerprint(file: &File) -> Result<Fingerprint> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

/// Whether `directory` is on the volume of `file`.
fn same_device(file: &File, directory: &Path) -> Result<bool> {
    let file = file.metadata().map_err(|error| error.to_string())?;
    let directory = fs::metadata(directory).map_err(|error| error.to_string())?;
    Ok(file.dev() == directory.dev())
}

/// Whether `file` lives on an APFS volume, the only one whose copies are
/// clones. Other volumes that take hard links (HFS+, NFS) have no clones:
/// `fs::copy` would copy every byte there, without cancellation or a check.
/// Each APFS mount point listed by `mount` is compared by device, which
/// also finds the data volume behind firmlinks such as /Users.
fn on_apfs(file: &File) -> Result<bool> {
    let device = file.metadata().map_err(|error| error.to_string())?.dev();
    let Ok(output) = std::process::Command::new("/sbin/mount").output() else {
        return Ok(false);
    };
    if !output.status.success() {
        return Ok(false);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(apfs_mount_point)
        .any(|point| fs::metadata(point).is_ok_and(|metadata| metadata.dev() == device)))
}

/// The mount point of a `mount` line such as
/// `/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled)`, when the
/// volume is APFS.
fn apfs_mount_point(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once(" on ")?;
    let (point, options) = rest.rsplit_once(" (")?;
    (options.split([',', ')']).next()? == "apfs").then_some(point)
}

/// The length and SHA-256 of a regular file, read without following links.
fn file_digest(path: &Path, active: &AtomicBool) -> Result<Option<(u64, [u8; 32])>> {
    validate_path(path)?;
    let mut file = private_options()
        .read(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        active_import(active)?;
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(Some((length, hasher.finalize())));
        }
        hasher.update(&buffer[..count]);
        length += count as u64;
    }
}

fn same_bytes(source: &mut File, destination: &Path, active: &AtomicBool) -> Result<bool> {
    active_import(active)?;
    validate_path(destination)?;
    if !fs::symlink_metadata(destination)
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Ok(false);
    }
    let mut destination = private_options()
        .read(true)
        .open(destination)
        .map_err(|error| error.to_string())?;
    let source_metadata = source.metadata().map_err(|error| error.to_string())?;
    let destination_metadata = destination.metadata().map_err(|error| error.to_string())?;
    if !destination_metadata.is_file() || source_metadata.len() != destination_metadata.len() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if source_metadata.dev() == destination_metadata.dev()
            && source_metadata.ino() == destination_metadata.ino()
        {
            return Ok(true);
        }
    }
    source
        .seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut source_buffer = [0_u8; 65_536];
    let mut destination_buffer = [0_u8; 65_536];
    loop {
        active_import(active)?;
        let length = source
            .read(&mut source_buffer)
            .map_err(|error| error.to_string())?;
        if length == 0 {
            return Ok(true);
        }
        destination
            .read_exact(&mut destination_buffer[..length])
            .map_err(|error| error.to_string())?;
        if source_buffer[..length] != destination_buffer[..length] {
            return Ok(false);
        }
    }
}

/// Creates a hard link when the filesystems allow it, otherwise copies to a
/// temporary file. Final publication uses `hard_link` to avoid overwriting
/// a file created by a concurrent process. The destination is Mynou's own
/// layout ([`placement`] without Plex).
pub fn import_file(source: &Path, library: &Path, request: &Request) -> Result<PathBuf> {
    import_file_cancellable(source, library, request, &AtomicBool::new(true))
}

pub fn import_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, &[], active, false)
}

/// Imports a native torrent payload by copy. A hard link would give the payload
/// a second name, and the native client rejects any media file whose link count
/// is not one, so it could no longer seed or verify the download.
#[cfg(test)]
pub(crate) fn import_native_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, &[], active, true)
}

/// Imports an upgrade in place of the version it replaces: the file takes the
/// folder and name of the first of `replaced`, so when the extension is the
/// same the new file takes its place in one step. A file of another name
/// (another extension) is left for the caller to delete. No other existing
/// file is ever overwritten.
pub fn import_upgrade_file(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
) -> Result<PathBuf> {
    import_upgrade_file_cancellable(source, library, request, replaced, &AtomicBool::new(true))
}

pub fn import_upgrade_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, replaced, active, false)
}

/// The upgrade counterpart of [`import_native_file_cancellable`].
#[cfg(test)]
pub(crate) fn import_native_upgrade_file_cancellable(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
    active: &AtomicBool,
) -> Result<PathBuf> {
    import_to_target(source, library, request, replaced, active, true)
}

fn import_to_target(
    source: &Path,
    library: &Path,
    request: &Request,
    replaced: &[String],
    active: &AtomicBool,
    copy_source: bool,
) -> Result<PathBuf> {
    let placement = placement(library, request, replaced, &mut || Ok(None))?;
    import_placed(source, library, &placement, replaced, active, copy_source)
}

/// Imports `source` at `placement`. Only a regular file named in `replaced`
/// (the version an upgrade replaces) may be overwritten; `copy_source`
/// copies a native torrent payload instead of linking it.
pub(crate) fn import_placed(
    source: &Path,
    library: &Path,
    placement: &Placement,
    replaced: &[String],
    active: &AtomicBool,
    copy_source: bool,
) -> Result<PathBuf> {
    let destination = placement.destination(library, source)?;
    import_destination(source, library, destination, replaced, active, copy_source)
}

/// Removes `.mynou-*.tmp` regular files that an interrupted import left in
/// `directory`. The caller holds the library import lock, so no live import can
/// own one. Best effort: a failure here must not fail the import.
#[cfg(unix)]
fn remove_stale_temporaries(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(".mynou-") || !name.ends_with(".tmp") {
            continue;
        }
        let path = entry.path();
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
            let _ = fs::remove_file(&path);
        }
    }
}

fn import_destination(
    source: &Path,
    library: &Path,
    destination: PathBuf,
    replaced: &[String],
    active: &AtomicBool,
    copy_source: bool,
) -> Result<PathBuf> {
    // Only a regular file the replaced version recorded may be overwritten.
    let replaceable = |destination: &Path| {
        replaced.iter().any(|file| Path::new(file) == destination)
            && fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.is_file())
    };
    #[cfg(unix)]
    {
        active_import(active)?;
        validate_path(source)?;
        validate_path(library)?;
        if !fs::symlink_metadata(source)
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("media source must be a regular file".to_owned());
        }
        let mut input = private_options()
            .read(true)
            .open(source)
            .map_err(|error| format!("cannot access media source: {error}"))?;
        if !input
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("media source must be a regular file".to_owned());
        }
        create_directory(library)?;
        let lock_path = library.join(".organizer.lock");
        validate_path(&lock_path)?;
        if let Ok(metadata) = fs::symlink_metadata(&lock_path)
            && !metadata.is_file()
        {
            return Err("import lock must be a regular file".to_owned());
        }
        let lock = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| error.to_string())?;
        if !lock
            .metadata()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Err("import lock must be a regular file".to_owned());
        }
        try_lock_file(&lock).map_err(|error| format!("{IMPORT_IN_PROGRESS}: {error}"))?;
        let directory = destination
            .parent()
            .ok_or_else(|| "missing destination directory".to_owned())?;
        create_directory(directory)?;
        if destination
            .try_exists()
            .map_err(|error| error.to_string())?
        {
            if same_bytes(&mut input, &destination, active)? {
                return Ok(destination);
            }
            if !replaceable(&destination) {
                return Err(DIFFERENT_FILE.to_owned());
            }
        }
        remove_stale_temporaries(directory);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temporary = directory.join(format!(
            ".mynou-{}-{timestamp}-{}.tmp",
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let mut owns_temporary = false;
        // A copy on the source's own APFS volume is a copy-on-write clone:
        // instant, and sharing the source's blocks until either file changes.
        // Any other volume keeps the cancellable, verified copy below.
        let clone = copy_source && same_device(&input, directory)? && on_apfs(&input)?;
        let result = (|| {
            if !copy_source && fs::hard_link(source, &temporary).is_ok() {
                owns_temporary = true;
                if !same_bytes(&mut input, &temporary, active)? {
                    return Err("source changed during import".to_owned());
                }
            } else if clone {
                let before = fingerprint(&input)?;
                owns_temporary = true;
                // `fs::copy` clones with `fclonefileat` on macOS.
                fs::copy(source, &temporary)
                    .map_err(|error| format!("cannot copy media: {error}"))?;
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                    .map_err(|error| error.to_string())?;
                // The path still names the file that was opened and checked,
                // unchanged, and the copy has its length.
                let named = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
                let copied = fs::symlink_metadata(&temporary).map_err(|error| error.to_string())?;
                if !named.is_file()
                    || (named.dev(), named.ino()) != (before.0, before.1)
                    || fingerprint(&input)? != before
                    || !copied.is_file()
                    || copied.len() != before.2
                {
                    return Err("source changed during copying".to_owned());
                }
                private_options()
                    .read(true)
                    .open(&temporary)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| error.to_string())?;
            } else {
                let mut output = private_options()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .map_err(|error| format!("cannot access temporary file: {error}"))?;
                owns_temporary = true;
                let before = fingerprint(&input)?;
                input
                    .seek(SeekFrom::Start(0))
                    .map_err(|error| error.to_string())?;
                // The bytes are hashed as they are copied, then the copy is
                // read back once: the source is not read a second time.
                let mut copied = 0_u64;
                let mut hasher = Sha256::new();
                let mut buffer = vec![0_u8; 1 << 20];
                loop {
                    active_import(active)?;
                    let count = input
                        .read(&mut buffer)
                        .map_err(|error| format!("cannot read media: {error}"))?;
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                    output
                        .write_all(&buffer[..count])
                        .map_err(|error| format!("cannot copy media: {error}"))?;
                    copied = copied
                        .checked_add(count as u64)
                        .ok_or_else(|| "media too large".to_owned())?;
                }
                if copied != before.2 {
                    return Err("media size changed during import".to_owned());
                }
                output
                    .flush()
                    .and_then(|_| output.sync_all())
                    .map_err(|error| error.to_string())?;
                drop(output);
                if fingerprint(&input)? != before {
                    return Err("source changed during copying".to_owned());
                }
                if file_digest(&temporary, active)? != Some((copied, hasher.finalize())) {
                    return Err("the copy differs from the source".to_owned());
                }
            }
            input
                .sync_all()
                .map_err(|error| format!("cannot synchronize media: {error}"))?;
            active_import(active)?;
            match fs::hard_link(&temporary, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !same_bytes(&mut input, &destination, active)? {
                        if !replaceable(&destination) {
                            return Err(DIFFERENT_FILE.to_owned());
                        }
                        // The earlier version's file becomes the new one at once.
                        fs::rename(&temporary, &destination)
                            .map_err(|error| format!("cannot replace the earlier file: {error}"))?;
                        owns_temporary = false;
                    }
                }
                Err(error) => {
                    return Err(format!(
                        "atomic publication without overwriting is unsupported on this filesystem: {error}"
                    ));
                }
            }
            sync_directory(directory)?;
            Ok(destination.clone())
        })();
        let cleanup = if owns_temporary {
            fs::remove_file(&temporary)
        } else {
            Ok(())
        };
        if result.is_ok() {
            cleanup.map_err(|error| format!("cannot clean up import: {error}"))?;
            sync_directory(directory)?;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mynou-organizer-{}-{}",
                std::process::id(),
                TEMPORARY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn request() -> Request {
        Request {
            kind: "movie".to_owned(),
            title: "Movie".to_owned(),
            year: 2026,
            season: 0,
            episode: 0,
            source_path: None,
            source_url: None,
            runtime_minutes: None,
            source_numbering: None,
            tmdb_id: None,
        }
    }

    #[test]
    fn shared_import_keeps_payload_private_and_refuses_to_overwrite_a_different_copy() {
        use std::os::unix::fs::MetadataExt;
        let directory = Directory::new();
        let source = directory.0.join("source.mp4");
        fs::write(&source, b"original bytes").unwrap();
        let library = directory.0.join("library");
        let mut file = crate::pack::SharedFile {
            torrent_id: "a".repeat(40),
            file_path: "Pack/shared.mp4".into(),
            tmdb_id: 42,
            title: "Fixture Series".into(),
            year: 2024,
            season: 1,
            first_episode: 1,
            last_episode: 2,
            import_path: String::new(),
        };
        file.import_path = shared_target(&library, &file)
            .unwrap()
            .to_str()
            .unwrap()
            .into();
        let active = AtomicBool::new(true);
        let destination =
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap();
        assert_eq!(fs::metadata(&source).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(&destination).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&destination).unwrap().ino()
        );
        assert_eq!(
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap(),
            destination
        );
        fs::write(&source, b"modified bytes").unwrap();
        assert!(
            import_shared_file_cancellable(&source, &library, &file, &active)
                .unwrap_err()
                .contains("different file")
        );
        assert_eq!(fs::read(&destination).unwrap(), b"original bytes");
    }

    #[test]
    fn shared_import_copies_payloads_larger_than_the_copy_buffer() {
        let directory = Directory::new();
        let source = directory.0.join("source.mp4");
        let bytes: Vec<u8> = (0..=255).cycle().take(150_000).collect();
        fs::write(&source, &bytes).unwrap();
        let library = directory.0.join("library");
        let mut file = crate::pack::SharedFile {
            torrent_id: "b".repeat(40),
            file_path: "Pack/large.mp4".into(),
            tmdb_id: 43,
            title: "Large Series".into(),
            year: 2024,
            season: 1,
            first_episode: 1,
            last_episode: 2,
            import_path: String::new(),
        };
        file.import_path = shared_target(&library, &file)
            .unwrap()
            .to_str()
            .unwrap()
            .into();
        let active = AtomicBool::new(true);
        let destination =
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap();
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert_eq!(
            import_shared_file_cancellable(&source, &library, &file, &active).unwrap(),
            destination
        );
    }

    #[test]
    fn import_is_idempotent_and_never_overwrites_different_content() {
        let directory = Directory::new();
        let source = directory.0.join("source.mkv");
        fs::write(&source, b"original content").unwrap();
        let library = directory.0.join("library");
        let destination = import_file(&source, &library, &request()).unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"original content");
        assert_eq!(fs::read(&destination).unwrap(), b"original content");
        assert_eq!(
            import_file(&source, &library, &request()).unwrap(),
            destination
        );
        let different = directory.0.join("other.mkv");
        fs::write(&different, b"different content").unwrap();
        assert!(
            import_file(&different, &library, &request())
                .unwrap_err()
                .contains("different")
        );
        assert_eq!(fs::read(&destination).unwrap(), b"original content");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&source).unwrap().ino(),
                fs::metadata(&destination).unwrap().ino()
            );
        }
    }

    #[test]
    fn native_payload_imports_copy_and_keep_the_payload_inode_private() {
        use std::os::unix::fs::MetadataExt;
        let directory = Directory::new();
        let source = directory.0.join("payload.mkv");
        fs::write(&source, b"payload bytes").unwrap();
        let library = directory.0.join("library");
        let active = AtomicBool::new(true);
        let destination =
            import_native_file_cancellable(&source, &library, &request(), &active).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"payload bytes");
        assert_eq!(fs::metadata(&source).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(&destination).unwrap().nlink(), 1);
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(&destination).unwrap().ino()
        );
        assert_eq!(
            import_native_file_cancellable(&source, &library, &request(), &active).unwrap(),
            destination
        );
        let better = directory.0.join("better.mkv");
        fs::write(&better, b"better payload").unwrap();
        let replaced = [destination.to_string_lossy().into_owned()];
        let upgrade = import_native_upgrade_file_cancellable(
            &better,
            &library,
            &request(),
            &replaced,
            &active,
        )
        .unwrap();
        assert_eq!(upgrade, destination);
        assert_eq!(fs::read(&upgrade).unwrap(), b"better payload");
        assert_eq!(fs::metadata(&better).unwrap().nlink(), 1);
        assert_eq!(fs::metadata(&upgrade).unwrap().nlink(), 1);
    }

    #[test]
    fn import_reclaims_stale_temporaries_and_keeps_everything_else() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let source = directory.0.join("payload.mkv");
        fs::write(&source, b"payload bytes").unwrap();
        let library = directory.0.join("library");
        let destination_directory = target(&source, &library, &request())
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        fs::create_dir_all(&destination_directory).unwrap();
        let stale = destination_directory.join(".mynou-1-2-3.tmp");
        let other_tmp = destination_directory.join("notes.tmp");
        let other_hidden = destination_directory.join(".mynou-keep.txt");
        let link = destination_directory.join(".mynou-9-9-9.tmp.link.tmp");
        fs::write(&stale, vec![7_u8; 4096]).unwrap();
        fs::write(&other_tmp, b"keep").unwrap();
        fs::write(&other_hidden, b"keep").unwrap();
        symlink(&source, &link).unwrap();
        let active = AtomicBool::new(true);
        let destination =
            import_native_file_cancellable(&source, &library, &request(), &active).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"payload bytes");
        assert!(!stale.exists());
        assert!(other_tmp.exists());
        assert!(other_hidden.exists());
        let link_type = fs::symlink_metadata(&link).unwrap().file_type();
        assert!(link_type.is_symlink());
        assert_eq!(fs::read(&source).unwrap(), b"payload bytes");
    }

    #[test]
    fn episode_layout_and_title_sanitization_stay_inside_library() {
        let directory = Directory::new();
        let source = directory.0.join("source.mp4");
        fs::write(&source, b"media").unwrap();
        let library = directory.0.join("library");
        let mut request = request();
        request.kind = "episode".to_owned();
        request.title = "../../ Series / danger".to_owned();
        request.season = 2;
        request.episode = 7;
        let destination = import_file(&source, &library, &request).unwrap();
        assert!(destination.starts_with(&library));
        assert_eq!(
            destination.parent().unwrap().file_name().unwrap(),
            "Season 02"
        );
        assert!(
            destination
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("S02E07.mp4")
        );
        assert!(
            destination
                .components()
                .all(|part| part != Component::ParentDir)
        );
    }

    #[test]
    fn cancelled_import_has_no_filesystem_effect_and_preserves_source() {
        let directory = Directory::new();
        let source = directory.0.join("source.mkv");
        fs::write(&source, b"intact source").unwrap();
        let library = directory.0.join("library");
        let error = import_file_cancellable(&source, &library, &request(), &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert!(!library.exists());
        assert_eq!(fs::read(source).unwrap(), b"intact source");
    }

    #[cfg(unix)]
    #[test]
    fn source_and_destination_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let source = directory.0.join("source.mkv");
        fs::write(&source, b"media").unwrap();
        let alias = directory.0.join("alias.mkv");
        symlink(&source, &alias).unwrap();
        assert!(import_file(&alias, &directory.0.join("library"), &request()).is_err());
        let outside = directory.0.join("outside");
        fs::create_dir(&outside).unwrap();
        let library = directory.0.join("library");
        symlink(&outside, &library).unwrap();
        assert!(import_file(&source, &library, &request()).is_err());
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    fn episode(title: &str, season: u32, number: u32) -> Request {
        let mut request = request();
        request.kind = "episode".into();
        request.title = title.into();
        request.season = season;
        request.episode = number;
        request
    }

    fn listed(item: usize, season: Option<u32>, path: &Path) -> ListedFile {
        ListedFile {
            item,
            season,
            path: path.to_str().unwrap().into(),
        }
    }

    fn place(library: &Path, request: &Request, files: Option<Vec<ListedFile>>) -> Placement {
        let mut files = Some(files);
        placement(library, request, &[], &mut || Ok(files.take().flatten())).unwrap()
    }

    #[test]
    fn names_turn_colons_into_dashes_and_keep_other_replacements() {
        assert_eq!(
            filename_part("Mission: Impossible").unwrap(),
            "Mission - Impossible"
        );
        assert_eq!(
            filename_part("Mission : Impossible").unwrap(),
            "Mission - Impossible"
        );
        assert_eq!(filename_part("Re:Zero").unwrap(), "Re-Zero");
        assert_eq!(filename_part("Title:").unwrap(), "Title");
        assert_eq!(filename_part("What? / Why*").unwrap(), "What_ _ Why_");
        assert_eq!(
            legacy_filename_part("Mission: Impossible").unwrap(),
            "Mission_ Impossible"
        );
        let mut film = request();
        film.title = "Mission: Impossible".into();
        film.year = 1996;
        let library = Path::new("/library");
        assert_eq!(
            target(Path::new("/downloads/a.mkv"), library, &film).unwrap(),
            library.join("Mission - Impossible (1996)/Mission - Impossible (1996).mkv")
        );
    }

    #[test]
    fn a_folder_mynou_named_with_underscores_is_kept() {
        let directory = Directory::new();
        let library = directory.0.join("library");
        let legacy = library.join("Mission_ Impossible (1996)");
        fs::create_dir_all(&legacy).unwrap();
        let mut film = request();
        film.title = "Mission: Impossible".into();
        film.year = 1996;
        let placed = place(&library, &film, None);
        assert_eq!(placed.directory, legacy);
        assert_eq!(placed.stem, "Mission - Impossible (1996)");
        assert!(
            placed.notes[0].starts_with("folder_kept: "),
            "{:?}",
            placed.notes
        );
        // Once the new name exists, it wins.
        fs::create_dir_all(library.join("Mission - Impossible (1996)")).unwrap();
        let placed = place(&library, &film, None);
        assert_eq!(
            placed.directory,
            library.join("Mission - Impossible (1996)")
        );
        assert!(placed.notes.is_empty());

        let series = library.join("Star Trek_ Picard");
        fs::create_dir_all(series.join("Season 01")).unwrap();
        let placed = place(&library, &episode("Star Trek: Picard", 1, 3), None);
        assert_eq!(placed.directory, series.join("Season 01"));
        assert_eq!(placed.stem, "Star Trek - Picard - S01E03");
    }

    #[test]
    fn an_episode_goes_into_the_folder_that_holds_its_season() {
        let directory = Directory::new();
        let library = directory.0.join("library");
        let show = library.join("Show");
        // A new series gets the padded name.
        assert_eq!(
            place(&library, &episode("Show", 2, 1), None).directory,
            show.join("Season 02")
        );
        fs::create_dir_all(show.join("Season 2")).unwrap();
        fs::create_dir_all(show.join("Specials")).unwrap();
        fs::create_dir_all(show.join("Season 3")).unwrap();
        fs::write(show.join("Season 4"), b"a file, not a folder").unwrap();
        assert_eq!(
            place(&library, &episode("Show", 2, 1), None).directory,
            show.join("Season 2")
        );
        assert_eq!(
            place(&library, &episode("Show", 0, 1), None).directory,
            show.join("Specials")
        );
        assert_eq!(
            place(&library, &episode("Show", 4, 1), None).directory,
            show.join("Season 04")
        );
        // Two folders for one season: Mynou's own name, and a note.
        fs::create_dir_all(show.join("Season 03")).unwrap();
        let placed = place(&library, &episode("Show", 3, 1), None);
        assert_eq!(placed.directory, show.join("Season 03"));
        assert!(placed.notes[0].starts_with("folder_fallback: 2 folders hold season 3"));
        for (name, season) in [
            ("Season 2", Some(2)),
            ("season.02", Some(2)),
            ("Saison 10", Some(10)),
            ("S01", Some(1)),
            ("Specials", Some(0)),
            ("Season 00", Some(0)),
            ("Sample", None),
            ("Seasons 1-3", None),
            ("Extras", None),
        ] {
            assert_eq!(season_number(name), season, "{name}");
        }
    }

    #[test]
    fn the_folder_plex_lists_is_used_when_it_is_one_real_folder_inside_the_library() {
        use std::os::unix::fs::symlink;
        let directory = Directory::new();
        let library = directory.0.join("library");
        let mut film = request();
        film.title = "Mission: Impossible".into();
        film.year = 1996;
        let existing = library.join("Mission Impossible 1996");
        fs::create_dir_all(&existing).unwrap();
        let file = |folder: &Path| vec![listed(0, None, &folder.join("film.mkv"))];
        let placed = place(&library, &film, Some(file(&existing)));
        assert_eq!(placed.directory, existing);
        assert_eq!(placed.stem, "Mission - Impossible (1996)");
        assert!(placed.notes[0].starts_with("plex_folder: "));

        let default = library.join("Mission - Impossible (1996)");
        let outside = directory.0.join("elsewhere/Mission (1996)");
        fs::create_dir_all(&outside).unwrap();
        let link = library.join("Linked");
        symlink(&outside, &link).unwrap();
        let mut two = file(&existing);
        two.push(listed(1, None, &library.join("Other/film.mkv")));
        for (files, reason) in [
            (file(&outside), "is outside"),
            (file(&link), "symbolic link"),
            (two, "2 folders"),
            (file(&library.join("Missing")), "missing from the disk"),
            (vec![listed(0, None, &library.join("film.mkv"))], "itself"),
            (file(Path::new("relative")), "outside"),
        ] {
            let placed = place(&library, &film, Some(files));
            assert_eq!(placed.directory, default, "{reason}");
            assert!(
                placed.notes[0].starts_with("folder_fallback: ")
                    && placed.notes[0].contains(reason),
                "{reason}: {:?}",
                placed.notes
            );
        }
    }

    #[test]
    fn a_series_folder_and_its_season_come_from_the_episodes_plex_lists() {
        let directory = Directory::new();
        let library = directory.0.join("library");
        let show = library.join("La Casa de Papel (2017)");
        let season = show.join("Book Two");
        fs::create_dir_all(show.join("Season 1")).unwrap();
        fs::create_dir_all(&season).unwrap();
        let files = vec![
            listed(0, Some(1), &show.join("Season 1/e1.mkv")),
            listed(0, Some(2), &season.join("e1.mkv")),
        ];
        let placed = place(&library, &episode("Money Heist", 2, 5), Some(files.clone()));
        assert_eq!(placed.directory, season);
        assert_eq!(placed.stem, "Money Heist - S02E05");
        // A season Plex does not list yet goes beside the others.
        let placed = place(&library, &episode("Money Heist", 3, 1), Some(files));
        assert_eq!(placed.directory, show.join("Season 03"));
        // One season folder: the series is its parent.
        let placed = place(
            &library,
            &episode("Money Heist", 1, 2),
            Some(vec![listed(0, Some(1), &show.join("Season 1/e1.mkv"))]),
        );
        assert_eq!(placed.directory, show.join("Season 1"));
        // Two series in Plex for one title.
        let placed = place(
            &library,
            &episode("Money Heist", 1, 2),
            Some(vec![
                listed(0, Some(1), &show.join("Season 1/e1.mkv")),
                listed(1, Some(1), &library.join("Money Heist/Season 1/e1.mkv")),
            ]),
        );
        assert_eq!(placed.directory, library.join("Money Heist/Season 01"));
        assert!(placed.notes[0].contains("2 folders"));
    }

    #[test]
    fn an_upgrade_keeps_the_folder_and_name_of_the_file_it_replaces() {
        let directory = Directory::new();
        let library = directory.0.join("library");
        let folder = library.join("Mission_ Impossible (1996)");
        fs::create_dir_all(&folder).unwrap();
        let earlier = folder.join("Mission_ Impossible (1996).mkv");
        fs::write(&earlier, b"earlier").unwrap();
        let mut film = request();
        film.title = "Mission: Impossible".into();
        film.year = 1996;
        let replaced = [earlier.to_str().unwrap().to_owned()];
        let placed = placement(&library, &film, &replaced, &mut || {
            panic!("Plex is not asked for an upgrade")
        })
        .unwrap();
        assert_eq!(placed.directory, folder);
        assert_eq!(placed.stem, "Mission_ Impossible (1996)");
        // A file outside the library is not followed.
        let outside = directory.0.join("outside.mkv");
        fs::write(&outside, b"outside").unwrap();
        let replaced = [outside.to_str().unwrap().to_owned()];
        let placed = placement(&library, &film, &replaced, &mut || Ok(None)).unwrap();
        assert_eq!(placed.directory, folder);
        assert!(placed.notes[0].starts_with("folder_fallback: the folder of the file"));
    }

    #[test]
    fn subtitle_names_carry_language_and_flags() {
        for (words, whole, tags) in [
            ("fr", false, ".fr"),
            ("en.forced", false, ".en.forced"),
            ("French.SDH", false, ".fr.sdh"),
            ("vostfr", false, ".fr"),
            ("VFF", false, ".fr"),
            ("fre", false, ".fr"),
            ("pt-BR", false, ".pt"),
            ("hi", false, ".sdh"),
            ("forced.eng.2", false, ".en.forced"),
            ("klingon", false, ""),
            ("", false, ""),
            ("2_English", true, ".en"),
            ("3_French (Forced)", true, ".fr.forced"),
            ("It.2017.1080p", true, ""),
            ("Forced.Entry.Spanish", true, ".es"),
        ] {
            assert_eq!(subtitle_tags(words, whole), tags, "{words}");
        }
    }

    #[test]
    fn a_release_brings_its_own_subtitles_only() {
        let video = PathBuf::from("/r/Movie.mkv");
        let files: Vec<PathBuf> = [
            "/r/Movie.mkv",
            "/r/Movie.fr.srt",
            "/r/Movie.en.forced.srt",
            "/r/Movie.nfo",
            "/r/Subs/Movie.de.ass",
            "/r/Moviegoer.srt",
            "/r/Movie.idx",
            "/r/Movie.sub",
            "/r/Movie.es.sub",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let found = release_subtitles(&video, &files, true);
        let names: Vec<(String, &str)> = found
            .iter()
            .map(|s| (s.source.to_str().unwrap().to_owned(), s.tags.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                ("/r/Movie.fr.srt".to_owned(), ".fr"),
                ("/r/Movie.en.forced.srt".to_owned(), ".en.forced"),
                ("/r/Subs/Movie.de.ass".to_owned(), ".de"),
                // The only video's release: its other subtitles are its own.
                ("/r/Moviegoer.srt".to_owned(), ""),
                ("/r/Movie.idx".to_owned(), ""),
                ("/r/Movie.sub".to_owned(), ""),
            ]
        );
        // Beside several videos, only names that follow the video count.
        let mut pack = files.clone();
        pack.push(PathBuf::from("/r/Other.mkv"));
        pack.push(PathBuf::from("/r/Sample/movie-sample.mkv"));
        assert_eq!(release_subtitles(&video, &pack, true).len(), 5);
        assert_eq!(release_subtitles(&video, &files, false).len(), 5);
        let only_samples = [
            video.clone(),
            PathBuf::from("/r/Sample/sample.mkv"),
            PathBuf::from("/r/English.srt"),
        ];
        assert_eq!(
            release_subtitles(&video, &only_samples, true)[0].tags,
            ".en"
        );
    }

    #[test]
    fn subtitles_follow_the_video_and_never_overwrite() {
        let directory = Directory::new();
        let release = directory.0.join("release");
        fs::create_dir_all(&release).unwrap();
        let video = release.join("Movie.mkv");
        fs::write(&video, b"video").unwrap();
        fs::write(release.join("Movie.fr.srt"), b"french").unwrap();
        fs::write(release.join("Movie.en.forced.srt"), b"forced").unwrap();
        fs::write(release.join("Movie.nfo"), b"notes").unwrap();
        let library = directory.0.join("library");
        let imported = import_file(&video, &library, &request()).unwrap();
        let folder = imported.parent().unwrap().to_path_buf();
        // An earlier French subtitle is already there.
        fs::write(folder.join("Movie (2026).fr.srt"), b"earlier french").unwrap();
        let subtitles = release_subtitles(&video, &neighbours(&video), false);
        let active = AtomicBool::new(true);
        let (done, notes) =
            import_subtitles(&imported, &library, &subtitles, &active, false).unwrap();
        assert_eq!(
            done,
            [folder.join("Movie (2026).en.forced.srt").to_str().unwrap()]
        );
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("subtitle_kept: "), "{notes:?}");
        assert_eq!(
            fs::read(folder.join("Movie (2026).fr.srt")).unwrap(),
            b"earlier french"
        );
        assert_eq!(
            fs::read(folder.join("Movie (2026).en.forced.srt")).unwrap(),
            b"forced"
        );
        let mut names: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "Movie (2026).en.forced.srt",
                "Movie (2026).fr.srt",
                "Movie (2026).mkv"
            ]
        );
        // Again: the subtitle already imported is found done.
        let (again, _) = import_subtitles(&imported, &library, &subtitles, &active, true).unwrap();
        assert_eq!(again, done);
        assert_eq!(fs::read(release.join("Movie.fr.srt")).unwrap(), b"french");

        // An upgrade takes the video's place and brings its own subtitles;
        // the earlier version keeps its own.
        let better = directory.0.join("better");
        fs::create_dir_all(&better).unwrap();
        let upgrade = better.join("Movie.2160p.mkv");
        fs::write(&upgrade, b"better video").unwrap();
        fs::write(better.join("Movie.2160p.en.forced.srt"), b"new forced").unwrap();
        fs::write(better.join("Movie.2160p.es.srt"), b"spanish").unwrap();
        let replaced = [imported.to_str().unwrap().to_owned()];
        let upgraded = import_upgrade_file(&upgrade, &library, &request(), &replaced).unwrap();
        assert_eq!(upgraded, imported);
        let subtitles = release_subtitles(&upgrade, &neighbours(&upgrade), false);
        let (done, notes) =
            import_subtitles(&upgraded, &library, &subtitles, &active, false).unwrap();
        assert_eq!(done, [folder.join("Movie (2026).es.srt").to_str().unwrap()]);
        assert!(notes[0].starts_with("subtitle_kept: "), "{notes:?}");
        assert_eq!(
            fs::read(folder.join("Movie (2026).en.forced.srt")).unwrap(),
            b"forced"
        );
        assert_eq!(
            fs::read(folder.join("Movie (2026).fr.srt")).unwrap(),
            b"earlier french"
        );
    }

    #[test]
    fn the_runtime_check_refuses_another_film_and_accepts_a_long_cut() {
        let film = request();
        let check = |minutes: f64, runtime| check_runtime(&film, Some(minutes * 60.0), runtime);
        let error = check(22.0, Ok(120)).unwrap_err();
        assert!(error.starts_with("runtime_mismatch: "), "{error}");
        assert!(
            error.contains("22 min") && error.contains("120 min"),
            "{error}"
        );
        assert!(check(193.0, Ok(120)).is_err());
        assert!(
            check(150.0, Ok(120))
                .unwrap()
                .starts_with("runtime_checked: ")
        );
        assert!(check(84.0, Ok(120)).is_ok());
        let skipped = check(22.0, Err("no runtime is known for this film".into())).unwrap();
        assert!(skipped.starts_with("runtime_check_skipped: "));
        assert!(
            check_runtime(&film, None, Ok(120))
                .unwrap()
                .contains("duration could not be read")
        );
        let short = check_runtime(&episode("Show", 1, 1), Some(20.0 * 60.0), Ok(45)).unwrap_err();
        assert!(short.contains("the episode runs 45 min"), "{short}");
    }
}
