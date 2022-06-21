// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::convert::TryFrom;
use std::error;
use std::fmt;
use std::thread;

use crate::chainstate::stacks::boot::BOOT_CODE_COSTS_2_TESTNET;
use crate::chainstate::stacks::boot::{
    BOOT_CODE_COSTS, BOOT_CODE_COSTS_2, BOOT_CODE_COST_VOTING_TESTNET as BOOT_CODE_COST_VOTING,
    BOOT_CODE_POX_TESTNET, COSTS_2_NAME,
};
use crate::chainstate::stacks::db::StacksAccount;
use crate::chainstate::stacks::db::StacksChainState;
use crate::chainstate::stacks::events::{StacksTransactionEvent, StacksTransactionReceipt};
use crate::chainstate::stacks::index::marf::MARF;
use crate::chainstate::stacks::index::ClarityMarfTrieId;
use crate::chainstate::stacks::index::MarfTrieId;
use crate::chainstate::stacks::Error as ChainstateError;
use crate::chainstate::stacks::StacksMicroblockHeader;
use crate::chainstate::stacks::TransactionAuth;
use crate::chainstate::stacks::TransactionPayload;
use crate::chainstate::stacks::TransactionPublicKeyEncoding;
use crate::chainstate::stacks::TransactionSmartContract;
use crate::chainstate::stacks::TransactionSpendingCondition;
use crate::chainstate::stacks::TransactionVersion;
use crate::chainstate::stacks::{SinglesigHashMode, SinglesigSpendingCondition, StacksTransaction};
use crate::core::StacksEpoch;
use crate::core::FIRST_STACKS_BLOCK_ID;
use crate::core::GENESIS_EPOCH;
use crate::types::chainstate::BlockHeaderHash;
use crate::types::chainstate::SortitionId;
use crate::types::chainstate::StacksBlockId;
use crate::types::chainstate::TrieHash;
use crate::util::secp256k1::MessageSignature;
use crate::util_lib::boot::{boot_code_acc, boot_code_addr, boot_code_id, boot_code_tx_auth};
use crate::util_lib::strings::StacksString;
use crate::{
    burnchains::Burnchain,
    clarity_vm::database::marf::{MarfedKV, WritableMarfStore},
};
use crate::{clarity_vm::database::marf::ReadOnlyMarfStore, core::StacksEpochId};
use clarity::vm::analysis;
use clarity::vm::analysis::AnalysisDatabase;
use clarity::vm::analysis::{errors::CheckError, errors::CheckErrors, ContractAnalysis};
use clarity::vm::ast;
use clarity::vm::ast::{errors::ParseError, errors::ParseErrors, ContractAST};
use clarity::vm::contexts::{AssetMap, Environment, OwnedEnvironment};
use clarity::vm::costs::{CostTracker, ExecutionCost, LimitedCostTracker};
use clarity::vm::database::{
    BurnStateDB, ClarityDatabase, HeadersDB, RollbackWrapper, RollbackWrapperPersistedLog,
    STXBalance, SqliteConnection, NULL_BURN_STATE_DB, NULL_HEADER_DB,
};
use clarity::vm::errors::Error as InterpreterError;
use clarity::vm::representations::SymbolicExpression;
use clarity::vm::types::{
    AssetIdentifier, PrincipalData, QualifiedContractIdentifier, TypeSignature, Value,
};
use clarity::vm::ContractName;
use stacks_common::types::chainstate::BurnchainHeaderHash;

use crate::util_lib::db::Error as DatabaseError;
pub use clarity::vm::clarity::ClarityConnection;
pub use clarity::vm::clarity::Error;
use clarity::vm::clarity::TransactionConnection;

///
/// A high-level interface for interacting with the Clarity VM.
///
/// ClarityInstance takes ownership of a MARF + Sqlite store used for
///   it's data operations.
/// The ClarityInstance defines a `begin_block(bhh, bhh, bhh) -> ClarityBlockConnection`
///    function.
/// ClarityBlockConnections are used for executing transactions within the context of
///    a single block.
/// Only one ClarityBlockConnection may be open at a time (enforced by the borrow checker)
///   and ClarityBlockConnections must be `commit_block`ed or `rollback_block`ed before discarding
///   begining the next connection (enforced by runtime panics).
///
/// Note on generics and abstracting the structs in `clarity_vm::clarity` into `libclarity`: while
///   multiple consumers of `libclarity` may need a high-level interface like
///   instance -> block -> transaction, their lifetime parameters make the use of rust traits very
///   difficult (in all likelihood, it would require higher-ordered traits, which is a
///   discussed-but-not-yet-implemented feature of rust). Instead, consumers of `libclarity` which
///   wish to benefit from some abstraction of high-level interfaces should implement the
///   `TransactionConnection` trait, which contains auto implementations for the typical transaction
///   types in a Clarity-based blockchain.
///
pub struct ClarityInstance {
    datastore: MarfedKV,
    mainnet: bool,
}

///
/// This struct represents a "sealed" or "finished" Clarity block that
/// has *not* yet been committed. This struct allows consumers of the
/// `clarity_vm` module's high level interface to separate the
/// completion of the Clarity operations in a Stacks block from the
/// final commit to the database.
///
/// This is necessary to allow callers complete other operations like
/// preparing a commitment to the chainstate headers MARF, and
/// issuring event dispatches, before the Clarity database commits.
///
pub struct PreCommitClarityBlock<'a> {
    datastore: WritableMarfStore<'a>,
    commit_to: StacksBlockId,
}

///
/// A high-level interface for Clarity VM interactions within a single block.
///
pub struct ClarityBlockConnection<'a, 'b> {
    datastore: WritableMarfStore<'a>,
    header_db: &'b dyn HeadersDB,
    burn_state_db: &'b dyn BurnStateDB,
    cost_track: Option<LimitedCostTracker>,
    mainnet: bool,
    epoch: StacksEpochId,
}

