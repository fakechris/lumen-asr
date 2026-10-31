//! Credential storage for provider API keys.
//!
//! `config.toml` is a plain file in Application Support, so API keys written
//! there are readable by anything running as the user. On macOS the keys live
//! in the login Keychain instead and the file keeps an empty string. Other
//! platforms keep today's plaintext behaviour ([`platform_store`] returns
//! `None`) until a native credential store is wired up.
//!
//! The policy is deliberately conservative: a key is only removed from the file
//! after the Keychain accepted it, and a Keychain item is only deleted when the
//! app previously observed its value and the user then cleared it.

use std::sync::Arc;

use parking_lot::Mutex;

/// Keychain service shared by every provider credential.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.lumenopen.asr.credentials";

/// One credential slot in [`crate::config::AppConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSlot {
    AsrApiKey,
    CorrectorApiKey,
}

impl SecretSlot {
    pub const ALL: [SecretSlot; 2] = [SecretSlot::AsrApiKey, SecretSlot::CorrectorApiKey];

    pub fn account(self) -> &'static str {
        match self {
            SecretSlot::AsrApiKey => "asr-api-key",
            SecretSlot::CorrectorApiKey => "corrector-api-key",
        }
    }
}

/// Minimal secret backend so the migration logic can be tested without the
/// real Keychain.
pub trait SecretStore: Send + Sync {
    fn get(&self, slot: SecretSlot) -> Result<Option<String>, String>;
    fn set(&self, slot: SecretSlot, value: &str) -> Result<(), String>;
    fn delete(&self, slot: SecretSlot) -> Result<(), String>;
}

/// What the app last knows about a slot's stored value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SlotSync {
    /// Never read successfully; the stored value is unknown. Saving an empty
    /// value in this state must not delete anything.
    #[default]
    Unknown,
    /// The store holds exactly this value (empty = no item).
    Known(String),
}

/// Per-config record of the last synced value of each slot. Shared between
/// clones of one loaded config so a save through any clone updates it.
#[derive(Debug, Clone, Default)]
pub struct SecretSync(Arc<Mutex<[SlotSync; 2]>>);

impl SecretSync {
    fn index(slot: SecretSlot) -> usize {
        match slot {
            SecretSlot::AsrApiKey => 0,
            SecretSlot::CorrectorApiKey => 1,
        }
    }

    pub fn get(&self, slot: SecretSlot) -> SlotSync {
        self.0.lock()[Self::index(slot)].clone()
    }

    pub fn set(&self, slot: SecretSlot, state: SlotSync) {
        self.0.lock()[Self::index(slot)] = state;
    }
}

/// Result of reconciling one slot while loading the config file.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The file carried a plaintext key that is now in the store; the file
    /// should be rewritten without it.
    Migrated,
    /// Nothing in the file needs rewriting.
    Unchanged,
}

/// Reconcile one slot at load time. `value` is the field as parsed from the
/// file and is replaced by the stored key when the file has none.
pub fn load_slot(
    store: &dyn SecretStore,
    sync: &SecretSync,
    slot: SecretSlot,
    value: &mut String,
) -> LoadOutcome {
    if !value.is_empty() {
        return match store.set(slot, value) {
            Ok(()) => {
                sync.set(slot, SlotSync::Known(value.clone()));
                LoadOutcome::Migrated
            }
            Err(error) => {
                tracing::warn!(
                    account = slot.account(),
                    %error,
                    "keychain write failed; keeping API key in config file"
                );
                LoadOutcome::Unchanged
            }
        };
    }
    match store.get(slot) {
        Ok(stored) => {
            let stored = stored.unwrap_or_default();
            sync.set(slot, SlotSync::Known(stored.clone()));
            *value = stored;
        }
        Err(error) => {
            tracing::warn!(account = slot.account(), %error, "keychain read failed");
        }
    }
    LoadOutcome::Unchanged
}

