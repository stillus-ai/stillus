// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

use std::sync::Mutex;
use zeroize::Zeroizing;

static ACCESS: Mutex<()> = Mutex::new(());
const SERVICE: &str = "stillus/ai";

/// Errors deliberately contain neither OS error text nor secret bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CredentialError;

pub trait CredentialStore: Send + Sync {
    fn insert(&self, value: &str) -> Result<String, CredentialError>;
    fn read(&self, reference: &str) -> Result<Zeroizing<String>, CredentialError>;
    fn delete(&self, reference: &str) -> Result<(), CredentialError>;
}

pub struct SystemCredentials;

fn entry(reference: &str) -> Result<keyring::Entry, CredentialError> {
    if !reference.starts_with("ai/key/")
        || reference.len() != 39
        || !reference[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(CredentialError);
    }
    keyring::Entry::new(SERVICE, reference).map_err(|_| CredentialError)
}

impl CredentialStore for SystemCredentials {
    fn insert(&self, value: &str) -> Result<String, CredentialError> {
        let _access = ACCESS.lock().map_err(|_| CredentialError)?;
        let reference = format!("ai/key/{}", uuid::Uuid::new_v4().simple());
        let credential = entry(&reference)?;
        match credential.get_password() {
            Err(keyring::Error::NoEntry) => {}
            _ => return Err(CredentialError),
        }
        credential
            .set_password(value)
            .map_err(|_| CredentialError)?;
        Ok(reference)
    }

    fn read(&self, reference: &str) -> Result<Zeroizing<String>, CredentialError> {
        let _access = ACCESS.lock().map_err(|_| CredentialError)?;
        entry(reference)?
            .get_password()
            .map(Zeroizing::new)
            .map_err(|_| CredentialError)
    }

    fn delete(&self, reference: &str) -> Result<(), CredentialError> {
        let _access = ACCESS.lock().map_err(|_| CredentialError)?;
        match entry(reference)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(CredentialError),
        }
    }
}
