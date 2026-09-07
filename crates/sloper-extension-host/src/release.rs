//! Exact-byte release signatures and monotonic, renewable extension trust.

use std::{
    collections::{
        BTreeMap,
        BTreeSet,
    },
    fmt,
    str,
};

use aws_lc_rs::signature::{
    ED25519,
    UnparsedPublicKey,
};
use base64::{
    Engine as _,
    engine::general_purpose::URL_SAFE_NO_PAD,
};
use ed25519_dalek::VerifyingKey;
use serde::{
    Deserialize,
    Serialize,
};
use sha2::{
    Digest as _,
    Sha256,
};
use sloper_extension_spec::{
    Manifest,
    ManifestError,
};
use time::{
    Duration,
    OffsetDateTime,
    UtcOffset,
    format_description::well_known::Rfc3339,
};

use crate::{
    AdmittedComponent,
    ComponentError,
    Engine,
    Error as HostError,
    extract_manifest,
};

// Bound allocation before decoding or hashing untrusted objects.
const MAX_RELEASE_BYTES: usize = 16 * 1024;
const MAX_TRUST_BYTES: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_KEYS: usize = 64;
const MAX_REVOCATIONS: usize = 16_384;
const TRUST_LIFETIME: Duration = Duration::days(30);

/// Admission failures retain stable release and trust categories.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The component does not carry one valid embedded declaration.
    #[error("extension.undeclared")]
    Component(#[from] ComponentError),
    /// The embedded manifest does not satisfy its closed schema.
    #[error("extension.undeclared")]
    Manifest(#[from] ManifestError),
    /// Actual host-world instantiation rejected the component.
    #[error("extension.incompatible")]
    Admission(#[source] Box<HostError>),
    /// Ordinary installation requires an envelope.
    #[error("extension.unsigned")]
    Unsigned,
    /// The envelope has an invalid field, encoding, or bound.
    #[error("release.invalid: {0}")]
    Invalid(&'static str),
    /// The closed signed JSON payload could not be decoded.
    #[error("release.invalid: malformed payload")]
    Json(#[from] serde_json::Error),
    /// An unpadded base64url segment could not be decoded.
    #[error("release.invalid: malformed base64url")]
    Base64(#[from] base64::DecodeError),
    /// The original payload segment does not verify under its release key.
    #[error("release.signature-invalid")]
    SignatureInvalid,
    /// The nonrevoked release verification key is absent from trust.
    #[error("release.key-unknown")]
    KeyUnknown,
    /// Exact object length or digest differs from the signed declaration.
    #[error("release.digest-mismatch")]
    DigestMismatch,
    /// Current trust or a permanent tombstone revokes this release.
    #[error("release.revoked")]
    Revoked,
    /// An embedded root could not authenticate a valid trust document.
    #[error("trust.invalid")]
    TrustInvalid(#[source] Box<Error>),
    /// A generation decreased or equal-generation bytes changed.
    #[error("trust.rollback")]
    TrustRollback,
    /// New installation or connection admission requires fresh trust.
    #[error("trust.stale")]
    TrustStale,
}

impl Error {
    #[cold]
    pub(crate) fn admission(error: HostError) -> Self {
        Self::Admission(Box::new(error))
    }

    #[cold]
    pub(crate) const fn unsigned() -> Self {
        Self::Unsigned
    }

    pub(crate) fn invalid(reason: &'static str) -> Self {
        Self::Invalid(reason)
    }

    pub(crate) fn signature_invalid() -> Self {
        Self::SignatureInvalid
    }

    pub(crate) fn key_unknown() -> Self {
        Self::KeyUnknown
    }

    pub(crate) fn digest_mismatch() -> Self {
        Self::DigestMismatch
    }

    pub(crate) fn revoked() -> Self {
        Self::Revoked
    }

    pub(crate) fn trust_invalid(source: Self) -> Self {
        Self::TrustInvalid(Box::new(source))
    }

    pub(crate) fn trust_rollback() -> Self {
        Self::TrustRollback
    }

    pub(crate) fn trust_stale() -> Self {
        Self::TrustStale
    }

    /// Stable release or trust category.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Component(error) => {
                if error
                    .findings()
                    .iter()
                    .any(|finding| finding.code == "MANIFEST_WORLD_INVALID")
                {
                    "extension.incompatible"
                } else {
                    "extension.undeclared"
                }
            },
            Self::Manifest(_) => "extension.undeclared",
            Self::Admission(_) => "extension.incompatible",
            Self::Unsigned => "extension.unsigned",
            Self::Invalid(_) | Self::Json(_) | Self::Base64(_) => "release.invalid",
            Self::SignatureInvalid => "release.signature-invalid",
            Self::KeyUnknown => "release.key-unknown",
            Self::DigestMismatch => "release.digest-mismatch",
            Self::Revoked => "release.revoked",
            Self::TrustInvalid(_) => "trust.invalid",
            Self::TrustRollback => "trust.rollback",
            Self::TrustStale => "trust.stale",
        }
    }
}

/// The two distinct root public keys supplied explicitly by the caller.
///
/// Roots authenticate trust documents only. Callers cannot replace these keys
/// with values from a release envelope or a downloaded trust document.
#[derive(Clone, Debug)]
pub struct TrustRoots {
    keys: [[u8; 32]; 2],
}

impl TrustRoots {
    /// Validates the two provisioned Ed25519 verification keys.
    ///
    /// # Errors
    /// Returns `trust.invalid` for equal, malformed, or weak public keys.
    pub fn new(keys: [[u8; 32]; 2]) -> Result<Self, Error> {
        if keys[0] == keys[1] || keys.iter().any(|key| !valid_public_key(key)) {
            return Err(Error::trust_invalid(Error::invalid("invalid root keys")));
        }
        Ok(Self {
            keys,
        })
    }

    /// Decodes the two unpadded base64url keys supplied by the caller.
    ///
    /// # Errors
    /// Returns `trust.invalid` for invalid encodings or verification keys.
    pub fn from_base64(keys: [&str; 2]) -> Result<Self, Error> {
        let decode_key = |key: &str| -> Result<[u8; 32], Error> {
            decode(key)?
                .try_into()
                .map_err(|_| Error::invalid("invalid root key length"))
        };
        Self::new([
            decode_key(keys[0]).map_err(Error::trust_invalid)?,
            decode_key(keys[1]).map_err(Error::trust_invalid)?,
        ])
    }
}

/// Exact release line authenticated by an active or retired release key.
#[derive(Clone)]
pub struct Release {
    payload: ReleasePayload,
    envelope: Vec<u8>,
    digest: String,
}

/// Verified trust candidate. The caller must persist it atomically before use.
#[derive(Clone)]
pub struct Trust {
    payload: TrustPayload,
    document: Vec<u8>,
    trusted_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    tombstones: BTreeMap<String, RevocationReason>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleasePayload {
    extension: String,
    version: String,
    publisher: String,
    manifest: ObjectIdentity,
    component: ObjectIdentity,
    key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ObjectIdentity {
    digest: String,
    bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrustPayload {
    generation: u64,
    issued: String,
    expires: String,
    keys: Vec<ReleaseKey>,
    revoked: Vec<Revocation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseKey {
    id: String,
    public: String,
    state: KeyState,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum KeyState {
    Active,
    Retired,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Revocation {
    subject: String,
    reason: RevocationReason,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RevocationReason {
    Compromised,
    Malicious,
    Broken,
    Mistaken,
}

impl RevocationReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Compromised => "compromised",
            Self::Malicious => "malicious",
            Self::Broken => "broken",
            Self::Mistaken => "mistaken",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "compromised" => Ok(Self::Compromised),
            "malicious" => Ok(Self::Malicious),
            "broken" => Ok(Self::Broken),
            "mistaken" => Ok(Self::Mistaken),
            _ => Err(Error::invalid("unknown revocation reason")),
        }
    }
}

impl Trust {
    /// Authenticates a proposed update and preserves permanent revocations.
    ///
    /// The caller must persist the returned exact document, trusted-time floor
    /// and tombstones atomically before admitting any release with it.
    ///
    /// # Errors
    /// Returns `trust.invalid` for invalid documents and `trust.rollback` for
    /// lower generations or changed bytes at the accepted generation.
    pub fn verify(
        document: &[u8],
        roots: &TrustRoots,
        now: OffsetDateTime,
        previous: Option<&Self>,
    ) -> Result<Self, Error> {
        let (payload, expires_at) = Self::authenticate(document, roots).map_err(Error::trust_invalid)?;
        if let Some(previous) = previous
            && (payload.generation < previous.payload.generation
                || (payload.generation == previous.payload.generation && document != previous.document))
        {
            return Err(Error::trust_rollback());
        }
        let issued = timestamp(&payload.issued).map_err(Error::trust_invalid)?;
        let trusted_at = previous.map_or(now.max(issued), |previous| now.max(issued).max(previous.trusted_at));
        let mut tombstones = previous.map_or_else(BTreeMap::new, |previous| previous.tombstones.clone());
        for revocation in &payload.revoked {
            if permanent_subject(&revocation.subject) {
                tombstones
                    .entry(revocation.subject.clone())
                    .or_insert(revocation.reason);
            }
        }
        Ok(Self {
            payload,
            document: document.to_vec(),
            trusted_at,
            expires_at,
            tombstones,
        })
    }

    /// Reauthenticates committed trust bytes and irreversible local state.
    ///
    /// # Errors
    /// Returns `trust.invalid` for invalid documents or persisted tombstones.
    pub fn restore(
        document: &[u8],
        roots: &TrustRoots,
        trusted_at: OffsetDateTime,
        tombstones: &[(String, String)],
    ) -> Result<Self, Error> {
        let mut trust = Self::verify(document, roots, trusted_at, None)?;
        let mut subjects = BTreeSet::new();
        for (subject, reason) in tombstones {
            if !permanent_subject(subject) || !subjects.insert(subject) {
                return Err(Error::trust_invalid(Error::invalid("invalid persisted tombstone")));
            }
            trust.tombstones.insert(
                subject.clone(),
                RevocationReason::parse(reason).map_err(Error::trust_invalid)?,
            );
        }
        Ok(trust)
    }

    fn authenticate(document: &[u8], roots: &TrustRoots) -> Result<(TrustPayload, OffsetDateTime), Error> {
        let signed = SignedLine::parse(document, MAX_TRUST_BYTES)?;
        let payload: TrustPayload = serde_json::from_slice(&signed.payload)?;
        if !roots
            .keys
            .iter()
            .any(|root| signed.verify("sloper.trust", root).is_ok())
        {
            return Err(Error::signature_invalid());
        }
        let issued = timestamp(&payload.issued)?;
        let expires_at = timestamp(&payload.expires)?;
        if expires_at - issued != TRUST_LIFETIME || payload.generation > i64::MAX as u64 {
            return Err(Error::invalid("invalid trust lifetime or generation"));
        }
        if payload.keys.len() > MAX_KEYS || payload.revoked.len() > MAX_REVOCATIONS {
            return Err(Error::invalid("too many keys or revocations"));
        }
        let mut keys = BTreeSet::new();
        for key in &payload.keys {
            if !key_id(&key.id) || !keys.insert(&key.id) {
                return Err(Error::invalid("invalid or duplicate release key"));
            }
            let public = decode(&key.public)?;
            if public
                .as_slice()
                .try_into()
                .ok()
                .is_none_or(|bytes| !valid_public_key(bytes))
                || roots.keys.iter().any(|root| root.as_slice() == public)
            {
                return Err(Error::invalid("invalid Ed25519 public key"));
            }
        }
        let mut previous = None;
        for revocation in &payload.revoked {
            if !permanent_subject(&revocation.subject) && !extension_name(&revocation.subject) {
                return Err(Error::invalid("invalid revocation subject"));
            }
            if previous.is_some_and(|previous| previous >= revocation.subject.as_str()) {
                return Err(Error::invalid("revocation subjects must be unique and sorted"));
            }
            previous = Some(revocation.subject.as_str());
        }
        Ok((payload, expires_at))
    }

    /// Requires fresh trust for a new installation or connection.
    ///
    /// # Errors
    /// Returns `trust.stale` when the trusted clock has reached expiration.
    pub fn require_fresh(&self, now: OffsetDateTime) -> Result<(), Error> {
        if self.is_stale(now) {
            Err(Error::trust_stale())
        } else {
            Ok(())
        }
    }

    /// Reports expiry using the maximum of the clock and remembered floor.
    #[must_use]
    pub fn is_stale(&self, now: OffsetDateTime) -> bool {
        now.max(self.trusted_at) >= self.expires_at
    }

    /// Advances the floor even while trust is stale; persist before exposing
    /// it.
    pub fn advance_time(&mut self, now: OffsetDateTime) {
        self.trusted_at = self.trusted_at.max(now);
    }

    /// Rechecks a pinned installed release without imposing a freshness gate.
    ///
    /// # Errors
    /// Returns `release.revoked` for an envelope digest, component digest,
    /// signing key, or extension revocation.
    pub fn require_release(&self, release: &Release) -> Result<(), Error> {
        self.require_subject(&release.digest)?;
        self.require_subject(&release.payload.component.digest)?;
        self.require_subject(&format!("key:{}", release.payload.key))?;
        self.require_subject(&release.payload.extension)
    }

    fn require_subject(&self, subject: &str) -> Result<(), Error> {
        if self.tombstones.contains_key(subject)
            || self
                .payload
                .revoked
                .binary_search_by(|revocation| revocation.subject.as_str().cmp(subject))
                .is_ok()
        {
            Err(Error::revoked())
        } else {
            Ok(())
        }
    }

    /// The accepted signed trust generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.payload.generation
    }

    /// The exact root-signed ASCII document.
    #[must_use]
    pub fn document(&self) -> &[u8] {
        &self.document
    }

    /// The monotone trusted-time floor to persist.
    #[must_use]
    pub fn trusted_at(&self) -> OffsetDateTime {
        self.trusted_at
    }

    /// The signed document expiration instant.
    #[must_use]
    pub fn expires_at(&self) -> OffsetDateTime {
        self.expires_at
    }

    /// The permanent revocation subjects and their original reasons.
    #[must_use]
    pub fn tombstones(&self) -> Vec<(String, String)> {
        self.tombstones
            .iter()
            .map(|(subject, reason)| (subject.clone(), reason.as_str().to_owned()))
            .collect()
    }
}

impl Release {
    /// Authenticates an exact envelope under the accepted release keys.
    ///
    /// A revoked signing key is rejected before key lookup. Digest and name
    /// revocations are applied by [`Trust::require_release`] after exact object
    /// verification during admission, or before an installed attempt. Freshness
    /// is checked separately for a new installation.
    ///
    /// # Errors
    /// Returns the invalid, signature, unknown-key, or revocation category.
    pub fn verify(envelope: &[u8], trust: &Trust) -> Result<Self, Error> {
        let signed = SignedLine::parse(envelope, MAX_RELEASE_BYTES)?;
        let payload: ReleasePayload = serde_json::from_slice(&signed.payload)?;
        payload.validate()?;
        // Revocation precedes key lookup, including keys removed from later trust.
        trust.require_subject(&format!("key:{}", payload.key))?;
        let key = trust
            .payload
            .keys
            .iter()
            .find(|key| key.id == payload.key)
            .ok_or_else(Error::key_unknown)?;
        signed.verify("sloper.release", &decode(&key.public)?)?;
        Ok(Self {
            payload,
            envelope: envelope.to_vec(),
            digest: digest(envelope),
        })
    }

    /// Verifies component length and digest before manifest extraction.
    ///
    /// # Errors
    /// Returns `release.digest-mismatch` when the exact bytes differ.
    pub fn verify_component(&self, component: &[u8]) -> Result<(), Error> {
        self.payload.component.verify(component)
    }

    /// Verifies the exact manifest bytes extracted from the component.
    ///
    /// The host calls this only after verifying the complete component and
    /// extracting exactly one top-level declaration.
    ///
    /// # Errors
    /// Returns `release.digest-mismatch` when the exact bytes differ.
    pub fn verify_manifest(&self, manifest: &[u8]) -> Result<(), Error> {
        self.payload.manifest.verify(manifest)
    }

    /// Requires the validated embedded declaration to name this release.
    ///
    /// # Errors
    /// Returns `release.invalid` for a different extension or exact version.
    pub fn require_identity(&self, name: &str, version: &str) -> Result<(), Error> {
        if self.payload.extension != name || self.payload.version != version {
            return Err(Error::invalid("manifest identity does not match release"));
        }
        Ok(())
    }

    /// The SHA-256 identity of the entire envelope line.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The exact release-signed ASCII envelope line.
    #[must_use]
    pub fn envelope(&self) -> &[u8] {
        &self.envelope
    }

    /// The signed extension name.
    #[must_use]
    pub fn extension(&self) -> &str {
        &self.payload.extension
    }

    /// The signed exact semantic version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.payload.version
    }

    /// The release verification key identifier.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.payload.key
    }

    /// The publisher display name captured at publication.
    #[must_use]
    pub fn publisher(&self) -> &str {
        &self.payload.publisher
    }

    /// The expected SHA-256 digest of the complete component.
    #[must_use]
    pub fn component_digest(&self) -> &str {
        &self.payload.component.digest
    }

    /// The expected SHA-256 digest of the embedded declaration.
    #[must_use]
    pub fn manifest_digest(&self) -> &str {
        &self.payload.manifest.digest
    }
}

impl ReleasePayload {
    fn validate(&self) -> Result<(), Error> {
        let version = semver::Version::parse(&self.version).map_err(|_| Error::invalid("invalid exact version"))?;
        if !extension_name(&self.extension)
            || version.to_string() != self.version
            || !version.build.is_empty()
            || !key_id(&self.key)
            || self.publisher.is_empty()
            || self.publisher.chars().count() > 64 || self.publisher.chars().any(|c| {
            c.is_control()
                || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        }) {
            return Err(Error::invalid("invalid release identity"));
        }
        self.manifest.validate(MAX_MANIFEST_BYTES)?;
        self.component.validate(MAX_COMPONENT_BYTES)
    }
}

impl ObjectIdentity {
    fn validate(&self, maximum: u64) -> Result<(), Error> {
        if !valid_digest(&self.digest) || self.bytes == 0 || self.bytes > maximum {
            return Err(Error::invalid("invalid object digest or size"));
        }
        Ok(())
    }

    fn verify(&self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() as u64 != self.bytes || digest(bytes) != self.digest {
            return Err(Error::digest_mismatch());
        }
        Ok(())
    }
}

struct SignedLine<'a> {
    segment: &'a str,
    payload: Vec<u8>,
    signature: Vec<u8>,
}

impl<'a> SignedLine<'a> {
    fn parse(line: &'a [u8], limit: usize) -> Result<Self, Error> {
        if line.is_empty() || line.len() > limit || !line.is_ascii() {
            return Err(Error::invalid("invalid signed line size or encoding"));
        }
        let line = str::from_utf8(line).map_err(|_| Error::invalid("invalid ASCII line"))?;
        let (segment, signature) = line
            .split_once('.')
            .ok_or_else(|| Error::invalid("missing signature segment"))?;
        let payload = decode(segment)?;
        let signature = decode(signature)?;
        if signature.len() != 64 {
            return Err(Error::invalid("invalid signature length"));
        }
        Ok(Self {
            segment,
            payload,
            signature,
        })
    }

    fn verify(&self, domain: &str, key: &[u8]) -> Result<(), Error> {
        let mut message = Vec::with_capacity(domain.len() + 1 + self.segment.len());
        message.extend_from_slice(domain.as_bytes());
        message.push(0);
        message.extend_from_slice(self.segment.as_bytes());
        UnparsedPublicKey::new(&ED25519, key)
            .verify(&message, &self.signature)
            .map_err(|_| Error::signature_invalid())
    }
}

fn decode(segment: &str) -> Result<Vec<u8>, Error> {
    if segment.is_empty()
        || !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::invalid("invalid unpadded base64url segment"));
    }
    let bytes = URL_SAFE_NO_PAD.decode(segment)?;
    if URL_SAFE_NO_PAD.encode(&bytes) != segment {
        return Err(Error::invalid("noncanonical base64url"));
    }
    Ok(bytes)
}

fn timestamp(value: &str) -> Result<OffsetDateTime, Error> {
    let time = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| Error::invalid("invalid UTC timestamp"))?;
    if time.offset() != UtcOffset::UTC || !value.ends_with('Z') {
        return Err(Error::invalid("timestamp must use UTC"));
    }
    Ok(time)
}

