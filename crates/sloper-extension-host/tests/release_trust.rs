//! Signed trust acceptance over an actual compiled component and embedded
//! manifest.

use std::{
    error::Error as StdError,
    fmt::Debug,
    sync::OnceLock,
};

use aws_lc_rs::signature::{
    Ed25519KeyPair,
    KeyPair,
};
use base64::{
    Engine as _,
    engine::general_purpose::URL_SAFE_NO_PAD,
};
use serde_json::{
    Value,
    json,
};
use sha2::{
    Digest as _,
    Sha256,
};
use sloper_extension_host::{
    Engine,
    Release,
    ReleaseError,
    Trust,
    TrustRoots,
    extract_manifest,
};
use time::{
    Duration,
    OffsetDateTime,
    format_description::well_known::Rfc3339,
};

const HEX: &[u8; 16] = b"0123456789abcdef";

static MANIFEST: OnceLock<&'static [u8]> = OnceLock::new();

type TestResult = Result<(), Box<dyn StdError>>;

fn key(seed: u8) -> Ed25519KeyPair {
    // Deterministic test seeds used only by this test suite.
    Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).expect("32-byte Ed25519 test seed")
}

fn public(key: &Ed25519KeyPair) -> [u8; 32] {
    key.public_key()
        .as_ref()
        .try_into()
        .expect("Ed25519 keys contain 32 bytes")
}

fn roots() -> TrustRoots {
    TrustRoots::new([public(&key(1)), public(&key(2))]).expect("distinct test roots")
}

fn instant() -> OffsetDateTime {
    OffsetDateTime::parse("2026-09-01T00:00:00Z", &Rfc3339).expect("fixed fixture instant")
}

fn hash(bytes: &[u8]) -> String {
    let mut output = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 15)]));
    }
    output
}

fn line_raw(key: &Ed25519KeyPair, domain: &str, bytes: &[u8]) -> Vec<u8> {
    let segment = URL_SAFE_NO_PAD.encode(bytes);
    let signature = key.sign(format!("{domain}\0{segment}").as_bytes());
    format!("{segment}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref())).into_bytes()
}

fn line(key: &Ed25519KeyPair, domain: &str, payload: &Value) -> Vec<u8> {
    line_raw(key, domain, &serde_json::to_vec(payload).expect("test JSON encodes"))
}

fn trust_payload(generation: u64) -> Value {
    json!({"generation":generation,"issued":"2026-09-01T00:00:00Z","expires":"2026-10-01T00:00:00Z", "keys":[{"id":"release-1","public":URL_SAFE_NO_PAD.encode(public(&key(3))),"state":"active"}], "revoked":[]})
}

fn trust(payload: &Value, previous: Option<&Trust>) -> Result<Trust, ReleaseError> {
    Trust::verify(&line(&key(1), "sloper.trust", payload), &roots(), instant(), previous)
}

fn component() -> &'static [u8] {
    include_bytes!("fixtures/host-guest.wasm")
}

fn manifest() -> &'static [u8] {
    MANIFEST.get_or_init(|| extract_manifest(component()).expect("compiled Rust fixture has one manifest"))
}

fn release_payload() -> Value {
    json!({"extension":"sloper.host-test","version":"0.0.1","publisher":"Acme", "manifest":{"digest":hash(manifest()),"bytes":manifest().len()}, "component":{"digest":hash(component()),"bytes":component().len()},"key":"release-1"})
}

fn envelope(payload: &Value) -> Vec<u8> {
    line(&key(3), "sloper.release", payload)
}

#[track_caller]
fn assert_code<T: Debug>(result: Result<T, ReleaseError>, expected: &str) {
    let error = result.expect_err("deliberately invalid signed fixture must fail");
    assert_eq!(error.code(), expected, "failure={error:?}");
}

#[tokio::test]
async fn signed_compiled_component_is_admitted_by_actual_host() -> TestResult {
    let trust = trust(&trust_payload(1), None)?;
    let envelope = envelope(&release_payload());
    let admitted = Engine::new()?.admit_release(&envelope, component(), &trust).await?;
    assert_eq!(admitted.release().digest(), hash(&envelope));
    assert_eq!(admitted.component().manifest_bytes(), manifest());
    assert_eq!(admitted.component().manifest().name, "sloper.host-test");
    assert_eq!(admitted.component().component(), component());
    Ok(())
}

