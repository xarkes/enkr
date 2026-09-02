//! Device identity persistence — the *only* thing the sync layer keeps on
//! disk. Everything else (space keys, membership logs, outbox, seq state) is
//! in-memory and re-fetched/re-derived from the server; content durability is
//! the note database's job (see the `needs_push` flag in `note.rs`).
//!
//! Format: the 16-byte [`DeviceSeed`] both keys are derived from, written once
//! on first run — a flat file natively, hex in `localStorage` on wasm32. Shaped
//! so the production replacement is an OS keychain entry, not a database.
//!
//! The seed rather than the keys, because 16 bytes is small enough to be a
//! 12-word BIP39 phrase and therefore small enough for a person to write down.
//! That is the only recovery path there is: the relay holds nothing but
//! ciphertext, so a device whose seed is gone leaves its owner with synced
//! content nobody can ever decrypt. See `enkr/TODO.md` under Shipping.

use std::path::PathBuf;

use enkr_proto::crypto::{DeviceIdentity, DeviceSeed};

#[derive(Clone, Debug)]
pub enum IdentityStore {
    /// Fresh identity per session (tests / throwaway runs).
    InMemory,
    /// 64-byte key file, created on first run.
    Path(PathBuf),
    /// Browser `localStorage`, under this key — the wasm32 equivalent of
    /// [`Self::Path`], created on first run.
    ///
    /// `localStorage` rather than the IndexedDB the note database uses
    /// (`note.rs`): this is 64 bytes that must be readable *synchronously*,
    /// since `SyncClient::spawn` resolves the identity before the engine
    /// starts and there is no way to block on a future on wasm32. IndexedDB
    /// is async-only, so using it would mean restructuring startup around
    /// an await for a value that fits in a single small string. Neither
    /// store is a secure enclave — see the module doc comment on what this
    /// is shaped to become.
    #[cfg(target_arch = "wasm32")]
    LocalStorage(String),
}

pub(crate) fn load_or_create(store: &IdentityStore) -> Result<DeviceIdentity, String> {
    let path = match store {
        IdentityStore::InMemory => return Ok(DeviceIdentity::generate()),
        #[cfg(target_arch = "wasm32")]
        IdentityStore::LocalStorage(key) => return load_or_create_web(key),
        IdentityStore::Path(path) => path,
    };

    match std::fs::read(path) {
        Ok(bytes) => Ok(DeviceIdentity::from_seed(&decode_seed(&bytes, path)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let seed = DeviceSeed::generate();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            write_private(path, &seed.0).map_err(|e| format!("write device key: {e}"))?;
            Ok(DeviceIdentity::from_seed(&seed))
        }
        Err(err) => Err(format!("read device key {}: {err}", path.display())),
    }
}

/// Read the stored seed, refusing anything else.
///
/// A 64-byte file is the pre-seed layout (`signing_sk ‖ kex_sk`), which has no
/// seed behind it and so cannot be expressed as a phrase. Failing loudly is the
/// point: silently minting a new identity would orphan the device from every
/// space it was admitted to, which is far worse than an error telling the owner
/// what happened.
fn decode_seed(bytes: &[u8], path: &std::path::Path) -> Result<DeviceSeed, String> {
    match <[u8; 16]>::try_from(bytes) {
        Ok(seed) => Ok(DeviceSeed(seed)),
        Err(_) if bytes.len() == 64 => Err(format!(
            "{} predates recovery phrases (64-byte key, no seed behind it). \
             Delete it to start a new identity — that device will need re-inviting \
             to any shared space.",
            path.display()
        )),
        Err(_) => Err(format!("corrupt device key file: {}", path.display())),
    }
}

/// This device's recovery phrase, read back from wherever the seed is stored.
///
/// Read on demand rather than kept in memory: the phrase is the whole identity,
/// and there is no reason for it to sit in the process between the rare moments
/// a user asks to see it.
pub fn recovery_phrase(store: &IdentityStore) -> Result<String, String> {
    Ok(seed_to_phrase(&read_seed(store)?))
}

/// Rebuild this device's identity from a phrase.
///
/// `overwrite` guards the destructive case: replacing an existing identity
/// orphans this device from every space the old key was admitted to, and no
/// warning after the fact can undo it. A fresh install passes `false` and gets
/// an error if something is already there.
pub fn restore_from_phrase(
    store: &IdentityStore,
    phrase: &str,
    overwrite: bool,
) -> Result<(), String> {
    let seed = phrase_to_seed(phrase)?;
    if !overwrite && read_seed(store).is_ok() {
        return Err("this device already has an identity; restoring would replace it".into());
    }
    write_seed(store, &seed)
}

fn read_seed(store: &IdentityStore) -> Result<DeviceSeed, String> {
    match store {
        IdentityStore::InMemory => Err("this session has no stored identity".into()),
        IdentityStore::Path(path) => {
            let bytes = std::fs::read(path)
                .map_err(|err| format!("read device key {}: {err}", path.display()))?;
            decode_seed(&bytes, path)
        }
        #[cfg(target_arch = "wasm32")]
        IdentityStore::LocalStorage(key) => {
            let stored = web_storage()?
                .get_item(key)
                .map_err(|e| format!("read device key from localStorage[{key}]: {e:?}"))?
                .ok_or_else(|| format!("no device key in localStorage[{key}]"))?;
            decode_hex(&stored).ok_or_else(|| format!("corrupt device key in localStorage[{key}]"))
        }
    }
}

fn write_seed(store: &IdentityStore, seed: &DeviceSeed) -> Result<(), String> {
    match store {
        IdentityStore::InMemory => Err("this session has no stored identity".into()),
        IdentityStore::Path(path) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // `write_private` refuses to clobber (create_new), which is what
            // makes the `overwrite` decision explicit rather than incidental.
            let _ = std::fs::remove_file(path);
            write_private(path, &seed.0).map_err(|e| format!("write device key: {e}"))
        }
        #[cfg(target_arch = "wasm32")]
        IdentityStore::LocalStorage(key) => web_storage()?
            .set_item(key, &encode_hex(&seed.0))
            .map_err(|e| format!("write device key to localStorage[{key}]: {e:?}")),
    }
}

