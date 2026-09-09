// Copyright 2026 Evgeniy Udodov
// SPDX-License-Identifier: GPL-3.0-only

#![forbid(unsafe_code)]

//! Streaming, passphrase-protected Stillus containers inside the age v1 format.

use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::iter;

use age::secrecy::{ExposeSecret, SecretString};
use rand::RngCore;
use rand::rngs::OsRng;

pub const AGE_PREFIX: &[u8] = b"age-encryption.org/v1\n";
pub const ARMORED_AGE_PREFIX: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----\n";
pub const ARMORED_AGE_CRLF_PREFIX: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----\r\n";
pub const SCRYPT_LOG_N: u8 = 18;
pub const MAX_ORIGINAL_FILENAME_BYTES: usize = 255;
pub const MAX_PAYLOAD_BYTES: u64 = 1 << 40;

const APP_MAGIC: &[u8; 8] = b"NTRMSEC1";
const BODY_APP_MAGIC: &[u8; 8] = b"NTRMSEC2";
const PAYLOAD_VERSION: u8 = 1;
const FIXED_HEADER_BYTES: usize = APP_MAGIC.len() + 1 + 1 + 2 + 8;
const BODY_FIXED_HEADER_BYTES: usize = BODY_APP_MAGIC.len() + 1 + 1 + 8;
const BODY_KIND: u8 = 1;
#[cfg(any(test, feature = "test-utils"))]
const TEST_SCRYPT_LOG_N: u8 = 2;
const NEUTRAL_ERROR: &str = "secure envelope operation failed";

#[derive(Clone)]
pub struct MasterPassword(SecretString);

impl MasterPassword {
    pub fn new(value: String) -> Self {
        Self(SecretString::from(value))
    }

    fn secret(&self) -> SecretString {
        self.0.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.0.expose_secret().is_empty()
    }

    pub fn same_secret(&self, other: &Self) -> bool {
        self.0.expose_secret().as_bytes() == other.0.expose_secret().as_bytes()
    }
}

impl From<String> for MasterPassword {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvelopeKind {
    Note,
    Recovery,
    WorkspaceVerifier,
    EngineSecret,
}

impl EnvelopeKind {
    fn encode(self) -> u8 {
        match self {
            Self::Note => 1,
            Self::Recovery => 2,
            Self::WorkspaceVerifier => 3,
            Self::EngineSecret => 4,
        }
    }