#[test]
fn exact_segment_bytes_and_domain_are_authenticated() -> TestResult {
    let trust = trust(&trust_payload(1), None)?;
    let payload = release_payload();
    let compact = envelope(&payload);
    let pretty = line_raw(&key(3), "sloper.release", &serde_json::to_vec_pretty(&payload)?);
    let release = Release::verify(&pretty, &trust)?;
    assert_ne!(release.digest(), Release::verify(&compact, &trust)?.digest());
    let old_signature = compact
        .split(|byte| *byte == b'.')
        .nth(1)
        .ok_or("signature is present")?;
    let pretty_segment = pretty.split(|byte| *byte == b'.').next().ok_or("segment is present")?;
    let substituted = [pretty_segment, b".", old_signature].concat();
    assert_code(Release::verify(&substituted, &trust), "release.signature-invalid");
    assert_code(
        Release::verify(&line(&key(3), "sloper.trust", &payload), &trust),
        "release.signature-invalid",
    );
    Ok(())
}

#[test]
fn both_embedded_roots_verify_but_roles_do_not_overlap() -> TestResult {
    let payload = trust_payload(1);
    Trust::verify(&line(&key(2), "sloper.trust", &payload), &roots(), instant(), None)?;
    assert_code(
        Trust::verify(&line(&key(3), "sloper.trust", &payload), &roots(), instant(), None),
        "trust.invalid",
    );
    assert_code(TrustRoots::new([public(&key(1)), public(&key(1))]), "trust.invalid");
    assert_code(TrustRoots::new([[0; 32], public(&key(2))]), "trust.invalid");
    let mut payload = payload;
    payload["keys"][0]["public"] = json!(URL_SAFE_NO_PAD.encode(public(&key(1))));
    assert_code(trust(&payload, None), "trust.invalid");
    Ok(())
}

#[test]
fn trust_generation_preserves_the_full_nonnegative_signed_integer_domain() -> TestResult {
    for generation in [
        0,
        9_007_199_254_740_991,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        u64::try_from(i64::MAX)?,
    ] {
        assert_eq!(trust(&trust_payload(generation), None)?.generation(), generation);
    }
    for generation in ["-1", "-0", "1.0", "1e0", "9223372036854775808", "\"1\""] {
        let payload = format!(
            "{{\"generation\":{generation},\"issued\":\"2026-09-01T00:00:00Z\",\"expires\":\"2026-10-01T00:00:00Z\",\"\
             keys\":[],\"revoked\":[]}}"
        );
        assert_code(
            Trust::verify(
                &line_raw(&key(1), "sloper.trust", payload.as_bytes()),
                &roots(),
                instant(),
                None,
            ),
            "trust.invalid",
        );
    }
    Ok(())
}

#[test]
fn closed_json_and_canonical_base64_are_required() -> TestResult {
    let trust = trust(&trust_payload(1), None)?;
    let mut payload = release_payload();
    payload["unknown"] = json!(true);
    assert_code(Release::verify(&envelope(&payload), &trust), "release.invalid");
    let duplicate = serde_json::to_string(&release_payload())?.replacen('{', "{\"key\":\"release-1\",", 1);
    assert_code(
        Release::verify(&line_raw(&key(3), "sloper.release", duplicate.as_bytes()), &trust),
        "release.invalid",
    );
    let valid = envelope(&release_payload());
    for bad in [
        [valid.as_slice(), b"\n"].concat(),
        [valid.as_slice(), b"="].concat(),
        [valid.as_slice(), b".x"].concat(),
        vec![b'x'; 16 * 1024 + 1],
        b".x".to_vec(),
        b"x.".to_vec(),
        b"x.x".to_vec(),
    ] {
        assert_code(Release::verify(&bad, &trust), "release.invalid");
    }
    let mut payload = release_payload();
    payload["component"]["bytes"] = json!(1.0);
    assert_code(Release::verify(&envelope(&payload), &trust), "release.invalid");
    Ok(())
}

#[test]
fn declared_size_limits_and_publisher_bounds_are_applied_before_hashing() -> TestResult {
    let trust = trust(&trust_payload(1), None)?;
    for (field, bytes) in [("manifest", 256 * 1024 + 1), ("component", 64 * 1024 * 1024 + 1)] {
        let mut payload = release_payload();
        payload[field]["bytes"] = json!(bytes);
        assert_code(Release::verify(&envelope(&payload), &trust), "release.invalid");
    }
    for publisher in ["x".repeat(65), "unsafe\u{202e}name".into(), String::new()] {
        let mut payload = release_payload();
        payload["publisher"] = json!(publisher);
        assert_code(Release::verify(&envelope(&payload), &trust), "release.invalid");
    }
    for name in ["acme..demo", "acme.a--b", "acme.Demo"] {
        let mut payload = release_payload();
        payload["extension"] = json!(name);
        assert_code(Release::verify(&envelope(&payload), &trust), "release.invalid");
    }
    Ok(())
}

