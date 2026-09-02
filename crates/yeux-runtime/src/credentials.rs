//! Opaque credential leases for runtime-owned network/provider adapters.
//!
//! A tool receives only a handle in its effect set.  The broker is the one
//! component allowed to resolve that handle, and the resolved value is held in
//! a non-serializable lease whose debug representation is deliberately
//! redacted.

use std::{collections::BTreeMap, sync::RwLock};

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential handle is unavailable: {0}")]
    Unavailable(String),
    #[error("credential broker is unavailable")]
    BrokerUnavailable,
}

/// A short-lived secret value.  It cannot be serialized, displayed, or
/// compared to a string accidentally.  Consumers must use [`Self::with_value`]
/// for the narrow operation that needs the secret.
pub struct CredentialLease(String);

impl CredentialLease {
    /// Construct a lease inside a broker implementation. Callers should keep
    /// the value in memory only for the duration of the broker operation.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn with_value<T>(&self, operation: impl FnOnce(&str) -> T) -> T {
        operation(&self.0)
    }
}

impl std::fmt::Debug for CredentialLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialLease(REDACTED)")
    }
}

/// Authority for resolving opaque secret handles.  Tools should never receive
/// the returned lease; only provider/network adapters owned by the runtime may
/// request one.
#[async_trait]
pub trait CredentialBroker: Send + Sync {
    async fn resolve(&self, handle: &str) -> Result<CredentialLease, CredentialError>;
}

#[derive(Debug, Default)]
pub struct NoCredentials;

#[async_trait]
impl CredentialBroker for NoCredentials {
    async fn resolve(&self, handle: &str) -> Result<CredentialLease, CredentialError> {
        Err(CredentialError::Unavailable(handle.to_owned()))
    }
}

/// In-memory broker useful for local development and deterministic fixtures.
/// The map is never exposed through `Debug` or serialization.
#[derive(Default)]
pub struct InMemoryCredentialBroker {
    values: RwLock<BTreeMap<String, String>>,
}

impl std::fmt::Debug for InMemoryCredentialBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InMemoryCredentialBroker(REDACTED)")
    }
}

impl InMemoryCredentialBroker {
    pub fn insert(&self, handle: impl Into<String>, value: impl Into<String>) {
        if let Ok(mut values) = self.values.write() {
            values.insert(handle.into(), value.into());
        }
    }
}

#[async_trait]
impl CredentialBroker for InMemoryCredentialBroker {
    async fn resolve(&self, handle: &str) -> Result<CredentialLease, CredentialError> {
        let value = self
            .values
            .read()
            .map_err(|_| CredentialError::BrokerUnavailable)?
            .get(handle)
            .cloned()
            .ok_or_else(|| CredentialError::Unavailable(handle.to_owned()))?;
        Ok(CredentialLease::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn leases_are_redacted_and_resolve_by_handle() {
        let broker = InMemoryCredentialBroker::default();
        broker.insert("provider-key", "super-secret");
        let lease = broker.resolve("provider-key").await.unwrap();
        assert!(!format!("{lease:?}").contains("super-secret"));
        assert_eq!(lease.with_value(str::to_owned), "super-secret");
    }

    #[tokio::test]
    async fn missing_handles_fail_closed() {
        let error = NoCredentials.resolve("missing").await.unwrap_err();
        assert!(matches!(error, CredentialError::Unavailable(handle) if handle == "missing"));
    }
}