    fn decode(value: u8) -> Result<Self, SecureError> {
        match value {
            1 => Ok(Self::Note),
            2 => Ok(Self::Recovery),
            3 => Ok(Self::WorkspaceVerifier),
            4 => Ok(Self::EngineSecret),
            _ => Err(SecureError),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct EnvelopeMetadata {
    pub kind: EnvelopeKind,
    pub original_filename: String,
    pub payload_len: u64,
}

impl EnvelopeMetadata {
    pub fn new(
        kind: EnvelopeKind,
        original_filename: String,
        payload_len: u64,
    ) -> Result<Self, SecureError> {
        validate_original_filename(&original_filename)?;
        if payload_len > MAX_PAYLOAD_BYTES {
            return Err(SecureError);
        }
        Ok(Self {
            kind,
            original_filename,
            payload_len,
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct SecureError;

impl fmt::Debug for SecureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecureError")
    }
}

impl fmt::Display for SecureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(NEUTRAL_ERROR)
    }
}

impl std::error::Error for SecureError {}

pub struct EnvelopeWriter<W: Write> {
    inner: age::stream::StreamWriter<W>,
    remaining: u64,
}

impl<W: Write> EnvelopeWriter<W> {
    pub fn new(
        output: W,
        password: &MasterPassword,
        metadata: EnvelopeMetadata,
    ) -> Result<Self, SecureError> {
        Self::with_work_factor(output, password, metadata, SCRYPT_LOG_N)
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn new_for_test(
        output: W,
        password: &MasterPassword,
        metadata: EnvelopeMetadata,
    ) -> Result<Self, SecureError> {
        Self::with_work_factor(output, password, metadata, TEST_SCRYPT_LOG_N)
    }

    fn with_work_factor(
        output: W,
        password: &MasterPassword,
        metadata: EnvelopeMetadata,
        work_factor: u8,
    ) -> Result<Self, SecureError> {
        if password.is_empty() {
            return Err(SecureError);
        }
        validate_original_filename(&metadata.original_filename)?;
        if metadata.payload_len > MAX_PAYLOAD_BYTES {
            return Err(SecureError);
        }

        let mut recipient = age::scrypt::Recipient::new(password.secret());
        recipient.set_work_factor(work_factor);
        let encryptor =
            age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
                .map_err(|_| SecureError)?;
        let mut inner = encryptor.wrap_output(output).map_err(|_| SecureError)?;
        write_app_header(&mut inner, &metadata).map_err(|_| SecureError)?;

        Ok(Self {
            inner,
            remaining: metadata.payload_len,
        })
    }

    pub fn finish(self) -> Result<W, SecureError> {
        if self.remaining != 0 {
            return Err(SecureError);
        }
        self.inner.finish().map_err(|_| SecureError)
    }
}

impl<W: Write> Write for EnvelopeWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buffer.len()).map_err(|_| neutral_io_error())?;
        if length > self.remaining {
            return Err(neutral_io_error());
        }
        let written = self.inner.write(buffer).map_err(|_| neutral_io_error())?;
        self.remaining = self.remaining.saturating_sub(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().map_err(|_| neutral_io_error())
    }
}

// Normalize only the ASCII armor emitted by age, never plaintext or binary age
// data. age uses the host line ending; Stillus writes portable LF on every OS.
struct LfArmorOutput<W>(W);

impl<W: Write> Write for LfArmorOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for part in bytes.split(|byte| *byte == b'\r') {
            self.0.write_all(part)?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// Streaming ASCII-armored variant used by canonical workspace security
/// files. It retains the generic authenticated metadata header while keeping
/// the on-disk representation inspectable as an age envelope.
pub struct ArmoredEnvelopeWriter<W: Write> {
    inner: EnvelopeWriter<age::armor::ArmoredWriter<LfArmorOutput<W>>>,
}

impl<W: Write> ArmoredEnvelopeWriter<W> {
    pub fn new(
        output: W,
        password: &MasterPassword,
        metadata: EnvelopeMetadata,
    ) -> Result<Self, SecureError> {
        Self::with_work_factor(output, password, metadata, SCRYPT_LOG_N)
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn new_for_test(
        output: W,
        password: &MasterPassword,
        metadata: EnvelopeMetadata,
    ) -> Result<Self, SecureError> {
        Self::with_work_factor(output, password, metadata, TEST_SCRYPT_LOG_N)
    }

    fn with_work_factor(
        output: W,
        password: &MasterPassword,
        metadata: EnvelopeMetadata,
        work_factor: u8,
    ) -> Result<Self, SecureError> {
        let armor = age::armor::ArmoredWriter::wrap_output(
            LfArmorOutput(output),
            age::armor::Format::AsciiArmor,
        )
        .map_err(|_| SecureError)?;
        Ok(Self {
            inner: EnvelopeWriter::with_work_factor(armor, password, metadata, work_factor)?,
        })
    }

    pub fn finish(self) -> Result<W, SecureError> {
        self.inner
            .finish()?
            .finish()
            .map(|output| output.0)
            .map_err(|_| SecureError)
    }
}

impl<W: Write> Write for ArmoredEnvelopeWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct EnvelopeReader<R: Read> {
    metadata: EnvelopeMetadata,
    inner: age::stream::StreamReader<R>,
    remaining: u64,
    authenticated_eof: bool,
}

impl<R: Read> EnvelopeReader<R> {
    pub fn metadata(&self) -> &EnvelopeMetadata {
        &self.metadata
    }

    fn authenticate_eof(&mut self) -> io::Result<()> {
        if self.authenticated_eof {
            return Ok(());
        }
        let mut trailing = [0_u8; 1];
        match self.inner.read(&mut trailing) {
            Ok(0) => {
                self.authenticated_eof = true;
                Ok(())
            }
            Ok(_) | Err(_) => Err(neutral_io_error()),
        }
    }
}

impl<R: Read> Read for EnvelopeReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.authenticate_eof()?;
            return Ok(0);
        }

        let requested = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let final_read = requested as u64 == self.remaining;
        if final_read {
            let mut staged = vec![0_u8; requested];
            let read = self
                .inner
                .read(&mut staged)
                .map_err(|_| neutral_io_error())?;
            if read == 0 {
                return Err(neutral_io_error());
            }
            self.remaining -= read as u64;
            if self.remaining == 0 {
                self.authenticate_eof()?;
            }
            buffer[..read].copy_from_slice(&staged[..read]);
            Ok(read)
        } else {
            let read = self
                .inner
                .read(&mut buffer[..requested])
                .map_err(|_| neutral_io_error())?;
            if read == 0 {
                return Err(neutral_io_error());
            }
            self.remaining -= read as u64;
            Ok(read)
        }
    }
}

pub fn decrypt<R: Read>(
    input: R,
    password: &MasterPassword,
    expected_kind: EnvelopeKind,
) -> Result<EnvelopeReader<R>, SecureError> {
    if password.is_empty() {
        return Err(SecureError);
    }
    let decryptor = age::Decryptor::new(input).map_err(|_| SecureError)?;
    if !decryptor.is_scrypt() {
        return Err(SecureError);
    }
    let mut identity = age::scrypt::Identity::new(password.secret());
    identity.set_max_work_factor(SCRYPT_LOG_N);
    let mut inner = decryptor
        .decrypt(iter::once(&identity as &dyn age::Identity))
        .map_err(|_| SecureError)?;
    let metadata = read_app_header(&mut inner)?;
    if metadata.kind != expected_kind {
        return Err(SecureError);
    }
    let remaining = metadata.payload_len;
    let mut reader = EnvelopeReader {
        metadata,
        inner,
        remaining,
        authenticated_eof: false,
    };
    if remaining == 0 {
        reader.authenticate_eof().map_err(|_| SecureError)?;
    }
    Ok(reader)
}

pub fn decrypt_armored<R: Read>(
    input: R,
    password: &MasterPassword,
    expected_kind: EnvelopeKind,
) -> Result<EnvelopeReader<age::armor::ArmoredReader<BufReader<R>>>, SecureError> {
    decrypt(
        age::armor::ArmoredReader::new(input),
        password,
        expected_kind,
    )
}

/// Streaming ASCII-armored age envelope for the Markdown body of a protected
/// note. The YAML front matter is deliberately written outside this envelope.
pub struct BodyEnvelopeWriter<W: Write> {
    inner: age::stream::StreamWriter<age::armor::ArmoredWriter<LfArmorOutput<W>>>,
    remaining: u64,
}

impl<W: Write> BodyEnvelopeWriter<W> {
    pub fn new(output: W, password: &MasterPassword, body_len: u64) -> Result<Self, SecureError> {
        Self::with_work_factor(output, password, body_len, SCRYPT_LOG_N)
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn new_for_test(
        output: W,
        password: &MasterPassword,
        body_len: u64,
    ) -> Result<Self, SecureError> {
        Self::with_work_factor(output, password, body_len, TEST_SCRYPT_LOG_N)
    }

    fn with_work_factor(
        output: W,
        password: &MasterPassword,
        body_len: u64,
        work_factor: u8,
    ) -> Result<Self, SecureError> {
        if password.is_empty() || body_len > MAX_PAYLOAD_BYTES {
            return Err(SecureError);
        }
        let armor = age::armor::ArmoredWriter::wrap_output(
            LfArmorOutput(output),
            age::armor::Format::AsciiArmor,
        )
        .map_err(|_| SecureError)?;
        let mut recipient = age::scrypt::Recipient::new(password.secret());
        recipient.set_work_factor(work_factor);
        let encryptor =
            age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
                .map_err(|_| SecureError)?;
        let mut inner = encryptor.wrap_output(armor).map_err(|_| SecureError)?;
        write_body_header(&mut inner, body_len).map_err(|_| SecureError)?;
        Ok(Self {
            inner,
            remaining: body_len,
        })
    }

    pub fn finish(self) -> Result<W, SecureError> {
        if self.remaining != 0 {
            return Err(SecureError);
        }
        self.inner
            .finish()
            .map_err(|_| SecureError)?
            .finish()
            .map(|output| output.0)
            .map_err(|_| SecureError)
    }
}

impl<W: Write> Write for BodyEnvelopeWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buffer.len()).map_err(|_| neutral_io_error())?;
        if length > self.remaining {
            return Err(neutral_io_error());
        }
        let written = self.inner.write(buffer).map_err(|_| neutral_io_error())?;
        self.remaining = self.remaining.saturating_sub(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().map_err(|_| neutral_io_error())
    }
}

pub struct BodyEnvelopeReader<R: Read> {
    inner: age::stream::StreamReader<age::armor::ArmoredReader<BufReader<R>>>,
    body_len: u64,
    remaining: u64,
    authenticated_eof: bool,
}

impl<R: Read> BodyEnvelopeReader<R> {
    pub fn body_len(&self) -> u64 {
        self.body_len
    }

