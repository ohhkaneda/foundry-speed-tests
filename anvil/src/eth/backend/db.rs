//! Helper types for working with [revm](foundry_evm::revm)

use crate::{mem::state::trie_hash_db, revm::AccountInfo, U256};
use anvil_core::eth::trie::KeccakHasher;
use ethers::{
    prelude::{Address, Bytes, H160},
    types::H256,
    utils::keccak256,
};
use forge::revm::KECCAK_EMPTY;
use foundry_common::errors::FsPathError;
use foundry_evm::{
    executor::{
        backend::{snapshot::StateSnapshot, DatabaseError, DatabaseResult, MemDb},
        DatabaseRef,
    },
    revm::{
        db::{CacheDB, DbAccount},
        Bytecode, Database, DatabaseCommit,
    },
    HashMap,
};
use hash_db::HashDB;
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path};

/// Type alias for the `HashDB` representation of the Database
pub type AsHashDB = Box<dyn HashDB<KeccakHasher, Vec<u8>>>;

/// Helper trait get access to the data in `HashDb` form
#[auto_impl::auto_impl(Box)]
pub trait MaybeHashDatabase: DatabaseRef<Error = DatabaseError> {
    /// Return the DB as read-only hashdb and the root key
    fn maybe_as_hash_db(&self) -> Option<(AsHashDB, H256)> {
        None
    }
    /// Return the storage DB as read-only hashdb and the storage root of the account
    fn maybe_account_db(&self, _addr: Address) -> Option<(AsHashDB, H256)> {
        None
    }

    /// Clear the state and move it into a new `StateSnapshot`
    fn clear_into_snapshot(&mut self) -> StateSnapshot;

    /// Clears the entire database
    fn clear(&mut self);

    /// Reverses `clear_into_snapshot` by initializing the db's state with the snapshot
    fn init_from_snapshot(&mut self, snapshot: StateSnapshot);
}

impl<'a, T: 'a + MaybeHashDatabase + ?Sized> MaybeHashDatabase for &'a T
where
    &'a T: DatabaseRef<Error = DatabaseError>,
{
    fn maybe_as_hash_db(&self) -> Option<(AsHashDB, H256)> {
        T::maybe_as_hash_db(self)
    }
    fn maybe_account_db(&self, addr: Address) -> Option<(AsHashDB, H256)> {
        T::maybe_account_db(self, addr)
    }

    fn clear_into_snapshot(&mut self) -> StateSnapshot {
        unreachable!("never called for DatabaseRef")
    }

    fn clear(&mut self) {}

    fn init_from_snapshot(&mut self, _snapshot: StateSnapshot) {}
}

/// This bundles all required revm traits
pub trait Db:
    DatabaseRef<Error = DatabaseError>
    + Database<Error = DatabaseError>
    + DatabaseCommit
    + MaybeHashDatabase
    + fmt::Debug
    + Send
    + Sync
{
    /// Inserts an account
    fn insert_account(&mut self, address: Address, account: AccountInfo);

    /// Sets the nonce of the given address
    fn set_nonce(&mut self, address: Address, nonce: u64) -> DatabaseResult<()> {
        let mut info = self.basic(address)?.unwrap_or_default();
        info.nonce = nonce;
        self.insert_account(address, info);
        Ok(())
    }

    /// Sets the balance of the given address
    fn set_balance(&mut self, address: Address, balance: U256) -> DatabaseResult<()> {
        let mut info = self.basic(address)?.unwrap_or_default();
        info.balance = balance;
        self.insert_account(address, info);
        Ok(())
    }

    /// Sets the balance of the given address
    fn set_code(&mut self, address: Address, code: Bytes) -> DatabaseResult<()> {
        let mut info = self.basic(address)?.unwrap_or_default();
        let code_hash = if code.as_ref().is_empty() {
            KECCAK_EMPTY
        } else {
            H256::from_slice(&keccak256(code.as_ref())[..])
        };
        info.code_hash = code_hash;
        info.code = Some(Bytecode::new_raw(code.0).to_checked());
        self.insert_account(address, info);
        Ok(())
    }

    /// Sets the balance of the given address
    fn set_storage_at(&mut self, address: Address, slot: U256, val: U256) -> DatabaseResult<()>;

    /// inserts a blockhash for the given number
    fn insert_block_hash(&mut self, number: U256, hash: H256);

    /// Write all chain data to serialized bytes buffer
    fn dump_state(&self) -> DatabaseResult<Option<SerializableState>>;

    /// Deserialize and add all chain data to the backend storage
    fn load_state(&mut self, buf: SerializableState) -> DatabaseResult<bool>;

    /// Creates a new snapshot
    fn snapshot(&mut self) -> U256;

    /// Reverts a snapshot
    ///
    /// Returns `true` if the snapshot was reverted
    fn revert(&mut self, snapshot: U256) -> bool;

    /// Returns the state root if possible to compute
    fn maybe_state_root(&self) -> Option<H256> {
        None
    }

    /// Returns the current, standalone state of the Db
    fn current_state(&self) -> StateDb;
}

