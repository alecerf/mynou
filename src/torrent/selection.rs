//! Durable file interests are additive: one request cannot discard another's bytes.
use super::metainfo::Meta;
use crate::{Result, json::Value};
use std::collections::BTreeSet;

pub const MAX_SELECTED_FILES: usize = 1024;
const MAX_SELECTION_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FileSelection {
    #[default]
    All,
    Paths(BTreeSet<String>),
}

pub fn validate_selection_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.chars().any(char::is_control)
        || path.contains(['\\', ':'])
        || path.split('/').count() > 65
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | "..") || part.len() > 255)
    {
        return Err("File selection requires a relative path with normal components".into());
    }
    Ok(())
}

impl FileSelection {
    pub fn paths(paths: &[String]) -> Result<Self> {
        if paths.is_empty() || paths.len() > MAX_SELECTED_FILES {
            return Err("File selection requires 1 to 1,024 distinct paths".into());
        }
        let values: BTreeSet<_> = paths.iter().cloned().collect();
        if values.len() != paths.len() {
            return Err("Duplicate selected file path".into());
        }
        let selection = Self::Paths(values);
        selection.validate()?;
        Ok(selection)
    }

    pub fn validate(&self) -> Result<()> {
        if let Self::Paths(paths) = self {
            if paths.is_empty()
                || paths.len() > MAX_SELECTED_FILES
                || paths.iter().map(String::len).sum::<usize>() > MAX_SELECTION_BYTES
            {
                return Err("File selection exceeds its path count or 1 MiB bound".into());
            }
            for path in paths {
                validate_selection_path(path)?;
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::All => Value::Null,
            Self::Paths(paths) => Value::Array(paths.iter().cloned().map(Value::String).collect()),
        }
    }

    pub fn from_json(value: &Value) -> Result<Self> {
        match value {
            Value::Null => Ok(Self::All),
            Value::Array(paths) if paths.len() <= MAX_SELECTED_FILES => {
                let paths = paths
                    .iter()
                    .map(|path| {
                        path.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| "Selected path must be a string".to_owned())
                    })
                    .collect::<Result<Vec<_>>>()?;
                Self::paths(&paths)
            }
            _ => Err("File selection must be null or a bounded array of paths".into()),
        }
    }

    pub(super) fn merge(&self, added: &Self, meta: Option<&Meta>) -> Result<Self> {
        added.validate()?;
        if let Some(meta) = meta {
            SelectionPlan::new(meta, added)?;
        }
        match (self, added) {
            (Self::All, _) | (_, Self::All) => Ok(Self::All),
            (Self::Paths(known), Self::Paths(added)) => {
                // Authenticated metadata can prove that an unresolved old path
                // has no file and therefore no piece interest. Retain every real
                // interest while permitting an explicit correction to proceed.
                let valid: BTreeSet<_> = meta
                    .into_iter()
                    .flat_map(|meta| &meta.files)
                    .filter(|file| !file.padding)
                    .map(|file| file.path.to_string_lossy().into_owned())
                    .collect();
                let mut paths: BTreeSet<_> = known
                    .iter()
                    .filter(|path| meta.is_none() || valid.contains(*path))
                    .cloned()
                    .collect();
                paths.extend(added.iter().cloned());
                let merged = Self::Paths(paths);
                merged.validate()?;
                Ok(merged)
            }
        }
    }
}

/// Operator controls may expand an existing selection, including to all files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionUpdate {
    All,
    Indices(Vec<usize>),
}

impl SelectionUpdate {
    pub fn from_json(value: &Value) -> Result<Self> {
        let fields = value
            .as_object()
            .ok_or("Selection update must be an object")?;
        if fields.len() != 1 {
            return Err("Selection update requires exactly one of all or indices".into());
        }
        if let Some(all) = fields.get("all") {
            return if all.as_bool() == Some(true) {
                Ok(Self::All)
            } else {
                Err("Selection all must be true".into())
            };
        }
        let indices = fields
            .get("indices")
            .and_then(Value::as_array)
            .ok_or("Selection indices must be an array")?;
        if indices.is_empty() || indices.len() > MAX_SELECTED_FILES {
            return Err("Selection requires 1 to 1,024 distinct file indices".into());
        }
        let mut seen = BTreeSet::new();
        let mut parsed = Vec::with_capacity(indices.len());
        for value in indices {
            let index = value
                .as_u64()
                .and_then(|number| usize::try_from(number).ok())
                .filter(|index| *index < 100_000)
                .ok_or("Invalid selected file index")?;
            if !seen.insert(index) {
                return Err("Duplicate selected file index".into());
            }
            parsed.push(index);
        }
        Ok(Self::Indices(parsed))
    }
}

pub(super) struct SelectionPlan {
    pub files: Vec<bool>,
    pub pieces: Vec<bool>,
    pub storage: Vec<bool>,
}

impl SelectionPlan {
    pub fn new(meta: &Meta, selection: &FileSelection) -> Result<Self> {
        selection.validate()?;
        let mut remaining = match selection {
            FileSelection::All => BTreeSet::new(),
            FileSelection::Paths(paths) => paths.clone(),
        };
        let files: Vec<_> = meta
            .files
            .iter()
            .map(|file| {
                !file.padding
                    && match selection {
                        FileSelection::All => true,
                        FileSelection::Paths(_) => {
                            remaining.remove(&file.path.to_string_lossy().into_owned())
                        }
                    }
            })
            .collect();
        if !remaining.is_empty() {
            return Err(
                "Mapped torrent file is absent or ambiguous in authenticated metadata".into(),
            );
        }
        let mut pieces = vec![matches!(selection, FileSelection::All); meta.count()];
        for (file, wanted) in meta.files.iter().zip(&files) {
            if *wanted && file.length != 0 {
                let first = (file.offset / meta.piece_length as u64) as usize;
                let end = ((file.offset + file.length).div_ceil(meta.piece_length as u64)) as usize;
                pieces[first..end].fill(true);
            }
        }
        let storage = meta
            .files
            .iter()
            .zip(&files)
            .map(|(file, wanted)| {
                if file.padding {
                    return false;
                }
                if *wanted {
                    return true;
                }
                let first = (file.offset / meta.piece_length as u64) as usize;
                let end = (file.offset + file.length).div_ceil(meta.piece_length as u64) as usize;
                file.length != 0 && pieces[first..end].iter().any(|piece| *piece)
            })
            .collect();
        Ok(Self {
            files,
            pieces,
            storage,
        })
    }

    pub fn complete(&self, have: &[bool]) -> bool {
        self.pieces
            .iter()
            .zip(have)
            .all(|(wanted, have)| !*wanted || *have)
    }

    pub fn progress(&self, have: &[bool]) -> f64 {
        let total = self.pieces.iter().filter(|wanted| **wanted).count();
        if total == 0 {
            return 1.0;
        }
        self.pieces
            .iter()
            .zip(have)
            .filter(|(wanted, have)| **wanted && **have)
            .count() as f64
            / total as f64
    }
}