///
/// Interface for Clarity VM interactions within a given transaction.
///
///   commit the transaction to the block with .commit()
///   rollback the transaction by dropping this struct.
pub struct ClarityTransactionConnection<'a, 'b> {
    log: Option<RollbackWrapperPersistedLog>,
    store: &'a mut WritableMarfStore<'b>,
    header_db: &'a dyn HeadersDB,
    burn_state_db: &'a dyn BurnStateDB,
    cost_track: &'a mut Option<LimitedCostTracker>,
    mainnet: bool,
    epoch: StacksEpochId,
}

pub struct ClarityReadOnlyConnection<'a> {
    datastore: ReadOnlyMarfStore<'a>,
    header_db: &'a dyn HeadersDB,
    burn_state_db: &'a dyn BurnStateDB,
    epoch: StacksEpochId,
}

impl From<ChainstateError> for Error {
    fn from(e: ChainstateError) -> Self {
        match e {
            ChainstateError::InvalidStacksTransaction(msg, _) => Error::BadTransaction(msg),
            ChainstateError::CostOverflowError(_, after, budget) => Error::CostError(after, budget),
            ChainstateError::ClarityError(x) => x,
            x => Error::BadTransaction(format!("{:?}", &x)),
        }
    }
}

/// A macro for doing take/replace on a closure.
///   macro is needed rather than a function definition because
///   otherwise, we end up breaking the borrow checker when
///   passing a mutable reference across a function boundary.
macro_rules! using {
    ($to_use: expr, $msg: expr, $exec: expr) => {{
        let object = $to_use.take().expect(&format!(
            "BUG: Transaction connection lost {} handle.",
            $msg
        ));
        let (object, result) = ($exec)(object);
        $to_use.replace(object);
        result
    }};
}

impl<'a, 'b> ClarityBlockConnection<'a, 'b> {
    /// Reset the block's total execution to the given cost, if there is a cost tracker at all.
    /// Used by the miner to "undo" applying a transaction that exceeded the budget.
    pub fn reset_block_cost(&mut self, cost: ExecutionCost) -> () {
        if let Some(ref mut cost_tracker) = self.cost_track {
            cost_tracker.set_total(cost);
        }
    }

    pub fn set_cost_tracker(&mut self, tracker: LimitedCostTracker) -> LimitedCostTracker {
        let old = self
            .cost_track
            .take()
            .expect("BUG: Clarity block connection lost cost tracker instance");
        self.cost_track.replace(tracker);
        old
    }

    /// Get the current cost so far
    pub fn cost_so_far(&self) -> ExecutionCost {
        match self.cost_track {
            Some(ref track) => track.get_total(),
            None => ExecutionCost::zero(),
        }
    }

    /// Returns the block limit for the block being created.
    pub fn block_limit(&self) -> Option<ExecutionCost> {
        match self.cost_track {
            Some(ref track) => Some(track.get_limit()),
            None => None,
        }
    }
}

impl ClarityInstance {
    pub fn new(mainnet: bool, datastore: MarfedKV) -> ClarityInstance {
        ClarityInstance { datastore, mainnet }
    }

