//! Progress of storage migrations for the command line and `GET /api/status`.
//!
//! Each migration of an earlier storage format reports its stage and how many
//! records it has written out of how many it knows of. Reports name only the
//! store and stage: never a path, title or identifier. Entries are kept per
//! storage directory, so one process can open several stores independently.
use crate::json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Retained entries; the oldest finished ones are forgotten first.
const MAX_ENTRIES: usize = 64;
/// The reporter sees a stage change at once and record counts at this pace.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// The public state of one migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    /// Which store: `jobs`, `series`, `requesters`, `irc`, `feeds`, `indexers`
    /// or `transfers`, or one of the job index upgrades `library_schedule`
    /// and `upgrade_children`.
    pub store: &'static str,
    /// `validating`, `preserving`, `copying`, `indexing`, `verifying`,
    /// `promoting` or `complete`, or `failed` when the migration stopped with
    /// an error.
    pub stage: &'static str,
    pub done: u64,
    pub total: Option<u64>,
    /// The migration continued the staged work of an interrupted attempt.
    pub resumed: bool,
}

impl Migration {
    pub fn to_json(&self) -> Value {
        let mut value = Value::object();
        value.insert("store", self.store);
        value.insert("stage", self.stage);
        value.insert("done", Value::Number(self.done as f64));
        value.insert(
            "total",
            self.total
                .map_or(Value::Null, |total| Value::Number(total as f64)),
        );
        value.insert("resumed", self.resumed);
        value
    }

    /// One line for a terminal.
    pub fn describe(&self) -> String {
        let count = match self.total {
            Some(total) => format!("{} of {total} records", self.done),
            None => format!("{} records", self.done),
        };
        format!(
            "migrating {} storage: {} ({count}){}",
            self.store,
            self.stage,
            if self.resumed { ", resumed" } else { "" }
        )
    }

    fn finished(&self) -> bool {
        matches!(self.stage, "complete" | "failed")
    }
}

struct Entry {
    directory: PathBuf,
    migration: Migration,
    reported: Option<(&'static str, Instant)>,
}

struct Registry {
    entries: Vec<Entry>,
    reporter: Option<fn(&Migration)>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    entries: Vec::new(),
    reporter: None,
});

/// Receives each stage change, and record counts about once a second. The
/// command line prints them; the service also exposes them in its status.
pub fn set_reporter(reporter: fn(&Migration)) {
    if let Ok(mut registry) = REGISTRY.lock() {
        registry.reporter = Some(reporter);
    }
}

/// Migrations of the stores in `directories` started by this process, in order.
pub fn migrations(directories: &[&Path]) -> Vec<Migration> {
    REGISTRY.lock().map_or_else(
        |_| Vec::new(),
        |registry| {
            registry
                .entries
                .iter()
                .filter(|entry| directories.contains(&entry.directory.as_path()))
                .map(|entry| entry.migration.clone())
                .collect()
        },
    )
}

/// The `migrations` array of a status report.
pub fn report(directories: &[&Path]) -> Value {
    Value::Array(
        migrations(directories)
            .iter()
            .map(Migration::to_json)
            .collect(),
    )
}

/// Tracks one running migration. Dropping it before `finish` records the
/// migration as failed at its last stage count, ready to resume.
pub struct Tracker {
    directory: PathBuf,
    store: &'static str,
    finished: bool,
    /// False when there is nothing to migrate, such as a first start that
    /// creates an empty index: such a tracker reports nothing.
    active: bool,
}

impl Tracker {
    pub fn start(directory: &Path, store: &'static str, total: Option<u64>) -> Self {
        Self::start_if(true, directory, store, total)
    }

    /// Tracks the migration only when `legacy` data exists to migrate.
    pub fn start_if(
        legacy: bool,
        directory: &Path,
        store: &'static str,
        total: Option<u64>,
    ) -> Self {
        let tracker = Self {
            directory: directory.to_path_buf(),
            store,
            finished: false,
            active: legacy,
        };
        tracker.update(|migration| {
            *migration = Migration {
                store,
                stage: "validating",
                done: 0,
                total,
                resumed: false,
            };
        });
        tracker
    }

