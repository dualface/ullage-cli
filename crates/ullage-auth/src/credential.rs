use std::collections::BTreeMap;
use std::fmt;

use zeroize::Zeroize;

use crate::CredentialError;

pub(crate) const NAMESPACE: &str = "dev.onevoke.ullage.credentials";
const MAGIC: &[u8; 8] = b"ULLAGEC2";
const MAX_RECORD_BYTES: usize = 1024 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_FIELD_NAME_BYTES: usize = 64;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CredentialKey {
    provider: String,
    account_id: String,
}

impl CredentialKey {
    pub fn new(
        provider: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Result<Self, CredentialError> {
        let provider = provider.into();
        let account_id = account_id.into();
        validate_component("provider", &provider)?;
        validate_component("account ID", &account_id)?;
        Ok(Self {
            provider,
            account_id,
        })
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn service_name(&self) -> String {
        format!("{NAMESPACE}.{}", self.provider)
    }

    pub fn entry_name(&self) -> &str {
        &self.account_id
    }

    pub(crate) fn stable_bytes(&self) -> Vec<u8> {
        let mut result =
            Vec::with_capacity(NAMESPACE.len() + self.provider.len() + self.account_id.len() + 2);
        result.extend_from_slice(NAMESPACE.as_bytes());
        result.push(0);
        result.extend_from_slice(self.provider.as_bytes());
        result.push(0);
        result.extend_from_slice(self.account_id.as_bytes());
        result
    }
}

impl fmt::Debug for CredentialKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialKey")
            .field("provider", &self.provider)
            .field("account_id", &"[REDACTED]")
            .finish()
    }
}

fn validate_component(label: &'static str, value: &str) -> Result<(), CredentialError> {
    if value.is_empty() || value.len() > 255 || value.chars().any(char::is_control) {
        return Err(CredentialError::InvalidKey(label));
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Zeroize)]
#[zeroize(drop)]
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credential {
    fields: BTreeMap<String, SecretValue>,
}