    pub fn with_marf<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut MARF<StacksBlockId>) -> R,
    {
        f(self.datastore.get_marf())
    }

    pub fn is_mainnet(&self) -> bool {
        self.mainnet
    }

    /// Returns the Stacks epoch of the burn block that elected `stacks_block`
    fn get_epoch_of(
        stacks_block: &StacksBlockId,
        header_db: &dyn HeadersDB,
        burn_state_db: &dyn BurnStateDB,
    ) -> StacksEpoch {
        // Special case the first Stacks block -- it is not elected in any burn block
        //  so we specifically set its epoch to GENESIS_EPOCH.
        if stacks_block == &*FIRST_STACKS_BLOCK_ID {
            return burn_state_db
                .get_stacks_epoch_by_epoch_id(&GENESIS_EPOCH)
                .expect("Failed to obtain the Genesis StacksEpoch");
        }

        let burn_height = header_db
            .get_burn_block_height_for_block(stacks_block)
            .expect(&format!(
                "Failed to get burn block height of {}",
                stacks_block
            ));
        burn_state_db.get_stacks_epoch(burn_height).expect(&format!(
            "Failed to get Stacks epoch for height = {}",
            burn_height
        ))
    }

    pub fn begin_block<'a, 'b>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
        header_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a, 'b> {
        let mut datastore = self.datastore.begin(current, next);

        let epoch = Self::get_epoch_of(current, header_db, burn_state_db);
        let cost_track = {
            let mut clarity_db = datastore.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB);
            Some(
                LimitedCostTracker::new(
                    self.mainnet,
                    epoch.block_limit.clone(),
                    &mut clarity_db,
                    epoch.epoch_id,
                )
                .expect("FAIL: problem instantiating cost tracking"),
            )
        };

        ClarityBlockConnection {
            datastore,
            header_db,
            burn_state_db,
            cost_track,
            mainnet: self.mainnet,
            epoch: epoch.epoch_id,
        }
    }

    pub fn begin_genesis_block<'a, 'b>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
        header_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a, 'b> {
        let datastore = self.datastore.begin(current, next);

        let epoch = GENESIS_EPOCH;

        let cost_track = Some(LimitedCostTracker::new_free());

        ClarityBlockConnection {
            datastore,
            header_db,
            burn_state_db,
            cost_track,
            mainnet: self.mainnet,
            epoch,
        }
    }

    /// begin a genesis block with the default cost contract
    ///  used in testing + benchmarking
    pub fn begin_test_genesis_block<'a, 'b>(
        &'a mut self,
        current: &StacksBlockId,
        next: &StacksBlockId,
        header_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a, 'b> {
        let writable = self.datastore.begin(current, next);

        let epoch = GENESIS_EPOCH;

        let cost_track = Some(LimitedCostTracker::new_free());

        let mut conn = ClarityBlockConnection {
            datastore: writable,
            header_db,
            burn_state_db,
            cost_track,
            mainnet: self.mainnet,
            epoch,
        };

        let use_mainnet = self.mainnet;
        conn.as_transaction(|clarity_db| {
            let (ast, _) = clarity_db
                .analyze_smart_contract(&boot_code_id("costs", use_mainnet), BOOT_CODE_COSTS)
                .unwrap();
            clarity_db
                .initialize_smart_contract(
                    &boot_code_id("costs", use_mainnet),
                    &ast,
                    BOOT_CODE_COSTS,
                    |_, _| false,
                )
                .unwrap();
        });

        conn.as_transaction(|clarity_db| {
            let (ast, analysis) = clarity_db
                .analyze_smart_contract(
                    &boot_code_id("cost-voting", use_mainnet),
                    &*BOOT_CODE_COST_VOTING,
                )
                .unwrap();
            clarity_db
                .initialize_smart_contract(
                    &boot_code_id("cost-voting", use_mainnet),
                    &ast,
                    &*BOOT_CODE_COST_VOTING,
                    |_, _| false,
                )
                .unwrap();

            clarity_db
                .save_analysis(&boot_code_id("cost-voting", use_mainnet), &analysis)
                .unwrap();
        });

        conn.as_transaction(|clarity_db| {
            let (ast, _) = clarity_db
                .analyze_smart_contract(&boot_code_id("pox", use_mainnet), &*BOOT_CODE_POX_TESTNET)
                .unwrap();
            clarity_db
                .initialize_smart_contract(
                    &boot_code_id("pox", use_mainnet),
                    &ast,
                    &*BOOT_CODE_POX_TESTNET,
                    |_, _| false,
                )
                .unwrap();
        });

        conn
    }

    pub fn drop_unconfirmed_state(&mut self, block: &StacksBlockId) {
        let datastore = self.datastore.begin_unconfirmed(block);
        datastore.rollback_unconfirmed()
    }

    pub fn begin_unconfirmed<'a, 'b>(
        &'a mut self,
        current: &StacksBlockId,
        header_db: &'b dyn HeadersDB,
        burn_state_db: &'b dyn BurnStateDB,
    ) -> ClarityBlockConnection<'a, 'b> {
        let mut datastore = self.datastore.begin_unconfirmed(current);

        let epoch = Self::get_epoch_of(current, header_db, burn_state_db);

        let cost_track = {
            let mut clarity_db = datastore.as_clarity_db(&NULL_HEADER_DB, &NULL_BURN_STATE_DB);
            Some(
                LimitedCostTracker::new(
                    self.mainnet,
                    epoch.block_limit.clone(),
                    &mut clarity_db,
                    epoch.epoch_id,
                )
                .expect("FAIL: problem instantiating cost tracking"),
            )
        };

        ClarityBlockConnection {
            datastore,
            header_db,
            burn_state_db,
            cost_track,
            mainnet: self.mainnet,
            epoch: epoch.epoch_id,
        }
    }

    /// Open a read-only connection at `at_block`. This will be evaluated in the Stacks epoch that
    ///  was active *during* the evaluation of `at_block`
    pub fn read_only_connection<'a>(
        &'a mut self,
        at_block: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> ClarityReadOnlyConnection<'a> {
        self.read_only_connection_checked(at_block, header_db, burn_state_db)
            .expect(&format!("BUG: failed to open block {}", at_block))
    }

    /// Open a read-only connection at `at_block`. This will be evaluated in the Stacks epoch that
    ///  was active *during* the evaluation of `at_block`
    pub fn read_only_connection_checked<'a>(
        &'a mut self,
        at_block: &StacksBlockId,
        header_db: &'a dyn HeadersDB,
        burn_state_db: &'a dyn BurnStateDB,
    ) -> Result<ClarityReadOnlyConnection<'a>, Error> {
        let mut datastore = self.datastore.begin_read_only_checked(Some(at_block))?;
        let epoch = {
            let mut db = datastore.as_clarity_db(header_db, burn_state_db);
            db.begin();
            let result = db.get_clarity_epoch_version();
            db.roll_back();
            result
        };

        Ok(ClarityReadOnlyConnection {
            datastore,
            header_db,
            burn_state_db,
            epoch,
        })
    }

    pub fn trie_exists_for_block(&mut self, bhh: &StacksBlockId) -> Result<bool, DatabaseError> {
        let mut datastore = self.datastore.begin_read_only(None);
        datastore.trie_exists_for_block(bhh)
    }

    /// Evaluate program read-only at `at_block`. This will be evaluated in the Stacks epoch that
    ///  was active *during* the evaluation of `at_block`
    pub fn eval_read_only(
        &mut self,
        at_block: &StacksBlockId,
        header_db: &dyn HeadersDB,
        burn_state_db: &dyn BurnStateDB,
        contract: &QualifiedContractIdentifier,
        program: &str,
    ) -> Result<Value, Error> {
        let mut read_only_conn = self.datastore.begin_read_only(Some(at_block));
        let mut clarity_db = read_only_conn.as_clarity_db(header_db, burn_state_db);
        let epoch_id = {
            clarity_db.begin();
            let result = clarity_db.get_clarity_epoch_version();
            clarity_db.roll_back();
            result
        };

        let mut env = OwnedEnvironment::new_free(self.mainnet, clarity_db, epoch_id);
        env.eval_read_only(contract, program)
            .map(|(x, _, _)| x)
            .map_err(Error::from)
    }

    pub fn destroy(self) -> MarfedKV {
        self.datastore
    }
}

impl<'a, 'b> ClarityConnection for ClarityBlockConnection<'a, 'b> {
    /// Do something with ownership of the underlying DB that involves only reading.
    fn with_clarity_db_readonly_owned<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(ClarityDatabase) -> (R, ClarityDatabase),
    {
        let mut db =
            ClarityDatabase::new(&mut self.datastore, &self.header_db, &self.burn_state_db);
        db.begin();
        let (result, mut db) = to_do(db);
        db.roll_back();
        result
    }

    fn with_analysis_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R,
    {
        let mut db = AnalysisDatabase::new(&mut self.datastore);
        db.begin();
        let result = to_do(&mut db);
        db.roll_back();
        result
    }

