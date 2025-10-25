//! Error types for the zenoh-backend-redb storage backend.

/// Result type alias for operations that may fail with a RedbBackendError.
pub type Result<T> = std::result::Result<T, RedbBackendError>;

/// Error types for the redb storage backend.
#[derive(Debug, thiserror::Error)]
pub enum RedbBackendError {
    /// Error during database operations.
    #[error("Database error: {0}")]
    DatabaseError(Box<redb::DatabaseError>),

    /// Error during table operations.
    #[error("Table error: {0}")]
    TableError(Box<redb::TableError>),

    /// Error during storage operations.
    #[error("Storage error: {0}")]
    StorageError(Box<redb::StorageError>),

    /// Error during commit operations.
    #[error("Commit error: {0}")]
    CommitError(Box<redb::CommitError>),

    /// Error during transaction operations.
    #[error("Transaction error: {0}")]
    TransactionError(Box<redb::TransactionError>),

    /// Configuration error.
    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// Serialization/deserialization error.
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Key encoding error.
    #[error("Key encoding error: {0}")]
    KeyEncodingError(String),

    /// Value encoding error.
    #[error("Value encoding error: {0}")]
    ValueEncodingError(String),

    /// Invalid key expression.
    #[error("Invalid key expression: {0}")]
    InvalidKeyExpression(String),

    /// Storage not found.
    #[error("Storage not found: {0}")]
    StorageNotFound(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// Zenoh error.
    #[error("Zenoh error: {0}")]
    ZenohError(String),

    /// Generic error with custom message.
    #[error("{0}")]
    Other(String),
}

impl RedbBackendError {
    /// Create a configuration error.
    pub fn config<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::ConfigError(msg.into())
    }

    /// Create a serialization error.
    pub fn serialization<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::SerializationError(msg.into())
    }

    /// Create a key encoding error.
    pub fn key_encoding<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::KeyEncodingError(msg.into())
    }

    /// Create a value encoding error.
    pub fn value_encoding<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::ValueEncodingError(msg.into())
    }

    /// Create an invalid key expression error.
    pub fn invalid_key<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::InvalidKeyExpression(msg.into())
    }

    /// Create a storage not found error.
    pub fn storage_not_found<S: Into<String>>(name: S) -> Self {
        RedbBackendError::StorageNotFound(name.into())
    }

    /// Create a Zenoh error.
    pub fn zenoh<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::ZenohError(msg.into())
    }

    /// Create a generic error.
    pub fn other<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::Other(msg.into())
    }
}

// Convert from serde_json errors
impl From<serde_json::Error> for RedbBackendError {
    fn from(err: serde_json::Error) -> Self {
        RedbBackendError::SerializationError(err.to_string())
    }
}

// Convert from zenoh errors
impl From<zenoh::Error> for RedbBackendError {
    fn from(err: zenoh::Error) -> Self {
        RedbBackendError::ZenohError(err.to_string())
    }
}

// Manual From implementations for boxed redb errors
impl From<redb::DatabaseError> for RedbBackendError {
    fn from(err: redb::DatabaseError) -> Self {
        RedbBackendError::DatabaseError(Box::new(err))
    }
}

impl From<redb::TableError> for RedbBackendError {
    fn from(err: redb::TableError) -> Self {
        RedbBackendError::TableError(Box::new(err))
    }
}

impl From<redb::StorageError> for RedbBackendError {
    fn from(err: redb::StorageError) -> Self {
        RedbBackendError::StorageError(Box::new(err))
    }
}

impl From<redb::CommitError> for RedbBackendError {
    fn from(err: redb::CommitError) -> Self {
        RedbBackendError::CommitError(Box::new(err))
    }
}

impl From<redb::TransactionError> for RedbBackendError {
    fn from(err: redb::TransactionError) -> Self {
        RedbBackendError::TransactionError(Box::new(err))
    }
}
