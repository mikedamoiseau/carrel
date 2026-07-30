//! Profile soft-lock keychain layer (A-M1).
//!
//! Owns keychain interaction and the Argon2id KDF for the profile soft-lock
//! feature. See `docs/superpowers/specs/2026-07-07-profile-soft-lock-design.md`
//! for the full design. This module has no Tauri dependency — command
//! wiring, session state, and the frontend are later milestones.
//!
//! `hash_password`/`verify_password` run Argon2id at OWASP-interactive
//! parameters and are CPU/memory-heavy (~19 MiB, 2 iterations); callers on
//! an async runtime should wrap them in `spawn_blocking` (not this module's
//! concern).
//!
//! ## Fail-closed contract (Decision 7)
//!
//! [`load_lock`] and [`has_lock`] return `Ok(None)` / `Ok(false)` **only**
//! when the keychain reports [`keyring::Error::NoEntry`] ("no lock set").
//! Any other keychain failure (permission denied, storage unavailable, …) is
//! surfaced as `Err` so callers block the profile switch rather than
//! silently treating "can't verify" as "unlocked".

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, Params};
use rand_core::OsRng;
use secrecy::{ExposeSecret, SecretString};
use std::fmt::Write as _;

use crate::error::{FolioError, FolioResult};

/// Keychain service prefix (Decision 10): namespaces profile-lock entries so
/// they never collide with other Carrel keychain users (backup secrets, the
/// web PIN).
///
/// Still says `folio` after the Carrel rename — it keys profile-lock passwords
/// already in the keychain. See CLAUDE.md, "Legacy `folio` identifiers".
const KEYRING_SERVICE_PREFIX: &str = "com.mike.folio.profile-lock";
/// Fixed account name — the service string (see [`keyring_key`]) is what
/// varies per profile, mirroring the `backup.rs` convention of a
/// per-secret service with a constant `"default"` user.
const KEYRING_USER: &str = "default";

/// OWASP-interactive Argon2id parameters (Decision 2): ~19 MiB memory,
/// 2 iterations, 1 degree of parallelism.
fn argon2_params() -> Params {
    Params::new(19 * 1024, 2, 1, None).expect("hardcoded Argon2 params are always valid")
}

fn argon2() -> Argon2<'static> {
    Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2_params(),
    )
}

/// Sanitized, namespaced keychain service key for a profile.
///
/// Profile names may contain spaces, unicode, or punctuation. Rather than
/// attempt a lossy sanitize, the UTF-8 bytes of the name are hex-encoded:
/// the mapping is injective, so distinct profile names always yield
/// distinct, stable, ASCII-safe keys.
fn keyring_key(profile: &str) -> String {
    let mut key = String::with_capacity(KEYRING_SERVICE_PREFIX.len() + 1 + profile.len() * 2);
    key.push_str(KEYRING_SERVICE_PREFIX);
    key.push('/');
    for byte in profile.as_bytes() {
        write!(key, "{byte:02x}").expect("writing to a String cannot fail");
    }
    key
}

fn entry(profile: &str) -> FolioResult<keyring::Entry> {
    keyring::Entry::new(&keyring_key(profile), KEYRING_USER)
        .map_err(|e| FolioError::internal(format!("Failed to access keychain: {e}")))
}

/// Argon2id-hash `password` into a PHC string suitable for storage.
pub fn hash_password(password: &SecretString) -> FolioResult<String> {
    let salt = SaltString::generate(&mut OsRng);
    argon2()
        .hash_password(password.expose_secret().as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| FolioError::internal(format!("Failed to hash password: {e}")))
}