    fn authenticate_eof(&mut self) -> io::Result<()> {
        if self.authenticated_eof {
            return Ok(());
        }
        let mut trailing = [0_u8; 1];
        match self.inner.read(&mut trailing) {
            Ok(0) => {
                self.authenticated_eof = true;
                Ok(())
            }
            Ok(_) | Err(_) => Err(neutral_io_error()),
        }
    }
}

impl<R: Read> Read for BodyEnvelopeReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.authenticate_eof()?;
            return Ok(0);
        }
        let requested = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let final_read = requested as u64 == self.remaining;
        if final_read {
            let mut staged = vec![0_u8; requested];
            let read = self
                .inner
                .read(&mut staged)
                .map_err(|_| neutral_io_error())?;
            if read == 0 {
                return Err(neutral_io_error());
            }
            self.remaining -= read as u64;
            if self.remaining == 0 {
                self.authenticate_eof()?;
            }
            buffer[..read].copy_from_slice(&staged[..read]);
            Ok(read)
        } else {
            let read = self
                .inner
                .read(&mut buffer[..requested])
                .map_err(|_| neutral_io_error())?;
            if read == 0 {
                return Err(neutral_io_error());
            }
            self.remaining -= read as u64;
            Ok(read)
        }
    }
}

pub fn decrypt_body<R: Read>(
    input: R,
    password: &MasterPassword,
) -> Result<BodyEnvelopeReader<R>, SecureError> {
    if password.is_empty() {
        return Err(SecureError);
    }
    let armored = age::armor::ArmoredReader::new(input);
    let decryptor = age::Decryptor::new(armored).map_err(|_| SecureError)?;
    if !decryptor.is_scrypt() {
        return Err(SecureError);
    }
    let mut identity = age::scrypt::Identity::new(password.secret());
    identity.set_max_work_factor(SCRYPT_LOG_N);
    let mut inner = decryptor
        .decrypt(iter::once(&identity as &dyn age::Identity))
        .map_err(|_| SecureError)?;
    let remaining = read_body_header(&mut inner)?;
    let mut reader = BodyEnvelopeReader {
        inner,
        body_len: remaining,
        remaining,
        authenticated_eof: false,
    };
    if remaining == 0 {
        reader.authenticate_eof().map_err(|_| SecureError)?;
    }
    Ok(reader)
}