    fn get_epoch(&self) -> StacksEpochId {
        self.epoch
    }
}

impl ClarityConnection for ClarityReadOnlyConnection<'_> {
    /// Do something with ownership of the underlying DB that involves only reading.
    fn with_clarity_db_readonly_owned<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(ClarityDatabase) -> (R, ClarityDatabase),
    {
        let mut db = self
            .datastore
            .as_clarity_db(&self.header_db, &self.burn_state_db);
        db.begin();
        let (result, mut db) = to_do(db);
        db.roll_back();
        result
    }

    fn with_analysis_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R,
    {
        let mut db = self.datastore.as_analysis_db();
        db.begin();
        let result = to_do(&mut db);
        db.roll_back();
        result
    }

    fn get_epoch(&self) -> StacksEpochId {
        self.epoch
    }
}

impl<'a> PreCommitClarityBlock<'a> {
    pub fn commit(self) {
        debug!("Committing Clarity block connection"; "index_block" => %self.commit_to);
        self.datastore.commit_to(&self.commit_to);
    }
}

impl<'a, 'b> ClarityBlockConnection<'a, 'b> {
    /// Rolls back all changes in the current block by
    /// (1) dropping all writes from the current MARF tip,
    /// (2) rolling back side-storage
    pub fn rollback_block(self) {
        // this is a "lower-level" rollback than the roll backs performed in
        //   ClarityDatabase or AnalysisDatabase -- this is done at the backing store level.
        debug!("Rollback Clarity datastore");
        self.datastore.rollback_block();
    }

    /// Rolls back all unconfirmed state in the current block by
    /// (1) dropping all writes from the current MARF tip,
    /// (2) rolling back side-storage
    pub fn rollback_unconfirmed(self) {
        // this is a "lower-level" rollback than the roll backs performed in
        //   ClarityDatabase or AnalysisDatabase -- this is done at the backing store level.
        debug!("Rollback unconfirmed Clarity datastore");
        self.datastore.rollback_unconfirmed();
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.
    #[cfg(test)]
    pub fn commit_block(self) -> LimitedCostTracker {
        debug!("Commit Clarity datastore");
        self.datastore.test_commit();

        self.cost_track.unwrap()
    }

    pub fn precommit_to_block(self, final_bhh: StacksBlockId) -> PreCommitClarityBlock<'a> {
        self.cost_track
            .expect("Clarity block connection lost cost tracker before commitment");
        PreCommitClarityBlock {
            datastore: self.datastore,
            commit_to: final_bhh,
        }
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.  Commits to a different
    /// block hash than the one opened (i.e. since the caller
    /// may not have known the "real" block hash at the
    /// time of opening).
    pub fn commit_to_block(self, final_bhh: &StacksBlockId) -> LimitedCostTracker {
        debug!("Commit Clarity datastore to {}", final_bhh);
        self.datastore.commit_to(final_bhh);

        self.cost_track.unwrap()
    }

    /// Commits all changes in the current block by
    /// (1) committing the current MARF tip to storage,
    /// (2) committing side-storage.
    ///    before this saves, it updates the metadata headers in
    ///    the sidestore so that they don't get stepped on after
    ///    a miner re-executes a constructed block.
    pub fn commit_mined_block(self, bhh: &StacksBlockId) -> LimitedCostTracker {
        debug!("Commit mined Clarity datastore to {}", bhh);
        self.datastore.commit_mined_block(bhh);

        self.cost_track.unwrap()
    }

    /// Save all unconfirmed state by
    /// (1) committing the current unconfirmed MARF to storage,
    /// (2) committing side-storage
    /// Unconfirmed data has globally-unique block hashes that are cryptographically derived from a
    /// confirmed block hash, so they're exceedingly unlikely to conflict with existing blocks.
    pub fn commit_unconfirmed(self) -> LimitedCostTracker {
        debug!("Save unconfirmed Clarity datastore");
        self.datastore.commit_unconfirmed();

        self.cost_track.unwrap()
    }

    pub fn initialize_epoch_2_05(&mut self) -> Result<StacksTransactionReceipt, Error> {
        // use the `using!` statement to ensure that the old cost_tracker is placed
        //  back in all branches after initialization
        using!(self.cost_track, "cost tracker", |old_cost_tracker| {
            // epoch initialization is *free*
            self.cost_track.replace(LimitedCostTracker::new_free());

            let mainnet = self.mainnet;

            // get the boot code account information
            //  for processing the pox contract initialization
            let tx_version = if mainnet {
                TransactionVersion::Mainnet
            } else {
                TransactionVersion::Testnet
            };

            let boot_code_address = boot_code_addr(mainnet);

            let boot_code_auth = boot_code_tx_auth(boot_code_address);

            let boot_code_nonce = self.with_clarity_db_readonly(|db| {
                db.get_account_nonce(&boot_code_address.clone().into())
            });

            let boot_code_account = boot_code_acc(boot_code_address, boot_code_nonce);

            // instantiate costs 2 contract...
            let cost_2_code = if mainnet {
                &*BOOT_CODE_COSTS_2
            } else {
                &*BOOT_CODE_COSTS_2_TESTNET
            };

            let payload = TransactionPayload::SmartContract(TransactionSmartContract {
                name: ContractName::try_from(COSTS_2_NAME)
                    .expect("FATAL: invalid boot-code contract name"),
                code_body: StacksString::from_str(cost_2_code)
                    .expect("FATAL: invalid boot code body"),
            });

            let costs_2_contract_tx =
                StacksTransaction::new(tx_version.clone(), boot_code_auth.clone(), payload);

            let initialization_receipt = self.as_transaction(|tx_conn| {
                // bump the epoch in the Clarity DB
                tx_conn
                    .with_clarity_db(|db| {
                        db.set_clarity_epoch_version(StacksEpochId::Epoch2_05);
                        Ok(())
                    })
                    .unwrap();

                // initialize with a synthetic transaction
                let receipt = StacksChainState::process_transaction_payload(
                    tx_conn,
                    &costs_2_contract_tx,
                    &boot_code_account,
                )
                .expect("FATAL: Failed to process PoX 2 contract initialization");

                receipt
            });

            if initialization_receipt.result != Value::okay_true()
                || initialization_receipt.post_condition_aborted
            {
                panic!(
                    "FATAL: Failure processing Costs 2 contract initialization: {:#?}",
                    &initialization_receipt
                );
            }

            (old_cost_tracker, Ok(initialization_receipt))
        })
    }

    pub fn start_transaction_processing<'c>(&'c mut self) -> ClarityTransactionConnection<'c, 'a> {
        let store = &mut self.datastore;
        let cost_track = &mut self.cost_track;
        let header_db = &self.header_db;
        let burn_state_db = &self.burn_state_db;
        let mainnet = self.mainnet;
        let mut log = RollbackWrapperPersistedLog::new();
        log.nest();
        ClarityTransactionConnection {
            store,
            cost_track,
            header_db,
            burn_state_db,
            log: Some(log),
            mainnet,
            epoch: self.epoch,
        }
    }