/// Persist one slot before the file is written. Returns `true` when the store
/// now holds `value`, so the file may omit it; `false` keeps the plaintext in
/// the file so a failing store never loses the key.
pub fn save_slot(
    store: &dyn SecretStore,
    sync: &SecretSync,
    slot: SecretSlot,
    value: &str,
) -> bool {
    let previous = sync.get(slot);
    if previous == SlotSync::Known(value.to_owned()) {
        return true;
    }
    if value.is_empty() {
        // Only delete an item we have seen; an unknown state may hide a key
        // that a failed read could not surface.
        match previous {
            SlotSync::Known(_) => match store.delete(slot) {
                Ok(()) => sync.set(slot, SlotSync::Known(String::new())),
                Err(error) => tracing::warn!(
                    account = slot.account(),
                    %error,
                    "keychain delete failed; the old key may reappear on next launch"
                ),
            },
            // The stored value is unknown, so nothing is deleted — but the
            // user's clear must still stick for this session: recording
            // Known(empty) keeps a later retry read from resurrecting the
            // hidden key (next launch re-reads the store as before).
            SlotSync::Unknown => sync.set(slot, SlotSync::Known(String::new())),
        }
        return true;
    }
    match store.set(slot, value) {
        Ok(()) => {
            sync.set(slot, SlotSync::Known(value.to_owned()));
            true
        }
        Err(error) => {
            tracing::warn!(
                account = slot.account(),
                %error,
                "keychain write failed; keeping API key in config file"
            );
            false
        }
    }
}