pub fn is_armored_age_prefix(prefix: &[u8]) -> bool {
    prefix.starts_with(ARMORED_AGE_PREFIX) || prefix.starts_with(ARMORED_AGE_CRLF_PREFIX)
}

pub fn is_age_prefix(prefix: &[u8]) -> bool {
    prefix.starts_with(AGE_PREFIX)
}

/// Validates the public age header without attempting to open the encrypted
/// file key. This distinguishes a legacy binary age envelope from Markdown
/// that merely begins with the age magic line.
pub fn is_scrypt_age_envelope(input: impl Read) -> bool {
    age::Decryptor::new(input).is_ok_and(|decryptor| decryptor.is_scrypt())
}

pub fn opaque_note_filename() -> Result<String, SecureError> {
    let mut random = [0_u8; 16];
    OsRng.try_fill_bytes(&mut random).map_err(|_| SecureError)?;
    let mut output = String::with_capacity(40);
    output.push_str("ntrm-");
    for byte in random {
        use fmt::Write as _;
        write!(&mut output, "{byte:02x}").map_err(|_| SecureError)?;
    }
    output.push_str(".md");
    Ok(output)
}

fn write_app_header(writer: &mut impl Write, metadata: &EnvelopeMetadata) -> io::Result<()> {
    let filename = metadata.original_filename.as_bytes();
    let filename_len = u16::try_from(filename.len()).map_err(|_| neutral_io_error())?;
    let mut fixed = [0_u8; FIXED_HEADER_BYTES];
    fixed[..APP_MAGIC.len()].copy_from_slice(APP_MAGIC);
    fixed[8] = PAYLOAD_VERSION;
    fixed[9] = metadata.kind.encode();
    fixed[10..12].copy_from_slice(&filename_len.to_le_bytes());
    fixed[12..20].copy_from_slice(&metadata.payload_len.to_le_bytes());
    writer.write_all(&fixed)?;
    writer.write_all(filename)
}

