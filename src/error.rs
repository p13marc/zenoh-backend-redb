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

    /// Error setting the durability level on a write transaction.
    #[error("Durability error: {0}")]
    SetDurabilityError(Box<redb::SetDurabilityError>),

    /// Error compacting the database file.
    #[error("Compaction error: {0}")]
    CompactionError(Box<redb::CompactionError>),

    /// Serialization/deserialization error.
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Key encoding error.
    #[error("Key encoding error: {0}")]
    KeyEncodingError(String),

    /// I/O error.
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// Generic error with custom message.
    #[error("{0}")]
    Other(String),
}

impl RedbBackendError {
    /// Create a serialization error.
    pub fn serialization<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::SerializationError(msg.into())
    }

    /// Create a key encoding error.
    pub fn key_encoding<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::KeyEncodingError(msg.into())
    }

    /// Create a generic error.
    pub fn other<S: Into<String>>(msg: S) -> Self {
        RedbBackendError::Other(msg.into())
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

impl From<redb::SetDurabilityError> for RedbBackendError {
    fn from(err: redb::SetDurabilityError) -> Self {
        RedbBackendError::SetDurabilityError(Box::new(err))
    }
}

impl From<redb::CompactionError> for RedbBackendError {
    fn from(err: redb::CompactionError) -> Self {
        RedbBackendError::CompactionError(Box::new(err))
    }
}
