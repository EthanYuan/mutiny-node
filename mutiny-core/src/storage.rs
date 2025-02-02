use crate::ldkstorage::CHANNEL_MANAGER_KEY;
use crate::logging::MutinyLogger;
use crate::nodemanager::{ChannelClosure, NodeStorage};
use crate::utils::{now, spawn, DBTasks, Task};
use crate::vss::{MutinyVssClient, VssKeyValueItem};
use crate::{
    encrypt::{decrypt_with_password, encrypt, encryption_key_from_pass, Cipher},
    DEVICE_LOCK_INTERVAL_SECS,
};
use crate::{
    error::{MutinyError, MutinyStorageError},
    event::PaymentInfo,
};
use crate::{event::HTLCStatus, MutinyInvoice};
use crate::{labels::LabelStorage, TransactionDetails};
use async_trait::async_trait;
use bdk_chain::Merge;
use bdk_wallet::ChangeSet;
use bip39::Mnemonic;
use bitcoin::hashes::Hash;
use bitcoin::Txid;
use futures_util::lock::Mutex;
use hex_conservative::*;
use lightning::{ln::PaymentHash, util::logger::Logger};
use lightning::{log_debug, log_trace};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

pub const SUBSCRIPTION_TIMESTAMP: &str = "subscription_timestamp";
pub const KEYCHAIN_STORE_KEY: &str = "bdk_keychain";
pub const MNEMONIC_KEY: &str = "mnemonic";
pub(crate) const NEED_FULL_SYNC_KEY: &str = "needs_full_sync";
pub const NODES_KEY: &str = "nodes";
pub const SERVICE_TOKENS: &str = "service_tokens";
const FEE_ESTIMATES_KEY: &str = "fee_estimates";
pub const BITCOIN_PRICE_CACHE_KEY: &str = "bitcoin_price_cache";
const FIRST_SYNC_KEY: &str = "first_sync";
pub const LAST_NWC_SYNC_TIME_KEY: &str = "last_nwc_sync_time";
pub(crate) const DEVICE_ID_KEY: &str = "device_id";
pub const DEVICE_LOCK_KEY: &str = "device_lock";
pub(crate) const EXPECTED_NETWORK_KEY: &str = "network";
pub const PAYMENT_INBOUND_PREFIX_KEY: &str = "payment_inbound/";
pub const PAYMENT_OUTBOUND_PREFIX_KEY: &str = "payment_outbound/";
pub const TRANSACTION_DETAILS_PREFIX_KEY: &str = "transaction_details/";
pub(crate) const ONCHAIN_PREFIX: &str = "onchain_tx/";
pub const LAST_DM_SYNC_TIME_KEY: &str = "last_dm_sync_time";
pub const LAST_HERMES_SYNC_TIME_KEY: &str = "last_hermes_sync_time";
pub const NOSTR_PROFILE_METADATA: &str = "nostr_profile_metadata";
pub const NOSTR_CONTACT_LIST: &str = "nostr_contact_list";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelayedKeyValueItem {
    pub key: String,
    pub value: Value,
    pub version: u32,
    pub write_time: u128,
}

impl From<DelayedKeyValueItem> for VssKeyValueItem {
    fn from(item: DelayedKeyValueItem) -> Self {
        VssKeyValueItem {
            key: item.key,
            value: item.value,
            version: item.version,
        }
    }
}

fn needs_encryption(key: &str) -> bool {
    match key {
        MNEMONIC_KEY => true,
        str if str.starts_with(CHANNEL_MANAGER_KEY) => true,
        _ => false,
    }
}

pub fn encrypt_value(
    key: impl AsRef<str>,
    value: Value,
    cipher: Option<Cipher>,
) -> Result<Value, MutinyError> {
    // Only bother encrypting if a password is set
    let res = match cipher {
        Some(c) if needs_encryption(key.as_ref()) => {
            let str = serde_json::to_string(&value)?;
            let ciphertext = encrypt(&str, c)?;
            Value::String(ciphertext)
        }
        _ => value,
    };

    Ok(res)
}