    pub fn total(&self, total: u64) {
        self.update(|migration| migration.total = Some(total));
    }

    pub fn resumed(&self) {
        self.update(|migration| migration.resumed = true);
    }

    pub fn stage(&self, stage: &'static str, done: u64) {
        self.update(|migration| {
            migration.stage = stage;
            migration.done = done;
        });
    }

    pub fn finish(mut self) {
        self.finished = true;
        self.update(|migration| {
            migration.stage = "complete";
            if let Some(total) = migration.total {
                migration.done = total;
            }
        });
    }

    fn update(&self, change: impl FnOnce(&mut Migration)) {
        if !self.active {
            return;
        }
        let Ok(mut registry) = REGISTRY.lock() else {
            return;
        };
        let position = registry
            .entries
            .iter()
            .position(|entry| {
                entry.directory == self.directory
                    && entry.migration.store == self.store
                    && !entry.migration.finished()
            })
            .unwrap_or_else(|| {
                if registry.entries.len() >= MAX_ENTRIES {
                    let oldest = registry
                        .entries
                        .iter()
                        .position(|entry| entry.migration.finished())
                        .unwrap_or(0);
                    registry.entries.remove(oldest);
                }
                registry.entries.push(Entry {
                    directory: self.directory.clone(),
                    migration: Migration {
                        store: self.store,
                        stage: "validating",
                        done: 0,
                        total: None,
                        resumed: false,
                    },
                    reported: None,
                });
                registry.entries.len() - 1
            });
        let reporter = registry.reporter;
        let entry = &mut registry.entries[position];
        change(&mut entry.migration);
        let due = entry.reported.is_none_or(|(stage, at)| {
            stage != entry.migration.stage || at.elapsed() >= REPORT_INTERVAL
        });
        if due {
            entry.reported = Some((entry.migration.stage, Instant::now()));
            let migration = entry.migration.clone();
            drop(registry);
            if let Some(reporter) = reporter {
                reporter(&migration);
            }
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        if !self.finished {
            self.update(|migration| migration.stage = "failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_migration_reports_its_stages_counts_and_failure_per_directory() {
        let directory = std::env::temp_dir().join("mynou-progress-unit-a");
        let other = std::env::temp_dir().join("mynou-progress-unit-b");
        let tracker = Tracker::start(&directory, "jobs", Some(130));
        tracker.resumed();
        tracker.stage("copying", 64);
        let running = migrations(&[&directory]);
        assert_eq!(
            running,
            vec![Migration {
                store: "jobs",
                stage: "copying",
                done: 64,
                total: Some(130),
                resumed: true,
            }]
        );
        assert!(migrations(&[&other]).is_empty());
        assert_eq!(
            running[0].describe(),
            "migrating jobs storage: copying (64 of 130 records), resumed"
        );
        tracker.finish();
        assert_eq!(migrations(&[&directory])[0].stage, "complete");
        assert_eq!(migrations(&[&directory])[0].done, 130);
        // A failed attempt stays visible beside the completed one.
        let failing = Tracker::start(&other, "series", None);
        failing.stage("copying", 3);
        drop(failing);
        let failed = migrations(&[&other]);
        assert_eq!(failed.len(), 1);
        assert_eq!((failed[0].stage, failed[0].done), ("failed", 3));
        // Creating an empty index of a fresh store is not a migration.
        let idle = Tracker::start_if(false, &other, "feeds", Some(0));
        idle.stage("copying", 0);
        drop(idle);
        assert_eq!(migrations(&[&other]).len(), 1);
        let json = crate::json::stringify(&report(&[&other]));
        assert_eq!(
            json,
            r#"[{"done":3,"resumed":false,"stage":"failed","store":"series","total":null}]"#
        );
    }
}