fn write_body_header(writer: &mut impl Write, body_len: u64) -> io::Result<()> {
    let mut fixed = [0_u8; BODY_FIXED_HEADER_BYTES];
    fixed[..BODY_APP_MAGIC.len()].copy_from_slice(BODY_APP_MAGIC);
    fixed[8] = PAYLOAD_VERSION;
    fixed[9] = BODY_KIND;
    fixed[10..18].copy_from_slice(&body_len.to_le_bytes());
    writer.write_all(&fixed)
}

fn read_body_header(reader: &mut impl Read) -> Result<u64, SecureError> {
    let mut fixed = [0_u8; BODY_FIXED_HEADER_BYTES];
    reader.read_exact(&mut fixed).map_err(|_| SecureError)?;
    if &fixed[..BODY_APP_MAGIC.len()] != BODY_APP_MAGIC
        || fixed[8] != PAYLOAD_VERSION
        || fixed[9] != BODY_KIND
    {
        return Err(SecureError);
    }
    let body_len = u64::from_le_bytes(fixed[10..18].try_into().map_err(|_| SecureError)?);
    if body_len > MAX_PAYLOAD_BYTES {
        return Err(SecureError);
    }
    Ok(body_len)
}

fn read_app_header(reader: &mut impl Read) -> Result<EnvelopeMetadata, SecureError> {
    let mut fixed = [0_u8; FIXED_HEADER_BYTES];
    reader.read_exact(&mut fixed).map_err(|_| SecureError)?;
    if &fixed[..APP_MAGIC.len()] != APP_MAGIC || fixed[8] != PAYLOAD_VERSION {
        return Err(SecureError);
    }
    let kind = EnvelopeKind::decode(fixed[9])?;
    let filename_len = usize::from(u16::from_le_bytes([fixed[10], fixed[11]]));
    if filename_len == 0 || filename_len > MAX_ORIGINAL_FILENAME_BYTES {
        return Err(SecureError);
    }
    let payload_len = u64::from_le_bytes(fixed[12..20].try_into().map_err(|_| SecureError)?);
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(SecureError);
    }
    let mut filename = vec![0_u8; filename_len];
    reader.read_exact(&mut filename).map_err(|_| SecureError)?;
    let original_filename = String::from_utf8(filename).map_err(|_| SecureError)?;
    EnvelopeMetadata::new(kind, original_filename, payload_len)
}

fn validate_original_filename(value: &str) -> Result<(), SecureError> {
    if value.is_empty()
        || value.len() > MAX_ORIGINAL_FILENAME_BYTES
        || matches!(value, "." | "..")
        || value
            .chars()
            .any(|character| character == '\0' || matches!(character, '/' | '\\'))
    {
        return Err(SecureError);
    }
    Ok(())
}

