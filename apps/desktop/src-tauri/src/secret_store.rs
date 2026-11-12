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
#[cfg_attr(test, allow(dead_code))]
const KEYCHAIN_SERVICE: &str = "com.lumenopen.asr.credentials";

/// One credential slot in [`crate::config::AppConfig`].
///
/// The ASR key is **provider-scoped** (`asr-api-key-<provider>`): the app has
/// one settings field but several keyed cloud providers (MiniMax /
/// Volcengine / OpenAI / custom), and a single shared item would hand one
/// vendor's key to another on every provider switch — a guaranteed
/// provider-side 401. [`SecretSlot::AsrApiKeyLegacy`] is the pre-scoping
/// account, kept readable only for the one-time migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretSlot {
    AsrApiKey {
        provider: Arc<str>,
    },
    /// The pre-scoping `asr-api-key` item; read by the load-time migration
    /// only. It maps onto the same config field so the shared save/load
    /// plumbing stays total, but it never appears in [`SecretSlot::active`].
    AsrApiKeyLegacy,
    CorrectorApiKey,
}

impl SecretSlot {
    /// The slots backing the config's credential fields: the corrector key
    /// plus the ASR key scoped to the *currently configured* provider.
    pub fn active(asr_provider: &str) -> Vec<SecretSlot> {
        vec![
            SecretSlot::AsrApiKey {
                provider: Self::normalize_provider(asr_provider).into(),
            },
            SecretSlot::CorrectorApiKey,
        ]
    }

    pub fn account(&self) -> String {
        match self {
            SecretSlot::AsrApiKey { provider } => format!("asr-api-key-{provider}"),
            SecretSlot::AsrApiKeyLegacy => "asr-api-key".to_owned(),
            SecretSlot::CorrectorApiKey => "corrector-api-key".to_owned(),
        }
    }

    /// Canonical provider id for slot accounts: lowercase, trimmed — matching
    /// [`crate::dictation::canonical_asr_provider`]'s casing rules.
    fn normalize_provider(provider: &str) -> String {
        provider.trim().to_ascii_lowercase()
    }
}

/// Minimal secret backend so the migration logic can be tested without the
/// real Keychain.
pub trait SecretStore: Send + Sync {
    fn get(&self, slot: SecretSlot) -> Result<Option<String>, String>;

    /// A read with no client-side timeout. On macOS a read can block on the
    /// keychain access prompt for as long as the user takes to answer it —
    /// minutes or hours — and the point of this variant is to observe that
    /// very completion instead of polling (each new poll risks another
    /// prompt). The default matches `get` for backends that never block.
    fn get_patient(&self, slot: SecretSlot) -> Result<Option<String>, String> {
        self.get(slot)
    }

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
    /// Both ASR variants share index 0: the config tracks exactly one ASR
    /// key (the current provider's), whatever account it syncs against.
    fn index(slot: &SecretSlot) -> usize {
        match slot {
            SecretSlot::AsrApiKey { .. } | SecretSlot::AsrApiKeyLegacy => 0,
            SecretSlot::CorrectorApiKey => 1,
        }
    }

    pub fn get(&self, slot: &SecretSlot) -> SlotSync {
        self.0.lock()[Self::index(slot)].clone()
    }