pub fn decrypt_value(
    key: impl AsRef<str>,
    value: Value,
    password: Option<&str>,
) -> Result<Value, MutinyError> {
    // Only bother encrypting if a password is set
    let json: Value = match password {
        Some(pw) if needs_encryption(key.as_ref()) => {
            let str: String = serde_json::from_value(value)?;
            let ciphertext = decrypt_with_password(&str, pw)?;
            serde_json::from_str(&ciphertext)?
        }
        _ => value,
    };

    Ok(json)
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexItem {
    pub timestamp: Option<u64>,
    pub key: String,
}

impl PartialOrd for IndexItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IndexItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self.timestamp, other.timestamp) {
            (Some(a), Some(b)) => b.cmp(&a).then_with(|| self.key.cmp(&other.key)),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => self.key.cmp(&other.key),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedValue {
    pub version: u32,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLock {
    pub time: u32,
    pub device: String,
}

impl DeviceLock {
    pub fn remaining_secs(&self) -> u64 {
        let now = now().as_secs();
        let diff = now.saturating_sub(self.time as u64);
        (DEVICE_LOCK_INTERVAL_SECS * 2).saturating_sub(diff)
    }

    /// Check if the device is locked
    /// This is determined if the time is less than 2 minutes ago
    pub fn is_locked(&self, id: &str) -> bool {
        let now = now().as_secs();
        let diff = now.saturating_sub(self.time as u64);
        diff < DEVICE_LOCK_INTERVAL_SECS * 2 && self.device != id
    }

    // Check if the device is the last one to have the lock
    pub fn is_last_locker(&self, id: &str) -> bool {
        self.device == id
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait MutinyStorage: Clone + Sized + Send + Sync + 'static {
    /// Get the password used to encrypt the storage
    fn password(&self) -> Option<&str>;

    /// Get the encryption key used for storage
    fn cipher(&self) -> Option<Cipher>;

    /// Get the VSS client used for storage
    fn vss_client(&self) -> Option<Arc<MutinyVssClient>>;

    /// An index of the activity in the storage, this should be a list of (timestamp, key) tuples
    /// This is used to for getting a sorted list of keys quickly
    fn activity_index(&self) -> Arc<RwLock<BTreeSet<IndexItem>>>;

    /// Write a raw data without encript into storage
    fn write_raw<T>(&self, items: Vec<(String, T)>) -> Result<(), MutinyError>
    where
        T: Serialize + Send;

    async fn write_vss(
        &self,
        key: String,
        value: Value,
        version: Option<u32>,
    ) -> Result<(), MutinyError> {
        // save to VSS if it is enabled
        if let (Some(vss), Some(version)) = (self.vss_client(), version) {
            let item = VssKeyValueItem {
                key,
                value,
                version,
            };

            vss.put_objects(vec![item]).await
        } else {
            Ok(())
        }
    }

    /// Set a value in the storage, the function will encrypt the value if needed
    fn write_data<T>(&self, key: String, value: T, version: Option<u32>) -> Result<(), MutinyError>
    where
        T: Serialize + Send,
    {
        let data = serde_json::to_value(value).map_err(|e| MutinyError::PersistenceFailed {
            source: MutinyStorageError::SerdeError { source: e },
        })?;

        // encrypt value in async block so it can be done in parallel
        // with the VSS call
        let local_data = data.clone();
        let key_clone = key.clone();
        let json: Value = encrypt_value(key_clone.clone(), local_data, self.cipher())?;
        self.write_raw(vec![(key_clone, json)])?;

        // save to VSS by spawn an async task
        self.spawn({
            let db = self.clone();
            async move { db.write_vss(key, data, version).await }
        });

        Ok(())
    }

    fn get_delayed_objects(&self) -> Arc<Mutex<HashMap<String, DelayedKeyValueItem>>>;

    /// Get a value from the storage, use get_data if you want the value to be decrypted
    fn get<T>(&self, key: impl AsRef<str>) -> Result<Option<T>, MutinyError>
    where
        T: for<'de> Deserialize<'de>;

    /// Get a value from the storage, the function will decrypt the value if needed
    fn get_data<T>(&self, key: impl AsRef<str>) -> Result<Option<T>, MutinyError>
    where
        T: for<'de> Deserialize<'de>,
    {
        match self.get(&key)? {
            None => Ok(None),
            Some(value) => {
                let json: Value = decrypt_value(key, value, self.password())?;
                let data: T = serde_json::from_value(json)?;
                Ok(Some(data))
            }
        }
    }

    /// Delete a set of values from the storage
    fn delete(&self, keys: &[impl AsRef<str>]) -> Result<(), MutinyError>;

    /// Start the storage, this will be called before any other methods
    async fn start(&mut self) -> Result<(), MutinyError>;

    /// Stop the storage, this will be called when the application is shutting down
    async fn stop(&self);

    /// Check if the storage is connected
    fn connected(&self) -> Result<bool, MutinyError>;

    /// Scan the storage for keys with a given prefix and suffix, this will return a list of keys
    /// If this function does not properly filter the keys, it can cause major problems.
    fn scan_keys(&self, prefix: &str, suffix: Option<&str>) -> Result<Vec<String>, MutinyError>;

    /// Scan the storage for keys with a given prefix and suffix, and then gets their values
    fn scan<T>(&self, prefix: &str, suffix: Option<&str>) -> Result<HashMap<String, T>, MutinyError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let keys = self.scan_keys(prefix, suffix)?;

        let mut map = HashMap::with_capacity(keys.len());

        for key in keys {
            let kv = self.get_data::<T>(key.clone())?;
            if let Some(v) = kv {
                map.insert(key, v);
            }
        }

        Ok(map)
    }

    /// Insert a mnemonic into the storage
    fn insert_mnemonic(&self, mnemonic: Mnemonic) -> Result<Mnemonic, MutinyError> {
        self.write_data(MNEMONIC_KEY.to_string(), &mnemonic, None)?;
        Ok(mnemonic)
    }

    /// Get the mnemonic from the storage
    fn get_mnemonic(&self) -> Result<Option<Mnemonic>, MutinyError> {
        self.get_data(MNEMONIC_KEY)
    }

    fn change_password(
        &mut self,
        new: Option<String>,
        new_cipher: Option<Cipher>,
    ) -> Result<(), MutinyError>;

    fn change_password_and_rewrite_storage(
        &mut self,
        old: Option<String>,
        new: Option<String>,
    ) -> Result<(), MutinyError> {
        // check if old password is correct
        if old != self.password().map(|s| s.to_owned()) {
            return Err(MutinyError::IncorrectPassword);
        }

        // get all of our keys
        let mut keys: Vec<String> = self.scan_keys("", None)?;
        // get the ones that need encryption
        keys.retain(|k| needs_encryption(k));

        // decrypt all of the values
        let mut values: HashMap<String, Value> = HashMap::new();
        for key in keys {
            let value = self.get_data(&key)?;
            if let Some(v) = value {
                values.insert(key.to_owned(), v);
            }
        }

        // change the password
        let new_cipher = new
            .as_ref()
            .filter(|p| !p.is_empty())
            .map(|p| encryption_key_from_pass(p))
            .transpose()?;
        self.change_password(new, new_cipher)?;

        // encrypt all of the values
        for (key, value) in values {
            self.write_data(key, value, None)?;
        }

        Ok(())
    }

    /// Override the storage with the new JSON object
    async fn import(json: Value) -> Result<(), MutinyError>;

    /// Deletes all data from the storage
    async fn clear() -> Result<(), MutinyError>;

    /// Deletes all data from the storage and removes lock from VSS
    async fn delete_all(&self) -> Result<(), MutinyError> {
        Self::clear().await?;
        // remove lock from VSS if is is enabled
        if self.vss_client().is_some() {
            let device = self.get_device_id()?;
            // set time to 0 to unlock
            let lock = DeviceLock { time: 0, device };
            // still update the version so it is written to VSS
            let time = now().as_secs() as u32;
            self.write_data(DEVICE_LOCK_KEY.to_string(), lock, Some(time))?;
        }

        Ok(())
    }

    /// Gets the node indexes from storage
    fn get_nodes(&self) -> Result<NodeStorage, MutinyError> {
        let res: Option<NodeStorage> = self.get_data(NODES_KEY)?;
        match res {
            Some(nodes) => Ok(nodes),
            None => Ok(NodeStorage::default()),
        }
    }

    /// Inserts the node indexes into storage
    fn insert_nodes(&self, nodes: &NodeStorage) -> Result<(), MutinyError> {
        let version = Some(nodes.version);
        self.write_data(NODES_KEY.to_string(), nodes, version)
    }

    /// Get the current fee estimates from storage
    /// The key is block target, the value is the fee in satoshis per byte
    fn get_fee_estimates(&self) -> Result<Option<HashMap<String, f64>>, MutinyError> {
        self.get_data(FEE_ESTIMATES_KEY)
    }

    /// Inserts the fee estimates into storage
    /// The key is block target, the value is the fee in satoshis per byte
    fn insert_fee_estimates(&self, fees: HashMap<String, f64>) -> Result<(), MutinyError> {
        self.write_data(FEE_ESTIMATES_KEY.to_string(), fees, None)
    }

    /// Gets a channel closure and handles setting the user_channel_id if needed
    fn get_channel_closure(&self, key: &str) -> Result<Option<ChannelClosure>, MutinyError> {
        if let Some(mut closure) = self.get_data::<ChannelClosure>(key)? {
            closure.set_user_channel_id_from_key(key)?;
            Ok(Some(closure))
        } else {
            Ok(None)
        }
    }

    /// Get the current bitcoin price cache from storage
    fn get_bitcoin_price_cache(&self) -> Result<HashMap<String, f32>, MutinyError> {
        Ok(self.get_data(BITCOIN_PRICE_CACHE_KEY)?.unwrap_or_default())
    }

    /// Inserts the bitcoin price cache into storage
    fn insert_bitcoin_price_cache(&self, prices: HashMap<String, f32>) -> Result<(), MutinyError> {
        self.write_data(BITCOIN_PRICE_CACHE_KEY.to_string(), prices, None)
    }

    fn has_done_first_sync(&self) -> Result<bool, MutinyError> {
        self.get_data::<bool>(FIRST_SYNC_KEY)
            .map(|v| v == Some(true))
    }

    fn set_done_first_sync(&self) -> Result<(), MutinyError> {
        self.write_data(FIRST_SYNC_KEY.to_string(), true, None)
    }

    fn get_dm_sync_time(&self, is_hermes: bool) -> Result<Option<u64>, MutinyError> {
        let key = if is_hermes {
            LAST_HERMES_SYNC_TIME_KEY
        } else {
            LAST_DM_SYNC_TIME_KEY
        };
        self.get_data(key)
    }

    fn set_dm_sync_time(&self, time: u64, is_hermes: bool) -> Result<(), MutinyError> {
        let key = if is_hermes {
            LAST_HERMES_SYNC_TIME_KEY
        } else {
            LAST_DM_SYNC_TIME_KEY
        };

        // only update if the time is newer
        let current = self.get_dm_sync_time(is_hermes)?.unwrap_or_default();
        if current < time {
            self.write_data(key.to_string(), time, None)
        } else {
            Ok(())
        }
    }

    fn get_nwc_sync_time(&self) -> Result<Option<u64>, MutinyError> {
        self.get_data(LAST_NWC_SYNC_TIME_KEY)
    }

    fn set_nwc_sync_time(&self, time: u64) -> Result<(), MutinyError> {
        // only update if the time is newer
        let current = self.get_nwc_sync_time()?.unwrap_or_default();
        if current < time {
            self.write_data(LAST_NWC_SYNC_TIME_KEY.to_string(), time, None)
        } else {
            Ok(())
        }
    }

    fn get_device_id(&self) -> Result<String, MutinyError> {
        match self.get_data(DEVICE_ID_KEY)? {
            Some(id) => Ok(id),
            None => {
                let new_id = Uuid::new_v4().to_string();
                self.write_data(DEVICE_ID_KEY.to_string(), &new_id, None)?;
                Ok(new_id)
            }
        }
    }

    fn get_device_lock(&self) -> Result<Option<DeviceLock>, MutinyError> {
        self.get_data(DEVICE_LOCK_KEY)
    }

    fn set_device_lock(&self, logger: &MutinyLogger) -> Result<(), MutinyError> {
        let device = self.get_device_id()?;
        if let Some(lock) = self.get_device_lock()? {
            if lock.is_locked(&device) {
                log_debug!(logger, "current device is {}", device);
                log_debug!(logger, "locked device is {}", lock.device);
                return Err(MutinyError::AlreadyRunning);
            }
        }

        let time = now().as_secs() as u32;
        let lock = DeviceLock { time, device };
        self.write_data(DEVICE_LOCK_KEY.to_string(), lock, Some(time))
    }

    fn release_device_lock(&self, logger: &MutinyLogger) -> Result<(), MutinyError> {
        let device = self.get_device_id()?;
        if let Some(lock) = self.get_device_lock()? {
            if lock.is_locked(&device) {
                log_debug!(logger, "current device is {}", device);
                log_debug!(logger, "locked device is {}", lock.device);
                return Err(MutinyError::AlreadyRunning);
            }
        }

        let time = 0;
        let lock = DeviceLock { time, device };
        let version = now().as_secs() as u32;
        self.write_data(DEVICE_LOCK_KEY.to_string(), lock, Some(version))
    }

    async fn fetch_device_lock(&self) -> Result<Option<DeviceLock>, MutinyError>;

    /// Write Wallet changeset
    fn write_changes(&self, changeset: &ChangeSet) -> Result<(), MutinyError> {
        if changeset.is_empty() {
            return Ok(());
        }

        match self.get_data::<ChangeSet>(KEYCHAIN_STORE_KEY)? {
            Some(mut keychain_store) => {
                keychain_store.merge(changeset.clone());
                self.write_data(KEYCHAIN_STORE_KEY.to_string(), keychain_store, None)
            }
            None => self.write_data(KEYCHAIN_STORE_KEY.to_string(), changeset, None),
        }
    }

    /// Read Wallet changeset
    fn read_changes(&self) -> Result<Option<ChangeSet>, MutinyError> {
        self.get_data(KEYCHAIN_STORE_KEY)
    }

    /// Spawn background task to run db tasks
    fn spawn<Fut: Task>(&self, _fut: Fut);
}

#[derive(Clone)]
pub struct MemoryStorage {
    pub password: Option<String>,
    pub cipher: Option<Cipher>,
    pub memory: Arc<RwLock<HashMap<String, Value>>>,
    pub vss_client: Option<Arc<MutinyVssClient>>,
    delayed_keys: Arc<Mutex<HashMap<String, DelayedKeyValueItem>>>,
    pub activity_index: Arc<RwLock<BTreeSet<IndexItem>>>,
    tasks: Arc<DBTasks>,
}

impl MemoryStorage {
    pub fn new(
        password: Option<String>,
        cipher: Option<Cipher>,
        vss_client: Option<Arc<MutinyVssClient>>,
    ) -> Self {
        Self {
            cipher,
            password,
            memory: Arc::new(RwLock::new(HashMap::new())),
            vss_client,
            delayed_keys: Arc::new(Mutex::new(HashMap::new())),
            activity_index: Arc::new(RwLock::new(BTreeSet::new())),
            tasks: Arc::new(DBTasks::default()),
        }
    }

    pub async fn load_from_vss(&self) -> Result<(), MutinyError> {
        if let Some(vss) = self.vss_client() {
            let keys = vss.list_key_versions(None).await?;
            let mut items = HashMap::new();
            for key in keys {
                let obj = vss.get_object(&key.key).await?;
                items.insert(key.key, obj.value);
            }
            let mut map = self
                .memory
                .try_write()
                .map_err(|e| MutinyError::write_err(e.into()))?;
            map.extend(items);
        }

        Ok(())
    }
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new(None, None, None)
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl MutinyStorage for MemoryStorage {
    fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    fn cipher(&self) -> Option<Cipher> {
        self.cipher.to_owned()
    }

    fn vss_client(&self) -> Option<Arc<MutinyVssClient>> {
        self.vss_client.clone()
    }

    fn activity_index(&self) -> Arc<RwLock<BTreeSet<IndexItem>>> {
        self.activity_index.clone()
    }

    fn write_raw<T>(&self, items: Vec<(String, T)>) -> Result<(), MutinyError>
    where
        T: Serialize + Send,
    {
        let mut map = self
            .memory
            .try_write()
            .map_err(|e| MutinyError::write_err(e.into()))?;
        for (key, value) in items {
            let data = serde_json::to_value(value).map_err(|e| MutinyError::PersistenceFailed {
                source: MutinyStorageError::SerdeError { source: e },
            })?;
            map.insert(key, data);
        }

        Ok(())
    }

    fn get<T>(&self, key: impl AsRef<str>) -> Result<Option<T>, MutinyError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let map = self
            .memory
            .try_read()
            .map_err(|e| MutinyError::read_err(e.into()))?;

        match map.get(key.as_ref()) {
            None => Ok(None),
            Some(value) => {
                let data: T = serde_json::from_value(value.to_owned())?;
                Ok(Some(data))
            }
        }
    }

    fn delete(&self, keys: &[impl AsRef<str>]) -> Result<(), MutinyError> {
        let mut map = self
            .memory
            .try_write()
            .map_err(|e| MutinyError::write_err(e.into()))?;

        for key in keys {
            map.remove(key.as_ref());
        }

        Ok(())
    }

    async fn start(&mut self) -> Result<(), MutinyError> {
        Ok(())
    }

    async fn stop(&self) {
        self.tasks.wait().await
    }

    fn connected(&self) -> Result<bool, MutinyError> {
        Ok(false)
    }

    fn scan_keys(&self, prefix: &str, suffix: Option<&str>) -> Result<Vec<String>, MutinyError> {
        let map = self
            .memory
            .try_read()
            .map_err(|e| MutinyError::read_err(e.into()))?;

        Ok(map
            .keys()
            .filter(|key| {
                key.starts_with(prefix) && (suffix.is_none() || key.ends_with(suffix.unwrap()))
            })
            .cloned()
            .collect())
    }

    fn change_password(
        &mut self,
        new: Option<String>,
        new_cipher: Option<Cipher>,
    ) -> Result<(), MutinyError> {
        self.password = new;
        self.cipher = new_cipher;
        Ok(())
    }

    async fn import(_json: Value) -> Result<(), MutinyError> {
        Ok(())
    }

    async fn clear() -> Result<(), MutinyError> {
        Ok(())
    }

    async fn fetch_device_lock(&self) -> Result<Option<DeviceLock>, MutinyError> {
        self.get_device_lock()
    }

    fn get_delayed_objects(&self) -> Arc<Mutex<HashMap<String, DelayedKeyValueItem>>> {
        self.delayed_keys.clone()
    }

    fn spawn<Fut: Task>(&self, fut: Fut) {
        let db_tasks = self.tasks.clone();
        db_tasks.inc_started();
        spawn(async move {
            let res = fut.await;
            db_tasks.inc_done();
            res.expect("DB task error")
        });
    }
}

// Dummy implementation for testing or if people want to ignore persistence
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl MutinyStorage for () {
    fn password(&self) -> Option<&str> {
        None
    }

    fn cipher(&self) -> Option<Cipher> {
        None
    }

    fn vss_client(&self) -> Option<Arc<MutinyVssClient>> {
        None
    }

    fn activity_index(&self) -> Arc<RwLock<BTreeSet<IndexItem>>> {
        Arc::new(RwLock::new(BTreeSet::new()))
    }

    fn write_raw<T: Serialize + Send>(&self, _: Vec<(String, T)>) -> Result<(), MutinyError> {
        Ok(())
    }

    fn get<T>(&self, _key: impl AsRef<str>) -> Result<Option<T>, MutinyError>
    where
        T: for<'de> Deserialize<'de>,
    {
        Ok(None)
    }

    fn delete(&self, _: &[impl AsRef<str>]) -> Result<(), MutinyError> {
        Ok(())
    }

    async fn start(&mut self) -> Result<(), MutinyError> {
        Ok(())
    }

    async fn stop(&self) {}

    fn connected(&self) -> Result<bool, MutinyError> {
        Ok(false)
    }

    fn scan_keys(&self, _prefix: &str, _suffix: Option<&str>) -> Result<Vec<String>, MutinyError> {
        Ok(Vec::new())
    }

    fn change_password(
        &mut self,
        _new: Option<String>,
        _new_cipher: Option<Cipher>,
    ) -> Result<(), MutinyError> {
        Ok(())
    }

    async fn import(_json: Value) -> Result<(), MutinyError> {
        Ok(())
    }

    async fn clear() -> Result<(), MutinyError> {
        Ok(())
    }

    async fn fetch_device_lock(&self) -> Result<Option<DeviceLock>, MutinyError> {
        self.get_device_lock()
    }

    fn get_delayed_objects(&self) -> Arc<Mutex<HashMap<String, DelayedKeyValueItem>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn spawn<Fut: futures::future::Future<Output = Result<(), MutinyError>> + 'static>(
        &self,
        _fut: Fut,
    ) {
    }
}

pub(crate) fn transaction_details_key(internal_id: Txid) -> String {
    format!(
        "{}{:x}",
        TRANSACTION_DETAILS_PREFIX_KEY,
        internal_id.to_raw_hash(),
    )
}

#[allow(dead_code)]
pub(crate) fn persist_transaction_details<S: MutinyStorage>(
    storage: &S,
    transaction_details: &TransactionDetails,
) -> Result<(), MutinyError> {
    let key = transaction_details_key(transaction_details.internal_id);
    storage.write_data(key.clone(), transaction_details, None)?;

    // insert into activity index
    match transaction_details.confirmation_time {
        bdk_chain::ConfirmationTime::Confirmed { height: _, time } => {
            let index = storage.activity_index();
            let mut index = index.try_write()?;
            // remove old version
            index.remove(&IndexItem {
                timestamp: None, // timestamp would be None for Unconfirmed
                key: key.clone(),
            });
            index.insert(IndexItem {
                timestamp: Some(time),
                key,
            });
        }
        bdk_chain::ConfirmationTime::Unconfirmed { .. } => {
            let index = storage.activity_index();
            let mut index = index.try_write()?;
            index.insert(IndexItem {
                timestamp: None,
                key,
            });
        }
    }

    Ok(())
}

#[allow(dead_code)]
// Deletes the transaction detail and removes the pending index if it exists
pub(crate) fn delete_transaction_details<S: MutinyStorage>(
    storage: &S,
    txid: Txid,
) -> Result<(), MutinyError> {
    let key = transaction_details_key(txid);
    storage.delete(&[key.clone()])?;

    // delete the pending index item, if it exists
    let index = storage.activity_index();
    let mut index = index.try_write()?;
    index.remove(&IndexItem {
        timestamp: None, // timestamp would be None for Unconfirmed
        key: key.clone(),
    });

    Ok(())
}

pub(crate) fn get_transaction_details<S: MutinyStorage>(
    storage: &S,
    internal_id: Txid,
    logger: &MutinyLogger,
) -> Option<TransactionDetails> {
    let key = transaction_details_key(internal_id);
    log_trace!(logger, "Trace: checking payment key: {key}");
    match storage.get_data(&key).transpose() {
        Some(Ok(v)) => Some(v),
        _ => None,
    }
}

pub(crate) fn payment_key(inbound: bool, payment_hash: &[u8; 32]) -> String {
    if inbound {
        format!("{}{}", PAYMENT_INBOUND_PREFIX_KEY, payment_hash.as_hex())
    } else {
        format!("{}{}", PAYMENT_OUTBOUND_PREFIX_KEY, payment_hash.as_hex())
    }
}

pub(crate) fn persist_payment_info<S: MutinyStorage>(
    storage: &S,
    payment_hash: &[u8; 32],
    payment_info: &PaymentInfo,
    inbound: bool,
) -> Result<(), MutinyError> {
    let key = payment_key(inbound, payment_hash);
    storage.write_data(
        key.clone(),
        payment_info.clone(),
        Some(payment_info.last_update as u32),
    )?;

    // insert into activity index
    match payment_info.status {
        HTLCStatus::InFlight => {
            let index = storage.activity_index();
            let mut index = index.try_write()?;
            index.insert(IndexItem {
                timestamp: None,
                key,
            });
        }
        HTLCStatus::Succeeded => {
            let index = storage.activity_index();
            let mut index = index.try_write()?;
            // remove old version
            index.remove(&IndexItem {
                timestamp: None, // timestamp would be None for InFlight / Pending
                key: key.clone(),
            });
            index.insert(IndexItem {
                timestamp: Some(payment_info.last_update),
                key,
            });
        }
        HTLCStatus::Failed => {
            let index = storage.activity_index();
            let mut index = index.try_write()?;
            index.remove(&IndexItem {
                timestamp: None, // timestamp would be None for InFlight / Pending
                key,
            });
        }
        HTLCStatus::Pending => {} // don't add to index until invoice is paid
    }

    Ok(())
}

pub(crate) fn get_invoice_by_hash<S: MutinyStorage>(
    hash: &bitcoin::hashes::sha256::Hash,
    storage: &S,
    logger: &MutinyLogger,
) -> Result<MutinyInvoice, MutinyError> {
    let (payment_info, inbound) = get_payment_info(storage, hash, logger)?;
    let labels_map = storage.get_invoice_labels()?;
    let labels = payment_info
        .bolt11
        .as_ref()
        .and_then(|inv| labels_map.get(inv).cloned())
        .unwrap_or_default();

    MutinyInvoice::from(
        payment_info,
        PaymentHash(*hash.as_byte_array()),
        inbound,
        labels,
    )
}

pub(crate) fn get_payment_info<S: MutinyStorage>(
    storage: &S,
    payment_hash: &bitcoin::hashes::sha256::Hash,
    logger: &MutinyLogger,
) -> Result<(PaymentInfo, bool), MutinyError> {
    // try inbound first
    let payment_hash = payment_hash.as_byte_array();
    if let Some(payment_info) = read_payment_info(storage, payment_hash, true, logger) {
        return Ok((payment_info, true));
    }

    // if no inbound check outbound
    match read_payment_info(storage, payment_hash, false, logger) {
        Some(payment_info) => Ok((payment_info, false)),
        None => Err(MutinyError::NotFound),
    }
}

pub(crate) fn read_payment_info<S: MutinyStorage>(
    storage: &S,
    payment_hash: &[u8; 32],
    inbound: bool,
    logger: &MutinyLogger,
) -> Option<PaymentInfo> {
    let key = payment_key(inbound, payment_hash);
    log_trace!(logger, "Trace: checking payment key: {key}");
    match storage.get_data(&key).transpose() {
        Some(Ok(v)) => Some(v),
        _ => {
            // To scan for the old format that had `_{node_id}` at the end
            if let Ok(map) = storage.scan(&key, None) {
                map.into_values().next()
            } else {
                None
            }
        }
    }
}

pub(crate) fn list_payment_info<S: MutinyStorage>(
    storage: &S,
    inbound: bool,
) -> Result<Vec<(PaymentHash, PaymentInfo)>, MutinyError> {
    let prefix = match inbound {
        true => PAYMENT_INBOUND_PREFIX_KEY,
        false => PAYMENT_OUTBOUND_PREFIX_KEY,
    };
    let map: HashMap<String, PaymentInfo> = storage.scan(prefix, None)?;

    // convert keys to PaymentHash
    Ok(map
        .into_iter()
        .map(|(key, value)| {
            let payment_hash_str = get_payment_hash_from_key(key.as_str(), prefix);
            let hash: [u8; 32] =
                FromHex::from_hex(payment_hash_str).expect("key should be a sha256 hash");
            (PaymentHash(hash), value)
        })
        .collect())
}

#[derive(Clone)]
pub struct OnChainStorage<S: MutinyStorage>(pub(crate) S);

// impl<K, S: MutinyStorage> PersistBackend<K> for OnChainStorage<S>
// where
//     K: Default + Clone + Append + serde::Serialize + serde::de::DeserializeOwned,
// {
//     type WriteError = MutinyError;
//     type LoadError = MutinyError;

//     fn write_changes(&mut self, changeset: &K) -> Result<(), Self::WriteError> {
//         if changeset.is_empty() {
//             return Ok(());
//         }

//         match self.0.get_data::<K>(KEYCHAIN_STORE_KEY)? {
//             Some(mut keychain_store) => {
//                 keychain_store.append(changeset.clone());
//                 self.0
//                     .set_data(KEYCHAIN_STORE_KEY.to_string(), keychain_store, None)
//             }
//             None => self
//                 .0
//                 .set_data(KEYCHAIN_STORE_KEY.to_string(), changeset, None),
//         }
//     }

//     fn load_from_persistence(&mut self) -> Result<Option<K>, Self::LoadError> {
//         self.0.get_data(KEYCHAIN_STORE_KEY)
//     }
// }

pub(crate) fn get_payment_hash_from_key<'a>(key: &'a str, prefix: &str) -> &'a str {
    key.trim_start_matches(prefix)
        .splitn(2, '_') // To support the old format that had `_{node_id}` at the end
        .collect::<Vec<&str>>()[0]
}