    pub fn as_transaction<F, R>(&mut self, todo: F) -> R
    where
        F: FnOnce(&mut ClarityTransactionConnection) -> R,
    {
        let mut tx = self.start_transaction_processing();
        let r = todo(&mut tx);
        tx.commit();
        r
    }

    pub fn seal(&mut self) -> TrieHash {
        self.datastore.seal()
    }

    pub fn destruct(self) -> WritableMarfStore<'a> {
        self.datastore
    }
}

impl<'a, 'b> ClarityConnection for ClarityTransactionConnection<'a, 'b> {
    /// Do something with ownership of the underlying DB that involves only reading.
    fn with_clarity_db_readonly_owned<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(ClarityDatabase) -> (R, ClarityDatabase),
    {
        using!(self.log, "log", |log| {
            let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
            let mut db = ClarityDatabase::new_with_rollback_wrapper(
                rollback_wrapper,
                &self.header_db,
                &self.burn_state_db,
            );
            db.begin();
            let (r, mut db) = to_do(db);
            db.roll_back();
            (db.destroy().into(), r)
        })
    }

    fn with_analysis_db_readonly<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase) -> R,
    {
        self.with_analysis_db(|mut db, cost_tracker| {
            db.begin();
            let result = to_do(&mut db);
            db.roll_back();
            (cost_tracker, result)
        })
    }

    fn get_epoch(&self) -> StacksEpochId {
        self.epoch
    }
}

impl<'a, 'b> Drop for ClarityTransactionConnection<'a, 'b> {
    fn drop(&mut self) {
        if thread::panicking() {
            // if the thread is panicking, we've likely lost our cost_tracker handle,
            //  so don't expect() one, or we'll end up panicking while panicking.
            match self.cost_track.as_mut() {
                Some(t) => t.reset_memory(),
                None => {
                    error!("Failed to reset the memory of the Clarity transaction's cost_track handle while thread panicking");
                }
            }
        } else {
            self.cost_track
                .as_mut()
                .expect("BUG: Transaction connection lost cost_tracker handle.")
                .reset_memory();
        }
    }
}

impl<'a, 'b> TransactionConnection for ClarityTransactionConnection<'a, 'b> {
    fn with_abort_callback<F, A, R, E>(
        &mut self,
        to_do: F,
        abort_call_back: A,
    ) -> Result<(R, AssetMap, Vec<StacksTransactionEvent>, bool), E>
    where
        A: FnOnce(&AssetMap, &mut ClarityDatabase) -> bool,
        F: FnOnce(&mut OwnedEnvironment) -> Result<(R, AssetMap, Vec<StacksTransactionEvent>), E>,
    {
        using!(self.log, "log", |log| {
            using!(self.cost_track, "cost tracker", |cost_track| {
                let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
                let mut db = ClarityDatabase::new_with_rollback_wrapper(
                    rollback_wrapper,
                    &self.header_db,
                    &self.burn_state_db,
                );

                // wrap the whole contract-call in a claritydb transaction,
                //   so we can abort on call_back's boolean retun
                db.begin();
                let mut vm_env =
                    OwnedEnvironment::new_cost_limited(self.mainnet, db, cost_track, self.epoch);
                let result = to_do(&mut vm_env);
                let (mut db, cost_track) = vm_env
                    .destruct()
                    .expect("Failed to recover database reference after executing transaction");
                // DO NOT reset memory usage yet -- that should happen only when the TX commits.

                let result = match result {
                    Ok((value, asset_map, events)) => {
                        let aborted = abort_call_back(&asset_map, &mut db);
                        if aborted {
                            db.roll_back();
                        } else {
                            db.commit();
                        }
                        Ok((value, asset_map, events, aborted))
                    }
                    Err(e) => {
                        db.roll_back();
                        Err(e)
                    }
                };

                (cost_track, (db.destroy().into(), result))
            })
        })
    }

    fn with_analysis_db<F, R>(&mut self, to_do: F) -> R
    where
        F: FnOnce(&mut AnalysisDatabase, LimitedCostTracker) -> (LimitedCostTracker, R),
    {
        using!(self.cost_track, "cost tracker", |cost_track| {
            using!(self.log, "log", |log| {
                let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
                let mut db = AnalysisDatabase::new_with_rollback_wrapper(rollback_wrapper);
                let r = to_do(&mut db, cost_track);
                (db.destroy().into(), r)
            })
        })
    }
}

impl<'a, 'b> ClarityTransactionConnection<'a, 'b> {
    /// Do something to the underlying DB that involves writing.
    pub fn with_clarity_db<F, R>(&mut self, to_do: F) -> Result<R, Error>
    where
        F: FnOnce(&mut ClarityDatabase) -> Result<R, Error>,
    {
        using!(self.log, "log", |log| {
            let rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
            let mut db = ClarityDatabase::new_with_rollback_wrapper(
                rollback_wrapper,
                &self.header_db,
                &self.burn_state_db,
            );

            db.begin();
            let result = to_do(&mut db);
            if result.is_ok() {
                db.commit();
            } else {
                db.roll_back();
            }

            (db.destroy().into(), result)
        })
    }