#[tokio::test]
async fn digest_verification_precedes_declaration_and_world_admission() -> TestResult {
    let trust = trust(&trust_payload(1), None)?;
    let envelope = envelope(&release_payload());
    let engine = Engine::new()?;
    assert_code(
        engine.admit_release(&envelope, b"malformed", &trust).await,
        "release.digest-mismatch",
    );
    let mut payload = release_payload();
    payload["manifest"]["digest"] = json!(hash(b"changed"));
    assert_code(
        engine
            .admit_release(&self::envelope(&payload), component(), &trust)
            .await,
        "release.digest-mismatch",
    );
    assert_code(
        engine.admit_release(&[], component(), &trust).await,
        "extension.unsigned",
    );
    let mut payload = release_payload();
    payload["extension"] = json!("acme.other");
    assert_code(
        engine
            .admit_release(&self::envelope(&payload), component(), &trust)
            .await,
        "release.invalid",
    );
    Ok(())
}

#[test]
fn same_generation_replay_advances_floor_and_never_revives_stale_trust() -> TestResult {
    let document = line(&key(1), "sloper.trust", &trust_payload(2));
    let mut accepted = Trust::verify(&document, &roots(), instant(), None)?;
    accepted.advance_time(instant() + Duration::days(31));
    let replay = Trust::verify(&document, &roots(), instant(), Some(&accepted))?;
    assert_eq!(replay.trusted_at(), accepted.trusted_at());
    assert_code(replay.require_fresh(instant()), "trust.stale");
    assert_code(trust(&trust_payload(1), Some(&replay)), "trust.rollback");
    let mut changed = trust_payload(2);
    changed["revoked"] = json!([{"subject":"sloper.host-test","reason":"broken"}]);
    assert_code(trust(&changed, Some(&replay)), "trust.rollback");
    let restored = Trust::restore(replay.document(), &roots(), replay.trusted_at(), &replay.tombstones())?;
    assert_code(restored.require_fresh(instant()), "trust.stale");
    Ok(())
}

#[test]
fn digest_and_key_tombstones_survive_omission_but_name_revocations_can_end() -> TestResult {
    let envelope = envelope(&release_payload());
    for subject in [hash(&envelope), hash(component()), "key:release-1".into()] {
        let mut payload = trust_payload(1);
        payload["revoked"] = json!([{"subject":subject,"reason":"compromised"}]);
        let accepted = trust(&payload, None)?;
        let mut next = trust_payload(2);
        next["keys"] = json!([]);
        let next = trust(&next, Some(&accepted))?;
        if subject.starts_with("key:") {
            assert_code(Release::verify(&envelope, &next), "release.revoked");
        }
        let restored = Trust::restore(next.document(), &roots(), next.trusted_at(), &next.tombstones())?;
        assert_eq!(restored.tombstones(), accepted.tombstones());
    }
    let mut payload = trust_payload(1);
    payload["revoked"] = json!([{"subject":"sloper.host-test","reason":"malicious"}]);
    let revoked = trust(&payload, None)?;
    assert_code(
        revoked.require_release(&Release::verify(&envelope, &revoked)?),
        "release.revoked",
    );
    let reconsidered = trust(&trust_payload(2), Some(&revoked))?;
    reconsidered.require_release(&Release::verify(&envelope, &reconsidered)?)?;
    assert!(
        reconsidered.tombstones().is_empty(),
        "name revocation is current-document state"
    );
    Ok(())
}

#[tokio::test]
async fn admission_checks_component_identity_before_name_and_digest_revocation() -> TestResult {
    let envelope = envelope(&release_payload());
    let engine = Engine::new()?;
    for subject in [hash(&envelope), hash(component()), "sloper.host-test".into()] {
        let mut payload = trust_payload(1);
        payload["revoked"] = json!([{"subject":subject,"reason":"malicious"}]);
        let trust = trust(&payload, None)?;
        assert_code(
            engine.admit_release(&envelope, b"different component", &trust).await,
            "release.digest-mismatch",
        );
        assert_code(
            engine.admit_release(&envelope, component(), &trust).await,
            "release.revoked",
        );
    }
    Ok(())
}