#[cfg(test)]
mod tests {
    use crate::test_utils::*;

    use crate::{encrypt::encryption_key_from_pass, storage::MemoryStorage};
    use crate::{keymanager, storage::MutinyStorage};

    use wasm_bindgen_test::{wasm_bindgen_test as test, wasm_bindgen_test_configure};

    wasm_bindgen_test_configure!(run_in_browser);

    #[test]
    async fn insert_and_get_mnemonic_no_password() {
        let test_name = "insert_and_get_mnemonic_no_password";
        log!("{}", test_name);

        let seed = keymanager::generate_seed(12).unwrap();

        let storage = MemoryStorage::default();
        let mnemonic = storage.insert_mnemonic(seed).unwrap();

        let stored_mnemonic = storage.get_mnemonic().unwrap();
        assert_eq!(Some(mnemonic), stored_mnemonic);
    }

    #[test]
    async fn insert_and_get_mnemonic_with_password() {
        let test_name = "insert_and_get_mnemonic_with_password";
        log!("{}", test_name);

        let seed = keymanager::generate_seed(12).unwrap();

        let pass = uuid::Uuid::new_v4().to_string();
        let cipher = encryption_key_from_pass(&pass).unwrap();
        let storage = MemoryStorage::new(Some(pass), Some(cipher), None);

        let mnemonic = storage.insert_mnemonic(seed).unwrap();

        let stored_mnemonic = storage.get_mnemonic().unwrap();
        assert_eq!(Some(mnemonic), stored_mnemonic);
    }