/// Reject any PHC that is not exactly this module's policy: Argon2id,
/// version 0x13, and the OWASP-interactive cost params from
/// [`argon2_params`]. Argon2's verifier derives its work from the params
/// embedded in the *stored* hash, so a corrupted or maliciously overwritten
/// keychain entry carrying an extreme `m`/`t` would otherwise drive unbounded
/// memory/CPU on unlock. Validating up front keeps out-of-policy material a
/// controlled error rather than an OOM (Decision 7's fail-closed intent).
fn validate_policy(parsed: &PasswordHash) -> FolioResult<()> {
    let out_of_policy =
        |what: &str| FolioError::invalid(format!("Out-of-policy password hash: {what}"));

    if parsed.algorithm != argon2::Algorithm::Argon2id.ident() {
        return Err(out_of_policy("algorithm"));
    }
    // `None` means the PHC omitted the version field; only an explicit,
    // matching version is accepted.
    if parsed.version != Some(argon2::Version::V0x13 as u32) {
        return Err(out_of_policy("version"));
    }
    let params =
        Params::try_from(parsed).map_err(|e| out_of_policy(&format!("unparseable params: {e}")))?;
    let expected = argon2_params();
    if params.m_cost() != expected.m_cost()
        || params.t_cost() != expected.t_cost()
        || params.p_cost() != expected.p_cost()
    {
        return Err(out_of_policy("cost parameters"));
    }
    Ok(())
}

/// Constant-time verify of `password` against a stored PHC string. Never
/// re-hashes and compares with `==`.
pub fn verify_password(password: &SecretString, phc: &str) -> FolioResult<bool> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| FolioError::invalid(format!("Malformed password hash: {e}")))?;
    validate_policy(&parsed)?;
    match argon2().verify_password(password.expose_secret().as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(FolioError::internal(format!(
            "Failed to verify password: {e}"
        ))),
    }
}

/// Maps a raw keyring `get_password` result to the fail-closed contract:
/// `NoEntry` ("no lock set") becomes `Ok(None)`; every other keychain
/// failure becomes `Err` (Decision 7). Split out as a pure function so the
/// mapping is unit-testable without a real or mocked keychain entry.
fn map_load_result(result: Result<String, keyring::Error>) -> FolioResult<Option<String>> {
    match result {
        Ok(phc) => Ok(Some(phc)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(FolioError::internal(format!("Keychain error: {e}"))),
    }
}

/// Read the stored PHC string for `profile`. `Ok(None)` only on "no lock
/// set" (`NoEntry`); any other keychain failure is `Err` (Decision 7).
pub fn load_lock(profile: &str) -> FolioResult<Option<String>> {
    map_load_result(entry(profile)?.get_password())
}

/// Store `phc` as the lock for `profile`, overwriting any existing value.
/// Rejects any PHC that is not this module's policy so out-of-policy material
/// can never be persisted (Decision 7).
pub fn set_lock(profile: &str, phc: &str) -> FolioResult<()> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| FolioError::invalid(format!("Malformed password hash: {e}")))?;
    validate_policy(&parsed)?;
    entry(profile)?
        .set_password(phc)
        .map_err(|e| FolioError::internal(format!("Failed to store profile lock: {e}")))
}

/// Remove the lock for `profile`, if any. Ignores `NoEntry` — clearing an
/// already-unlocked profile is not an error.
pub fn clear_lock(profile: &str) -> FolioResult<()> {
    match entry(profile)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(FolioError::internal(format!(
            "Failed to clear profile lock: {e}"
        ))),
    }
}

/// Whether `profile` currently has a lock set. `Ok(false)` only on
/// `NoEntry`; any other keychain failure is `Err` (Decision 7).
pub fn has_lock(profile: &str) -> FolioResult<bool> {
    Ok(load_lock(profile)?.is_some())
}