fn key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn extension_name(value: &str) -> bool {
    value.len() <= 128
        && value.contains('.')
        && value.split('.').all(|label| {
            label.bytes().next().is_some_and(|byte| byte.is_ascii_lowercase())
                && label.split('-').all(|part| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                })
        })
}

fn permanent_subject(subject: &str) -> bool {
    valid_digest(subject) || subject.strip_prefix("key:").is_some_and(key_id)
}

fn valid_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 15)]));
    }
    output
}

fn valid_public_key(bytes: &[u8; 32]) -> bool {
    VerifyingKey::from_bytes(bytes).is_ok_and(|key| !key.is_weak())
}

impl fmt::Debug for Release {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Release")
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Trust {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Trust")
            .field("generation", &self.payload.generation)
            .field("trusted_at", &self.trusted_at)
            .field("expires_at", &self.expires_at)
            .field("tombstones", &self.tombstones.len())
            .finish_non_exhaustive()
    }
}

/// Exact signed identity and component admitted together by the actual host.
#[derive(Debug)]
pub struct AdmittedRelease {
    release: Release,
    component: AdmittedComponent,
}

impl AdmittedRelease {
    /// The verified immutable envelope identity.
    #[must_use]
    pub fn release(&self) -> &Release {
        &self.release
    }