    /// What's our total (block-wide) resource use so far?
    pub fn cost_so_far(&self) -> ExecutionCost {
        match self.cost_track {
            Some(ref track) => track.get_total(),
            None => ExecutionCost::zero(),
        }
    }

    /// Evaluate a poison-microblock transaction
    pub fn run_poison_microblock(
        &mut self,
        sender: &PrincipalData,
        mblock_header_1: &StacksMicroblockHeader,
        mblock_header_2: &StacksMicroblockHeader,
    ) -> Result<Value, Error> {
        self.with_abort_callback(
            |vm_env| {
                vm_env
                    .execute_in_env(sender.clone(), |env| {
                        env.run_as_transaction(|env| {
                            StacksChainState::handle_poison_microblock(
                                env,
                                mblock_header_1,
                                mblock_header_2,
                            )
                        })
                    })
                    .map_err(Error::from)
            },
            |_, _| false,
        )
        .and_then(|(value, ..)| Ok(value))
    }

    /// Commit the changes from the edit log.
    /// panics if there is more than one open savepoint
    pub fn commit(mut self) {
        let log = self
            .log
            .take()
            .expect("BUG: Transaction Connection lost db log connection.");
        let mut rollback_wrapper = RollbackWrapper::from_persisted_log(self.store, log);
        if rollback_wrapper.depth() != 1 {
            panic!(
                "Attempted to commit transaction with {} != 1 rollbacks",
                rollback_wrapper.depth()
            );
        }
        rollback_wrapper.commit();
        // now we can reset the memory usage for the edit-log
        self.cost_track
            .as_mut()
            .expect("BUG: Transaction connection lost cost tracker connection.")
            .reset_memory();
    }

    /// Evaluate a raw Clarity snippit
    #[cfg(test)]
    pub fn clarity_eval_raw(&mut self, code: &str) -> Result<Value, Error> {
        let (result, _, _, _) = self.with_abort_callback(
            |vm_env| vm_env.eval_raw(code).map_err(Error::from),
            |_, _| false,
        )?;
        Ok(result)
    }

    #[cfg(test)]
    pub fn eval_read_only(
        &mut self,
        contract: &QualifiedContractIdentifier,
        code: &str,
    ) -> Result<Value, Error> {
        let (result, _, _, _) = self.with_abort_callback(
            |vm_env| vm_env.eval_read_only(contract, code).map_err(Error::from),
            |_, _| false,
        )?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::NO_PARAMS;

    use crate::chainstate::stacks::index::storage::TrieFileStorage;
    use clarity::vm::analysis::errors::CheckErrors;
    use clarity::vm::database::{ClarityBackingStore, STXBalance};
    use clarity::vm::types::{StandardPrincipalData, Value};

    use crate::core::{PEER_VERSION_EPOCH_1_0, PEER_VERSION_EPOCH_2_0, PEER_VERSION_EPOCH_2_05};
    use clarity::vm::test_util::{TEST_BURN_STATE_DB, TEST_HEADER_DB};

    use crate::chainstate::stacks::index::ClarityMarfTrieId;
    use crate::clarity_vm::database::marf::MarfedKV;

    use super::*;

    #[test]
    pub fn bad_syntax_test() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);

        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            let contract = "(define-public (foo (x int) (y uint)) (ok (+ x y)))";

            let _e = conn
                .as_transaction(|tx| tx.analyze_smart_contract(&contract_identifier, &contract))
                .unwrap_err();

            // okay, let's try it again:

            let _e = conn
                .as_transaction(|tx| tx.analyze_smart_contract(&contract_identifier, &contract))
                .unwrap_err();