impl Credential {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        name: impl Into<String>,
        value: SecretValue,
    ) -> Result<Option<SecretValue>, CredentialError> {
        let name = name.into();
        if name.is_empty()
            || name.len() > MAX_FIELD_NAME_BYTES
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(CredentialError::InvalidFieldName);
        }
        if self.fields.len() >= MAX_FIELDS && !self.fields.contains_key(&name) {
            return Err(CredentialError::CredentialTooLarge);
        }
        Ok(self.fields.insert(name, value))
    }

    pub fn get(&self, name: &str) -> Option<&SecretValue> {
        self.fields.get(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<SecretValue> {
        self.fields.remove(name)
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("field_names", &self.fields.keys().collect::<Vec<_>>())
            .field("values", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CredentialVersion {
    generation: u64,
    revision: u64,
}

impl CredentialVersion {
    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn revision(self) -> u64 {
        self.revision
    }

    pub(crate) fn new(generation: u64, revision: u64) -> Self {
        Self {
            generation,
            revision,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct StoredCredential {
    version: CredentialVersion,
    credential: Credential,
}

impl StoredCredential {
    pub fn version(&self) -> CredentialVersion {
        self.version
    }

    pub fn revision(&self) -> u64 {
        self.version.revision
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    pub fn into_credential(self) -> Credential {
        self.credential
    }

    pub(crate) fn new(version: CredentialVersion, credential: Credential) -> Self {
        Self {
            version,
            credential,
        }
    }
}

impl fmt::Debug for StoredCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCredential")
            .field("version", &self.version)
            .field("credential", &self.credential)
            .finish()
    }
}

pub(crate) enum StoredRecord {
    Active(StoredCredential),
    Tombstone { generation: u64 },
}

impl StoredRecord {
    pub(crate) fn active(self) -> Result<StoredCredential, CredentialError> {
        match self {
            Self::Active(stored) => Ok(stored),
            Self::Tombstone { .. } => Err(CredentialError::NotFound),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        match self {
            Self::Active(stored) => stored.version.generation,
            Self::Tombstone { generation } => *generation,
        }
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, CredentialError> {
        let (revision, credential) = match self {
            Self::Active(stored) => (stored.version.revision, Some(&stored.credential)),
            Self::Tombstone { .. } => (0, None),
        };
        let mut encoded_len = MAGIC.len() + 8 + 8 + 1 + 4;
        for (name, value) in credential.into_iter().flat_map(|value| &value.fields) {
            let value_len = u32::try_from(value.expose().len())
                .map_err(|_| CredentialError::CredentialTooLarge)?;
            encoded_len = encoded_len
                .checked_add(2)
                .and_then(|length| length.checked_add(name.len()))
                .and_then(|length| length.checked_add(4))
                .and_then(|length| length.checked_add(value_len as usize))
                .ok_or(CredentialError::CredentialTooLarge)?;
            if encoded_len > MAX_RECORD_BYTES {
                return Err(CredentialError::CredentialTooLarge);
            }
        }

        let mut output = Vec::with_capacity(encoded_len);
        output.extend_from_slice(MAGIC);
        output.extend_from_slice(&self.generation().to_be_bytes());
        output.extend_from_slice(&revision.to_be_bytes());
        output.push(u8::from(credential.is_some()));
        output.extend_from_slice(
            &(credential.map_or(0, |value| value.fields.len()) as u32).to_be_bytes(),
        );
        for (name, value) in credential.into_iter().flat_map(|value| &value.fields) {
            output.extend_from_slice(&(name.len() as u16).to_be_bytes());
            output.extend_from_slice(name.as_bytes());
            let value_len = u32::try_from(value.expose().len())
                .map_err(|_| CredentialError::CredentialTooLarge)?;
            output.extend_from_slice(&value_len.to_be_bytes());
            output.extend_from_slice(value.expose());
        }
        debug_assert_eq!(output.len(), encoded_len);
        Ok(output)
    }

    pub(crate) fn decode(mut input: Vec<u8>) -> Result<Self, CredentialError> {
        let result = decode_record(&input);
        input.zeroize();
        result
    }
}

fn decode_record(input: &[u8]) -> Result<StoredRecord, CredentialError> {
    if input.len() > MAX_RECORD_BYTES
        || input.len() < MAGIC.len() + 21
        || &input[..MAGIC.len()] != MAGIC
    {
        return Err(CredentialError::CorruptCredential);
    }
    let mut cursor = MAGIC.len();
    let generation = read_u64(input, &mut cursor)?;
    let revision = read_u64(input, &mut cursor)?;
    let active = *take(input, &mut cursor, 1)?
        .first()
        .ok_or(CredentialError::CorruptCredential)?;
    if generation == 0
        || active > 1
        || (active == 1 && revision == 0)
        || (active == 0 && revision != 0)
    {
        return Err(CredentialError::CorruptCredential);
    }
    let field_count = read_u32(input, &mut cursor)? as usize;
    if field_count > MAX_FIELDS {
        return Err(CredentialError::CorruptCredential);
    }
    if active == 0 {
        if field_count != 0 || cursor != input.len() {
            return Err(CredentialError::CorruptCredential);
        }
        return Ok(StoredRecord::Tombstone { generation });
    }
    let mut credential = Credential::new();
    for _ in 0..field_count {
        let name_len = read_u16(input, &mut cursor)? as usize;
        let name = take(input, &mut cursor, name_len)?;
        let name = std::str::from_utf8(name).map_err(|_| CredentialError::CorruptCredential)?;
        let value_len = read_u32(input, &mut cursor)? as usize;
        let value = take(input, &mut cursor, value_len)?.to_vec();
        if credential
            .insert(name, SecretValue::new(value))
            .map_err(|_| CredentialError::CorruptCredential)?
            .is_some()
        {
            return Err(CredentialError::CorruptCredential);
        }
    }
    if cursor != input.len() {
        return Err(CredentialError::CorruptCredential);
    }
    Ok(StoredRecord::Active(StoredCredential::new(
        CredentialVersion::new(generation, revision),
        credential,
    )))
}

fn take<'a>(input: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], CredentialError> {
    let end = cursor
        .checked_add(len)
        .ok_or(CredentialError::CorruptCredential)?;
    let value = input
        .get(*cursor..end)
        .ok_or(CredentialError::CorruptCredential)?;
    *cursor = end;
    Ok(value)
}

fn read_u16(input: &[u8], cursor: &mut usize) -> Result<u16, CredentialError> {
    let bytes: [u8; 2] = take(input, cursor, 2)?
        .try_into()
        .map_err(|_| CredentialError::CorruptCredential)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32(input: &[u8], cursor: &mut usize) -> Result<u32, CredentialError> {
    let bytes: [u8; 4] = take(input, cursor, 4)?
        .try_into()
        .map_err(|_| CredentialError::CorruptCredential)?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64(input: &[u8], cursor: &mut usize) -> Result<u64, CredentialError> {
    let bytes: [u8; 8] = take(input, cursor, 8)?
        .try_into()
        .map_err(|_| CredentialError::CorruptCredential)?;
    Ok(u64::from_be_bytes(bytes))
}
