//! Opt-in synthetic storage measurements, compiled only into the test harness.
use crate::json::{self, Value};
use std::{path::PathBuf, time::Instant};

pub(crate) struct Fixture {
    pub directory: PathBuf,
    pub size: u64,
    pub build: bool,
    report: Value,
}
impl Fixture {
    pub fn get(name: &str) -> Option<Self> {
        if std::env::var("MYNOU_GROWTH_CASE").ok().as_deref() != Some(name) {
            return None;
        }
        let directory = PathBuf::from(std::env::var_os("MYNOU_GROWTH_DIR").unwrap());
        let size: u64 = std::env::var("MYNOU_GROWTH_SIZE").unwrap().parse().unwrap();
        let phase = std::env::var("MYNOU_GROWTH_PHASE").unwrap();
        assert!(matches!(phase.as_str(), "build" | "read"));
        let mut report = Value::object();
        report.insert("case", name);
        report.insert("size", size.to_string());
        report.insert("phase", phase.clone());
        Some(Self {
            directory,
            size,
            build: phase == "build",
            report,
        })
    }
    pub fn measure<T>(&mut self, name: &str, operation: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = operation();
        self.report
            .insert(name, started.elapsed().as_micros().to_string());
        result
    }
    pub fn finish(self) {
        eprintln!("MYNOU_GROWTH {}", json::stringify(&self.report));
    }
}