            conn.commit_block();
        }
    }

    #[test]
    pub fn test_initialize_contract_tx_sender_contract_caller() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            // S1G2081040G2081040G2081040G208105NK8PE5 is the transient address
            let contract = "
                (begin 
                    (asserts! (is-eq tx-sender 'S1G2081040G2081040G2081040G208105NK8PE5)
                        (err tx-sender))

                    (asserts! (is-eq contract-caller 'S1G2081040G2081040G2081040G208105NK8PE5)
                        (err contract-caller))
                )";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.commit_block();
        }
    }

    #[test]
    pub fn tx_rollback() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);

        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let contract = "(define-public (foo (x int) (y int)) (ok (+ x y)))";

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            {
                let mut tx = conn.start_transaction_processing();

                let (ct_ast, ct_analysis) = tx
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                tx.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                tx.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            }

            // okay, let's try it again -- should pass since the prior contract
            //   publish was unwound
            {
                let mut tx = conn.start_transaction_processing();

                let contract = "(define-public (foo (x int) (y int)) (ok (+ x y)))";

                let (ct_ast, ct_analysis) = tx
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                tx.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                tx.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();

                tx.commit();
            }

            // should fail since the prior contract
            //   publish committed to the block
            {
                let mut tx = conn.start_transaction_processing();

                let contract = "(define-public (foo (x int) (y int)) (ok (+ x y)))";

                let (ct_ast, _ct_analysis) = tx
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                assert!(format!(
                    "{}",
                    tx.initialize_smart_contract(
                        &contract_identifier,
                        &ct_ast,
                        &contract,
                        |_, _| false
                    )
                    .unwrap_err()
                )
                .contains("ContractAlreadyExists"));

                tx.commit();
            }
        }
    }

    #[test]
    pub fn simple_test() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);

        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            let contract = "(define-public (foo (x int)) (ok (+ x x)))";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &StandardPrincipalData::transient().into(),
                    &contract_identifier,
                    "foo",
                    &[Value::Int(1)],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(2)).unwrap()
            );

            conn.commit_block();
        }

        let mut marf = clarity_instance.destroy();
        let mut conn = marf.begin_read_only(Some(&StacksBlockId([1 as u8; 32])));
        assert!(conn.get_contract_hash(&contract_identifier).is_ok());
    }

    #[test]
    pub fn test_block_roll_back() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        {
            let mut conn = clarity_instance.begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            let contract = "(define-public (foo (x int)) (ok (+ x x)))";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.rollback_block();
        }

        let mut marf = clarity_instance.destroy();

        let mut conn = marf.begin(&StacksBlockId::sentinel(), &StacksBlockId([0 as u8; 32]));
        // should not be in the marf.
        assert_eq!(
            conn.get_contract_hash(&contract_identifier).unwrap_err(),
            CheckErrors::NoSuchContract(contract_identifier.to_string()).into()
        );
        let sql = conn.get_side_store();
        // sqlite only have entries
        assert_eq!(
            0,
            sql.query_row::<u32, _, _>("SELECT COUNT(value) FROM data_table", NO_PARAMS, |row| row
                .get(0))
                .unwrap()
        );
    }

    #[test]
    fn test_unconfirmed() {
        let test_name = "/tmp/clarity_test_unconfirmed";
        if fs::metadata(test_name).is_ok() {
            fs::remove_dir_all(test_name).unwrap();
        }

        let confirmed_marf = MarfedKV::open(test_name, None, None).unwrap();
        let mut confirmed_clarity_instance = ClarityInstance::new(false, confirmed_marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();

        let contract = "
        (define-data-var bar int 0)
        (define-public (get-bar) (ok (var-get bar)))
        (define-public (set-bar (x int) (y int))
          (begin (var-set bar (/ x y)) (ok (var-get bar))))";

        // make an empty but confirmed block
        confirmed_clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        let marf = MarfedKV::open_unconfirmed(test_name, None, None).unwrap();

        let genesis_metadata_entries = marf
            .sql_conn()
            .query_row::<u32, _, _>(
                "SELECT COUNT(value) FROM metadata_table",
                NO_PARAMS,
                |row| row.get(0),
            )
            .unwrap();

        let mut clarity_instance = ClarityInstance::new(false, marf);

        // make an unconfirmed block off of the confirmed block
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.commit_unconfirmed();
        }

        // contract is still there, in unconfirmed status
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                conn.with_clarity_db_readonly(|ref mut tx| {
                    let src = tx.get_contract_src(&contract_identifier).unwrap();
                    assert_eq!(src, contract);
                });
            });

            conn.rollback_block();
        }

        // contract is still there, in unconfirmed status, even though the conn got explicitly
        // rolled back (but that should only drop the current TrieRAM)
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                conn.with_clarity_db_readonly(|ref mut tx| {
                    let src = tx.get_contract_src(&contract_identifier).unwrap();
                    assert_eq!(src, contract);
                });
            });

            conn.rollback_unconfirmed();
        }

        // contract is now absent, now that we did a rollback of unconfirmed state
        {
            let mut conn = clarity_instance.begin_unconfirmed(
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            conn.as_transaction(|conn| {
                conn.with_clarity_db_readonly(|ref mut tx| {
                    assert!(tx.get_contract_src(&contract_identifier).is_none());
                });
            });

            conn.commit_unconfirmed();
        }

        let mut marf = clarity_instance.destroy();
        let mut conn = marf.begin_unconfirmed(&StacksBlockId([0 as u8; 32]));

        // should not be in the marf.
        assert_eq!(
            conn.get_contract_hash(&contract_identifier).unwrap_err(),
            CheckErrors::NoSuchContract(contract_identifier.to_string()).into()
        );

        let sql = conn.get_side_store();
        // sqlite only have any metadata entries from the genesis block
        assert_eq!(
            genesis_metadata_entries,
            sql.query_row::<u32, _, _>(
                "SELECT COUNT(value) FROM metadata_table",
                NO_PARAMS,
                |row| row.get(0)
            )
            .unwrap()
        );
    }

    #[test]
    pub fn test_tx_roll_backs() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let sender = StandardPrincipalData::transient().into();

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            let contract = "
            (define-data-var bar int 0)
            (define-public (get-bar) (ok (var-get bar)))
            (define-public (set-bar (x int) (y int))
              (begin (var-set bar (/ x y)) (ok (var-get bar))))";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "get-bar",
                    &[],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(0)).unwrap()
            );

            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "set-bar",
                    &[Value::Int(1), Value::Int(1)],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(1)).unwrap()
            );

            let e = conn
                .as_transaction(|tx| {
                    tx.run_contract_call(
                        &sender,
                        &contract_identifier,
                        "set-bar",
                        &[Value::Int(10), Value::Int(1)],
                        |_, _| true,
                    )
                })
                .unwrap_err();
            let result_value = if let Error::AbortedByCallback(v, ..) = e {
                v.unwrap()
            } else {
                panic!("Expects a AbortedByCallback error")
            };

            assert_eq!(result_value, Value::okay(Value::Int(10)).unwrap());

            // prior transaction should have rolled back due to abort call back!
            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "get-bar",
                    &[],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(1)).unwrap()
            );

            assert!(format!(
                "{:?}",
                conn.as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "set-bar",
                    &[Value::Int(10), Value::Int(0)],
                    |_, _| true
                ))
                .unwrap_err()
            )
            .contains("DivisionByZero"));

            // prior transaction should have rolled back due to runtime error
            assert_eq!(
                conn.as_transaction(|tx| tx.run_contract_call(
                    &StandardPrincipalData::transient().into(),
                    &contract_identifier,
                    "get-bar",
                    &[],
                    |_, _| false
                ))
                .unwrap()
                .0,
                Value::okay(Value::Int(1)).unwrap()
            );

            conn.commit_block();
        }
    }

    #[test]
    pub fn test_post_condition_failure_contract_publish() {
        use crate::chainstate::stacks::db::*;
        use crate::chainstate::stacks::*;
        use crate::util_lib::strings::StacksString;
        use stacks_common::util::hash::Hash160;
        use stacks_common::util::secp256k1::MessageSignature;

        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);
        let sender = StandardPrincipalData::transient().into();

        let spending_cond = TransactionSpendingCondition::Singlesig(SinglesigSpendingCondition {
            signer: Hash160([0x11u8; 20]),
            hash_mode: SinglesigHashMode::P2PKH,
            key_encoding: TransactionPublicKeyEncoding::Compressed,
            nonce: 0,
            tx_fee: 1,
            signature: MessageSignature::from_raw(&vec![0xfe; 65]),
        });

        let contract = "(define-public (foo) (ok 1))";

        let mut tx1 = StacksTransaction::new(
            TransactionVersion::Mainnet,
            TransactionAuth::Standard(spending_cond.clone()),
            TransactionPayload::SmartContract(TransactionSmartContract {
                name: "hello-world".into(),
                code_body: StacksString::from_str(contract).unwrap(),
            })
            .into(),
        );

        let tx2 = StacksTransaction::new(
            TransactionVersion::Mainnet,
            TransactionAuth::Standard(spending_cond.clone()),
            TransactionPayload::SmartContract(TransactionSmartContract {
                name: "hello-world".into(),
                code_body: StacksString::from_str(contract).unwrap(),
            })
            .into(),
        );

        tx1.post_conditions.push(TransactionPostCondition::STX(
            PostConditionPrincipal::Origin,
            FungibleConditionCode::SentEq,
            100,
        ));

        let mut tx3 = StacksTransaction::new(
            TransactionVersion::Mainnet,
            TransactionAuth::Standard(spending_cond.clone()),
            TransactionPayload::ContractCall(TransactionContractCall {
                address: sender,
                contract_name: "hello-world".into(),
                function_name: "foo".into(),
                function_args: vec![],
            }),
        );

        tx3.post_conditions.push(TransactionPostCondition::STX(
            PostConditionPrincipal::Origin,
            FungibleConditionCode::SentEq,
            100,
        ));
        let stx_balance = STXBalance::initial(5000);
        let account = StacksAccount {
            principal: sender.into(),
            nonce: 0,
            stx_balance,
        };

        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            conn.as_transaction(|clarity_tx| {
                let receipt =
                    StacksChainState::process_transaction_payload(clarity_tx, &tx1, &account)
                        .unwrap();
                assert_eq!(receipt.post_condition_aborted, true);
            });
            conn.as_transaction(|clarity_tx| {
                StacksChainState::process_transaction_payload(clarity_tx, &tx2, &account).unwrap();
            });

            conn.as_transaction(|clarity_tx| {
                let receipt =
                    StacksChainState::process_transaction_payload(clarity_tx, &tx3, &account)
                        .unwrap();

                assert_eq!(receipt.post_condition_aborted, true);
            });

            conn.commit_block();
        }
    }

    #[test]
    pub fn test_block_limit() {
        let marf = MarfedKV::temporary();
        let mut clarity_instance = ClarityInstance::new(false, marf);
        let contract_identifier = QualifiedContractIdentifier::local("foo").unwrap();
        let sender = StandardPrincipalData::transient().into();

        pub struct BlockLimitBurnStateDB {}
        impl BurnStateDB for BlockLimitBurnStateDB {
            fn get_burn_block_height(&self, _sortition_id: &SortitionId) -> Option<u32> {
                None
            }

            fn get_burn_header_hash(
                &self,
                _height: u32,
                _sortition_id: &SortitionId,
            ) -> Option<BurnchainHeaderHash> {
                None
            }

            fn get_stacks_epoch(&self, _height: u32) -> Option<StacksEpoch> {
                // Note: We return this StacksEpoch for every input, because this test is not exercising
                // this method.
                Some(StacksEpoch {
                    epoch_id: StacksEpochId::Epoch20,
                    start_height: 0,
                    end_height: u64::MAX,
                    block_limit: ExecutionCost {
                        write_length: u64::MAX,
                        write_count: u64::MAX,
                        read_count: u64::MAX,
                        read_length: u64::MAX,
                        runtime: 100,
                    },
                    network_epoch: PEER_VERSION_EPOCH_2_0,
                })
            }

            fn get_stacks_epoch_by_epoch_id(
                &self,
                _epoch_id: &StacksEpochId,
            ) -> Option<StacksEpoch> {
                self.get_stacks_epoch(0)
            }
        }

        let burn_state_db = BlockLimitBurnStateDB {};
        clarity_instance
            .begin_test_genesis_block(
                &StacksBlockId::sentinel(),
                &StacksBlockId([0 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            )
            .commit_block();

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([0 as u8; 32]),
                &StacksBlockId([1 as u8; 32]),
                &TEST_HEADER_DB,
                &TEST_BURN_STATE_DB,
            );

            let contract = "
            (define-public (do-expand)
              (let ((list1 (list 1 2 3 4 5 6 7 8 9 10)))
                (let ((list2 (concat list1 list1)))
                  (let ((list3 (concat list2 list2)))
                    (let ((list4 (concat list3 list3)))
                      (ok (concat list4 list4)))))))
            ";

            conn.as_transaction(|conn| {
                let (ct_ast, ct_analysis) = conn
                    .analyze_smart_contract(&contract_identifier, &contract)
                    .unwrap();
                conn.initialize_smart_contract(&contract_identifier, &ct_ast, &contract, |_, _| {
                    false
                })
                .unwrap();
                conn.save_analysis(&contract_identifier, &ct_analysis)
                    .unwrap();
            });

            conn.commit_block();
        }

        {
            let mut conn = clarity_instance.begin_block(
                &StacksBlockId([1 as u8; 32]),
                &StacksBlockId([2 as u8; 32]),
                &TEST_HEADER_DB,
                &burn_state_db,
            );
            assert!(match conn
                .as_transaction(|tx| tx.run_contract_call(
                    &sender,
                    &contract_identifier,
                    "do-expand",
                    &[],
                    |_, _| false
                ))
                .unwrap_err()
            {
                Error::CostError(total, limit) => {
                    eprintln!("{}, {}", total, limit);
                    limit.runtime == 100 && total.runtime > 100
                }
                x => {
                    eprintln!("{}", x);
                    false
                }
            });

            conn.commit_block();
        }
    }
}