/// Whether a profile may be accessed right now, given whether it has a
/// stored lock and whether it has already been unlocked this session.
/// Pure logic shared by `switch_profile` and the web layer's per-request
/// gate (A-M2) so both enforce the identical rule: a profile with no lock
/// is always accessible; a locked profile requires having been unlocked
/// this session.
pub fn access_allowed(has_lock: bool, unlocked_this_session: bool) -> bool {
    !has_lock || unlocked_this_session
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    // ---- KDF ----

    #[test]
    fn hash_then_verify_round_trip_succeeds() {
        let password = secret("correct horse battery staple");
        let phc = hash_password(&password).expect("hash should succeed");
        assert!(verify_password(&password, &phc).expect("verify should succeed"));
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let phc = hash_password(&secret("correct horse battery staple")).unwrap();
        let ok = verify_password(&secret("wrong password"), &phc).expect("verify should not err");
        assert!(!ok);
    }

    #[test]
    fn verify_against_malformed_phc_returns_err_not_panic() {
        let result = verify_password(&secret("anything"), "not-a-valid-phc-string");
        assert!(result.is_err());
    }

    #[test]
    fn verify_rejects_out_of_policy_params_before_hashing() {
        // A syntactically valid PHC carrying extreme cost params (what a
        // corrupted/overwritten keychain entry could hold) must be rejected
        // up front, not fed to the verifier where huge `m` would OOM. The PHC
        // is a hand-written literal precisely so the test never runs the
        // 4 GiB hash itself — validate_policy rejects it by parsing the params,
        // before verify_password ever touches the KDF.
        let evil = "$argon2id$v=19$m=4194304,t=10,p=1\
                    $c29tZXNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo";
        let result = verify_password(&secret("whatever"), evil);
        assert!(result.is_err());
    }

    #[test]
    fn verify_rejects_non_argon2id_algorithm() {
        // Argon2i is a valid PHC but not our policy algorithm.
        let other = Argon2::new(
            argon2::Algorithm::Argon2i,
            argon2::Version::V0x13,
            argon2_params(),
        );
        let salt = SaltString::generate(&mut OsRng);
        let phc = other.hash_password(b"x", &salt).unwrap().to_string();
        assert!(verify_password(&secret("x"), &phc).is_err());
    }

    #[test]
    fn hash_produces_argon2id_phc_string() {
        // Sanity check on the params baked into every hash: algorithm tag
        // and the OWASP-interactive memory cost (19456 KiB = 19 * 1024).
        let phc = hash_password(&secret("hunter2")).unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(phc.contains("m=19456"));
        assert!(phc.contains("t=2"));
        assert!(phc.contains("p=1"));
    }

    // ---- keyring_key sanitization ----

    #[test]
    fn keyring_key_is_stable_for_the_same_name() {
        assert_eq!(keyring_key("Alice"), keyring_key("Alice"));
    }

    #[test]
    fn keyring_key_is_namespaced() {
        assert!(keyring_key("Alice").starts_with(KEYRING_SERVICE_PREFIX));
    }

    #[test]
    fn keyring_key_is_distinct_across_tricky_profile_names() {
        let names = [
            "default",
            "Alice",
            "alice", // case must not collide
            "Bob's Profile",
            "profile with spaces",
            "profil francais avec espace",
            "日本語プロファイル",
            "emoji-profile-😀",
            "weird;chars/here:too",
            "",
            " ",
        ];
        let mut keys = std::collections::HashSet::new();
        for name in names {
            assert!(
                keys.insert(keyring_key(name)),
                "collision producing keyring key for {name:?}"
            );
        }
    }

    // ---- fail-closed contract (Decision 7) ----

    #[test]
    fn map_load_result_no_entry_becomes_ok_none() {
        let result = map_load_result(Err(keyring::Error::NoEntry));
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn map_load_result_found_password_becomes_ok_some() {
        let result = map_load_result(Ok("$argon2id$...".to_string()));
        assert!(matches!(result, Ok(Some(_))));
    }

    // ---- access_allowed (A-M2) ----

    #[test]
    fn access_allowed_when_no_lock_regardless_of_session_state() {
        assert!(access_allowed(false, false));
        assert!(access_allowed(false, true));
    }

    #[test]
    fn access_denied_when_locked_and_not_unlocked_this_session() {
        assert!(!access_allowed(true, false));
    }

    #[test]
    fn access_allowed_when_locked_but_unlocked_this_session() {
        assert!(access_allowed(true, true));
    }

    #[test]
    fn map_load_result_other_keychain_error_becomes_err() {
        // Any non-NoEntry keychain failure must block the caller — it must
        // never be treated as "no lock set".
        let result = map_load_result(Err(keyring::Error::Invalid(
            "attr".to_string(),
            "simulated platform failure".to_string(),
        )));
        assert!(result.is_err());
    }
}