/// The credential store for this platform, or `None` to keep keys in the file.
pub fn platform_store() -> Option<&'static dyn SecretStore> {
    #[cfg(target_os = "macos")]
    {
        static STORE: macos::KeychainStore = macos::KeychainStore;
        Some(&STORE)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{SecretSlot, SecretStore, KEYCHAIN_SERVICE};

    /// Keychain calls can block on an access prompt; never let one stall
    /// startup or a settings save indefinitely.
    const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(3);

    pub struct KeychainStore;

    fn with_timeout<T, F>(operation: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, String> + Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("lumen-credentials-keychain".into())
            .spawn(move || {
                let _ = sender.send(operation());
            })
            .map_err(|error| format!("keychain worker unavailable: {error}"))?;
        match receiver.recv_timeout(KEYCHAIN_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "keychain timed out after {} ms",
                KEYCHAIN_TIMEOUT.as_millis()
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("keychain worker disconnected".to_owned())
            }
        }
    }

    impl SecretStore for KeychainStore {
        fn get(&self, slot: SecretSlot) -> Result<Option<String>, String> {
            with_timeout(move || {
                use security_framework::passwords::get_generic_password;
                use security_framework_sys::base::errSecItemNotFound;
                match get_generic_password(KEYCHAIN_SERVICE, slot.account()) {
                    Ok(bytes) => String::from_utf8(bytes)
                        .map(Some)
                        .map_err(|_| "keychain item is not valid UTF-8".to_owned()),
                    Err(error) if error.code() == errSecItemNotFound => Ok(None),
                    Err(error) => Err(format!("keychain lookup failed: {error}")),
                }
            })
        }

        fn set(&self, slot: SecretSlot, value: &str) -> Result<(), String> {
            let value = value.to_owned();
            with_timeout(move || {
                security_framework::passwords::set_generic_password(
                    KEYCHAIN_SERVICE,
                    slot.account(),
                    value.as_bytes(),
                )
                .map_err(|error| format!("keychain write failed: {error}"))
            })
        }

        fn delete(&self, slot: SecretSlot) -> Result<(), String> {
            with_timeout(move || {
                use security_framework::passwords::delete_generic_password;
                use security_framework_sys::base::errSecItemNotFound;
                match delete_generic_password(KEYCHAIN_SERVICE, slot.account()) {
                    Ok(()) => Ok(()),
                    Err(error) if error.code() == errSecItemNotFound => Ok(()),
                    Err(error) => Err(format!("keychain delete failed: {error}")),
                }
            })
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use super::*;

    /// In-memory store; `fail` makes every call error to model a locked or
    /// unavailable Keychain.
    #[derive(Default)]
    pub(crate) struct FakeStore {
        pub items: Mutex<HashMap<&'static str, String>>,
        pub fail: Mutex<bool>,
        pub writes: Mutex<usize>,
    }

    impl FakeStore {
        fn check(&self) -> Result<(), String> {
            if *self.fail.lock() {
                Err("unavailable".into())
            } else {
                Ok(())
            }
        }
    }

    impl SecretStore for FakeStore {
        fn get(&self, slot: SecretSlot) -> Result<Option<String>, String> {
            self.check()?;
            Ok(self.items.lock().get(slot.account()).cloned())
        }
        fn set(&self, slot: SecretSlot, value: &str) -> Result<(), String> {
            self.check()?;
            *self.writes.lock() += 1;
            self.items.lock().insert(slot.account(), value.to_owned());
            Ok(())
        }
        fn delete(&self, slot: SecretSlot) -> Result<(), String> {
            self.check()?;
            self.items.lock().remove(slot.account());
            Ok(())
        }
    }

    #[test]
    fn plaintext_key_is_migrated_into_store() {
        let store = FakeStore::default();
        let sync = SecretSync::default();
        let mut value = "sk-file".to_owned();
        let outcome = load_slot(&store, &sync, SecretSlot::AsrApiKey, &mut value);
        assert_eq!(outcome, LoadOutcome::Migrated);
        assert_eq!(value, "sk-file");
        assert_eq!(
            store.items.lock().get("asr-api-key").map(String::as_str),
            Some("sk-file")
        );
    }

    #[test]
    fn failed_migration_keeps_plaintext() {
        let store = FakeStore::default();
        *store.fail.lock() = true;
        let sync = SecretSync::default();
        let mut value = "sk-file".to_owned();
        let outcome = load_slot(&store, &sync, SecretSlot::AsrApiKey, &mut value);
        assert_eq!(outcome, LoadOutcome::Unchanged);
        assert_eq!(value, "sk-file");
        assert!(!save_slot(&store, &sync, SecretSlot::AsrApiKey, &value));
    }

    #[test]
    fn empty_file_is_filled_from_store() {
        let store = FakeStore::default();
        store
            .items
            .lock()
            .insert("corrector-api-key", "sk-kc".into());
        let sync = SecretSync::default();
        let mut value = String::new();
        load_slot(&store, &sync, SecretSlot::CorrectorApiKey, &mut value);
        assert_eq!(value, "sk-kc");
        // Unchanged value does not rewrite the Keychain on every save.
        assert!(save_slot(
            &store,
            &sync,
            SecretSlot::CorrectorApiKey,
            &value
        ));
        assert_eq!(*store.writes.lock(), 0);
    }

    #[test]
    fn unreadable_store_never_deletes_on_empty_save() {
        let store = FakeStore::default();
        store.items.lock().insert("asr-api-key", "sk-kc".into());
        *store.fail.lock() = true;
        let sync = SecretSync::default();
        let mut value = String::new();
        load_slot(&store, &sync, SecretSlot::AsrApiKey, &mut value);
        assert_eq!(value, "");
        *store.fail.lock() = false;
        assert!(save_slot(&store, &sync, SecretSlot::AsrApiKey, ""));
        assert_eq!(
            store.items.lock().get("asr-api-key").map(String::as_str),
            Some("sk-kc")
        );
    }

    #[test]
    fn clearing_a_known_key_deletes_it() {
        let store = FakeStore::default();
        store.items.lock().insert("asr-api-key", "sk-kc".into());
        let sync = SecretSync::default();
        let mut value = String::new();
        load_slot(&store, &sync, SecretSlot::AsrApiKey, &mut value);
        assert!(save_slot(&store, &sync, SecretSlot::AsrApiKey, ""));
        assert!(store.items.lock().get("asr-api-key").is_none());
    }

    #[test]
    fn clearing_while_unknown_keeps_the_clear_for_the_session() {
        // Startup read failed (store down), so the slot is Unknown and the
        // item still holds an old key. The user clears the field: nothing is
        // deleted, but the clear must stick — a later retry read must not
        // resurrect the hidden key.
        let store = FakeStore::default();
        *store.fail.lock() = true;
        store.items.lock().insert("asr-api-key", "sk-hidden".into());
        let mut cfg = crate::config::AppConfig::default();
        let sync = cfg.secrets.clone();
        let mut value = String::new();
        load_slot(&store, &sync, SecretSlot::AsrApiKey, &mut value); // read fails → Unknown
        assert!(save_slot(&store, &sync, SecretSlot::AsrApiKey, ""));
        *store.fail.lock() = false;
        assert!(!cfg.set_secret_from_retry(SecretSlot::AsrApiKey, "sk-hidden"));
        assert_eq!(cfg.asr.api_key, "");
        assert_eq!(
            sync.get(SecretSlot::AsrApiKey),
            SlotSync::Known(String::new())
        );
    }

    #[test]
    fn new_key_is_written_to_store() {
        let store = FakeStore::default();
        let sync = SecretSync::default();
        assert!(save_slot(
            &store,
            &sync,
            SecretSlot::CorrectorApiKey,
            "sk-new"
        ));
        assert_eq!(
            store
                .items
                .lock()
                .get("corrector-api-key")
                .map(String::as_str),
            Some("sk-new")
        );
        assert_eq!(
            sync.get(SecretSlot::CorrectorApiKey),
            SlotSync::Known("sk-new".into())
        );
    }
}