    // #[test]
    // async fn test_device_lock() {
    //     let test_name = "test_device_lock";
    //     log!("{}", test_name);

    //     let vss = std::sync::Arc::new(create_vss_client().await);
    //     let storage = MemoryStorage::new(None, None, Some(vss.clone()));
    //     storage.load_from_vss().await.unwrap();

    //     let logger = Arc::new(MutinyLogger::default());

    //     let id = storage.get_device_id().unwrap();
    //     let lock = storage.get_device_lock().unwrap();
    //     assert_eq!(None, lock);

    //     storage.set_device_lock(&logger).await.unwrap();
    //     // sleep 1 second to make sure it writes to VSS
    //     sleep(1_000).await;

    //     let lock = storage.get_device_lock().unwrap();
    //     assert!(lock.is_some());
    //     assert!(!lock.clone().unwrap().is_locked(&id));
    //     assert!(lock.clone().unwrap().is_last_locker(&id));
    //     assert!(lock.clone().unwrap().is_locked("different_id"));
    //     assert!(!lock.clone().unwrap().is_last_locker("different_id"));
    //     assert_eq!(lock.unwrap().device, id);

    //     // make sure we can set lock again, should work because same device id
    //     storage.set_device_lock(&logger).await.unwrap();
    //     // sleep 1 second to make sure it writes to VSS
    //     sleep(1_000).await;

    //     // create new storage with new device id and make sure we can't set lock
    //     let storage = MemoryStorage::new(None, None, Some(vss));
    //     storage.load_from_vss().await.unwrap();

    //     let new_id = storage.get_device_id().unwrap();
    //     assert_ne!(id, new_id);

    //     let lock = storage.get_device_lock().unwrap();
    //     assert!(lock.is_some());
    //     // not locked for active device
    //     assert!(!lock.clone().unwrap().is_locked(&id));
    //     assert!(lock.clone().unwrap().is_last_locker(&id));
    //     // is locked for new device
    //     assert!(lock.clone().unwrap().is_locked(&new_id));
    //     assert!(!lock.clone().unwrap().is_last_locker(&new_id));
    //     assert_eq!(lock.unwrap().device, id);

    //     assert_eq!(
    //         storage.set_device_lock(&logger).await,
    //         Err(crate::MutinyError::AlreadyRunning)
    //     );
    // }
}