/// The device's recovery phrase: its seed as 12 BIP39 words.
///
/// BIP39 rather than hex because it carries a checksum. A phrase is transcribed
/// by hand and typed back months later, and a silent single-character typo means
/// permanent, unrecoverable loss — which is the exact failure this exists to
/// prevent. The words also read as "keep this safe" in a way 32 hex characters
/// do not.
pub fn seed_to_phrase(seed: &DeviceSeed) -> String {
    bip39::Mnemonic::from_entropy(&seed.0)
        .expect("16 bytes is valid BIP39 entropy")
        .to_string()
}

/// Parse a recovery phrase back into a seed, rejecting a bad checksum.
pub fn phrase_to_seed(phrase: &str) -> Result<DeviceSeed, String> {
    let mnemonic = bip39::Mnemonic::parse_normalized(phrase.trim())
        .map_err(|err| format!("not a valid recovery phrase: {err}"))?;
    let (entropy, len) = mnemonic.to_entropy_array();
    if len != 16 {
        return Err(format!(
            "recovery phrase should be 12 words, this one carries {len} bytes"
        ));
    }
    Ok(DeviceSeed(entropy[..16].try_into().expect("checked len")))
}

/// `localStorage` counterpart of the file path branch above, with the same
/// contract: return the stored identity if there is one, create and persist
/// one if there isn't, and *fail* rather than silently minting a new
/// identity if something is there but unreadable.
///
/// That last part is the whole point of this module — a regenerated
/// identity silently orphans this device from every space it had been
/// admitted to, which is far worse than a visible error. For the same
/// reason an unavailable `localStorage` (private-mode restrictions, storage
/// disabled) is an error and not a quiet fall back to
/// [`IdentityStore::InMemory`]: the session would appear to work while
/// losing its identity on every reload, which is exactly the bug this
/// exists to prevent.
///
/// Hex, not raw bytes: `localStorage` values are UTF-16 strings and cannot
/// hold arbitrary byte sequences. 32 characters for a seed written once is
/// not worth a base64 dependency.
#[cfg(target_arch = "wasm32")]
fn load_or_create_web(key: &str) -> Result<DeviceIdentity, String> {
    let storage = web_storage()?;

    match storage.get_item(key) {
        Ok(Some(encoded)) => {
            let seed = decode_hex(&encoded).ok_or_else(|| {
                if encoded.len() == 128 {
                    format!(
                        "localStorage[{key}] predates recovery phrases (64-byte key, no \
                         seed behind it). Clear it to start a new identity — that device \
                         will need re-inviting to any shared space."
                    )
                } else {
                    format!("corrupt device key in localStorage[{key}]")
                }
            })?;
            Ok(DeviceIdentity::from_seed(&seed))
        }
        Ok(None) => {
            let seed = DeviceSeed::generate();
            storage
                .set_item(key, &encode_hex(&seed.0))
                .map_err(|e| format!("write device key to localStorage[{key}]: {e:?}"))?;
            Ok(DeviceIdentity::from_seed(&seed))
        }
        Err(e) => Err(format!("read device key from localStorage[{key}]: {e:?}")),
    }
}

