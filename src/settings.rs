//! Settings and secrets saved in the job store and managed through the API.
//!
//! A saved settings document is validated with the same rules before anything is written. The
//! supervised service applies it by replacing the engine generation, and keeps
//! it only once the new generation opened; other engines save it for the next
//! start. Secret values are write-only: reports carry names and sources.
use crate::{
    Result,
    config::{self, Config},
    engine::{Engine, lock},
    json::Value,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

fn document(body: &Value) -> Result<&Value> {
    body.get("settings")
        .filter(|value| value.as_object().is_some())
        .ok_or_else(|| "settings must be an object".to_owned())
}

impl Engine {
    /// Saved settings (or the defaults), the document the engine runs with,
    /// and whether a change is still applying or failed to apply.
    pub fn settings(&self) -> Result<Value> {
        let saved = lock(&self.saved_settings)?.clone();
        let pending = lock(&self.pending_settings)?
            .as_ref()
            .map(|config| config.document.clone());
        let current = pending
            .clone()
            .or(saved)
            .unwrap_or_else(config::default_json);
        let mut value = Value::object();
        value.insert(
            "restart_required",
            current.get("listen").and_then(Value::as_str) != Some(self.config.listen.as_str()),
        );
        value.insert("settings", current);
        value.insert("active", self.config.document.clone());
        value.insert("defaults", config::default_json());
        value.insert(
            "data_dir",
            self.config.data_dir.to_string_lossy().into_owned(),
        );
        value.insert("applying", pending.is_some());
        value.insert(
            "error",
            lock(&self.settings_error)?
                .clone()
                .map_or(Value::Null, Value::String),
        );
        Ok(value)
    }

    /// Validates and saves a whole settings document (`{"settings": {...}}`).
    pub fn update_settings(&self, body: &Value) -> Result<Value> {
        let document = document(body)?;
        let config = config::from_json(document, &self.config.data_dir)?;
        crate::access::require(&config.listen, lock(&self.store)?.access()?.is_some())?;
        *lock(&self.settings_error)? = None;
        let mut value = Value::object();
        if self.supervised.load(Ordering::Acquire) {
            *lock(&self.pending_settings)? = Some(config);
            value.insert("state", "applying");
        } else {
            self.save_settings(document)?;
            value.insert("state", "saved");
            value.insert("restart_required", true);
        }
        Ok(value)
    }

    /// Saves the document for the next start of this data directory.
    pub(crate) fn save_settings(&self, document: &Value) -> Result<()> {
        lock(&self.store)?.save_settings(document)?;
        *lock(&self.saved_settings)? = Some(document.clone());
        Ok(())
    }

    pub(crate) fn settings_need_reload(&self) -> bool {
        lock(&self.pending_settings).is_ok_and(|pending| pending.is_some())
    }

    pub(crate) fn take_pending_settings(&self) -> Option<Config> {
        lock(&self.pending_settings).ok()?.take()
    }

    /// Every secret the settings name, with where its value comes from.
    pub fn secrets(&self) -> Result<Value> {
        let saved = lock(&self.store)?.secrets()?;
        let document = lock(&self.saved_settings)?.clone();
        let mut documents = vec![&self.config.document];
        documents.extend(document.as_ref());
        Ok(secrets_report(&documents, &saved))
    }

    /// Saves (`{"name", "value"}`) or removes (`"value": null`) one secret.
    /// It applies at once; values are never returned.
    pub fn update_secret(&self, body: &Value) -> Result<Value> {
        let mut store = lock(&self.store)?;
        let mut secrets = store.secrets()?;
        apply_secret(&mut secrets, body)?;
        store.save_secrets(&secrets)?;
        config::install_secrets(secrets);
        drop(store);
        self.secrets()
    }
}

/// Names and sources of the secrets the documents name or the store holds.
pub fn secrets_report(documents: &[&Value], saved: &BTreeMap<String, String>) -> Value {
    let mut names: BTreeSet<String> = saved.keys().cloned().collect();
    for document in documents {
        names.extend(config::secret_names(document));
    }
    let mut value = Value::object();
    value.insert(
        "secrets",
        Value::Array(
            names
                .iter()
                .map(|name| {
                    let mut entry = Value::object();
                    entry.insert("name", name.as_str());
                    entry.insert(
                        "source",
                        if saved.contains_key(name) {
                            "stored"
                        } else {
                            config::source(name)
                        },
                    );
                    entry
                })
                .collect(),
        ),
    );
    value
}

/// Applies one `{"name", "value"}` update (a `null` value removes it).
pub fn apply_secret(secrets: &mut BTreeMap<String, String>, body: &Value) -> Result<()> {
    let map = body.as_object().ok_or("Expected a JSON object")?;
    if map
        .keys()
        .any(|key| !matches!(key.as_str(), "name" | "value"))
    {
        return Err("Secret updates accept only name and value".into());
    }
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or("name is required")?;
    config::check_secret_name(name)?;
    match body.get("value") {
        Some(Value::Null) => {
            secrets.remove(name);
        }
        Some(Value::String(value)) => {
            config::check_secret_value(value)?;
            secrets.insert(name.into(), value.clone());
            if secrets.len() > config::MAX_SECRETS {
                return Err("At most 256 secrets can be stored".into());
            }
        }
        _ => return Err("value must be a string, or null to remove the secret".into()),
    }
    Ok(())
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod tests;