fn neutral_io_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, NEUTRAL_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn password(value: &str) -> MasterPassword {
        MasterPassword::new(value.to_owned())
    }

    fn metadata(kind: EnvelopeKind, filename: &str, payload_len: usize) -> EnvelopeMetadata {
        EnvelopeMetadata::new(kind, filename.to_owned(), payload_len as u64).unwrap()
    }

    fn encrypt_test(
        password: &MasterPassword,
        metadata: EnvelopeMetadata,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut writer = EnvelopeWriter::new_for_test(Vec::new(), password, metadata).unwrap();
        writer.write_all(payload).unwrap();
        writer.finish().unwrap()
    }

    fn decrypt_all(
        ciphertext: &[u8],
        password: &MasterPassword,
        kind: EnvelopeKind,
    ) -> Result<(EnvelopeMetadata, Vec<u8>), SecureError> {
        let mut reader = decrypt(Cursor::new(ciphertext), password, kind)?;
        let metadata = reader.metadata().clone();
        let mut plaintext = Vec::new();
        reader
            .read_to_end(&mut plaintext)
            .map_err(|_| SecureError)?;
        Ok((metadata, plaintext))
    }

    fn malformed_envelope(password: &MasterPassword, plaintext: &[u8]) -> Vec<u8> {
        let mut recipient = age::scrypt::Recipient::new(password.secret());
        recipient.set_work_factor(TEST_SCRYPT_LOG_N);
        let encryptor =
            age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient)).unwrap();
        let mut writer = encryptor.wrap_output(Vec::new()).unwrap();
        writer.write_all(plaintext).unwrap();
        writer.finish().unwrap()
    }

    fn malformed_body_envelope(password: &MasterPassword, plaintext: &[u8]) -> Vec<u8> {
        let armor =
            age::armor::ArmoredWriter::wrap_output(Vec::new(), age::armor::Format::AsciiArmor)
                .unwrap();
        let mut recipient = age::scrypt::Recipient::new(password.secret());
        recipient.set_work_factor(TEST_SCRYPT_LOG_N);
        let encryptor =
            age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient)).unwrap();
        let mut writer = encryptor.wrap_output(armor).unwrap();
        writer.write_all(plaintext).unwrap();
        writer.finish().unwrap().finish().unwrap()
    }

    fn raw_body(body_len: u64, payload: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        write_body_header(&mut output, body_len).unwrap();
        output.extend_from_slice(payload);
        output
    }

    fn encrypt_body_test(password: &MasterPassword, payload: &[u8]) -> Vec<u8> {
        let mut writer =
            BodyEnvelopeWriter::new_for_test(Vec::new(), password, payload.len() as u64).unwrap();
        writer.write_all(payload).unwrap();
        writer.finish().unwrap()
    }

    fn decrypt_body_all(
        ciphertext: &[u8],
        password: &MasterPassword,
    ) -> Result<Vec<u8>, SecureError> {
        let mut reader = decrypt_body(Cursor::new(ciphertext), password)?;
        let mut output = Vec::new();
        reader.read_to_end(&mut output).map_err(|_| SecureError)?;
        Ok(output)
    }

    fn raw_header(kind: u8, version: u8, filename: &[u8], payload_len: u64) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(APP_MAGIC);
        output.push(version);
        output.push(kind);
        output.extend_from_slice(&(filename.len() as u16).to_le_bytes());
        output.extend_from_slice(&payload_len.to_le_bytes());
        output.extend_from_slice(filename);
        output
    }

    #[test]
    fn round_trips_unicode_empty_and_large_payloads() {
        let password = password("correct horse battery staple");
        let cases = [
            (EnvelopeKind::Note, "Пустая заметка.md", Vec::new()),
            (
                EnvelopeKind::Note,
                "Unicode 🦀.md",
                "Привет\n日本語\n🦀".as_bytes().to_vec(),
            ),
            (
                EnvelopeKind::Recovery,
                "ntrm-recovery.md",
                br#"{"body":"draft"}"#.to_vec(),
            ),
            (EnvelopeKind::Note, "Large.md", vec![0x5a; 512 * 1024]),
        ];

        for (kind, filename, payload) in cases {
            let expected = metadata(kind, filename, payload.len());
            let ciphertext = encrypt_test(&password, expected.clone(), &payload);
            assert!(is_age_prefix(&ciphertext));
            assert!(
                !ciphertext
                    .windows(filename.len())
                    .any(|part| part == filename.as_bytes())
            );
            let (actual, decrypted) = decrypt_all(&ciphertext, &password, kind).unwrap();
            assert!(actual == expected);
            assert_eq!(decrypted, payload);
        }
    }

    #[test]
    fn armored_body_round_trip_has_no_filename_or_public_hash() {
        let password = password("body password");
        let payload = b"Private title\nbody-secret-marker\n";
        let ciphertext = encrypt_body_test(&password, payload);
        assert!(is_armored_age_prefix(&ciphertext));
        assert!(
            !ciphertext
                .windows(payload.len())
                .any(|part| part == payload)
        );
        assert!(
            !ciphertext
                .windows(b"NTRMSEC2".len())
                .any(|part| part == b"NTRMSEC2")
        );
        assert!(
            !ciphertext
                .windows(b"filename".len())
                .any(|part| part == b"filename")
        );
        assert!(
            !ciphertext
                .windows(b"hash".len())
                .any(|part| part == b"hash")
        );
        let mut reader = decrypt_body(Cursor::new(&ciphertext), &password).unwrap();
        assert_eq!(reader.body_len(), payload.len() as u64);
        let mut decrypted = Vec::new();
        reader.read_to_end(&mut decrypted).unwrap();
        assert_eq!(reader.body_len(), payload.len() as u64);
        assert_eq!(decrypted, payload);
    }

    #[test]
    fn armor_writes_lf_and_reads_both_platform_line_endings() {
        let password = password("portable armor");
        let payload = b"body with original CRLF\r\nand LF\n";
        let encoded = encrypt_body_test(&password, payload);
        assert!(encoded.starts_with(ARMORED_AGE_PREFIX));
        assert!(!encoded.contains(&b'\r'));
        let crlf = String::from_utf8(encoded.clone())
            .unwrap()
            .replace('\n', "\r\n");
        for bytes in [encoded.as_slice(), crlf.as_bytes()] {
            assert!(is_armored_age_prefix(bytes));
            assert_eq!(decrypt_body_all(bytes, &password).unwrap(), payload);
        }
        for invalid in [
            b"-----BEGIN AGE ENCRYPTED FILE-----\rX".as_slice(),
            b"-----BEGIN AGE ENCRYPTED FILE-----X\n",
        ] {
            assert!(!is_armored_age_prefix(invalid));
        }
        let mut output = LfArmorOutput(Vec::new());
        for part in [b"first\r".as_slice(), b"\nsecond\r\n", b"third\n"] {
            output.write_all(part).unwrap();
        }
        assert_eq!(output.0, b"first\nsecond\nthird\n");
    }

    #[test]
    fn armored_body_rejects_wrong_password_tamper_truncation_trailing_and_length_mismatch() {
        let correct = password("correct body password");
        let ciphertext = encrypt_body_test(&correct, b"body payload");
        assert!(decrypt_body_all(&ciphertext, &password("wrong body password")).is_err());

        let mut tampered = ciphertext.clone();
        let index = tampered.len() / 2;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        assert!(decrypt_body_all(&tampered, &correct).is_err());

        let truncated = &ciphertext[..ciphertext.len() - 8];
        assert!(decrypt_body_all(truncated, &correct).is_err());

        let mut trailing = ciphertext;
        trailing.extend_from_slice(b"trailing-bytes");
        assert!(decrypt_body_all(&trailing, &correct).is_err());

        for malformed in [raw_body(2, b"x"), raw_body(1, b"xy")] {
            let encrypted = malformed_body_envelope(&correct, &malformed);
            assert!(decrypt_body_all(&encrypted, &correct).is_err());
        }
    }

    #[test]
    fn wrong_password_and_ciphertext_tamper_are_neutral() {
        let correct = password("correct-password");
        let ciphertext = encrypt_test(
            &correct,
            metadata(EnvelopeKind::Note, "Secret.md", 7),
            b"payload",
        );
        let wrong_error =
            match decrypt_all(&ciphertext, &password("wrong-password"), EnvelopeKind::Note) {
                Ok(_) => panic!("wrong password unexpectedly decrypted"),
                Err(error) => error,
            };

        let mut header_tamper = ciphertext.clone();
        let header_index = AGE_PREFIX.len() + 4;
        header_tamper[header_index] ^= 0x01;
        let header_error = match decrypt_all(&header_tamper, &correct, EnvelopeKind::Note) {
            Ok(_) => panic!("tampered age header unexpectedly decrypted"),
            Err(error) => error,
        };

        let mut body_tamper = ciphertext;
        let last = body_tamper.len() - 1;
        body_tamper[last] ^= 0x01;
        let body_error = match decrypt_all(&body_tamper, &correct, EnvelopeKind::Note) {
            Ok(_) => panic!("tampered age body unexpectedly decrypted"),
            Err(error) => error,
        };

        for error in [wrong_error, header_error, body_error] {
            assert_eq!(error.to_string(), NEUTRAL_ERROR);
            assert_eq!(format!("{error:?}"), "SecureError");
        }
    }

    #[test]
    fn rejects_version_kind_length_and_trailing_data() {
        let password = password("validation-password");
        let cases = [
            raw_header(1, 2, b"Note.md", 0),
            raw_header(99, PAYLOAD_VERSION, b"Note.md", 0),
            raw_header(1, PAYLOAD_VERSION, b"Note.md", MAX_PAYLOAD_BYTES + 1),
        ];
        for plaintext in cases {
            let ciphertext = malformed_envelope(&password, &plaintext);
            assert!(decrypt(&ciphertext[..], &password, EnvelopeKind::Note).is_err());
        }

        let recovery = encrypt_test(
            &password,
            metadata(EnvelopeKind::Recovery, "ntrm-note.md", 0),
            b"",
        );
        assert!(decrypt(&recovery[..], &password, EnvelopeKind::Note).is_err());

        let mut short = raw_header(1, PAYLOAD_VERSION, b"Note.md", 2);
        short.extend_from_slice(b"x");
        let short = malformed_envelope(&password, &short);
        let mut reader = decrypt(&short[..], &password, EnvelopeKind::Note).unwrap();
        assert!(io::copy(&mut reader, &mut io::sink()).is_err());

        let mut trailing = raw_header(1, PAYLOAD_VERSION, b"Note.md", 1);
        trailing.extend_from_slice(b"xy");
        let trailing = malformed_envelope(&password, &trailing);
        let mut reader = decrypt(&trailing[..], &password, EnvelopeKind::Note).unwrap();
        let mut output = Vec::new();
        assert!(reader.read_to_end(&mut output).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn writer_requires_exact_payload_length() {
        let password = password("writer-password");
        assert!(
            EnvelopeWriter::new_for_test(
                Vec::new(),
                &MasterPassword::new(String::new()),
                metadata(EnvelopeKind::Note, "Empty-password.md", 0),
            )
            .is_err()
        );
        let under = EnvelopeWriter::new_for_test(
            Vec::new(),
            &password,
            metadata(EnvelopeKind::Note, "Under.md", 2),
        )
        .unwrap();
        assert!(under.finish().is_err());

        let mut over = EnvelopeWriter::new_for_test(
            Vec::new(),
            &password,
            metadata(EnvelopeKind::Note, "Over.md", 1),
        )
        .unwrap();
        assert!(over.write_all(b"xy").is_err());
    }

    #[test]
    fn encryption_is_randomized_and_opaque_names_are_well_formed() {
        let password = password("randomness-password");
        let metadata = metadata(EnvelopeKind::Note, "Same.md", 4);
        let first = encrypt_test(&password, metadata.clone(), b"same");
        let second = encrypt_test(&password, metadata, b"same");
        assert_ne!(first, second);

        let first_name = opaque_note_filename().unwrap();
        let second_name = opaque_note_filename().unwrap();
        assert_ne!(first_name, second_name);
        for name in [first_name, second_name] {
            assert_eq!(name.len(), 40);
            assert!(name.starts_with("ntrm-"));
            assert!(name.ends_with(".md"));
            assert!(
                name[5..37]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            );
        }
    }

    #[test]
    fn rejects_invalid_original_filenames_and_prefixes() {
        assert!(EnvelopeMetadata::new(EnvelopeKind::Note, String::new(), 0).is_err());
        assert!(EnvelopeMetadata::new(EnvelopeKind::Note, "../Secret.md".to_owned(), 0).is_err());
        assert!(
            EnvelopeMetadata::new(
                EnvelopeKind::Note,
                "x".repeat(MAX_ORIGINAL_FILENAME_BYTES + 1),
                0,
            )
            .is_err()
        );
        assert!(is_age_prefix(AGE_PREFIX));
        assert!(!is_age_prefix(&AGE_PREFIX[..AGE_PREFIX.len() - 1]));
        assert!(!is_age_prefix(b"not-age"));
    }
}
