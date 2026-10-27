use crate::ports::credentials::{
    CredentialError, CredentialRecord, CredentialRef, CredentialStore,
};
use std::sync::Arc;
use thiserror::Error;

/// Backend-only credential lifecycle boundary. It deliberately returns bounded
/// errors so platform codes and secret-bearing values cannot cross a later IPC
/// or provider boundary accidentally.
#[derive(Clone)]
pub struct CredentialService {
    store: Arc<dyn CredentialStore>,
}

impl CredentialService {
    pub fn new(store: Arc<dyn CredentialStore>) -> Self {
        Self { store }
    }

    pub fn read(
        &self,
        reference: &CredentialRef,
    ) -> Result<CredentialRecord, CredentialServiceError> {
        self.store.read(reference).map_err(Into::into)
    }

    pub fn write(
        &self,
        reference: &CredentialRef,
        credential: &CredentialRecord,
    ) -> Result<(), CredentialServiceError> {
        self.store.write(reference, credential).map_err(Into::into)
    }

    pub fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialServiceError> {
        self.store.delete(reference).map_err(Into::into)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum CredentialServiceError {
    #[error("credential input is invalid")]
    InvalidInput,
    #[error("credential was not found")]
    NotFound,
    #[error("credential service is unavailable")]
    Unavailable,
}

impl From<CredentialError> for CredentialServiceError {
    fn from(error: CredentialError) -> Self {
        match error {
            CredentialError::InvalidReference
            | CredentialError::MissingUsername
            | CredentialError::InvalidUsername
            | CredentialError::InvalidSecret => Self::InvalidInput,
            CredentialError::NotFound => Self::NotFound,
            CredentialError::Platform(_) | CredentialError::UnsupportedPlatform => {
                Self::Unavailable
            }
        }
    }
}