    /// The exact bytes and validated manifest admitted by the host world.
    #[must_use]
    pub fn component(&self) -> &AdmittedComponent {
        &self.component
    }
}

impl Engine {
    /// Verifies the signature and content digests, then instantiates the host
    /// world.
    ///
    /// Callers must persist accepted trust revisions and require freshness
    /// before admitting a new release. The host verifies only explicitly
    /// supplied inputs.
    ///
    /// # Errors
    /// Preserves the stable release, manifest and host-world admission
    /// category.
    ///
    /// # Cancel safety
    /// Dropping the future installs nothing and exposes no admitted component.
    pub async fn admit_release(
        &self,
        envelope: &[u8],
        component: &[u8],
        trust: &Trust,
    ) -> Result<AdmittedRelease, Error> {
        if envelope.is_empty() {
            return Err(Error::unsigned());
        }
        let release = Release::verify(envelope, trust)?;
        release.verify_component(component)?;
        let manifest_bytes = extract_manifest(component)?;
        release.verify_manifest(manifest_bytes)?;
        let manifest = Manifest::parse(manifest_bytes)?;
        release.require_identity(&manifest.name, &manifest.version)?;
        trust.require_release(&release)?;
        let component = self.admit_component(component).await.map_err(Error::admission)?;
        Ok(AdmittedRelease {
            release,
            component,
        })
    }
}