/// Convenience impl only used to use any `Db` on the fly as the db layer for revm's CacheDB
/// This is useful to create blocks without actually writing to the `Db`, but rather in the cache of
/// the `CacheDB` see also
/// [Backend::pending_block()](crate::eth::backend::mem::Backend::pending_block())
impl<T: DatabaseRef<Error = DatabaseError> + Send + Sync + Clone + fmt::Debug> Db for CacheDB<T> {
    fn insert_account(&mut self, address: Address, account: AccountInfo) {
        self.insert_account_info(address, account)
    }

    fn set_storage_at(&mut self, address: Address, slot: U256, val: U256) -> DatabaseResult<()> {
        self.insert_account_storage(address, slot, val)
    }

    fn insert_block_hash(&mut self, number: U256, hash: H256) {
        self.block_hashes.insert(number, hash);
    }

    fn dump_state(&self) -> DatabaseResult<Option<SerializableState>> {
        Ok(None)
    }

    fn load_state(&mut self, _buf: SerializableState) -> DatabaseResult<bool> {
        Ok(false)
    }

    fn snapshot(&mut self) -> U256 {
        U256::zero()
    }

    fn revert(&mut self, _snapshot: U256) -> bool {
        false
    }

    fn current_state(&self) -> StateDb {
        StateDb::new(MemDb::default())
    }
}

impl<T: DatabaseRef<Error = DatabaseError>> MaybeHashDatabase for CacheDB<T> {
    fn maybe_as_hash_db(&self) -> Option<(AsHashDB, H256)> {
        Some(trie_hash_db(&self.accounts))
    }
    fn clear_into_snapshot(&mut self) -> StateSnapshot {
        let db_accounts = std::mem::take(&mut self.accounts);
        let mut accounts = HashMap::new();
        let mut account_storage = HashMap::new();

        for (addr, mut acc) in db_accounts {
            account_storage.insert(addr, std::mem::take(&mut acc.storage));
            let mut info = acc.info;
            info.code = self.contracts.remove(&info.code_hash);
            accounts.insert(addr, info);
        }
        let block_hashes = std::mem::take(&mut self.block_hashes);
        StateSnapshot { accounts, storage: account_storage, block_hashes }
    }

    fn clear(&mut self) {
        self.clear_into_snapshot();
    }

    fn init_from_snapshot(&mut self, snapshot: StateSnapshot) {
        let StateSnapshot { accounts, mut storage, block_hashes } = snapshot;

        for (addr, mut acc) in accounts {
            if let Some(code) = acc.code.take() {
                self.contracts.insert(acc.code_hash, code);
            }
            self.accounts.insert(
                addr,
                DbAccount {
                    info: acc,
                    storage: storage.remove(&addr).unwrap_or_default(),
                    ..Default::default()
                },
            );
        }
        self.block_hashes = block_hashes;
    }
}

/// Represents a state at certain point
pub struct StateDb(pub(crate) Box<dyn MaybeHashDatabase + Send + Sync>);

// === impl StateDB ===

impl StateDb {
    pub fn new(db: impl MaybeHashDatabase + Send + Sync + 'static) -> Self {
        Self(Box::new(db))
    }
}

impl DatabaseRef for StateDb {
    type Error = DatabaseError;
    fn basic(&self, address: H160) -> DatabaseResult<Option<AccountInfo>> {
        self.0.basic(address)
    }

    fn code_by_hash(&self, code_hash: H256) -> DatabaseResult<Bytecode> {
        self.0.code_by_hash(code_hash)
    }

    fn storage(&self, address: H160, index: U256) -> DatabaseResult<U256> {
        self.0.storage(address, index)
    }

    fn block_hash(&self, number: U256) -> DatabaseResult<H256> {
        self.0.block_hash(number)
    }
}

impl MaybeHashDatabase for StateDb {
    fn maybe_as_hash_db(&self) -> Option<(AsHashDB, H256)> {
        self.0.maybe_as_hash_db()
    }

    fn maybe_account_db(&self, addr: Address) -> Option<(AsHashDB, H256)> {
        self.0.maybe_account_db(addr)
    }

    fn clear_into_snapshot(&mut self) -> StateSnapshot {
        self.0.clear_into_snapshot()
    }

    fn clear(&mut self) {
        self.0.clear()
    }

    fn init_from_snapshot(&mut self, snapshot: StateSnapshot) {
        self.0.init_from_snapshot(snapshot)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SerializableState {
    pub accounts: HashMap<Address, SerializableAccountRecord>,
}

// === impl SerializableState ===

impl SerializableState {
    /// Loads the `Genesis` object from the given json file path
    pub fn load(path: impl AsRef<Path>) -> Result<Self, FsPathError> {
        let path = path.as_ref();
        if path.is_dir() {
            foundry_common::fs::read_json_file(&path.join("state.json"))
        } else {
            foundry_common::fs::read_json_file(path)
        }
    }

    /// This is used as the clap `value_parser` implementation
    pub(crate) fn parse(path: &str) -> Result<Self, String> {
        Self::load(path).map_err(|err| err.to_string())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SerializableAccountRecord {
    pub nonce: u64,
    pub balance: U256,
    pub code: Bytes,
    pub storage: HashMap<U256, U256>,
}