#[tokio::test]
async fn component_revocation_rejects_resigned_bytes_and_survives_trust_omission() -> TestResult {
    let mut trusted = trust_payload(1);
    trusted["keys"]
        .as_array_mut()
        .ok_or("keys is array")?
        .push(json!({"id":"release-2","public":URL_SAFE_NO_PAD.encode(public(&key(4))),"state":"active"}));
    let original_envelope = envelope(&release_payload());
    let mut resigned = release_payload();
    resigned["key"] = json!("release-2");
    let resigned_envelope = line(&key(4), "sloper.release", &resigned);
    assert_ne!(hash(&original_envelope), hash(&resigned_envelope));

    let clean = trust(&trusted, None)?;
    let original = Release::verify(&original_envelope, &clean)?;
    let replacement = Release::verify(&resigned_envelope, &clean)?;
    clean.require_release(&original)?;
    clean.require_release(&replacement)?;

    trusted["generation"] = json!(2);
    trusted["revoked"] = json!([{"subject":hash(component()),"reason":"malicious"}]);
    let revoked = trust(&trusted, Some(&clean))?;
    trusted["generation"] = json!(3);
    trusted["revoked"] = json!([]);
    let omitted = trust(&trusted, Some(&revoked))?;
    let restored = Trust::restore(
        omitted.document(),
        &roots(),
        omitted.trusted_at(),
        &omitted.tombstones(),
    )?;
    let engine = Engine::new()?;
    for accepted in [&revoked, &omitted, &restored] {
        assert_code(accepted.require_release(&original), "release.revoked");
        assert_code(accepted.require_release(&replacement), "release.revoked");
        assert_code(
            engine.admit_release(&resigned_envelope, component(), accepted).await,
            "release.revoked",
        );
    }
    Ok(())
}

#[test]
fn retired_keys_verify_and_resigning_changes_only_envelope_identity() -> TestResult {
    let mut payload = trust_payload(1);
    payload["keys"][0]["state"] = json!("retired");
    payload["keys"]
        .as_array_mut()
        .ok_or("keys is array")?
        .push(json!({"id":"release-2","public":URL_SAFE_NO_PAD.encode(public(&key(4))),"state":"active"}));
    let trust = trust(&payload, None)?;
    let old = Release::verify(&envelope(&release_payload()), &trust)?;
    let mut payload = release_payload();
    payload["key"] = json!("release-2");
    let new = Release::verify(&line(&key(4), "sloper.release", &payload), &trust)?;
    assert_ne!(old.digest(), new.digest());
    assert_eq!(old.component_digest(), new.component_digest());
    Ok(())
}

#[test]
fn expired_trust_allows_installed_release_but_rejects_new_installation() -> TestResult {
    let document = line(&key(1), "sloper.trust", &trust_payload(1));
    let trust = Trust::verify(&document, &roots(), instant() + Duration::days(31), None)?;
    let release = Release::verify(&envelope(&release_payload()), &trust)?;
    trust.require_release(&release)?;
    assert_code(trust.require_fresh(instant()), "trust.stale");
    Ok(())
}

#[test]
fn trust_rejects_invalid_lifetimes_key_sets_and_revocation_order() -> TestResult {
    for changed in [
        json!({"expires":"2026-10-02T00:00:00Z"}),
        json!({"issued":"2026-09-01T00:00:00+01:00"}),
        json!({"keys":[{"id":"release-1","public":URL_SAFE_NO_PAD.encode([0u8;32]),"state":"active"}]}),
        json!({"revoked":[{"subject":"acme.z","reason":"broken"},{"subject":"acme.a","reason":"broken"}]}),
        json!({"revoked":[{"subject":"acme.a","reason":"broken"},{"subject":"acme.a","reason":"broken"}]}),
        json!({"revoked":[{"subject":"key:","reason":"broken"}]}),
        json!({"revoked":[{"subject":"acme.a","reason":"other"}]}),
    ] {
        let mut payload = trust_payload(1);
        for (field, value) in changed.as_object().ok_or("changes object")? {
            payload[field] = value.clone();
        }
        assert_code(trust(&payload, None), "trust.invalid");
    }
    let mut payload = trust_payload(1);
    payload["keys"] = Value::Array(
        (0..65)
            .map(|i| json!({"id":format!("k{i}"),"public":URL_SAFE_NO_PAD.encode(public(&key(3))),"state":"retired"}))
            .collect(),
    );
    assert_code(trust(&payload, None), "trust.invalid");
    payload["keys"].as_array_mut().ok_or("keys array")?.pop();
    trust(&payload, None)?;
    Ok(())
}
