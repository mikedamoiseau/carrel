//! Desktop Kids-mode device state: an app-global flag plus the parents'-code
//! lockout counter, persisted in `{data_dir}/kids_mode.json`. Deliberately
//! not a per-profile setting (those live in each profile DB, and the app
//! always starts in "default"), so it survives restarts and profile switches.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CarrelError, CarrelResult};

const MAX_FAILURES: u32 = 5;
const LOCKOUT_SECS: i64 = 300;

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct KidsDeviceState {
    pub enabled: bool,
    pub failed_attempts: u32,
    pub locked_until: i64,
}

pub fn kids_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("kids_mode.json")
}

/// A missing file is "disabled"; a file that exists but cannot be parsed is
/// "enabled" (fail closed: corrupting the file must not leave Kids mode off).
pub fn read_kids_state(data_dir: &Path) -> KidsDeviceState {
    match std::fs::read_to_string(kids_state_path(data_dir)) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or(KidsDeviceState {
            enabled: true,
            ..Default::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => KidsDeviceState::default(),
        Err(_) => KidsDeviceState {
            enabled: true,
            ..Default::default()
        },
    }
}

pub fn write_kids_state(data_dir: &Path, s: &KidsDeviceState) -> CarrelResult<()> {
    let json = serde_json::to_string_pretty(s).map_err(|e| CarrelError::internal(e.to_string()))?;
    std::fs::write(kids_state_path(data_dir), json)
        .map_err(|e| CarrelError::internal(e.to_string()))?;
    Ok(())
}

/// Records one wrong code. The 5th failure locks for 300 s and resets the count.
pub fn register_failure(s: &KidsDeviceState, now: i64) -> KidsDeviceState {
    let failed = s.failed_attempts + 1;
    let mut next = s.clone();
    if failed >= MAX_FAILURES {
        next.failed_attempts = 0;
        next.locked_until = now + LOCKOUT_SECS;
    } else {
        next.failed_attempts = failed;
    }
    next
}

pub fn is_locked(s: &KidsDeviceState, now: i64) -> bool {
    now < s.locked_until
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_kids_state(dir.path()), KidsDeviceState::default());
    }

    #[test]
    fn malformed_file_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(kids_state_path(dir.path()), "{not json").unwrap();
        assert!(read_kids_state(dir.path()).enabled);
    }

    #[test]
    fn state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = KidsDeviceState {
            enabled: true,
            failed_attempts: 3,
            locked_until: 1234,
        };
        write_kids_state(dir.path(), &s).unwrap();
        assert_eq!(read_kids_state(dir.path()), s);
    }

    #[test]
    fn fifth_failure_locks_for_300s() {
        let mut s = KidsDeviceState {
            enabled: true,
            ..Default::default()
        };
        for _ in 0..4 {
            s = register_failure(&s, 1000);
            assert!(!is_locked(&s, 1000));
        }
        assert_eq!(s.failed_attempts, 4);
        s = register_failure(&s, 1000);
        assert_eq!(s.locked_until, 1300);
        assert_eq!(s.failed_attempts, 0);
        assert!(is_locked(&s, 1000));
        assert!(s.enabled);
    }

    #[test]
    fn lock_expires() {
        let s = KidsDeviceState {
            enabled: true,
            failed_attempts: 0,
            locked_until: 1300,
        };
        assert!(is_locked(&s, 1299));
        assert!(!is_locked(&s, 1300));
    }
}