#[cfg(target_arch = "wasm32")]
fn web_storage() -> Result<web_sys::Storage, String> {
    web_sys::window()
        .ok_or("no global `window` to read localStorage from")?
        .local_storage()
        .map_err(|e| format!("localStorage is unavailable: {e:?}"))?
        .ok_or_else(|| "localStorage is unavailable in this context".to_string())
}

#[cfg(any(target_arch = "wasm32", test))]
fn encode_hex(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(32), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(any(target_arch = "wasm32", test))]
fn decode_hex(text: &str) -> Option<DeviceSeed> {
    let text = text.as_bytes();
    if text.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (byte, pair) in out.iter_mut().zip(text.chunks_exact(2)) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        *byte = (hi * 16 + lo) as u8;
    }
    Some(DeviceSeed(out))
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phrase_rebuilds_the_same_identity() {
        let seed = DeviceSeed::generate();
        let phrase = seed_to_phrase(&seed);
        assert_eq!(phrase.split_whitespace().count(), 12, "{phrase}");

        let restored = phrase_to_seed(&phrase).expect("round-trip");
        assert_eq!(restored.0, seed.0);
        // The point of the whole feature: same words, same device.
        assert_eq!(
            DeviceIdentity::from_seed(&seed).device_pk(),
            DeviceIdentity::from_seed(&restored).device_pk()
        );
        assert_eq!(
            DeviceIdentity::from_seed(&seed).kex_pk(),
            DeviceIdentity::from_seed(&restored).kex_pk()
        );
    }

    #[test]
    fn a_mistyped_phrase_is_rejected_rather_than_silently_wrong() {
        // A *fixed* seed, not a generated one. 16 bytes of entropy is a
        // 12-word phrase, whose checksum is only 4 bits — so one word in
        // sixteen is a collision that swapping in still parses, and a
        // generated seed made this test fail roughly 5% of runs. Pinning the
        // vector keeps it deciding whether the checksum is *checked at all*,
        // which is what it is actually for.
        let phrase = seed_to_phrase(&DeviceSeed([0x24; 16]));
        let mut words: Vec<&str> = phrase.split_whitespace().collect();
        // Swap one word for another real word: without BIP39's checksum this
        // would parse into a *different* valid seed, silently handing the user
        // an identity that is not theirs and losing their spaces.
        let replacement = if words[3] == "zoo" { "abandon" } else { "zoo" };
        words[3] = replacement;
        let mistyped = words.join(" ");
        assert!(
            phrase_to_seed(&mistyped).is_err(),
            "a single wrong word must not parse"
        );

        assert!(phrase_to_seed("not even words at all").is_err());
        assert!(phrase_to_seed("").is_err());
    }

    #[test]
    fn signing_and_kex_keys_are_independent() {
        // Derived from one seed, but a compromise of one must not reveal the
        // other — separate HKDF info strings, not a split of the same bytes.
        let identity = DeviceIdentity::from_seed(&DeviceSeed::generate());
        let (signing, kex) = identity.to_bytes();
        assert_ne!(signing, kex);
    }

    #[test]
    fn a_pre_seed_key_file_fails_loudly() {
        let path =
            std::env::temp_dir().join(format!("enkr_legacy_key_{}.key", uuid::Uuid::new_v4()));
        std::fs::write(&path, [7u8; 64]).expect("write legacy key");
        let err = match load_or_create(&IdentityStore::Path(path.clone())) {
            Err(err) => err,
            Ok(_) => panic!("a 64-byte key has no seed and must not be silently replaced"),
        };
        assert!(err.contains("predates recovery phrases"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_phrase_restores_the_identity_on_a_fresh_install() {
        let original =
            std::env::temp_dir().join(format!("enkr_restore_a_{}.key", uuid::Uuid::new_v4()));
        let fresh =
            std::env::temp_dir().join(format!("enkr_restore_b_{}.key", uuid::Uuid::new_v4()));

        let first = load_or_create(&IdentityStore::Path(original.clone())).expect("create");
        let phrase = recovery_phrase(&IdentityStore::Path(original.clone())).expect("phrase");

        // A different install, given only the words.
        restore_from_phrase(&IdentityStore::Path(fresh.clone()), &phrase, false).expect("restore");
        let restored = load_or_create(&IdentityStore::Path(fresh.clone())).expect("load restored");
        assert_eq!(first.device_pk(), restored.device_pk());
        assert_eq!(first.kex_pk(), restored.kex_pk());

        // Restoring over an existing identity is refused unless asked for: it
        // orphans this device from every space the old key was admitted to.
        let other = seed_to_phrase(&DeviceSeed::generate());
        assert!(restore_from_phrase(&IdentityStore::Path(fresh.clone()), &other, false).is_err());
        restore_from_phrase(&IdentityStore::Path(fresh.clone()), &other, true)
            .expect("explicit overwrite");
        let replaced = load_or_create(&IdentityStore::Path(fresh.clone())).expect("load replaced");
        assert_ne!(first.device_pk(), replaced.device_pk());

        // A bad phrase must not leave the identity half-written.
        let before = std::fs::read(&fresh).unwrap();
        assert!(
            restore_from_phrase(&IdentityStore::Path(fresh.clone()), "nonsense", true).is_err()
        );
        assert_eq!(std::fs::read(&fresh).unwrap(), before);

        let _ = std::fs::remove_file(&original);
        let _ = std::fs::remove_file(&fresh);
    }

    #[test]
    fn identity_file_roundtrip_and_stability() {
        let path =
            std::env::temp_dir().join(format!("enkr_identity_test_{}.key", uuid::Uuid::new_v4()));
        let store = IdentityStore::Path(path.clone());

        let first = load_or_create(&store).expect("create identity");
        let second = load_or_create(&store).expect("reload identity");
        assert_eq!(first.device_pk(), second.device_pk());
        assert_eq!(first.kex_pk(), second.kex_pk());
        // The seed, not the derived keys — 16 bytes is what a 12-word phrase carries.
        assert_eq!(std::fs::read(&path).unwrap().len(), 16);

        // Corrupt file is reported, not silently regenerated (a regenerated
        // identity would orphan every share bound to the old key).
        std::fs::write(&path, b"short").unwrap();
        assert!(load_or_create(&store).is_err());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn in_memory_is_fresh_each_time() {
        let a = load_or_create(&IdentityStore::InMemory).unwrap();
        let b = load_or_create(&IdentityStore::InMemory).unwrap();
        assert_ne!(a.device_pk(), b.device_pk());
    }

    /// The wasm32 store keeps the identity as hex (`localStorage` holds
    /// strings, not bytes), so this codec sits directly between a device
    /// and its own identity across a page reload: a silent decode bug
    /// either orphans the device or wrongly reports corruption.
    #[test]
    fn hex_round_trips_every_byte_value() {
        let mut bytes = [0u8; 16];
        for (i, b) in bytes.iter_mut().enumerate() {
            // Both nibbles vary, and 0x00/0xff are both covered.
            *b = if i % 2 == 0 { i as u8 } else { 255 - i as u8 };
        }
        let encoded = encode_hex(&bytes);
        assert_eq!(encoded.len(), 32);
        assert!(encoded.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(decode_hex(&encoded).map(|s| s.0), Some(bytes));
    }

    #[test]
    fn hex_rejects_anything_that_is_not_exactly_16_bytes_of_hex() {
        let valid = encode_hex(&[7u8; 16]);
        assert_eq!(decode_hex(&valid).map(|s| s.0), Some([7u8; 16]));

        // Too short / too long / not hex at all — each must be reported as
        // corrupt rather than decoded into a wrong-but-plausible key.
        assert!(decode_hex("").is_none());
        assert!(decode_hex(&valid[..30]).is_none());
        assert!(decode_hex(&format!("{valid}00")).is_none());
        assert!(decode_hex(&"z".repeat(32)).is_none());
        // Right length, but a stray non-hex character partway through.
        let mut tampered = valid.clone();
        tampered.replace_range(16..17, "x");
        assert!(decode_hex(&tampered).is_none());
    }

    /// Both stores hold the same 16-byte seed, so an identity written by one is
    /// readable by the other — which is what lets the file and `localStorage`
    /// branches stay interchangeable.
    #[test]
    fn stored_seed_round_trips_through_hex() {
        let seed = DeviceSeed::generate();
        let restored = decode_hex(&encode_hex(&seed.0)).expect("round-trip");
        let identity = DeviceIdentity::from_seed(&seed);
        let reloaded = DeviceIdentity::from_seed(&restored);
        assert_eq!(identity.device_pk(), reloaded.device_pk());
        assert_eq!(identity.kex_pk(), reloaded.kex_pk());
    }
}
