//! Errors returned to an MCP client.
//!
//! [`AppError`] is a message plus an optional hint and extra JSON fields. Tool
//! handlers serialize it with [`AppError::to_json`]. It is not a panic.

use serde_json::{Map, Value};

/// Failure an agent should see, with optional structured fields.
#[derive(Debug)]
pub struct AppError {
    /// What went wrong, safe to show to the model.
    pub error: String,
    /// A next step, when one is known.
    pub hint: Option<String>,
    /// Extra JSON merged into [`AppError::to_json`]. Header values do not belong here.
    pub extra: Map<String, Value>,
}

impl AppError {
    /// Build an error with no hint and no extra fields.
    ///
    /// # Arguments
    ///
    /// * `error` - the message stored in [`AppError::error`].
    ///
    /// # Returns
    ///
    /// A new [`AppError`].
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            hint: None,
            extra: Map::new(),
        }
    }

    /// Attach a hint and return the same error.
    ///
    /// # Arguments
    ///
    /// * `hint` - text stored in [`AppError::hint`].
    ///
    /// # Returns
    ///
    /// `self`, so callers can chain.
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Add one extra JSON field and return the same error.
    ///
    /// # Arguments
    ///
    /// * `key` - field name in the object returned by [`AppError::to_json`].
    /// * `value` - JSON value for that field.
    ///
    /// # Returns
    ///
    /// `self`, so callers can chain.
    pub fn extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.insert(key.into(), value);
        self
    }

    /// JSON object with `error`, optional `hint`, and every extra field.
    ///
    /// # Returns
    ///
    /// A JSON object. `error` overwrites an extra field of the same name.
    pub fn to_json(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert("error".into(), Value::String(self.error.clone()));
        if let Some(hint) = &self.hint {
            map.insert("hint".into(), Value::String(hint.clone()));
        }
        Value::Object(map)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for AppError {}

impl From<rusqlite::Error> for AppError {
    fn from(value: rusqlite::Error) -> Self {
        Self::new(format!("sqlite: {value}"))
    }
}

impl From<zip::result::ZipError> for AppError {
    fn from(value: zip::result::ZipError) -> Self {
        Self::new(format!("zip: {value}"))
    }
}

impl From<csv::Error> for AppError {
    fn from(value: csv::Error) -> Self {
        Self::new(format!("csv: {value}"))
    }
}

impl From<reqwest::Error> for AppError {
    fn from(value: reqwest::Error) -> Self {
        Self::new(format!("http: {value}"))
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::new(format!("io: {value}"))
    }
}

impl From<prost::DecodeError> for AppError {
    fn from(value: prost::DecodeError) -> Self {
        Self::new(format!("protobuf: {value}"))
            .hint("the bytes are not a GTFS-Realtime FeedMessage")
    }
}

/// Shortcut for [`AppError::new`].
///
/// # Arguments
///
/// * `message` - the error text.
///
/// # Returns
///
/// A new [`AppError`] with no hint.
pub fn err(message: impl Into<String>) -> AppError {
    AppError::new(message)
}