    pub fn set(&self, slot: &SecretSlot, state: SlotSync) {
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
        // Migration policy (INV-1817 review): a key found in the file may
        // only fill an EMPTY keychain slot. An existing entry is the
        // official build's credential and wins — a local build's file key
        // (written while it had no Keychain) must never overwrite it.
        if let Ok(Some(stored)) = store.get(slot.clone()) {
            if stored != *value {
                tracing::info!(
                    account = %slot.account(),
                    "keychain already holds a different key; keeping it over the file value"
                );
                sync.set(&slot, SlotSync::Known(stored.clone()));
                *value = stored;
                return LoadOutcome::Migrated;
            }
        }
        return match store.set(slot.clone(), value) {
            Ok(()) => {
                sync.set(&slot, SlotSync::Known(value.clone()));
                LoadOutcome::Migrated
            }
            Err(error) => {
                tracing::warn!(
                    account = %slot.account(),
                    %error,
                    "keychain write failed; keeping API key in config file"
                );
                LoadOutcome::Unchanged
            }
        };
    }
    match store.get(slot.clone()) {
        Ok(stored) => {
            let stored = stored.unwrap_or_default();
            sync.set(&slot, SlotSync::Known(stored.clone()));
            *value = stored;
        }
        Err(error) => {
            tracing::warn!(account = %slot.account(), %error, "keychain read failed");
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
    let previous = sync.get(&slot);
    if previous == SlotSync::Known(value.to_owned()) {
        return true;
    }
    if value.is_empty() {
        // Only delete an item we have seen; an unknown state may hide a key
        // that a failed read could not surface.
        match previous {
            SlotSync::Known(_) => match store.delete(slot.clone()) {
                Ok(()) => sync.set(&slot, SlotSync::Known(String::new())),
                Err(error) => tracing::warn!(
                    account = %slot.account(),
                    %error,
                    "keychain delete failed; the old key may reappear on next launch"
                ),
            },
            // The stored value is unknown, so nothing is deleted — but the
            // user's clear must still stick for this session: recording
            // Known(empty) keeps a later retry read from resurrecting the
            // hidden key (next launch re-reads the store as before).
            SlotSync::Unknown => sync.set(&slot, SlotSync::Known(String::new())),
        }
        return true;
    }
    match store.set(slot.clone(), value) {
        Ok(()) => {
            sync.set(&slot, SlotSync::Known(value.to_owned()));
            true
        }
        Err(error) => {
            tracing::warn!(
                account = %slot.account(),
                %error,
                "keychain write failed; keeping API key in config file"
            );
            false
        }
    }
}

/// The credential store for this platform, or `None` to keep keys in the file.
///
/// On macOS the Keychain is only used by **official release builds**
/// (`LUMEN_OFFICIAL_BUILD=1` set by the release workflow; any other
/// value is not official). Every local
/// build is ad-hoc signed, and an ad-hoc signature changes on each
/// rebuild — a Keychain ACL cannot recognize the next binary, so every
/// key read would prompt for the login password (and "Always Allow"
/// cannot stick). Local and test builds therefore keep API keys in the
/// 0600 config file, exactly like non-macOS platforms. `LUMEN_KEYCHAIN=1`
/// forces the Keychain on (to exercise that path on a dev build), `=0`
/// forces it off. Test binaries are additionally hard-short-circuited:
/// they never touch the real Keychain even when `LUMEN_KEYCHAIN=1`
/// leaks into a test run.
pub fn platform_store() -> Option<&'static dyn SecretStore> {
    #[cfg(target_os = "macos")]
    {
        #[cfg(test)]
        {
            None
        }
        #[cfg(not(test))]
        {
            if !keychain_enabled(
                option_env!("LUMEN_OFFICIAL_BUILD").is_some(),
                std::env::var_os("LUMEN_KEYCHAIN").as_deref(),
            ) {
                return None;
            }
            static STORE: macos::KeychainStore = macos::KeychainStore;
            Some(&STORE)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Pure decision core of the Keychain policy, split out for tests.
#[cfg(target_os = "macos")]
fn keychain_enabled(official_build: bool, override_flag: Option<&std::ffi::OsStr>) -> bool {
    match override_flag {
        Some(value) if value == "1" => return true,
        Some(value) if value == "0" => return false,
        _ => {}
    }
    official_build
}

#[cfg(target_os = "macos")]
// Under `cfg(test)` the Keychain backend is hard-short-circuited away, so
// its items read as dead code to the test build; keep the module intact.
#[cfg_attr(test, allow(dead_code))]
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

    /// Read one credential item; shared by the timed and the patient path.
    fn read_keychain_item(slot: SecretSlot) -> Result<Option<String>, String> {
        use security_framework::passwords::get_generic_password;
        use security_framework_sys::base::errSecItemNotFound;
        let account = slot.account();
        match get_generic_password(KEYCHAIN_SERVICE, &account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| "keychain item is not valid UTF-8".to_owned()),
            Err(error) if error.code() == errSecItemNotFound => Ok(None),
            Err(error) => Err(format!("keychain lookup failed: {error}")),
        }
    }

    impl SecretStore for KeychainStore {
        fn get(&self, slot: SecretSlot) -> Result<Option<String>, String> {
            with_timeout(move || read_keychain_item(slot))
        }

        /// Untimed: blocks on the access prompt until the user answers it —
        /// however long that takes — so the caller observes the prompt's
        /// actual outcome instead of re-reading (and risking another prompt).
        fn get_patient(&self, slot: SecretSlot) -> Result<Option<String>, String> {
            read_keychain_item(slot)
        }

        fn set(&self, slot: SecretSlot, value: &str) -> Result<(), String> {
            let value = value.to_owned();
            with_timeout(move || {
                security_framework::passwords::set_generic_password(
                    KEYCHAIN_SERVICE,
                    &slot.account(),
                    value.as_bytes(),
                )
                .map_err(|error| format!("keychain write failed: {error}"))
            })
        }

        fn delete(&self, slot: SecretSlot) -> Result<(), String> {
            with_timeout(move || {
                use security_framework::passwords::delete_generic_password;
                use security_framework_sys::base::errSecItemNotFound;
                match delete_generic_password(KEYCHAIN_SERVICE, &slot.account()) {
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

    #[test]
    fn file_key_never_overwrites_an_existing_keychain_entry() {
        // Migration policy (INV-1817 review): the file value may only
        // fill an EMPTY slot; an existing stored key wins on load.
        let store = FakeStore::default();
        store
            .set(SecretSlot::CorrectorApiKey, "release-key")
            .unwrap();
        let sync = SecretSync::default();
        let mut field = "dev-key".to_string();
        let outcome = load_slot(&store, &sync, SecretSlot::CorrectorApiKey, &mut field);
        assert_eq!(field, "release-key", "stored credential wins");
        assert_eq!(outcome, LoadOutcome::Migrated);
        // Same value: plain no-op write path stays.
        let mut same = "release-key".to_string();
        assert_eq!(
            load_slot(&store, &sync, SecretSlot::CorrectorApiKey, &mut same),
            LoadOutcome::Migrated
        );
        // Empty slot: the file value migrates in.
        let mut fresh = "dev-key".to_string();
        assert_eq!(
            load_slot(&store, &sync, SecretSlot::AsrApiKeyLegacy, &mut fresh),
            LoadOutcome::Migrated
        );
        assert_eq!(
            store.get(SecretSlot::AsrApiKeyLegacy).unwrap().as_deref(),
            Some("dev-key")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_biniversals_never_get_the_real_keychain() {
        // Hard short-circuit: a test binary must never read or write the
        // login Keychain, whatever the build type or LUMEN_KEYCHAIN says.
        assert!(platform_store().is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_policy_off_for_local_builds_and_overridable() {
        use super::keychain_enabled;
        use std::ffi::OsStr;
        assert!(
            !keychain_enabled(false, None),
            "local (ad-hoc, ever-changing) builds never touch the Keychain"
        );
        assert!(
            keychain_enabled(true, None),
            "official release builds use it"
        );
        assert!(
            keychain_enabled(false, Some(OsStr::new("1"))),
            "explicit opt-in"
        );
        assert!(
            !keychain_enabled(true, Some(OsStr::new("0"))),
            "explicit opt-out"
        );
        // Unrecognized values fall back to the build type.
        assert!(!keychain_enabled(false, Some(OsStr::new("yes"))));
    }

    /// In-memory store; `fail` makes every call error to model a locked or
    /// unavailable Keychain.
    #[derive(Default)]
    pub(crate) struct FakeStore {
        pub items: Mutex<HashMap<String, String>>,
        pub fail: Mutex<bool>,
        /// Fail only reads (models a keychain access prompt pending on the
        /// item) while writes still succeed.
        pub fail_get: Mutex<bool>,
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
            if *self.fail_get.lock() {
                return Err("get unavailable".into());
            }
            self.check()?;
            Ok(self.items.lock().get(&slot.account()).cloned())
        }
        fn set(&self, slot: SecretSlot, value: &str) -> Result<(), String> {
            self.check()?;
            *self.writes.lock() += 1;
            self.items.lock().insert(slot.account(), value.to_owned());
            Ok(())
        }
        fn delete(&self, slot: SecretSlot) -> Result<(), String> {
            self.check()?;
            self.items.lock().remove(&slot.account());
            Ok(())
        }
    }

    /// The provider-scoped ASR slot for tests.
    fn asr_slot(provider: &str) -> SecretSlot {
        SecretSlot::AsrApiKey {
            provider: provider.into(),
        }
    }

    #[test]
    fn plaintext_key_is_migrated_into_store() {
        let store = FakeStore::default();
        let sync = SecretSync::default();
        let mut value = "sk-file".to_owned();
        let outcome = load_slot(&store, &sync, asr_slot("minimax"), &mut value);
        assert_eq!(outcome, LoadOutcome::Migrated);
        assert_eq!(value, "sk-file");
        assert_eq!(
            store
                .items
                .lock()
                .get("asr-api-key-minimax")
                .map(String::as_str),
            Some("sk-file")
        );
    }

    #[test]
    fn failed_migration_keeps_plaintext() {
        let store = FakeStore::default();
        *store.fail.lock() = true;
        let sync = SecretSync::default();
        let mut value = "sk-file".to_owned();
        let outcome = load_slot(&store, &sync, asr_slot("minimax"), &mut value);
        assert_eq!(outcome, LoadOutcome::Unchanged);
        assert_eq!(value, "sk-file");
        assert!(!save_slot(&store, &sync, asr_slot("minimax"), &value));
    }

    #[test]
    fn empty_file_is_filled_from_store() {
        let store = FakeStore::default();
        store
            .items
            .lock()
            .insert("corrector-api-key".to_owned(), "sk-kc".into());
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
        store
            .items
            .lock()
            .insert("asr-api-key-minimax".to_owned(), "sk-kc".into());
        *store.fail.lock() = true;
        let sync = SecretSync::default();
        let mut value = String::new();
        load_slot(&store, &sync, asr_slot("minimax"), &mut value);
        assert_eq!(value, "");
        *store.fail.lock() = false;
        assert!(save_slot(&store, &sync, asr_slot("minimax"), ""));
        assert_eq!(
            store
                .items
                .lock()
                .get("asr-api-key-minimax")
                .map(String::as_str),
            Some("sk-kc")
        );
    }

    #[test]
    fn clearing_a_known_key_deletes_it() {
        let store = FakeStore::default();
        store
            .items
            .lock()
            .insert("asr-api-key-minimax".to_owned(), "sk-kc".into());
        let sync = SecretSync::default();
        let mut value = String::new();
        load_slot(&store, &sync, asr_slot("minimax"), &mut value);
        assert!(save_slot(&store, &sync, asr_slot("minimax"), ""));
        assert!(store.items.lock().get("asr-api-key-minimax").is_none());
    }

    #[test]
    fn clearing_while_unknown_keeps_the_clear_for_the_session() {
        // Startup read failed (store down), so the slot is Unknown and the
        // item still holds an old key. The user clears the field: nothing is
        // deleted, but the clear must stick — a later retry read must not
        // resurrect the hidden key.
        let store = FakeStore::default();
        *store.fail.lock() = true;
        store
            .items
            .lock()
            .insert("asr-api-key-minimax".to_owned(), "sk-hidden".into());
        let mut cfg = crate::config::AppConfig::default();
        let sync = cfg.secrets.clone();
        let mut value = String::new();
        load_slot(&store, &sync, asr_slot("minimax"), &mut value); // read fails → Unknown
        assert!(save_slot(&store, &sync, asr_slot("minimax"), ""));
        *store.fail.lock() = false;
        assert!(!cfg.set_secret_from_retry(asr_slot("minimax"), "sk-hidden"));
        assert_eq!(cfg.asr.api_key, "");
        assert_eq!(
            sync.get(&asr_slot("minimax")),
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
            sync.get(&SecretSlot::CorrectorApiKey),
            SlotSync::Known("sk-new".into())
        );
    }

    #[test]
    fn slots_are_scoped_per_provider_and_the_legacy_account_survives() {
        // Distinct accounts per provider are the whole point of the scoping;
        // the legacy account stays readable for the one-time migration.
        assert_eq!(
            asr_slot("minimax").account(),
            "asr-api-key-minimax".to_owned()
        );
        assert_eq!(
            asr_slot("volcengine").account(),
            "asr-api-key-volcengine".to_owned()
        );
        assert_eq!(
            SecretSlot::active("LOCAL_QWEN")[0].account(),
            "asr-api-key-local_qwen"
        );
        assert_eq!(
            SecretSlot::AsrApiKeyLegacy.account(),
            "asr-api-key".to_owned()
        );
        // The active set scopes the ASR slot to the configured provider.
        assert_eq!(
            SecretSlot::active("minimax"),
            vec![asr_slot("minimax"), SecretSlot::CorrectorApiKey]
        );
    }
}
