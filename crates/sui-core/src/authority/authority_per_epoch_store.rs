// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use enum_dispatch::enum_dispatch;
use fastcrypto::groups::bls12381;
use fastcrypto_tbls::dkg_v1;
use fastcrypto_tbls::nodes::PartyId;
use fastcrypto_zkp::bn254::zk_login::{JwkId, OIDCProvider, JWK};
use fastcrypto_zkp::bn254::zk_login_api::ZkLoginEnv;
use futures::future::{join_all, select, Either};
use futures::FutureExt;
use itertools::{izip, Itertools};
use move_bytecode_utils::module_cache::SyncModuleCache;
use mysten_common::assert_reachable;
use mysten_common::sync::notify_once::NotifyOnce;
use mysten_common::sync::notify_read::NotifyRead;
use mysten_common::{debug_fatal, fatal};
use mysten_metrics::monitored_scope;
use nonempty::NonEmpty;
use parking_lot::RwLock;
use parking_lot::{Mutex, RwLockReadGuard, RwLockWriteGuard};
use prometheus::IntCounter;
use serde::{Deserialize, Serialize};
use sui_config::node::ExpensiveSafetyCheckConfig;
use sui_execution::{self, Executor};
use sui_macros::fail_point;
use sui_macros::fail_point_arg;
use sui_protocol_config::{Chain, PerObjectCongestionControlMode, ProtocolConfig, ProtocolVersion};
use sui_storage::mutex_table::{MutexGuard, MutexTable};
use sui_types::authenticator_state::{get_authenticator_state, ActiveJwk};
use sui_types::base_types::{
    AuthorityName, ConsensusObjectSequenceKey, EpochId, FullObjectID, ObjectID, SequenceNumber,
    TransactionDigest,
};
use sui_types::base_types::{ConciseableName, ObjectRef};
use sui_types::committee::Committee;
use sui_types::committee::CommitteeTrait;
use sui_types::crypto::{
    AuthorityPublicKeyBytes, AuthoritySignInfo, AuthorityStrongQuorumSignInfo, RandomnessRound,
};
use sui_types::digests::{ChainIdentifier, TransactionEffectsDigest};
use sui_types::dynamic_field::get_dynamic_field_from_store;
use sui_types::effects::{TransactionEffects, TransactionEffectsAPI};
use sui_types::error::{SuiError, SuiResult};
use sui_types::executable_transaction::VerifiedExecutableTransaction;
use sui_types::execution::{ExecutionTimeObservationKey, ExecutionTiming};
use sui_types::global_state_hash::GlobalStateHash;
use sui_types::message_envelope::TrustedEnvelope;
use sui_types::messages_checkpoint::{
    CheckpointContents, CheckpointSequenceNumber, CheckpointSignatureMessage, CheckpointSummary,
};
use sui_types::messages_consensus::{
    check_total_jwk_size, AuthorityCapabilitiesV1, AuthorityCapabilitiesV2, AuthorityIndex,
    ConsensusPosition, ConsensusTransaction, ConsensusTransactionKey, ConsensusTransactionKind,
    ExecutionTimeObservation, TimestampMs, VersionedDkgConfirmation,
};
use sui_types::signature::GenericSignature;
use sui_types::storage::{BackingPackageStore, InputKey, ObjectStore};
use sui_types::sui_system_state::epoch_start_sui_system_state::{
    EpochStartSystemState, EpochStartSystemStateTrait,
};
use sui_types::sui_system_state::{self, SuiSystemState};
use sui_types::transaction::{
    AuthenticatorStateUpdate, CallArg, CertifiedTransaction, InputObjectKind, ObjectArg,
    ProgrammableTransaction, SenderSignedData, StoredExecutionTimeObservations, Transaction,
    TransactionData, TransactionDataAPI, TransactionKey, TransactionKind, TxValidityCheckContext,
    VerifiedCertificate, VerifiedSignedTransaction, VerifiedTransaction,
};
use tap::TapOptional;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::Instant;
use tracing::{debug, error, info, instrument, trace, warn};
use typed_store::rocks::{default_db_options, DBBatch, DBMap, DBOptions, MetricConf};
use typed_store::rocks::{read_size_from_env, ReadWriteOptions};
use typed_store::rocksdb::Options;
use typed_store::DBMapUtils;
use typed_store::Map;

use super::authority_store_tables::ENV_VAR_LOCKS_BLOCK_CACHE_SIZE;
use super::consensus_tx_status_cache::{ConsensusTxStatus, ConsensusTxStatusCache};
use super::epoch_start_configuration::EpochStartConfigTrait;
use super::execution_time_estimator::{ConsensusObservations, ExecutionTimeEstimator};
use super::shared_object_congestion_tracker::{
    CongestionPerObjectDebt, SharedObjectCongestionTracker,
};
use super::shared_object_version_manager::AssignedVersions;
use super::transaction_deferral::{transaction_deferral_within_limit, DeferralKey, DeferralReason};
use super::transaction_reject_reason_cache::TransactionRejectReasonCache;
use crate::authority::epoch_start_configuration::EpochStartConfiguration;
use crate::authority::execution_time_estimator::EXTRA_FIELD_EXECUTION_TIME_ESTIMATES_KEY;
use crate::authority::shared_object_version_manager::{
    AssignedTxAndVersions, ConsensusSharedObjVerAssignment, Schedulable, SharedObjVerManager,
};
use crate::authority::AuthorityMetrics;
use crate::authority::ResolverWrapper;
use crate::checkpoints::{
    BuilderCheckpointSummary, CheckpointHeight, CheckpointServiceNotify, EpochStats,
    PendingCheckpointInfo, PendingCheckpointV2, PendingCheckpointV2Contents,
};
use crate::consensus_handler::{
    ConsensusCommitInfo, IndirectStateObserver, SequencedConsensusTransaction,
    SequencedConsensusTransactionKey, SequencedConsensusTransactionKind,
    VerifiedSequencedConsensusTransaction,
};
use crate::epoch::epoch_metrics::EpochMetrics;
use crate::epoch::randomness::{
    DkgStatus, RandomnessManager, RandomnessReporter, VersionedProcessedMessage,
    VersionedUsedProcessedMessages, SINGLETON_KEY,
};
use crate::epoch::reconfiguration::ReconfigState;
use crate::execution_cache::cache_types::CacheResult;
use crate::execution_cache::ObjectCacheRead;
use crate::fallback_fetch::do_fallback_lookup;
use crate::module_cache_metrics::ResolverMetrics;
use crate::post_consensus_tx_reorder::PostConsensusTxReorder;
use crate::signature_verifier::*;
use crate::stake_aggregator::{GenericMultiStakeAggregator, StakeAggregator};

/// The key where the latest consensus index is stored in the database.
// TODO: Make a single table (e.g., called `variables`) storing all our lonely variables in one place.
const LAST_CONSENSUS_STATS_ADDR: u64 = 0;
const RECONFIG_STATE_INDEX: u64 = 0;
const OVERRIDE_PROTOCOL_UPGRADE_BUFFER_STAKE_INDEX: u64 = 0;
pub const EPOCH_DB_PREFIX: &str = "epoch_";

// Types for randomness DKG.
pub(crate) type PkG = bls12381::G2Element;
pub(crate) type EncG = bls12381::G2Element;

#[path = "consensus_quarantine.rs"]
pub(crate) mod consensus_quarantine;

use consensus_quarantine::ConsensusCommitOutput;
use consensus_quarantine::ConsensusOutputCache;
use consensus_quarantine::ConsensusOutputQuarantine;

// CertLockGuard and CertTxGuard are functionally identical right now, but we retain a distinction
// anyway. If we need to support distributed object storage, having this distinction will be
// useful, as we will most likely have to re-implement a retry / write-ahead-log at that point.
pub struct CertLockGuard(#[allow(unused)] MutexGuard);
pub struct CertTxGuard(#[allow(unused)] CertLockGuard);

impl CertTxGuard {
    pub fn release(self) {}
    pub fn commit_tx(self) {}
    pub fn as_lock_guard(&self) -> &CertLockGuard {
        &self.0
    }
}

impl CertLockGuard {
    pub fn dummy_for_tests() -> Self {
        let lock = Arc::new(parking_lot::Mutex::new(()));
        Self(lock.try_lock_arc().unwrap())
    }
}

type JwkAggregator = GenericMultiStakeAggregator<(JwkId, JWK), true>;

type LocalExecutionTimeData = (
    ProgrammableTransaction,
    Vec<ExecutionTiming>,
    Duration,
    u64, // gas_price
);

pub enum CancelConsensusCertificateReason {
    CongestionOnObjects(Vec<ObjectID>),
    DkgFailed,
}

pub enum ConsensusCertificateResult {
    /// The consensus message was ignored (e.g. because it has already been processed).
    Ignored,
    /// An executable transaction (can be a user tx or a system tx)
    SuiTransaction(VerifiedExecutableTransaction),
    /// The transaction should be re-processed at a future commit, specified by the DeferralKey
    Deferred(DeferralKey),
    /// A message was processed which updates randomness state.
    RandomnessConsensusMessage,
    /// Everything else, e.g. AuthorityCapabilities, CheckpointSignatures, etc.
    ConsensusMessage,
    /// A system message in consensus was ignored (e.g. because of end of epoch).
    IgnoredSystem,
    /// A will-be-cancelled transaction. It'll still go through execution engine (but not be executed),
    /// unlock any owned objects, and return corresponding cancellation error according to
    /// `CancelConsensusCertificateReason`.
    Cancelled(
        (
            VerifiedExecutableTransaction,
            CancelConsensusCertificateReason,
        ),
    ),
}

/// ConsensusStats is versioned because we may iterate on the struct, and it is
/// stored on disk.
#[enum_dispatch]
pub trait ConsensusStatsAPI {
    fn is_initialized(&self) -> bool;

    fn get_num_messages(&self, authority: usize) -> u64;
    fn inc_num_messages(&mut self, authority: usize) -> u64;

    fn get_num_user_transactions(&self, authority: usize) -> u64;
    fn inc_num_user_transactions(&mut self, authority: usize) -> u64;
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[enum_dispatch(ConsensusStatsAPI)]
pub enum ConsensusStats {
    V1(ConsensusStatsV1),
}

impl ConsensusStats {
    pub fn new(size: usize) -> Self {
        Self::V1(ConsensusStatsV1 {
            num_messages: vec![0; size],
            num_user_transactions: vec![0; size],
        })
    }
}

impl Default for ConsensusStats {
    fn default() -> Self {
        Self::new(0)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ConsensusStatsV1 {
    pub num_messages: Vec<u64>,
    pub num_user_transactions: Vec<u64>,
}

impl ConsensusStatsAPI for ConsensusStatsV1 {
    fn is_initialized(&self) -> bool {
        !self.num_messages.is_empty()
    }

    fn get_num_messages(&self, authority: usize) -> u64 {
        self.num_messages[authority]
    }

    fn inc_num_messages(&mut self, authority: usize) -> u64 {
        self.num_messages[authority] += 1;
        self.num_messages[authority]
    }

    fn get_num_user_transactions(&self, authority: usize) -> u64 {
        self.num_user_transactions[authority]
    }

    fn inc_num_user_transactions(&mut self, authority: usize) -> u64 {
        self.num_user_transactions[authority] += 1;
        self.num_user_transactions[authority]
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Copy)]
pub struct ExecutionIndices {
    /// The round number of the last committed leader.
    pub last_committed_round: u64,
    /// The index of the last sub-DAG that was executed (either fully or partially).
    pub sub_dag_index: u64,
    /// The index of the last transaction was executed (used for crash-recovery).
    pub transaction_index: u64,
}

impl Ord for ExecutionIndices {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.last_committed_round,
            self.sub_dag_index,
            self.transaction_index,
        )
            .cmp(&(
                other.last_committed_round,
                other.sub_dag_index,
                other.transaction_index,
            ))
    }
}

impl PartialOrd for ExecutionIndices {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionIndicesWithStats {
    pub index: ExecutionIndices,
    // Hash is always 0 and kept for compatibility only.
    pub hash: u64,
    pub stats: ConsensusStats,
}

type ExecutionModuleCache = SyncModuleCache<ResolverWrapper>;

// Data related to VM and Move execution and type layout
pub struct ExecutionComponents {
    pub(crate) executor: Arc<dyn Executor + Send + Sync>,
    // TODO: use strategies (e.g. LRU?) to constraint memory usage
    pub(crate) module_cache: Arc<ExecutionModuleCache>,
    metrics: Arc<ResolverMetrics>,
}

#[cfg(test)]
#[path = "../unit_tests/authority_per_epoch_store_tests.rs"]
pub mod authority_per_epoch_store_tests;

pub struct AuthorityPerEpochStore {
    /// The name of this authority.
    pub(crate) name: AuthorityName,

    /// Committee of validators for the current epoch.
    committee: Arc<Committee>,

    /// Holds the underlying per-epoch typed store tables.
    /// This is an ArcSwapOption because it needs to be used concurrently,
    /// and it needs to be cleared at the end of the epoch.
    tables: ArcSwapOption<AuthorityEpochTables>,

    /// Holds the outputs of both consensus handler and checkpoint builder in memory
    /// until they are proven not to have forked by a certified checkpoint.
    consensus_quarantine: RwLock<ConsensusOutputQuarantine>,
    /// Holds variouis data from consensus_quarantine in a more easily accessible form.
    consensus_output_cache: ConsensusOutputCache,

    protocol_config: ProtocolConfig,

    // needed for re-opening epoch db.
    parent_path: PathBuf,
    db_options: Option<Options>,

    /// In-memory cache of the content from the reconfig_state db table.
    reconfig_state_mem: RwLock<ReconfigState>,
    consensus_notify_read: NotifyRead<SequencedConsensusTransactionKey, ()>,

    // Subscribers will get notified when a transaction is executed via checkpoint execution.
    executed_transactions_to_checkpoint_notify_read:
        NotifyRead<TransactionDigest, CheckpointSequenceNumber>,

    /// Batch verifier for certificates - also caches certificates and tx sigs that are known to have
    /// valid signatures. Lives in per-epoch store because the caching/batching is only valid
    /// within for certs within the current epoch.
    pub(crate) signature_verifier: SignatureVerifier,

    pub(crate) checkpoint_state_notify_read: NotifyRead<CheckpointSequenceNumber, GlobalStateHash>,

    running_root_notify_read: NotifyRead<CheckpointSequenceNumber, GlobalStateHash>,

    executed_digests_notify_read: NotifyRead<TransactionKey, TransactionDigest>,

    /// This is used to notify all epoch specific tasks that epoch has ended.
    epoch_alive_notify: NotifyOnce,

    /// Used to notify all epoch specific tasks that user certs are closed.
    user_certs_closed_notify: NotifyOnce,

    /// This lock acts as a barrier for tasks that should not be executed in parallel with reconfiguration
    /// See comments in AuthorityPerEpochStore::epoch_terminated() on how this is used
    /// Crash recovery note: we write next epoch in the database first, and then use this lock to
    /// wait for in-memory tasks for the epoch to finish. If node crashes at this stage validator
    /// will start with the new epoch(and will open instance of per-epoch store for a new epoch).
    epoch_alive: tokio::sync::RwLock<bool>,
    end_of_publish: Mutex<StakeAggregator<(), true>>,
    /// Pending certificates that are waiting to be sequenced by the consensus.
    /// This is an in-memory 'index' of a AuthorityPerEpochTables::pending_consensus_transactions.
    /// We need to keep track of those in order to know when to send EndOfPublish message.
    /// Lock ordering: this is a 'leaf' lock, no other locks should be acquired in the scope of this lock
    /// In particular, this lock is always acquired after taking read or write lock on reconfig state
    pending_consensus_certificates: RwLock<HashSet<TransactionDigest>>,

    /// MutexTable for transaction locks (prevent concurrent execution of same transaction)
    mutex_table: MutexTable<TransactionDigest>,
    /// Mutex table for shared version assignment
    version_assignment_mutex_table: MutexTable<ObjectID>,

    /// The moment when the current epoch started locally on this validator. Note that this
    /// value could be skewed if the node crashed and restarted in the middle of the epoch. That's
    /// ok because this is used for metric purposes and we could tolerate some skews occasionally.
    pub(crate) epoch_open_time: Instant,

    /// The moment when epoch is closed. We don't care much about crash recovery because it's
    /// a metric that doesn't have to be available for each epoch, and it's only used during
    /// the last few seconds of an epoch.
    epoch_close_time: RwLock<Option<Instant>>,
    pub(crate) metrics: Arc<EpochMetrics>,
    epoch_start_configuration: Arc<EpochStartConfiguration>,

    /// Execution state that has to restart at each epoch change
    execution_component: ExecutionComponents,

    /// ChainIdentifier is always the true id (digest of genesis checkpoint). Chain is the
    /// nominal identifier and can be overridden for testing purposes.
    chain: (ChainIdentifier, Chain),

    /// aggregator for JWK votes
    jwk_aggregator: Mutex<JwkAggregator>,

    /// State machine managing randomness DKG and generation.
    randomness_manager: OnceCell<tokio::sync::Mutex<RandomnessManager>>,
    randomness_reporter: OnceCell<RandomnessReporter>,

    /// Manages recording execution time observations and generating estimates.
    execution_time_estimator: tokio::sync::Mutex<Option<ExecutionTimeEstimator>>,
    tx_local_execution_time: OnceCell<mpsc::Sender<LocalExecutionTimeData>>,
    tx_object_debts: OnceCell<mpsc::Sender<Vec<ObjectID>>>,
    // Saved at end of epoch for propagating observations to the next.
    end_of_epoch_execution_time_observations: OnceCell<StoredExecutionTimeObservations>,

    pub(crate) consensus_tx_status_cache: Option<ConsensusTxStatusCache>,

    /// A cache that maintains the reject vote reason for a transaction.
    pub(crate) tx_reject_reason_cache: Option<TransactionRejectReasonCache>,

    /// Waiters for settlement transactions. Used by execution scheduler to wait for
    /// settlement transaction keys to resolve to transactions.
    /// Stored in AuthorityPerEpochStore so that it is automatically cleaned up at the end of the epoch.
    settlement_registrations: Arc<Mutex<HashMap<TransactionKey, SettlementRegistration>>>,
}
enum SettlementRegistration {
    Ready(Vec<VerifiedExecutableTransaction>),
    Waiting(oneshot::Sender<Vec<VerifiedExecutableTransaction>>),
}

/// AuthorityEpochTables contains tables that contain data that is only valid within an epoch.
#[derive(DBMapUtils)]
#[cfg_attr(tidehunter, tidehunter)]
pub struct AuthorityEpochTables {
    /// This is map between the transaction digest and transactions found in the `transaction_lock`.
    #[default_options_override_fn = "signed_transactions_table_default_config"]
    signed_transactions:
        DBMap<TransactionDigest, TrustedEnvelope<SenderSignedData, AuthoritySignInfo>>,

    /// Map from ObjectRef to transaction locking that object
    #[default_options_override_fn = "owned_object_transaction_locks_table_default_config"]
    owned_object_locked_transactions: DBMap<ObjectRef, LockDetailsWrapper>,

    /// Signatures over transaction effects that we have signed and returned to users.
    /// We store this to avoid re-signing the same effects twice.
    /// Note that this may contain signatures for effects from previous epochs, in the case
    /// that a user requests a signature for effects from a previous epoch. However, the
    /// signature is still epoch-specific and so is stored in the epoch store.
    effects_signatures: DBMap<TransactionDigest, AuthoritySignInfo>,

    /// When we sign a TransactionEffects, we must record the digest of the effects in order
    /// to detect and prevent equivocation when re-executing a transaction that may not have been
    /// committed to disk.
    /// Entries are removed from this table after the transaction in question has been committed
    /// to disk.
    signed_effects_digests: DBMap<TransactionDigest, TransactionEffectsDigest>,

    /// Signatures of transaction certificates that are executed locally.
    transaction_cert_signatures: DBMap<TransactionDigest, AuthorityStrongQuorumSignInfo>,

    /// Next available shared object versions for each shared object.
    next_shared_object_versions_v2: DBMap<ConsensusObjectSequenceKey, SequenceNumber>,

    /// Track which transactions have been processed in handle_consensus_transaction. We must be
    /// sure to advance next_shared_object_versions exactly once for each transaction we receive from
    /// consensus. But, we may also be processing transactions from checkpoints, so we need to
    /// track this state separately.
    ///
    /// Entries in this table can be garbage collected whenever we can prove that we won't receive
    /// another handle_consensus_transaction call for the given digest. This probably means at
    /// epoch change.
    consensus_message_processed: DBMap<SequencedConsensusTransactionKey, bool>,

    /// Map stores pending transactions that this authority submitted to consensus
    #[default_options_override_fn = "pending_consensus_transactions_table_default_config"]
    pending_consensus_transactions: DBMap<ConsensusTransactionKey, ConsensusTransaction>,

    /// The following table is used to store a single value (the corresponding key is a constant). The value
    /// represents the index of the latest consensus message this authority processed, running hash of
    /// transactions, and accumulated stats of consensus output.
    /// This field is written by a single process (consensus handler).
    last_consensus_stats: DBMap<u64, ExecutionIndicesWithStats>,

    /// This table contains current reconfiguration state for validator for current epoch
    reconfig_state: DBMap<u64, ReconfigState>,

    /// Validators that have sent EndOfPublish message in this epoch
    end_of_publish: DBMap<AuthorityName, ()>,

    /// Checkpoint builder maintains internal list of transactions it included in checkpoints here
    builder_digest_to_checkpoint: DBMap<TransactionDigest, CheckpointSequenceNumber>,

    /// Maps non-digest TransactionKeys to the corresponding digest after execution, for use
    /// by checkpoint builder.
    transaction_key_to_digest: DBMap<TransactionKey, TransactionDigest>,

    /// Stores pending signatures
    /// The key in this table is checkpoint sequence number and an arbitrary integer
    pub(crate) pending_checkpoint_signatures:
        DBMap<(CheckpointSequenceNumber, u64), CheckpointSignatureMessage>,

    /// Maps sequence number to checkpoint summary, used by CheckpointBuilder to build checkpoint within epoch
    builder_checkpoint_summary_v2: DBMap<CheckpointSequenceNumber, BuilderCheckpointSummary>,

    // Maps checkpoint sequence number to an accumulator with accumulated state
    // only for the checkpoint that the key references. Append-only, i.e.,
    // the accumulator is complete wrt the checkpoint
    pub state_hash_by_checkpoint: DBMap<CheckpointSequenceNumber, GlobalStateHash>,

    /// Maps checkpoint sequence number to the running (non-finalized) root state
    /// accumulator up th that checkpoint. This should be equivalent to the root
    /// state hash at end of epoch. Guaranteed to be written to in checkpoint
    /// sequence number order.
    #[rename = "running_root_accumulators"]
    pub running_root_state_hash: DBMap<CheckpointSequenceNumber, GlobalStateHash>,

    /// Record of the capabilities advertised by each authority.
    authority_capabilities: DBMap<AuthorityName, AuthorityCapabilitiesV1>,
    authority_capabilities_v2: DBMap<AuthorityName, AuthorityCapabilitiesV2>,

    /// Contains a single key, which overrides the value of
    /// ProtocolConfig::buffer_stake_for_protocol_upgrade_bps
    override_protocol_upgrade_buffer_stake: DBMap<u64, u64>,

    /// When transaction is executed via checkpoint executor, we store association here
    pub(crate) executed_transactions_to_checkpoint:
        DBMap<TransactionDigest, CheckpointSequenceNumber>,

    /// JWKs that have been voted for by one or more authorities but are not yet active.
    pending_jwks: DBMap<(AuthorityName, JwkId, JWK), ()>,

    /// JWKs that are currently available for zklogin authentication, and the round in which they
    /// became active.
    /// This would normally be stored as (JwkId, JWK) -> u64, but we need to be able to scan to
    /// find all Jwks for a given round
    active_jwks: DBMap<(u64, (JwkId, JWK)), ()>,

    /// Transactions that are being deferred until some future time
    deferred_transactions: DBMap<DeferralKey, Vec<VerifiedSequencedConsensusTransaction>>,

    // Tables for recording state for RandomnessManager.
    /// Records messages processed from other nodes. Updated when receiving a new dkg::Message
    /// via consensus.
    pub(crate) dkg_processed_messages_v2: DBMap<PartyId, VersionedProcessedMessage>,

    /// Records messages used to generate a DKG confirmation. Updated when enough DKG
    /// messages are received to progress to the next phase.
    pub(crate) dkg_used_messages_v2: DBMap<u64, VersionedUsedProcessedMessages>,

    /// Records confirmations received from other nodes. Updated when receiving a new
    /// dkg::Confirmation via consensus.
    pub(crate) dkg_confirmations_v2: DBMap<PartyId, VersionedDkgConfirmation>,
    /// Records the final output of DKG after completion, including the public VSS key and
    /// any local private shares.
    pub(crate) dkg_output: DBMap<u64, dkg_v1::Output<PkG, EncG>>,
    /// Holds the value of the next RandomnessRound to be generated.
    pub(crate) randomness_next_round: DBMap<u64, RandomnessRound>,
    /// Holds the value of the highest completed RandomnessRound (as reported to RandomnessReporter).
    pub(crate) randomness_highest_completed_round: DBMap<u64, RandomnessRound>,
    /// Holds the timestamp of the most recently generated round of randomness.
    pub(crate) randomness_last_round_timestamp: DBMap<u64, TimestampMs>,

    /// Accumulated per-object debts for congestion control.
    pub(crate) congestion_control_object_debts: DBMap<ObjectID, CongestionPerObjectDebt>,
    pub(crate) congestion_control_randomness_object_debts: DBMap<ObjectID, CongestionPerObjectDebt>,

    /// Execution time observations for congestion control.
    pub(crate) execution_time_observations:
        DBMap<(u64, AuthorityIndex), Vec<(ExecutionTimeObservationKey, Duration)>>,
}

fn signed_transactions_table_default_config() -> DBOptions {
    default_db_options()
        .optimize_for_write_throughput()
        .optimize_for_large_values_no_scan(1 << 10)
}

fn owned_object_transaction_locks_table_default_config() -> DBOptions {
    DBOptions {
        options: default_db_options()
            .optimize_for_write_throughput()
            .optimize_for_read(read_size_from_env(ENV_VAR_LOCKS_BLOCK_CACHE_SIZE).unwrap_or(1024))
            .options,
        rw_options: ReadWriteOptions::default().set_ignore_range_deletions(false),
    }
}

fn pending_consensus_transactions_table_default_config() -> DBOptions {
    default_db_options()
        .optimize_for_write_throughput()
        .optimize_for_large_values_no_scan(1 << 10)
}

impl AuthorityEpochTables {
    #[cfg(not(tidehunter))]
    pub fn open(epoch: EpochId, parent_path: &Path, db_options: Option<Options>) -> Self {
        Self::open_tables_read_write(
            Self::path(epoch, parent_path),
            MetricConf::new("epoch"),
            db_options,
            None,
        )
    }

    #[cfg(tidehunter)]
    pub fn open(epoch: EpochId, parent_path: &Path, db_options: Option<Options>) -> Self {
        tracing::warn!("AuthorityEpochTables using tidehunter");
        use typed_store::tidehunter_util::{
            default_cells_per_mutex, KeyIndexing, KeySpaceConfig, KeyType, ThConfig,
        };
        const MUTEXES: usize = 2 * 1024;
        let mut digest_prefix = vec![0; 8];
        digest_prefix[7] = 32;
        const VALUE_CACHE_SIZE: usize = 5000;
        let bloom_config = KeySpaceConfig::new().with_bloom_filter(0.001, 32_000);
        let lru_bloom_config = bloom_config.clone().with_value_cache_size(VALUE_CACHE_SIZE);
        let lru_only_config = KeySpaceConfig::new().with_value_cache_size(VALUE_CACHE_SIZE);
        let pending_checkpoint_signatures_config = KeySpaceConfig::new()
            .disable_unload()
            .with_value_cache_size(1000);
        let builder_checkpoint_summary_v2_config = pending_checkpoint_signatures_config.clone();
        let object_ref_indexing = KeyIndexing::Hash;
        let tx_digest_indexing = KeyIndexing::key_reduction(32, 0..16);
        let uniform_key = KeyType::uniform(default_cells_per_mutex());
        let sequence_key = KeyType::prefix_uniform(2, 4);
        let configs = vec![
            (
                "signed_transactions".to_string(),
                ThConfig::new_with_rm_prefix_indexing(
                    tx_digest_indexing.clone(),
                    MUTEXES,
                    uniform_key,
                    lru_bloom_config.clone(),
                    digest_prefix.clone(),
                ),
            ),
            (
                "owned_object_locked_transactions".to_string(),
                ThConfig::new_with_config_indexing(
                    object_ref_indexing,
                    MUTEXES * 2,
                    uniform_key,
                    bloom_config.clone(),
                ),
            ),
            (
                "effects_signatures".to_string(),
                ThConfig::new_with_rm_prefix_indexing(
                    tx_digest_indexing.clone(),
                    MUTEXES,
                    uniform_key,
                    lru_bloom_config.clone(),
                    digest_prefix.clone(),
                ),
            ),
            (
                "signed_effects_digests".to_string(),
                ThConfig::new_with_rm_prefix_indexing(
                    tx_digest_indexing.clone(),
                    MUTEXES,
                    uniform_key,
                    bloom_config.clone(),
                    digest_prefix.clone(),
                ),
            ),
            (
                "transaction_cert_signatures".to_string(),
                ThConfig::new_with_rm_prefix_indexing(
                    tx_digest_indexing.clone(),
                    MUTEXES,
                    uniform_key,
                    lru_bloom_config.clone(),
                    digest_prefix.clone(),
                ),
            ),
            (
                "next_shared_object_versions_v2".to_string(),
                ThConfig::new_with_config(32 + 8, MUTEXES, uniform_key, lru_only_config.clone()),
            ),
            (
                "consensus_message_processed".to_string(),
                ThConfig::new_with_config_indexing(
                    KeyIndexing::Hash,
                    MUTEXES,
                    uniform_key,
                    bloom_config.clone(),
                ),
            ),
            (
                "pending_consensus_transactions".to_string(),
                ThConfig::new_with_config_indexing(
                    KeyIndexing::Hash,
                    MUTEXES,
                    uniform_key,
                    KeySpaceConfig::default(),
                ),
            ),
            (
                "last_consensus_stats".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "reconfig_state".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "end_of_publish".to_string(),
                ThConfig::new(104, 1, KeyType::uniform(1)),
            ),
            (
                "builder_digest_to_checkpoint".to_string(),
                ThConfig::new_with_rm_prefix_indexing(
                    tx_digest_indexing.clone(),
                    MUTEXES * 4,
                    uniform_key,
                    lru_bloom_config.clone(),
                    digest_prefix.clone(),
                ),
            ),
            (
                "transaction_key_to_digest".to_string(),
                ThConfig::new_with_config_indexing(
                    KeyIndexing::Hash,
                    MUTEXES,
                    uniform_key,
                    KeySpaceConfig::default(),
                ),
            ),
            (
                "pending_checkpoint_signatures".to_string(),
                ThConfig::new_with_config(
                    8 + 8,
                    MUTEXES,
                    uniform_key,
                    pending_checkpoint_signatures_config,
                ),
            ),
            (
                "builder_checkpoint_summary_v2".to_string(),
                ThConfig::new_with_config(
                    8,
                    MUTEXES,
                    sequence_key,
                    builder_checkpoint_summary_v2_config,
                ),
            ),
            (
                "state_hash_by_checkpoint".to_string(),
                ThConfig::new_with_config(8, MUTEXES, sequence_key, bloom_config.clone()),
            ),
            (
                "running_root_accumulators".to_string(),
                ThConfig::new_with_config(8, MUTEXES, sequence_key, bloom_config.clone()),
            ),
            (
                "authority_capabilities".to_string(),
                ThConfig::new(104, MUTEXES, uniform_key),
            ),
            (
                "authority_capabilities_v2".to_string(),
                ThConfig::new(104, 1, KeyType::uniform(1)),
            ),
            (
                "override_protocol_upgrade_buffer_stake".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "executed_transactions_to_checkpoint".to_string(),
                ThConfig::new_with_rm_prefix_indexing(
                    tx_digest_indexing.clone(),
                    MUTEXES * 4,
                    uniform_key,
                    lru_bloom_config.clone(),
                    digest_prefix.clone(),
                ),
            ),
            (
                "pending_jwks".to_string(),
                ThConfig::new_with_config_indexing(
                    KeyIndexing::VariableLength,
                    1,
                    KeyType::uniform(1),
                    KeySpaceConfig::default(),
                ),
            ),
            (
                "active_jwks".to_string(),
                ThConfig::new_with_config_indexing(
                    KeyIndexing::VariableLength,
                    1,
                    KeyType::uniform(1),
                    KeySpaceConfig::default(),
                ),
            ),
            (
                "deferred_transactions".to_string(),
                ThConfig::new_with_indexing(KeyIndexing::Hash, MUTEXES, uniform_key),
            ),
            (
                "dkg_processed_messages_v2".to_string(),
                ThConfig::new(2, 1, KeyType::uniform(1)),
            ),
            (
                "dkg_used_messages_v2".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "dkg_confirmations_v2".to_string(),
                ThConfig::new(2, 1, KeyType::uniform(1)),
            ),
            (
                "dkg_output".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "randomness_next_round".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "randomness_highest_completed_round".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "randomness_last_round_timestamp".to_string(),
                ThConfig::new(8, 1, KeyType::uniform(1)),
            ),
            (
                "congestion_control_object_debts".to_string(),
                ThConfig::new_with_config(32, MUTEXES, uniform_key, lru_bloom_config.clone()),
            ),
            (
                "congestion_control_randomness_object_debts".to_string(),
                ThConfig::new(32, MUTEXES, uniform_key),
            ),
            (
                "execution_time_observations".to_string(),
                ThConfig::new(8 + 4, MUTEXES, uniform_key),
            ),
        ];
        Self::open_tables_read_write(
            Self::path(epoch, parent_path),
            MetricConf::new("epoch"),
            configs.into_iter().collect(),
        )
    }

    pub fn open_readonly(epoch: EpochId, parent_path: &Path) -> AuthorityEpochTablesReadOnly {
        Self::get_read_only_handle(
            Self::path(epoch, parent_path),
            None,
            None,
            MetricConf::new("epoch_readonly"),
        )
    }

    pub fn path(epoch: EpochId, parent_path: &Path) -> PathBuf {
        parent_path.join(format!("{}{}", EPOCH_DB_PREFIX, epoch))
    }

    fn load_reconfig_state(&self) -> SuiResult<ReconfigState> {
        let state = self
            .reconfig_state
            .get(&RECONFIG_STATE_INDEX)?
            .unwrap_or_default();
        Ok(state)
    }

    pub fn get_all_pending_consensus_transactions(&self) -> SuiResult<Vec<ConsensusTransaction>> {
        Ok(self
            .pending_consensus_transactions
            .safe_iter()
            .map(|item| item.map(|(_k, v)| v))
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// WARNING: This method is very subtle and can corrupt the database if used incorrectly.
    /// It should only be used in one-off cases or tests after fully understanding the risk.
    pub fn remove_executed_tx_subtle(&self, digest: &TransactionDigest) -> SuiResult {
        self.executed_transactions_to_checkpoint.remove(digest)?;
        Ok(())
    }

    pub fn get_last_consensus_index(&self) -> SuiResult<Option<ExecutionIndices>> {
        Ok(self
            .last_consensus_stats
            .get(&LAST_CONSENSUS_STATS_ADDR)?
            .map(|s| s.index))
    }

    pub fn get_last_consensus_stats(&self) -> SuiResult<Option<ExecutionIndicesWithStats>> {
        Ok(self.last_consensus_stats.get(&LAST_CONSENSUS_STATS_ADDR)?)
    }

    pub fn get_locked_transaction(&self, obj_ref: &ObjectRef) -> SuiResult<Option<LockDetails>> {
        Ok(self
            .owned_object_locked_transactions
            .get(obj_ref)?
            .map(|l| l.migrate().into_inner()))
    }

    pub fn multi_get_locked_transactions(
        &self,
        owned_input_objects: &[ObjectRef],
    ) -> SuiResult<Vec<Option<LockDetails>>> {
        Ok(self
            .owned_object_locked_transactions
            .multi_get(owned_input_objects)?
            .into_iter()
            .map(|l| l.map(|l| l.migrate().into_inner()))
            .collect())
    }

    pub fn write_transaction_locks(
        &self,
        signed_transaction: Option<VerifiedSignedTransaction>,
        locks_to_write: impl Iterator<Item = (ObjectRef, LockDetails)>,
    ) -> SuiResult {
        let mut batch = self.owned_object_locked_transactions.batch();
        batch.insert_batch(
            &self.owned_object_locked_transactions,
            locks_to_write.map(|(obj_ref, lock)| (obj_ref, LockDetailsWrapper::from(lock))),
        )?;
        if let Some(signed_transaction) = signed_transaction {
            batch.insert_batch(
                &self.signed_transactions,
                std::iter::once((
                    *signed_transaction.digest(),
                    signed_transaction.serializable_ref(),
                )),
            )?;
        }
        batch.write()?;
        Ok(())
    }

    fn get_all_deferred_transactions(
        &self,
    ) -> SuiResult<BTreeMap<DeferralKey, Vec<VerifiedSequencedConsensusTransaction>>> {
        Ok(self
            .deferred_transactions
            .safe_iter()
            .collect::<Result<_, _>>()?)
    }
}

pub(crate) const MUTEX_TABLE_SIZE: usize = 1024;

impl AuthorityPerEpochStore {
    #[instrument(name = "AuthorityPerEpochStore::new", level = "error", skip_all, fields(epoch = committee.epoch))]
    pub fn new(
        name: AuthorityName,
        committee: Arc<Committee>,
        parent_path: &Path,
        db_options: Option<Options>,
        metrics: Arc<EpochMetrics>,
        epoch_start_configuration: EpochStartConfiguration,
        backing_package_store: Arc<dyn BackingPackageStore + Send + Sync>,
        object_store: Arc<dyn ObjectStore + Send + Sync>,
        cache_metrics: Arc<ResolverMetrics>,
        signature_verifier_metrics: Arc<SignatureVerifierMetrics>,
        expensive_safety_check_config: &ExpensiveSafetyCheckConfig,
        chain: (ChainIdentifier, Chain),
        highest_executed_checkpoint: CheckpointSequenceNumber,
    ) -> SuiResult<Arc<Self>> {
        let current_time = Instant::now();
        let epoch_id = committee.epoch;
        metrics.current_epoch.set(epoch_id as i64);
        metrics
            .current_voting_right
            .set(committee.weight(&name) as i64);

        let tables = AuthorityEpochTables::open(epoch_id, parent_path, db_options.clone());
        let end_of_publish =
            StakeAggregator::from_iter(committee.clone(), tables.end_of_publish.safe_iter())?;
        let reconfig_state = tables
            .load_reconfig_state()
            .expect("Load reconfig state at initialization cannot fail");

        let epoch_alive_notify = NotifyOnce::new();
        let pending_consensus_transactions = tables.get_all_pending_consensus_transactions()?;
        let pending_consensus_certificates: HashSet<_> = pending_consensus_transactions
            .iter()
            .filter_map(|transaction| {
                if let ConsensusTransactionKind::CertifiedTransaction(certificate) =
                    &transaction.kind
                {
                    Some(*certificate.digest())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            epoch_start_configuration.epoch_start_state().epoch(),
            epoch_id
        );

        let epoch_start_configuration = Arc::new(epoch_start_configuration);
        info!("epoch flags: {:?}", epoch_start_configuration.flags());
        let protocol_version = epoch_start_configuration
            .epoch_start_state()
            .protocol_version();

        let chain_from_id = chain.0.chain();
        if chain_from_id == Chain::Mainnet || chain_from_id == Chain::Testnet {
            assert_eq!(
                chain_from_id, chain.1,
                "cannot override chain on production networks!"
            );
        }
        info!(
            "initializing epoch store from chain id {:?} to chain id {:?}",
            chain_from_id, chain.1
        );

        let protocol_config = ProtocolConfig::get_for_version(protocol_version, chain.1);

        let execution_component = ExecutionComponents::new(
            &protocol_config,
            backing_package_store,
            cache_metrics,
            expensive_safety_check_config,
        );

        let zklogin_env = match chain.1 {
            // Testnet and mainnet are treated the same since it is permanent.
            Chain::Mainnet | Chain::Testnet => ZkLoginEnv::Prod,
            _ => ZkLoginEnv::Test,
        };

        let supported_providers = protocol_config
            .zklogin_supported_providers()
            .iter()
            .map(|s| OIDCProvider::from_str(s).expect("Invalid provider string"))
            .collect::<Vec<_>>();

        let signature_verifier = SignatureVerifier::new(
            committee.clone(),
            signature_verifier_metrics,
            supported_providers,
            zklogin_env,
            protocol_config.verify_legacy_zklogin_address(),
            protocol_config.accept_zklogin_in_multisig(),
            protocol_config.accept_passkey_in_multisig(),
            protocol_config.zklogin_max_epoch_upper_bound_delta(),
            protocol_config.get_aliased_addresses().clone(),
            protocol_config.additional_multisig_checks(),
        );

        let authenticator_state_exists = epoch_start_configuration
            .authenticator_obj_initial_shared_version()
            .is_some();
        let authenticator_state_enabled =
            authenticator_state_exists && protocol_config.enable_jwk_consensus_updates();

        if authenticator_state_enabled {
            info!("authenticator_state enabled");
            let authenticator_state = get_authenticator_state(&*object_store)
                .expect("Read cannot fail")
                .expect("Authenticator state must exist");

            for active_jwk in &authenticator_state.active_jwks {
                let ActiveJwk { jwk_id, jwk, epoch } = active_jwk;
                assert!(epoch <= &epoch_id);
                signature_verifier.insert_jwk(jwk_id, jwk);
            }
        } else {
            info!("authenticator_state disabled");
        }

        let mut jwk_aggregator = JwkAggregator::new(committee.clone());

        for item in tables.pending_jwks.safe_iter() {
            let ((authority, id, jwk), _) = item?;
            jwk_aggregator.insert(authority, (id, jwk));
        }

        let jwk_aggregator = Mutex::new(jwk_aggregator);

        let consensus_output_cache = ConsensusOutputCache::new(&epoch_start_configuration, &tables);

        let execution_time_observations = tables
            .execution_time_observations
            .safe_iter()
            .collect::<Result<Vec<_>, _>>()?;
        let execution_time_estimator =
            if let PerObjectCongestionControlMode::ExecutionTimeEstimate(protocol_params) =
                protocol_config.per_object_congestion_control_mode()
            {
                Some(ExecutionTimeEstimator::new(
                    committee.clone(),
                    protocol_params,
                    // Load observations stored at end of previous epoch.
                    Self::get_stored_execution_time_observations(
                        &protocol_config,
                        committee.clone(),
                        &*object_store,
                        &metrics,
                        protocol_params.default_none_duration_for_new_keys,
                    )
                    // Load observations stored during the current epoch.
                    .chain(execution_time_observations.into_iter().flat_map(
                        |((generation, source), observations)| {
                            observations.into_iter().map(move |(key, duration)| {
                                (source, Some(generation), key, duration)
                            })
                        },
                    )),
                ))
            } else {
                None
            };

        let consensus_tx_status_cache = if protocol_config.mysticeti_fastpath() {
            Some(ConsensusTxStatusCache::new())
        } else {
            None
        };

        let tx_reject_reason_cache = if protocol_config.mysticeti_fastpath() {
            Some(TransactionRejectReasonCache::new(None, epoch_id))
        } else {
            None
        };

        let s = Arc::new(Self {
            name,
            committee: committee.clone(),
            protocol_config,
            tables: ArcSwapOption::new(Some(Arc::new(tables))),
            consensus_output_cache,
            consensus_quarantine: RwLock::new(ConsensusOutputQuarantine::new(
                highest_executed_checkpoint,
                metrics.clone(),
            )),
            parent_path: parent_path.to_path_buf(),
            db_options,
            reconfig_state_mem: RwLock::new(reconfig_state),
            epoch_alive_notify,
            user_certs_closed_notify: NotifyOnce::new(),
            epoch_alive: tokio::sync::RwLock::new(true),
            consensus_notify_read: NotifyRead::new(),
            executed_transactions_to_checkpoint_notify_read: NotifyRead::new(),
            signature_verifier,
            checkpoint_state_notify_read: NotifyRead::new(),
            running_root_notify_read: NotifyRead::new(),
            executed_digests_notify_read: NotifyRead::new(),
            end_of_publish: Mutex::new(end_of_publish),
            pending_consensus_certificates: RwLock::new(pending_consensus_certificates),
            mutex_table: MutexTable::new(MUTEX_TABLE_SIZE),
            version_assignment_mutex_table: MutexTable::new(MUTEX_TABLE_SIZE),
            epoch_open_time: current_time,
            epoch_close_time: Default::default(),
            metrics,
            epoch_start_configuration,
            execution_component,
            chain,
            jwk_aggregator,
            randomness_manager: OnceCell::new(),
            randomness_reporter: OnceCell::new(),
            execution_time_estimator: tokio::sync::Mutex::new(execution_time_estimator),
            tx_local_execution_time: OnceCell::new(),
            tx_object_debts: OnceCell::new(),
            end_of_epoch_execution_time_observations: OnceCell::new(),
            consensus_tx_status_cache,
            tx_reject_reason_cache,
            settlement_registrations: Default::default(),
        });

        s.update_buffer_stake_metric();
        Ok(s)
    }

    pub fn tables(&self) -> SuiResult<Arc<AuthorityEpochTables>> {
        match self.tables.load_full() {
            Some(tables) => Ok(tables),
            None => Err(SuiError::EpochEnded(self.epoch())),
        }
    }

    // Ideally the epoch tables handle should have the same lifetime as the outer AuthorityPerEpochStore,
    // and this function should be unnecessary. But unfortunately, Arc<AuthorityPerEpochStore> outlives the
    // epoch significantly right now, so we need to manually release the tables to release its memory usage.
    pub fn release_db_handles(&self) {
        // When the logic to release DB handles becomes obsolete, it may still be useful
        // to make sure AuthorityEpochTables is not used after the next epoch starts.
        self.tables.store(None);
    }

    // Returns true if authenticator state is enabled in the protocol config *and* the
    // authenticator state object already exists
    pub fn authenticator_state_enabled(&self) -> bool {
        self.protocol_config().enable_jwk_consensus_updates() && self.authenticator_state_exists()
    }

    pub fn authenticator_state_exists(&self) -> bool {
        self.epoch_start_configuration
            .authenticator_obj_initial_shared_version()
            .is_some()
    }

    // Returns true if randomness state is enabled in the protocol config *and* the
    // randomness state object already exists
    pub fn randomness_state_enabled(&self) -> bool {
        self.protocol_config().random_beacon() && self.randomness_state_exists()
    }

    pub fn randomness_state_exists(&self) -> bool {
        self.epoch_start_configuration
            .randomness_obj_initial_shared_version()
            .is_some()
    }

    pub fn randomness_reporter(&self) -> Option<RandomnessReporter> {
        self.randomness_reporter.get().cloned()
    }

    pub async fn set_randomness_manager(
        &self,
        mut randomness_manager: RandomnessManager,
    ) -> SuiResult<()> {
        let reporter = randomness_manager.reporter();
        let result = randomness_manager.start_dkg().await;
        if self
            .randomness_manager
            .set(tokio::sync::Mutex::new(randomness_manager))
            .is_err()
        {
            debug_fatal!(
                "BUG: `set_randomness_manager` called more than once; this should never happen"
            );
        }
        if self.randomness_reporter.set(reporter).is_err() {
            debug_fatal!(
                "BUG: `set_randomness_manager` called more than once; this should never happen"
            );
        }
        result
    }

    pub fn accumulator_root_exists(&self) -> bool {
        self.epoch_start_configuration
            .accumulator_root_obj_initial_shared_version()
            .is_some()
    }

    pub fn accumulators_enabled(&self) -> bool {
        self.protocol_config().enable_accumulators() && self.accumulator_root_exists()
    }

    pub fn coin_deny_list_state_exists(&self) -> bool {
        self.epoch_start_configuration
            .coin_deny_list_obj_initial_shared_version()
            .is_some()
    }

    pub fn coin_deny_list_v1_enabled(&self) -> bool {
        self.protocol_config().enable_coin_deny_list_v1() && self.coin_deny_list_state_exists()
    }

    pub fn bridge_exists(&self) -> bool {
        self.epoch_start_configuration
            .bridge_obj_initial_shared_version()
            .is_some()
    }

    pub fn bridge_committee_initiated(&self) -> bool {
        self.epoch_start_configuration.bridge_committee_initiated()
    }

    pub fn get_parent_path(&self) -> PathBuf {
        self.parent_path.clone()
    }

    /// Returns `&Arc<EpochStartConfiguration>`
    /// User can treat this `Arc` as `&EpochStartConfiguration`, or clone the Arc to pass as owned object
    pub fn epoch_start_config(&self) -> &Arc<EpochStartConfiguration> {
        &self.epoch_start_configuration
    }

    pub fn epoch_start_state(&self) -> &EpochStartSystemState {
        self.epoch_start_configuration.epoch_start_state()
    }

    pub fn get_chain_identifier(&self) -> ChainIdentifier {
        self.chain.0
    }

    pub fn get_chain(&self) -> Chain {
        self.chain.1
    }

    pub fn new_at_next_epoch(
        &self,
        name: AuthorityName,
        new_committee: Committee,
        epoch_start_configuration: EpochStartConfiguration,
        backing_package_store: Arc<dyn BackingPackageStore + Send + Sync>,
        object_store: Arc<dyn ObjectStore + Send + Sync>,
        expensive_safety_check_config: &ExpensiveSafetyCheckConfig,
        previous_epoch_last_checkpoint: CheckpointSequenceNumber,
    ) -> SuiResult<Arc<Self>> {
        assert_eq!(self.epoch() + 1, new_committee.epoch);
        self.record_reconfig_halt_duration_metric();
        self.record_epoch_total_duration_metric();
        Self::new(
            name,
            Arc::new(new_committee),
            &self.parent_path,
            self.db_options.clone(),
            self.metrics.clone(),
            epoch_start_configuration,
            backing_package_store,
            object_store,
            self.execution_component.metrics(),
            self.signature_verifier.metrics.clone(),
            expensive_safety_check_config,
            self.chain,
            previous_epoch_last_checkpoint,
        )
    }

    pub fn new_at_next_epoch_for_testing(
        &self,
        backing_package_store: Arc<dyn BackingPackageStore + Send + Sync>,
        object_store: Arc<dyn ObjectStore + Send + Sync>,
        expensive_safety_check_config: &ExpensiveSafetyCheckConfig,
        previous_epoch_last_checkpoint: CheckpointSequenceNumber,
    ) -> Arc<Self> {
        let next_epoch = self.epoch() + 1;
        let next_committee = Committee::new(
            next_epoch,
            self.committee.voting_rights.iter().cloned().collect(),
        );
        self.new_at_next_epoch(
            self.name,
            next_committee,
            self.epoch_start_configuration
                .new_at_next_epoch_for_testing(),
            backing_package_store,
            object_store,
            expensive_safety_check_config,
            previous_epoch_last_checkpoint,
        )
        .expect("failed to create new authority per epoch store")
    }

    pub fn committee(&self) -> &Arc<Committee> {
        &self.committee
    }

    pub fn protocol_config(&self) -> &ProtocolConfig {
        &self.protocol_config
    }

    pub fn epoch(&self) -> EpochId {
        self.committee.epoch
    }

    pub fn tx_validity_check_context(&self) -> TxValidityCheckContext {
        TxValidityCheckContext {
            config: &self.protocol_config,
            epoch: self.epoch(),
            accumulator_object_init_shared_version: self
                .epoch_start_configuration
                .accumulator_root_obj_initial_shared_version(),
        }
    }

    pub fn get_state_hash_for_checkpoint(
        &self,
        checkpoint: &CheckpointSequenceNumber,
    ) -> SuiResult<Option<GlobalStateHash>> {
        Ok(self
            .tables()?
            .state_hash_by_checkpoint
            .get(checkpoint)
            .expect("db error"))
    }

    pub fn insert_state_hash_for_checkpoint(
        &self,
        checkpoint: &CheckpointSequenceNumber,
        accumulator: &GlobalStateHash,
    ) -> SuiResult {
        self.tables()?
            .state_hash_by_checkpoint
            .insert(checkpoint, accumulator)
            .expect("db error");
        Ok(())
    }

    pub fn get_running_root_state_hash(
        &self,
        checkpoint: CheckpointSequenceNumber,
    ) -> SuiResult<Option<GlobalStateHash>> {
        Ok(self
            .tables()?
            .running_root_state_hash
            .get(&checkpoint)
            .expect("db error"))
    }

    pub fn get_highest_running_root_state_hash(
        &self,
    ) -> SuiResult<Option<(CheckpointSequenceNumber, GlobalStateHash)>> {
        Ok(self
            .tables()?
            .running_root_state_hash
            .reversed_safe_iter_with_bounds(None, None)?
            .next()
            .transpose()?)
    }

    pub fn insert_running_root_state_hash(
        &self,
        checkpoint: &CheckpointSequenceNumber,
        hash: &GlobalStateHash,
    ) -> SuiResult {
        self.tables()?
            .running_root_state_hash
            .insert(checkpoint, hash)?;
        self.running_root_notify_read.notify(checkpoint, hash);

        Ok(())
    }

    pub fn reference_gas_price(&self) -> u64 {
        self.epoch_start_state().reference_gas_price()
    }

    pub fn protocol_version(&self) -> ProtocolVersion {
        self.epoch_start_state().protocol_version()
    }

    pub fn module_cache(&self) -> &Arc<ExecutionModuleCache> {
        &self.execution_component.module_cache
    }

    pub fn executor(&self) -> &Arc<dyn Executor + Send + Sync> {
        &self.execution_component.executor
    }

    pub fn set_local_execution_time_channels(
        &self,
        tx_local_execution_time: mpsc::Sender<(
            ProgrammableTransaction,
            Vec<ExecutionTiming>,
            Duration,
            u64, // gas_price
        )>,
        tx_object_debts: mpsc::Sender<Vec<ObjectID>>,
    ) {
        if let Err(e) = self.tx_local_execution_time.set(tx_local_execution_time) {
            debug_fatal!(
                "failed to set tx_local_execution_time channel on AuthorityPerEpochStore: {e:?}"
            );
        }
        if let Err(e) = self.tx_object_debts.set(tx_object_debts) {
            debug_fatal!("failed to set tx_object_debts channel on AuthorityPerEpochStore: {e:?}");
        }
    }

    pub fn record_local_execution_time(
        &self,
        tx: &TransactionData,
        effects: &TransactionEffects,
        timings: Vec<ExecutionTiming>,
        total_duration: Duration,
    ) {
        let Some(tx_local_execution_time) = self.tx_local_execution_time.get() else {
            // Drop observations if no ExecutionTimeObserver has been configured.
            return;
        };

        if effects.status().is_cancelled() {
            return;
        }

        // Only record timings for PTBs with shared inputs.
        let TransactionKind::ProgrammableTransaction(ptb) = tx.kind() else {
            return;
        };
        if !ptb
            .inputs
            .iter()
            .any(|input| matches!(input, CallArg::Object(ObjectArg::SharedObject { .. })))
        {
            return;
        }

        if let Err(e) = tx_local_execution_time.try_send((
            ptb.clone(),
            timings,
            total_duration,
            tx.gas_data().price,
        )) {
            // This channel should not overflow, but if it does, don't wait; just log an error
            // and drop the observation.
            self.metrics.epoch_execution_time_measurements_dropped.inc();
            warn!("failed to send local execution time to observer: {e}");
        }
    }

    pub fn get_stored_execution_time_observations(
        protocol_config: &ProtocolConfig,
        committee: Arc<Committee>,
        object_store: &dyn ObjectStore,
        metrics: &EpochMetrics,
        use_none_generation: bool,
    ) -> impl Iterator<
        Item = (
            AuthorityIndex,
            Option<u64>,
            ExecutionTimeObservationKey,
            Duration,
        ),
    > {
        if !matches!(
            protocol_config.per_object_congestion_control_mode(),
            PerObjectCongestionControlMode::ExecutionTimeEstimate(_)
        ) {
            return itertools::Either::Left(std::iter::empty());
        }

        // Load stored execution time observations from the SuiSystemState object.
        let system_state =
            sui_system_state::get_sui_system_state(object_store).expect("System state must exist");
        let system_state = match system_state {
            SuiSystemState::V2(system_state) => system_state,
            SuiSystemState::V1(_) => {
                if committee.epoch() > 1 {
                    error!("`PerObjectCongestionControlMode::ExecutionTimeEstimate` cannot load execution time observations to SuiSystemState because it has an old version. This should not happen outside tests.");
                }
                return itertools::Either::Left(std::iter::empty());
            }
            #[cfg(msim)]
            SuiSystemState::SimTestV1(_)
            | SuiSystemState::SimTestShallowV2(_)
            | SuiSystemState::SimTestDeepV2(_) => {
                return itertools::Either::Left(std::iter::empty());
            }
        };
        // This is stored as a vector<u8> in Move, so we double-deserialize to get back
        //`StoredExecutionTimeObservations`.
        let Ok::<Vec<u8>, _>(stored_observations_bytes) = get_dynamic_field_from_store(
            object_store,
            system_state.extra_fields.id.id.bytes,
            &EXTRA_FIELD_EXECUTION_TIME_ESTIMATES_KEY,
        ) else {
            warn!("Could not find stored execution time observations. This should only happen in the first epcoh where ExecutionTimeEstimate mode is enabled.");
            return itertools::Either::Left(std::iter::empty());
        };
        let stored_observations: StoredExecutionTimeObservations =
            bcs::from_bytes(&stored_observations_bytes)
                .expect("failed to deserialize stored execution time estimates");
        let stored_observations = stored_observations.unwrap_v1();

        info!(
            "loaded stored execution time observations for {} keys",
            stored_observations.len()
        );
        metrics
            .epoch_execution_time_observations_loaded
            .set(stored_observations.len() as i64);
        assert_reachable!("successfully loads stored execution time observations");

        // Make a single flattened iterator with every stored observation, for consumption
        // by the `ExecutionTimeEstimator` constructor.
        itertools::Either::Right(stored_observations.into_iter().flat_map(
            move |(key, observations)| {
                let committee = committee.clone();
                observations
                    .into_iter()
                    .filter_map(move |(authority, duration)| {
                        committee
                            .authority_index(&authority)
                            .map(|authority_index| {
                                (
                                    authority_index,
                                    // For bug compatibility with previous version, can be
                                    // removed once set to true on mainnet.
                                    if use_none_generation { None } else { Some(0) },
                                    key.clone(),
                                    duration,
                                )
                            })
                    })
            },
        ))
    }

    pub fn get_end_of_epoch_execution_time_observations(&self) -> &StoredExecutionTimeObservations {
        self.end_of_epoch_execution_time_observations.get().expect(
            "`get_end_of_epoch_execution_time_observations` must not be called until end of epoch",
        )
    }

    pub fn acquire_tx_guard(&self, cert: &VerifiedExecutableTransaction) -> SuiResult<CertTxGuard> {
        let digest = cert.digest();
        Ok(CertTxGuard(self.acquire_tx_lock(digest)))
    }

    /// Acquire the lock for a tx without writing to the WAL.
    pub fn acquire_tx_lock(&self, digest: &TransactionDigest) -> CertLockGuard {
        CertLockGuard(self.mutex_table.acquire_lock(*digest))
    }

    pub fn store_reconfig_state(&self, new_state: &ReconfigState) -> SuiResult {
        self.tables()?
            .reconfig_state
            .insert(&RECONFIG_STATE_INDEX, new_state)?;
        Ok(())
    }

    pub fn insert_signed_transaction(&self, transaction: VerifiedSignedTransaction) -> SuiResult {
        Ok(self
            .tables()?
            .signed_transactions
            .insert(transaction.digest(), transaction.serializable_ref())?)
    }

    #[cfg(test)]
    pub fn delete_signed_transaction_for_test(&self, transaction: &TransactionDigest) {
        self.tables()
            .expect("test should not cross epoch boundary")
            .signed_transactions
            .remove(transaction)
            .unwrap();
    }

    #[cfg(test)]
    pub fn delete_object_locks_for_test(&self, objects: &[ObjectRef]) {
        for object in objects {
            self.tables()
                .expect("test should not cross epoch boundary")
                .owned_object_locked_transactions
                .remove(object)
                .unwrap();
        }
    }

    pub fn get_signed_transaction(
        &self,
        tx_digest: &TransactionDigest,
    ) -> SuiResult<Option<VerifiedSignedTransaction>> {
        Ok(self
            .tables()?
            .signed_transactions
            .get(tx_digest)?
            .map(|t| t.into()))
    }

    #[instrument(level = "trace", skip_all)]
    pub fn insert_tx_cert_sig(
        &self,
        tx_digest: &TransactionDigest,
        cert_sig: &AuthorityStrongQuorumSignInfo,
    ) -> SuiResult {
        let tables = self.tables()?;
        Ok(tables
            .transaction_cert_signatures
            .insert(tx_digest, cert_sig)?)
    }

    /// Record that a transaction has been executed in the current epoch.
    /// Used by checkpoint builder to cull dependencies from previous epochs.
    #[instrument(level = "trace", skip_all)]
    pub fn insert_executed_in_epoch(&self, tx_digest: &TransactionDigest) {
        self.consensus_output_cache
            .insert_executed_in_epoch(*tx_digest);
    }

    /// Record a mapping from a transaction key (such as TransactionKey::RandomRound) to its digest.
    pub(crate) fn insert_tx_key(
        &self,
        tx_key: TransactionKey,
        tx_digest: TransactionDigest,
    ) -> SuiResult {
        let _metrics_scope =
            mysten_metrics::monitored_scope("AuthorityPerEpochStore::insert_tx_key");

        if matches!(tx_key, TransactionKey::Digest(_)) {
            debug_fatal!("useless to insert a digest key");
            return Ok(());
        }

        let tables = self.tables()?;
        tables
            .transaction_key_to_digest
            .insert(&tx_key, &tx_digest)?;
        self.executed_digests_notify_read
            .notify(&tx_key, &tx_digest);
        Ok(())
    }

    pub fn tx_key_to_digest(&self, key: &TransactionKey) -> SuiResult<Option<TransactionDigest>> {
        let tables = self.tables()?;
        if let TransactionKey::Digest(digest) = key {
            Ok(Some(*digest))
        } else {
            Ok(tables.transaction_key_to_digest.get(key).expect("db error"))
        }
    }

    pub(crate) fn notify_settlement_transactions_ready(
        &self,
        tx_key: TransactionKey,
        txns: Vec<VerifiedExecutableTransaction>,
    ) {
        debug_assert!(matches!(tx_key, TransactionKey::AccumulatorSettlement(..)));
        let mut registrations = self.settlement_registrations.lock();
        if let Some(registration) = registrations.remove(&tx_key) {
            let SettlementRegistration::Waiting(tx) = registration else {
                fatal!("Settlement registration should be waiting");
            };
            tx.send(txns).unwrap();
        } else {
            registrations.insert(tx_key, SettlementRegistration::Ready(txns));
        }
    }

    pub(crate) async fn wait_for_settlement_transactions(
        &self,
        key: TransactionKey,
    ) -> Vec<VerifiedExecutableTransaction> {
        let rx = {
            let mut registrations = self.settlement_registrations.lock();
            if let Some(registration) = registrations.remove(&key) {
                let SettlementRegistration::Ready(txns) = registration else {
                    fatal!("Settlement registration should be ready");
                };
                return txns;
            } else {
                let (tx, rx) = oneshot::channel();
                registrations.insert(key, SettlementRegistration::Waiting(tx));
                rx
            }
        };

        rx.await.unwrap()
    }

    pub fn revert_executed_transaction(&self, tx_digest: &TransactionDigest) -> SuiResult {
        self.consensus_output_cache
            .remove_reverted_transaction(tx_digest);
        let tables = self.tables()?;
        let mut batch = tables.effects_signatures.batch();
        batch.delete_batch(&tables.effects_signatures, [*tx_digest])?;
        batch.write()?;
        Ok(())
    }

    pub fn insert_effects_digest_and_signature(
        &self,
        tx_digest: &TransactionDigest,
        effects_digest: &TransactionEffectsDigest,
        effects_signature: &AuthoritySignInfo,
    ) -> SuiResult {
        let tables = self.tables()?;
        let mut batch = tables.effects_signatures.batch();
        batch.insert_batch(&tables.effects_signatures, [(tx_digest, effects_signature)])?;
        batch.insert_batch(
            &tables.signed_effects_digests,
            [(tx_digest, effects_digest)],
        )?;
        batch.write()?;
        Ok(())
    }

    pub fn transactions_executed_in_cur_epoch(
        &self,
        digests: &[TransactionDigest],
    ) -> SuiResult<Vec<bool>> {
        let tables = self.tables()?;
        Ok(do_fallback_lookup(
            digests,
            |digest| {
                if self
                    .consensus_output_cache
                    .executed_in_current_epoch(digest)
                {
                    CacheResult::Hit(true)
                } else {
                    CacheResult::Miss
                }
            },
            |digests| {
                tables
                    .executed_transactions_to_checkpoint
                    .multi_contains_keys(digests)
                    .expect("db error")
            },
        ))
    }

    pub fn get_effects_signature(
        &self,
        tx_digest: &TransactionDigest,
    ) -> SuiResult<Option<AuthoritySignInfo>> {
        let tables = self.tables()?;
        Ok(tables.effects_signatures.get(tx_digest)?)
    }

    pub fn get_signed_effects_digest(
        &self,
        tx_digest: &TransactionDigest,
    ) -> SuiResult<Option<TransactionEffectsDigest>> {
        let tables = self.tables()?;
        Ok(tables.signed_effects_digests.get(tx_digest)?)
    }

    pub fn get_transaction_cert_sig(
        &self,
        tx_digest: &TransactionDigest,
    ) -> SuiResult<Option<AuthorityStrongQuorumSignInfo>> {
        Ok(self.tables()?.transaction_cert_signatures.get(tx_digest)?)
    }

    /// Resolves InputObjectKinds into InputKeys. `assigned_versions` is used to map shared inputs
    /// to specific object versions.
    pub(crate) fn get_input_object_keys(
        &self,
        key: &TransactionKey,
        objects: &[InputObjectKind],
        assigned_versions: &AssignedVersions,
    ) -> BTreeSet<InputKey> {
        let assigned_shared_versions = assigned_versions
            .iter()
            .cloned()
            .collect::<BTreeMap<_, _>>();
        objects
            .iter()
            .map(|kind| {
                match kind {
                    InputObjectKind::SharedMoveObject {
                        id,
                        initial_shared_version,
                        ..
                    } => {
                        // If we found assigned versions, but they are missing the assignment for
                        // this object, it indicates a serious inconsistency!
                        let Some(version) = assigned_shared_versions.get(&(*id, *initial_shared_version)) else {
                            panic!(
                                "Shared object version should have been assigned. key: {key:?}, \
                                obj id: {id:?}, initial_shared_version: {initial_shared_version:?}, \
                                assigned_shared_versions: {assigned_shared_versions:?}",
                            )
                        };
                        InputKey::VersionedObject {
                            id: FullObjectID::new(*id, Some(*initial_shared_version)),
                            version: *version,
                        }
                    }
                    InputObjectKind::MovePackage(id) => InputKey::Package { id: *id },
                    InputObjectKind::ImmOrOwnedMoveObject(objref) => InputKey::VersionedObject {
                        id: FullObjectID::new(objref.0, None),
                        version: objref.1,
                    },
                }
            })
            .collect()
    }

    pub fn get_last_consensus_stats(&self) -> SuiResult<ExecutionIndicesWithStats> {
        assert!(
            self.consensus_quarantine.read().is_empty(),
            "get_last_consensus_stats should only be called at startup"
        );
        match self.tables()?.get_last_consensus_stats()? {
            Some(stats) => Ok(stats),
            None => {
                let indices = self
                    .tables()?
                    .get_last_consensus_index()
                    .map(|x| x.unwrap_or_default())?;
                Ok(ExecutionIndicesWithStats {
                    index: indices,
                    hash: 0, // unused
                    stats: ConsensusStats::default(),
                })
            }
        }
    }

    pub fn get_accumulators_in_checkpoint_range(
        &self,
        from_checkpoint: CheckpointSequenceNumber,
        to_checkpoint: CheckpointSequenceNumber,
    ) -> SuiResult<Vec<(CheckpointSequenceNumber, GlobalStateHash)>> {
        self.tables()?
            .state_hash_by_checkpoint
            .safe_range_iter(from_checkpoint..=to_checkpoint)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Returns future containing the state accumulator for the given epoch
    /// once available.
    pub async fn notify_read_checkpoint_state_hasher(
        &self,
        checkpoints: &[CheckpointSequenceNumber],
    ) -> SuiResult<Vec<GlobalStateHash>> {
        let tables = self.tables()?;
        Ok(self
            .checkpoint_state_notify_read
            .read(
                "notify_read_checkpoint_state_hasher",
                checkpoints,
                |checkpoints| {
                    tables
                        .state_hash_by_checkpoint
                        .multi_get(checkpoints)
                        .expect("db error")
                },
            )
            .await)
    }

    pub async fn notify_read_running_root(
        &self,
        checkpoint: CheckpointSequenceNumber,
    ) -> SuiResult<GlobalStateHash> {
        let registration = self.running_root_notify_read.register_one(&checkpoint);
        let acc = self.tables()?.running_root_state_hash.get(&checkpoint)?;

        let result = match acc {
            Some(ready) => Either::Left(futures::future::ready(ready)),
            None => Either::Right(registration),
        }
        .await;

        Ok(result)
    }

    /// Called when transaction outputs are committed to disk
    #[instrument(level = "trace", skip_all)]
    pub fn handle_finalized_checkpoint(
        &self,
        checkpoint: &CheckpointSummary,
        digests: &[TransactionDigest],
    ) -> SuiResult<()> {
        let tables = match self.tables() {
            Ok(tables) => tables,
            // After Epoch ends, it is no longer necessary to remove pending transactions
            // because the table will not be used anymore and be deleted eventually.
            Err(SuiError::EpochEnded(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut batch = tables.signed_effects_digests.batch();

        // Now that the transaction effects are committed, we will never re-execute, so we
        // don't need to worry about equivocating.
        batch.delete_batch(&tables.signed_effects_digests, digests)?;

        let seq = *checkpoint.sequence_number();

        let mut quarantine = self.consensus_quarantine.write();
        quarantine.update_highest_executed_checkpoint(seq, self, &mut batch)?;
        batch.write()?;

        self.consensus_output_cache
            .remove_executed_in_epoch(digests);

        Ok(())
    }

    pub fn get_all_pending_consensus_transactions(&self) -> Vec<ConsensusTransaction> {
        self.tables()
            .expect("recovery should not cross epoch boundary")
            .get_all_pending_consensus_transactions()
            .expect("failed to get pending consensus transactions")
    }

    #[cfg(test)]
    pub fn get_next_object_version(
        &self,
        obj: &ObjectID,
        start_version: SequenceNumber,
    ) -> Option<SequenceNumber> {
        self.tables()
            .expect("test should not cross epoch boundary")
            .next_shared_object_versions_v2
            .get(&(*obj, start_version))
            .unwrap()
    }

    pub fn insert_finalized_transactions(
        &self,
        digests: &[TransactionDigest],
        sequence: CheckpointSequenceNumber,
    ) -> SuiResult {
        let _metrics_scope = mysten_metrics::monitored_scope(
            "AuthorityPerEpochStore::insert_finalized_transactions",
        );

        let mut batch = self.tables()?.executed_transactions_to_checkpoint.batch();
        batch.insert_batch(
            &self.tables()?.executed_transactions_to_checkpoint,
            digests.iter().map(|d| (*d, sequence)),
        )?;
        batch.write()?;
        trace!("Transactions {digests:?} finalized at checkpoint {sequence}");

        // Notify all readers that the transactions have been finalized as part of a checkpoint execution.
        for digest in digests {
            self.executed_transactions_to_checkpoint_notify_read
                .notify(digest, &sequence);
        }

        Ok(())
    }

    pub fn is_transaction_executed_in_checkpoint(
        &self,
        digest: &TransactionDigest,
    ) -> SuiResult<bool> {
        Ok(self
            .tables()?
            .executed_transactions_to_checkpoint
            .contains_key(digest)?)
    }

    pub fn transactions_executed_in_checkpoint(
        &self,
        digests: impl Iterator<Item = TransactionDigest>,
    ) -> SuiResult<Vec<bool>> {
        Ok(self
            .tables()?
            .executed_transactions_to_checkpoint
            .multi_contains_keys(digests)?)
    }

    pub fn get_transaction_checkpoint(
        &self,
        digest: &TransactionDigest,
    ) -> SuiResult<Option<CheckpointSequenceNumber>> {
        Ok(self
            .tables()?
            .executed_transactions_to_checkpoint
            .get(digest)?)
    }

    pub fn multi_get_transaction_checkpoint(
        &self,
        digests: &[TransactionDigest],
    ) -> SuiResult<Vec<Option<CheckpointSequenceNumber>>> {
        Ok(self
            .tables()?
            .executed_transactions_to_checkpoint
            .multi_get(digests)?
            .into_iter()
            .collect())
    }

    // For each key in objects_to_init, return the next version for that key as recorded in the
    // next_shared_object_versions table.
    //
    // If any keys are missing, then we need to initialize the table. We first check if a previous
    // version of that object has been written. If so, then the object was written in a previous
    // epoch, and we initialize next_shared_object_versions to that value. If no version of the
    // object has yet been written, we initialize the object to the initial version recorded in the
    // certificate (which is a function of the lamport version computation of the transaction that
    // created the shared object originally - which transaction may not yet have been executed on
    // this node).
    //
    // Because all paths that assign shared versions for a shared object transaction call this
    // function, it is impossible for parent_sync to be updated before this function completes
    // successfully for each affected object id.
    pub(crate) fn get_or_init_next_object_versions(
        &self,
        objects_to_init: &[ConsensusObjectSequenceKey],
        cache_reader: &dyn ObjectCacheRead,
    ) -> SuiResult<HashMap<ConsensusObjectSequenceKey, SequenceNumber>> {
        // get_or_init_next_object_versions can be called
        // from consensus or checkpoint executor,
        // so we need to protect version assignment with a critical section
        let _locks = self
            .version_assignment_mutex_table
            .acquire_locks(objects_to_init.iter().map(|(id, _)| *id));
        let tables = self.tables()?;

        let next_versions = self
            .consensus_quarantine
            .read()
            .get_next_shared_object_versions(&tables, objects_to_init)?;

        let uninitialized_objects: Vec<ConsensusObjectSequenceKey> = next_versions
            .iter()
            .zip(objects_to_init)
            .filter_map(|(next_version, id_and_version)| match next_version {
                None => Some(*id_and_version),
                Some(_) => None,
            })
            .collect();

        // The common case is that there are no uninitialized versions - this early return will
        // happen every time except the first time an object is used in an epoch.
        if uninitialized_objects.is_empty() {
            // unwrap ok - we already verified that next_versions is not missing any keys.
            return Ok(izip!(
                objects_to_init.iter().cloned(),
                next_versions.into_iter().map(|v| v.unwrap())
            )
            .collect());
        }

        let versions_to_write: Vec<_> = uninitialized_objects
            .iter()
            .map(|(id, initial_version)| {
                // Note: we don't actually need to read from the transaction here, as no writer
                // can update object_store until after get_or_init_next_object_versions
                // completes.
                match cache_reader.get_object(id) {
                    Some(obj) => {
                        if obj.owner().start_version() == Some(*initial_version) {
                            ((*id, *initial_version), obj.version())
                        } else {
                            // If we can't find a matching start version, treat the object as
                            // if it's absent.
                            if self.protocol_config().reshare_at_same_initial_version() {
                                if let Some(obj_start_version) = obj.owner().start_version() {
                                    assert!(*initial_version >= obj_start_version,
                                            "should be impossible to certify a transaction with a start version that must have only existed in a previous epoch; obj = {obj:?} initial_version = {initial_version:?}, obj_start_version = {obj_start_version:?}");
                                }
                            }
                            ((*id, *initial_version), *initial_version)
                        }
                    }
                    None => ((*id, *initial_version), *initial_version),
                }
            })
            .collect();

        let ret = izip!(objects_to_init.iter().cloned(), next_versions.into_iter(),)
            // take all the previously initialized versions
            .filter_map(|(key, next_version)| next_version.map(|v| (key, v)))
            // add all the versions we're going to write
            .chain(versions_to_write.iter().cloned())
            .collect();

        debug!(
            ?versions_to_write,
            "initializing next_shared_object_versions"
        );
        tables
            .next_shared_object_versions_v2
            .multi_insert(versions_to_write)?;

        Ok(ret)
    }

    /// Given list of certificates, assign versions for all shared objects used in them.
    /// We start with the current next_shared_object_versions table for each object, and build
    /// up the versions based on the dependencies of each certificate.
    /// However, in the end we do not update the next_shared_object_versions table, which keeps
    /// this function idempotent. We should call this function when we are assigning shared object
    /// versions outside of consensus and do not want to taint the next_shared_object_versions table.
    pub fn assign_shared_object_versions_idempotent<'a>(
        &self,
        cache_reader: &dyn ObjectCacheRead,
        assignables: impl Iterator<Item = &'a Schedulable<&'a VerifiedExecutableTransaction>> + Clone,
    ) -> SuiResult<AssignedTxAndVersions> {
        Ok(SharedObjVerManager::assign_versions_from_consensus(
            self,
            cache_reader,
            assignables,
            &BTreeMap::new(),
        )?
        .assigned_versions)
    }

    fn load_deferred_transactions_for_randomness(
        &self,
        output: &mut ConsensusCommitOutput,
    ) -> SuiResult<Vec<(DeferralKey, Vec<VerifiedSequencedConsensusTransaction>)>> {
        let (min, max) = DeferralKey::full_range_for_randomness();
        self.load_deferred_transactions(output, min, max)
    }

    fn load_and_process_deferred_transactions_for_randomness(
        &self,
        output: &mut ConsensusCommitOutput,
        previously_deferred_tx_digests: &mut HashMap<TransactionDigest, DeferralKey>,
        sequenced_randomness_transactions: &mut Vec<VerifiedSequencedConsensusTransaction>,
    ) -> SuiResult {
        let deferred_randomness_txs = self.load_deferred_transactions_for_randomness(output)?;
        trace!(
            "loading deferred randomness transactions: {:?}",
            deferred_randomness_txs
        );
        previously_deferred_tx_digests.extend(deferred_randomness_txs.iter().flat_map(
            |(deferral_key, txs)| {
                txs.iter().map(|tx| match tx.0.transaction.key() {
                    SequencedConsensusTransactionKey::External(
                        ConsensusTransactionKey::Certificate(digest),
                    ) => (digest, *deferral_key),
                    _ => {
                        panic!("deferred randomness transaction was not a user certificate: {tx:?}")
                    }
                })
            },
        ));
        sequenced_randomness_transactions
            .extend(deferred_randomness_txs.into_iter().flat_map(|(_, txs)| txs));
        Ok(())
    }

    fn load_deferred_transactions_for_up_to_consensus_round(
        &self,
        output: &mut ConsensusCommitOutput,
        consensus_round: u64,
    ) -> SuiResult<Vec<(DeferralKey, Vec<VerifiedSequencedConsensusTransaction>)>> {
        let (min, max) = DeferralKey::range_for_up_to_consensus_round(consensus_round);
        self.load_deferred_transactions(output, min, max)
    }

    // factoring of the above
    fn load_deferred_transactions(
        &self,
        output: &mut ConsensusCommitOutput,
        min: DeferralKey,
        max: DeferralKey,
    ) -> SuiResult<Vec<(DeferralKey, Vec<VerifiedSequencedConsensusTransaction>)>> {
        debug!("Query epoch store to load deferred txn {:?} {:?}", min, max);

        let (keys, txns) = {
            let mut keys = Vec::new();
            let mut txns = Vec::new();

            let deferred_transactions = self.consensus_output_cache.deferred_transactions.lock();

            for (key, transactions) in deferred_transactions.range(min..max) {
                debug!(
                    "Loaded {:?} deferred txn with deferral key {:?}",
                    transactions.len(),
                    key
                );
                keys.push(*key);
                txns.push((*key, transactions.clone()));
            }

            (keys, txns)
        };

        // verify that there are no duplicates - should be impossible due to
        // is_consensus_message_processed
        #[cfg(debug_assertions)]
        {
            let mut seen = HashSet::new();
            for deferred_txn_batch in &txns {
                for txn in &deferred_txn_batch.1 {
                    assert!(seen.insert(txn.0.key()));
                }
            }
        }

        output.delete_loaded_deferred_transactions(&keys);

        Ok(txns)
    }

    pub fn get_all_deferred_transactions_for_test(
        &self,
    ) -> Vec<(DeferralKey, Vec<VerifiedSequencedConsensusTransaction>)> {
        self.consensus_output_cache
            .deferred_transactions
            .lock()
            .iter()
            .map(|(key, txs)| (*key, txs.clone()))
            .collect()
    }

    fn should_defer(
        &self,
        tx_cost: Option<u64>,
        cert: &VerifiedExecutableTransaction,
        commit_info: &ConsensusCommitInfo,
        dkg_failed: bool,
        generating_randomness: bool,
        previously_deferred_tx_digests: &HashMap<TransactionDigest, DeferralKey>,
        shared_object_congestion_tracker: &SharedObjectCongestionTracker,
    ) -> Option<(DeferralKey, DeferralReason)> {
        // Defer transaction if it uses randomness but we aren't generating any this round.
        // Don't defer if DKG has permanently failed; in that case we need to ignore.
        if !dkg_failed
            && !generating_randomness
            && self.randomness_state_enabled()
            && cert.transaction_data().uses_randomness()
        {
            let deferred_from_round = previously_deferred_tx_digests
                .get(cert.digest())
                .map(|previous_key| previous_key.deferred_from_round())
                .unwrap_or(commit_info.round);
            return Some((
                DeferralKey::new_for_randomness(deferred_from_round),
                DeferralReason::RandomnessNotReady,
            ));
        }

        // Defer transaction if it uses shared objects that are congested.
        if let Some((deferral_key, congested_objects)) = shared_object_congestion_tracker
            .should_defer_due_to_object_congestion(
                tx_cost,
                cert,
                previously_deferred_tx_digests,
                commit_info,
            )
        {
            Some((
                deferral_key,
                DeferralReason::SharedObjectCongestion(congested_objects),
            ))
        } else {
            None
        }
    }

    /// Assign a sequence number for the shared objects of the input transaction based on the
    /// effects of that transaction.
    /// Used by full nodes who don't listen to consensus, and validators who catch up by state sync.
    // TODO: We should be able to pass in a vector of certs/effects and acquire them all at once.
    #[instrument(level = "trace", skip_all)]
    pub fn acquire_shared_version_assignments_from_effects(
        &self,
        certificate: &VerifiedExecutableTransaction,
        effects: &TransactionEffects,
        cache_reader: &dyn ObjectCacheRead,
    ) -> SuiResult<AssignedVersions> {
        let assigned_versions = SharedObjVerManager::assign_versions_from_effects(
            &[(certificate, effects)],
            self,
            cache_reader,
        );
        let (_, assigned_versions) = assigned_versions.0.into_iter().next().unwrap();
        Ok(assigned_versions)
    }

    /// When submitting a certificate caller **must** provide a ReconfigState lock guard
    /// and verify that it allows new user certificates
    pub fn insert_pending_consensus_transactions(
        &self,
        transactions: &[ConsensusTransaction],
        lock: Option<&RwLockReadGuard<ReconfigState>>,
    ) -> SuiResult {
        let key_value_pairs = transactions.iter().map(|tx| (tx.key(), tx));
        self.tables()?
            .pending_consensus_transactions
            .multi_insert(key_value_pairs)?;

        // UserTransaction exists only when mysticeti_fastpath is enabled in protocol config.
        let digests: Vec<_> = transactions
            .iter()
            .filter_map(|tx| match &tx.kind {
                ConsensusTransactionKind::CertifiedTransaction(cert) => Some(cert.digest()),
                ConsensusTransactionKind::UserTransaction(txn) => Some(txn.digest()),
                _ => None,
            })
            .collect();
        if !digests.is_empty() {
            let state = lock.expect("Must pass reconfiguration lock when storing certificate");
            // Caller is responsible for performing graceful check
            assert!(
                state.should_accept_user_certs(),
                "Reconfiguration state should allow accepting user transactions"
            );
            let mut pending_consensus_certificates = self.pending_consensus_certificates.write();
            pending_consensus_certificates.extend(digests);
        }

        Ok(())
    }

    pub fn remove_pending_consensus_transactions(
        &self,
        keys: &[ConsensusTransactionKey],
    ) -> SuiResult {
        self.tables()?
            .pending_consensus_transactions
            .multi_remove(keys)?;
        let mut pending_consensus_certificates = self.pending_consensus_certificates.write();
        for key in keys {
            if let ConsensusTransactionKey::Certificate(digest) = key {
                pending_consensus_certificates.remove(digest);
            }
        }
        Ok(())
    }

    pub fn pending_consensus_certificates_count(&self) -> usize {
        self.pending_consensus_certificates.read().len()
    }

    pub fn pending_consensus_certificates_empty(&self) -> bool {
        self.pending_consensus_certificates.read().is_empty()
    }

    pub fn pending_consensus_certificates(&self) -> HashSet<TransactionDigest> {
        self.pending_consensus_certificates.read().clone()
    }

    pub fn is_pending_consensus_certificate(&self, tx_digest: &TransactionDigest) -> bool {
        self.pending_consensus_certificates
            .read()
            .contains(tx_digest)
    }

    pub fn deferred_transactions_empty(&self) -> bool {
        self.consensus_output_cache
            .deferred_transactions
            .lock()
            .is_empty()
    }

    /// Check whether any certificates were processed by consensus.
    /// This handles multiple certificates at once.
    pub fn is_any_tx_certs_consensus_message_processed<'a>(
        &self,
        certificates: impl Iterator<Item = &'a CertifiedTransaction>,
    ) -> SuiResult<bool> {
        let keys = certificates.map(|cert| {
            SequencedConsensusTransactionKey::External(ConsensusTransactionKey::Certificate(
                *cert.digest(),
            ))
        });
        Ok(self
            .check_consensus_messages_processed(keys)?
            .into_iter()
            .any(|processed| processed))
    }

    /// Returns true if all messages with the given keys were processed by consensus.
    pub fn all_external_consensus_messages_processed(
        &self,
        keys: impl Iterator<Item = ConsensusTransactionKey>,
    ) -> SuiResult<bool> {
        let keys = keys.map(SequencedConsensusTransactionKey::External);
        Ok(self
            .check_consensus_messages_processed(keys)?
            .into_iter()
            .all(|processed| processed))
    }

    pub fn is_consensus_message_processed(
        &self,
        key: &SequencedConsensusTransactionKey,
    ) -> SuiResult<bool> {
        Ok(self
            .consensus_quarantine
            .read()
            .is_consensus_message_processed(key)
            || self
                .tables()?
                .consensus_message_processed
                .contains_key(key)?)
    }

    pub fn check_consensus_messages_processed(
        &self,
        keys: impl Iterator<Item = SequencedConsensusTransactionKey>,
    ) -> SuiResult<Vec<bool>> {
        let keys = keys.collect::<Vec<_>>();

        let consensus_quarantine = self.consensus_quarantine.read();
        let tables = self.tables()?;

        Ok(do_fallback_lookup(
            &keys,
            |key| {
                if consensus_quarantine.is_consensus_message_processed(key) {
                    CacheResult::Hit(true)
                } else {
                    CacheResult::Miss
                }
            },
            |keys| {
                tables
                    .consensus_message_processed
                    .multi_contains_keys(keys)
                    .expect("db error")
            },
        ))
    }

    pub async fn consensus_messages_processed_notify(
        &self,
        keys: Vec<SequencedConsensusTransactionKey>,
    ) -> Result<(), SuiError> {
        let registrations = self.consensus_notify_read.register_all(&keys);

        let unprocessed_keys_registrations = registrations
            .into_iter()
            .zip(self.check_consensus_messages_processed(keys.into_iter())?)
            .filter(|(_, processed)| !processed)
            .map(|(registration, _)| registration);

        join_all(unprocessed_keys_registrations).await;
        Ok(())
    }

    /// Get notified when transactions get executed as part of a checkpoint execution.
    pub async fn transactions_executed_in_checkpoint_notify(
        &self,
        digests: Vec<TransactionDigest>,
    ) -> Result<(), SuiError> {
        let registrations = self
            .executed_transactions_to_checkpoint_notify_read
            .register_all(&digests);

        let unprocessed_keys_registrations = registrations
            .into_iter()
            .zip(self.transactions_executed_in_checkpoint(digests.into_iter())?)
            .filter(|(_, processed)| !*processed)
            .map(|(registration, _)| registration);

        join_all(unprocessed_keys_registrations).await;
        Ok(())
    }

    pub fn has_sent_end_of_publish(&self, authority: &AuthorityName) -> SuiResult<bool> {
        Ok(self
            .end_of_publish
            .try_lock()
            .expect("No contention on end_of_publish lock")
            .contains_key(authority))
    }

    // Converts transaction keys to digests, waiting for digests to become available for any
    // non-digest keys.
    pub async fn notify_read_tx_key_to_digest(
        &self,
        keys: &[TransactionKey],
    ) -> SuiResult<Vec<TransactionDigest>> {
        let non_digest_keys: Vec<_> = keys
            .iter()
            .filter_map(|key| {
                if matches!(key, TransactionKey::Digest(_)) {
                    None
                } else {
                    Some(*key)
                }
            })
            .collect();

        let registrations = self
            .executed_digests_notify_read
            .register_all(&non_digest_keys);
        let executed_digests = self
            .tables()?
            .transaction_key_to_digest
            .multi_get(&non_digest_keys)?;
        let futures = executed_digests
            .into_iter()
            .zip(registrations)
            .map(|(d, r)| match d {
                // Note that Some() clause also drops registration that is already fulfilled
                Some(ready) => Either::Left(futures::future::ready(ready)),
                None => Either::Right(r),
            });
        let mut results = VecDeque::from(join_all(futures).await);

        Ok(keys
            .iter()
            .map(|key| {
                if let TransactionKey::Digest(digest) = key {
                    *digest
                } else {
                    results
                        .pop_front()
                        .expect("number of returned results should match number of non-digest keys")
                }
            })
            .collect())
    }

    /// Caller must call consensus_message_processed_notify before calling this to ensure that all
    /// user signatures are available.
    pub fn user_signatures_for_checkpoint(
        &self,
        transactions: &[VerifiedTransaction],
        digests: &[TransactionDigest],
    ) -> Vec<Vec<GenericSignature>> {
        assert_eq!(transactions.len(), digests.len());

        fn is_signature_expected(transaction: &VerifiedTransaction) -> bool {
            !matches!(
                transaction.inner().transaction_data().kind(),
                TransactionKind::RandomnessStateUpdate(_)
                    | TransactionKind::ProgrammableSystemTransaction(_)
            )
        }

        let result: Vec<_> = {
            let mut user_sigs = self
                .consensus_output_cache
                .user_signatures_for_checkpoints
                .lock();
            digests
                .iter()
                .zip(transactions.iter())
                .map(|(d, t)| {
                    // Some transactions (RandomnessStateUpdate and settlement transactions) don't go through
                    // consensus, but have system-generated signatures that are guaranteed to be the same,
                    // so we can just pull them from the transaction.
                    if is_signature_expected(t) {
                        // Expect is safe as long as consensus_message_processed_notify is called
                        // before this call, to ensure that all canonical user signatures are
                        // available.
                        user_sigs.remove(d).expect("signature should be available")
                    } else {
                        t.tx_signatures().to_vec()
                    }
                })
                .collect()
        };

        result
    }

    pub fn clear_override_protocol_upgrade_buffer_stake(&self) -> SuiResult {
        warn!(
            epoch = ?self.epoch(),
            "clearing buffer_stake_for_protocol_upgrade_bps override"
        );
        self.tables()?
            .override_protocol_upgrade_buffer_stake
            .remove(&OVERRIDE_PROTOCOL_UPGRADE_BUFFER_STAKE_INDEX)?;
        self.update_buffer_stake_metric();
        Ok(())
    }

    pub fn set_override_protocol_upgrade_buffer_stake(&self, new_stake_bps: u64) -> SuiResult {
        warn!(
            ?new_stake_bps,
            epoch = ?self.epoch(),
            "storing buffer_stake_for_protocol_upgrade_bps override"
        );
        self.tables()?
            .override_protocol_upgrade_buffer_stake
            .insert(
                &OVERRIDE_PROTOCOL_UPGRADE_BUFFER_STAKE_INDEX,
                &new_stake_bps,
            )?;
        self.update_buffer_stake_metric();
        Ok(())
    }

    fn update_buffer_stake_metric(&self) {
        self.metrics
            .effective_buffer_stake
            .set(self.get_effective_buffer_stake_bps() as i64);
    }

    pub fn get_effective_buffer_stake_bps(&self) -> u64 {
        self.tables()
            .expect("epoch initialization should have finished")
            .override_protocol_upgrade_buffer_stake
            .get(&OVERRIDE_PROTOCOL_UPGRADE_BUFFER_STAKE_INDEX)
            .expect("force_protocol_upgrade read cannot fail")
            .tap_some(|b| warn!("using overridden buffer stake value of {}", b))
            .unwrap_or_else(|| {
                self.protocol_config()
                    .buffer_stake_for_protocol_upgrade_bps()
            })
    }

    /// Record most recently advertised capabilities of all authorities
    pub fn record_capabilities(&self, capabilities: &AuthorityCapabilitiesV1) -> SuiResult {
        info!("received capabilities {:?}", capabilities);
        let authority = &capabilities.authority;
        let tables = self.tables()?;

        // Read-compare-write pattern assumes we are only called from the consensus handler task.
        if let Some(cap) = tables.authority_capabilities.get(authority)? {
            if cap.generation >= capabilities.generation {
                debug!(
                    "ignoring new capabilities {:?} in favor of previous capabilities {:?}",
                    capabilities, cap
                );
                return Ok(());
            }
        }
        tables
            .authority_capabilities
            .insert(authority, capabilities)?;
        Ok(())
    }

    /// Record most recently advertised capabilities of all authorities
    pub fn record_capabilities_v2(&self, capabilities: &AuthorityCapabilitiesV2) -> SuiResult {
        info!("received capabilities v2 {:?}", capabilities);
        let authority = &capabilities.authority;
        let tables = self.tables()?;

        // Read-compare-write pattern assumes we are only called from the consensus handler task.
        if let Some(cap) = tables.authority_capabilities_v2.get(authority)? {
            if cap.generation >= capabilities.generation {
                debug!(
                    "ignoring new capabilities {:?} in favor of previous capabilities {:?}",
                    capabilities, cap
                );
                return Ok(());
            }
        }
        tables
            .authority_capabilities_v2
            .insert(authority, capabilities)?;
        Ok(())
    }

    pub fn get_capabilities_v1(&self) -> SuiResult<Vec<AuthorityCapabilitiesV1>> {
        assert!(!self.protocol_config.authority_capabilities_v2());
        Ok(self
            .tables()?
            .authority_capabilities
            .safe_iter()
            .map(|item| item.map(|(_, v)| v))
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn get_capabilities_v2(&self) -> SuiResult<Vec<AuthorityCapabilitiesV2>> {
        assert!(self.protocol_config.authority_capabilities_v2());
        Ok(self
            .tables()?
            .authority_capabilities_v2
            .safe_iter()
            .map(|item| item.map(|(_, v)| v))
            .collect::<Result<Vec<_>, _>>()?)
    }

    fn record_jwk_vote(
        &self,
        output: &mut ConsensusCommitOutput,
        round: u64,
        authority: AuthorityName,
        id: &JwkId,
        jwk: &JWK,
    ) -> SuiResult {
        info!(
            "received jwk vote from {:?} for jwk ({:?}, {:?})",
            authority.concise(),
            id,
            jwk
        );

        if !self.authenticator_state_enabled() {
            info!(
                "ignoring vote because authenticator state object does exist yet
                (it will be created at the end of this epoch)"
            );
            return Ok(());
        }

        let mut jwk_aggregator = self.jwk_aggregator.lock();

        let votes = jwk_aggregator.votes_for_authority(authority);
        if votes
            >= self
                .protocol_config()
                .max_jwk_votes_per_validator_per_epoch()
        {
            warn!(
                "validator {:?} has already voted {} times this epoch, ignoring vote",
                authority, votes,
            );
            return Ok(());
        }

        output.insert_pending_jwk(authority, id.clone(), jwk.clone());

        let key = (id.clone(), jwk.clone());
        let previously_active = jwk_aggregator.has_quorum_for_key(&key);
        let insert_result = jwk_aggregator.insert(authority, key.clone());

        if !previously_active && insert_result.is_quorum_reached() {
            info!(epoch = ?self.epoch(), ?round, jwk = ?key, "jwk became active");
            output.insert_active_jwk(round, key);
        }

        Ok(())
    }

    pub(crate) fn get_new_jwks(&self, round: u64) -> SuiResult<Vec<ActiveJwk>> {
        self.consensus_quarantine.read().get_new_jwks(self, round)
    }

    pub fn jwk_active_in_current_epoch(&self, jwk_id: &JwkId, jwk: &JWK) -> bool {
        let jwk_aggregator = self.jwk_aggregator.lock();
        jwk_aggregator.has_quorum_for_key(&(jwk_id.clone(), jwk.clone()))
    }

    pub(crate) fn get_randomness_last_round_timestamp(&self) -> SuiResult<Option<TimestampMs>> {
        if let Some(ts) = self
            .consensus_quarantine
            .read()
            .get_randomness_last_round_timestamp()
        {
            Ok(Some(ts))
        } else {
            Ok(self
                .tables()?
                .randomness_last_round_timestamp
                .get(&SINGLETON_KEY)?)
        }
    }

    #[cfg(test)]
    pub fn test_insert_user_signature(
        &self,
        digest: TransactionDigest,
        signatures: Vec<GenericSignature>,
    ) {
        self.consensus_output_cache
            .user_signatures_for_checkpoints
            .lock()
            .insert(digest, signatures);
        let key = ConsensusTransactionKey::Certificate(digest);
        let key = SequencedConsensusTransactionKey::External(key);

        let mut output = ConsensusCommitOutput::default();
        output.record_consensus_message_processed(key.clone());
        output.set_default_commit_stats_for_testing();
        self.consensus_quarantine
            .write()
            .push_consensus_output(output, self)
            .expect("push_consensus_output should not fail");
        self.consensus_notify_read.notify(&key, &());
    }

    #[cfg(test)]
    pub(crate) fn push_consensus_output_for_tests(&self, output: ConsensusCommitOutput) {
        self.consensus_quarantine
            .write()
            .push_consensus_output(output, self)
            .expect("push_consensus_output should not fail");
    }

    fn process_user_signatures<'a>(&self, certificates: impl Iterator<Item = &'a Schedulable>) {
        let sigs: Vec<_> = certificates
            .filter_map(|s| match s {
                Schedulable::Transaction(certificate) | Schedulable::Withdraw(certificate, _) => {
                    Some((*certificate.digest(), certificate.tx_signatures().to_vec()))
                }
                Schedulable::RandomnessStateUpdate(_, _) => None,
                Schedulable::AccumulatorSettlement(_, _) => None,
            })
            .collect();

        let mut user_sigs = self
            .consensus_output_cache
            .user_signatures_for_checkpoints
            .lock();

        user_sigs.reserve(sigs.len());
        for (digest, sigs) in sigs {
            // User signatures are written in the same batch as consensus certificate processed flag,
            // which means we won't attempt to insert this twice for the same tx digest
            assert!(
                user_sigs.insert(digest, sigs).is_none(),
                "duplicate user signatures for transaction digest: {:?}",
                digest
            );
        }
    }

    pub fn get_reconfig_state_read_lock_guard(&self) -> RwLockReadGuard<ReconfigState> {
        self.reconfig_state_mem.read()
    }

    pub fn get_reconfig_state_write_lock_guard(&self) -> RwLockWriteGuard<ReconfigState> {
        self.reconfig_state_mem.write()
    }

    pub fn close_user_certs(&self, mut lock_guard: RwLockWriteGuard<'_, ReconfigState>) {
        lock_guard.close_user_certs();
        self.store_reconfig_state(&lock_guard)
            .expect("Updating reconfig state cannot fail");

        // Set epoch_close_time for metric purpose.
        let mut epoch_close_time = self.epoch_close_time.write();
        if epoch_close_time.is_none() {
            // Only update it the first time epoch is closed.
            *epoch_close_time = Some(Instant::now());

            self.user_certs_closed_notify
                .notify()
                .expect("user_certs_closed_notify called twice on same epoch store");
        }
    }

    pub async fn user_certs_closed_notify(&self) {
        self.user_certs_closed_notify.wait().await
    }

    /// Notify epoch is terminated, can only be called once on epoch store
    pub async fn epoch_terminated(&self) {
        // Notify interested tasks that epoch has ended
        self.epoch_alive_notify
            .notify()
            .expect("epoch_terminated called twice on same epoch store");
        // This `write` acts as a barrier - it waits for futures executing in
        // `within_alive_epoch` to terminate before we can continue here
        debug!("Epoch terminated - waiting for pending tasks to complete");
        *self.epoch_alive.write().await = false;
        debug!("All pending epoch tasks completed");
    }

    /// Waits for the notification about epoch termination
    pub async fn wait_epoch_terminated(&self) {
        self.epoch_alive_notify.wait().await
    }

    /// This function executes given future until epoch_terminated is called
    /// If future finishes before epoch_terminated is called, future result is returned
    /// If epoch_terminated is called before future is resolved, error is returned
    ///
    /// In addition to the early termination guarantee, this function also prevents epoch_terminated()
    /// if future is being executed.
    #[allow(clippy::result_unit_err)]
    pub async fn within_alive_epoch<F: Future + Send>(&self, f: F) -> Result<F::Output, ()> {
        // This guard is kept in the future until it resolves, preventing `epoch_terminated` to
        // acquire a write lock
        let guard = self.epoch_alive.read().await;
        if !*guard {
            return Err(());
        }
        let terminated = self.wait_epoch_terminated().boxed();
        let f = f.boxed();
        match select(terminated, f).await {
            Either::Left((_, _f)) => Err(()),
            Either::Right((result, _)) => Ok(result),
        }
    }

    #[instrument(level = "trace", skip_all)]
    pub fn verify_transaction(&self, tx: Transaction) -> SuiResult<VerifiedTransaction> {
        self.signature_verifier
            .verify_tx(tx.data())
            .map(|_| VerifiedTransaction::new_from_verified(tx))
    }

    /// Verifies transaction signatures and other data
    /// Important: This function can potentially be called in parallel and you can not rely on order of transactions to perform verification
    /// If this function return an error, transaction is skipped and is not passed to handle_consensus_transaction
    /// This function returns unit error and is responsible for emitting log messages for internal errors
    fn verify_consensus_transaction(
        &self,
        transaction: SequencedConsensusTransaction,
        skipped_consensus_txns: &IntCounter,
    ) -> Option<VerifiedSequencedConsensusTransaction> {
        let _scope = monitored_scope("VerifyConsensusTransaction");
        if self
            .is_consensus_message_processed(&transaction.transaction.key())
            .expect("Storage error")
        {
            trace!(
                consensus_index=?transaction.consensus_index.transaction_index,
                tracking_id=?transaction.transaction.get_tracking_id(),
                "handle_consensus_transaction UserTransaction [skip]",
            );
            skipped_consensus_txns.inc();
            return None;
        }
        // Signatures are verified as part of the consensus payload verification in SuiTxValidator
        match &transaction.transaction {
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::CertifiedTransaction(_certificate),
                ..
            }) => {}
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::UserTransaction(_tx),
                ..
            }) => {}
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::CheckpointSignature(data),
                ..
            }) => {
                if transaction.sender_authority() != data.summary.auth_sig().authority {
                    warn!(
                        "CheckpointSignature authority {} does not match its author from consensus {}",
                        data.summary.auth_sig().authority,
                        transaction.certificate_author_index
                    );
                    return None;
                }
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::EndOfPublish(authority),
                ..
            }) => {
                if &transaction.sender_authority() != authority {
                    warn!(
                        "EndOfPublish authority {} does not match its author from consensus {}",
                        authority, transaction.certificate_author_index
                    );
                    return None;
                }
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind:
                    ConsensusTransactionKind::CapabilityNotification(AuthorityCapabilitiesV1 {
                        authority,
                        ..
                    }),
                ..
            })
            | SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind:
                    ConsensusTransactionKind::CapabilityNotificationV2(AuthorityCapabilitiesV2 {
                        authority,
                        ..
                    }),
                ..
            }) => {
                if transaction.sender_authority() != *authority {
                    warn!(
                        "CapabilityNotification authority {} does not match its author from consensus {}",
                        authority, transaction.certificate_author_index
                    );
                    return None;
                }
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::NewJWKFetched(authority, id, jwk),
                ..
            }) => {
                if transaction.sender_authority() != *authority {
                    warn!(
                        "NewJWKFetched authority {} does not match its author from consensus {}",
                        authority, transaction.certificate_author_index,
                    );
                    return None;
                }
                if !check_total_jwk_size(id, jwk) {
                    warn!(
                        "{:?} sent jwk that exceeded max size",
                        transaction.sender_authority().concise()
                    );
                    return None;
                }
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::RandomnessStateUpdate(_round, _bytes),
                ..
            }) => {}
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::RandomnessDkgMessage(authority, _bytes),
                ..
            }) => {
                if transaction.sender_authority() != *authority {
                    warn!(
                        "RandomnessDkgMessage authority {} does not match its author from consensus {}",
                        authority, transaction.certificate_author_index
                    );
                    return None;
                }
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::RandomnessDkgConfirmation(authority, _bytes),
                ..
            }) => {
                if transaction.sender_authority() != *authority {
                    warn!(
                        "RandomnessDkgConfirmation authority {} does not match its author from consensus {}",
                        authority, transaction.certificate_author_index
                    );
                    return None;
                }
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::ExecutionTimeObservation(msg),
                ..
            }) => {
                if transaction.sender_authority() != msg.authority {
                    warn!(
                        "ExecutionTimeObservation authority {} does not match its author from consensus {}",
                        msg.authority, transaction.certificate_author_index
                    );
                    return None;
                }
            }
            SequencedConsensusTransactionKind::System(_) => {}
        }
        Some(VerifiedSequencedConsensusTransaction(transaction))
    }

    fn db_batch(&self) -> SuiResult<DBBatch> {
        Ok(self.tables()?.last_consensus_stats.batch())
    }

    #[cfg(test)]
    pub fn db_batch_for_test(&self) -> DBBatch {
        self.db_batch()
            .expect("test should not be write past end of epoch")
    }

    #[instrument(level = "debug", skip_all)]
    pub(crate) async fn process_consensus_transactions_and_commit_boundary<
        C: CheckpointServiceNotify,
    >(
        &self,
        transactions: Vec<SequencedConsensusTransaction>,
        consensus_stats: &ExecutionIndicesWithStats,
        checkpoint_service: &Arc<C>,
        cache_reader: &dyn ObjectCacheRead,
        consensus_commit_info: &ConsensusCommitInfo,
        indirect_state_observer: IndirectStateObserver,
        authority_metrics: &Arc<AuthorityMetrics>,
    ) -> SuiResult<(Vec<Schedulable>, AssignedTxAndVersions)> {
        // Split transactions into different types for processing.
        let verified_transactions: Vec<_> = transactions
            .into_iter()
            .filter_map(|transaction| {
                self.verify_consensus_transaction(
                    transaction,
                    &authority_metrics.skipped_consensus_txns,
                )
            })
            .collect();
        let mut system_transactions = Vec::with_capacity(verified_transactions.len());
        let mut current_commit_sequenced_consensus_transactions =
            Vec::with_capacity(verified_transactions.len());
        let mut current_commit_sequenced_randomness_transactions =
            Vec::with_capacity(verified_transactions.len());
        let mut end_of_publish_transactions = Vec::with_capacity(verified_transactions.len());
        let mut execution_time_observations = Vec::with_capacity(verified_transactions.len());
        for mut tx in verified_transactions {
            let key = tx.0.key();
            if tx.0.is_end_of_publish() {
                end_of_publish_transactions.push(tx);
            } else if let Some(observation) = tx.0.try_take_execution_time_observation() {
                execution_time_observations.push((key, observation));
            } else if tx.0.is_system() {
                system_transactions.push(tx);
            } else if tx
                .0
                .is_user_tx_with_randomness(self.randomness_state_enabled())
            {
                current_commit_sequenced_randomness_transactions.push(tx);
            } else {
                current_commit_sequenced_consensus_transactions.push(tx);
            }
        }

        let mut output = ConsensusCommitOutput::new(consensus_commit_info.round);

        // Load transactions deferred from previous commits.
        let deferred_txs: Vec<(DeferralKey, Vec<VerifiedSequencedConsensusTransaction>)> = self
            .load_deferred_transactions_for_up_to_consensus_round(
                &mut output,
                consensus_commit_info.round,
            )?
            .into_iter()
            .collect();
        let mut previously_deferred_tx_digests: HashMap<TransactionDigest, DeferralKey> =
            deferred_txs
                .iter()
                .flat_map(|(deferral_key, txs)| {
                    txs.iter().map(|tx| match tx.0.transaction.key() {
                        SequencedConsensusTransactionKey::External(
                            ConsensusTransactionKey::Certificate(digest),
                        ) => (digest, *deferral_key),
                        _ => panic!("deferred transaction was not a user certificate: {tx:?}"),
                    })
                })
                .collect();

        // Sequenced_transactions and sequenced_randomness_transactions store all transactions that will be sent to
        // process_consensus_transactions. We put deferred transactions at the beginning of the list before
        // PostConsensusTxReorder::reorder, so that for transactions with the same gas price, deferred transactions
        // will be placed earlier in the execution queue.
        let mut sequenced_transactions: Vec<VerifiedSequencedConsensusTransaction> =
            Vec::with_capacity(
                current_commit_sequenced_consensus_transactions.len()
                    + previously_deferred_tx_digests.len(),
            );
        let mut sequenced_randomness_transactions: Vec<VerifiedSequencedConsensusTransaction> =
            Vec::with_capacity(
                current_commit_sequenced_randomness_transactions.len()
                    + previously_deferred_tx_digests.len(),
            );

        let mut randomness_manager = self.randomness_manager.get().map(|rm| {
            rm.try_lock()
                .expect("should only ever be called from the commit handler thread")
        });
        let mut dkg_failed = false;
        let randomness_round = if self.randomness_state_enabled() {
            let randomness_manager = randomness_manager
                .as_mut()
                .expect("randomness manager should exist if randomness is enabled");
            match randomness_manager.dkg_status() {
                DkgStatus::Pending => None,
                DkgStatus::Failed => {
                    dkg_failed = true;
                    None
                }
                DkgStatus::Successful => {
                    // Generate randomness for this commit if DKG is successful and we are still
                    // accepting certs.
                    if self
                        // It is ok to just release lock here as functions called by this one are the
                        // only place that transition into RejectAllCerts state, and this function
                        // itself is always executed from consensus task.
                        .get_reconfig_state_read_lock_guard()
                        .should_accept_tx()
                    {
                        randomness_manager
                            .reserve_next_randomness(consensus_commit_info.timestamp, &mut output)?
                    } else {
                        None
                    }
                }
            }
        } else {
            None
        };

        // We should load any previously-deferred randomness-using tx:
        // - if DKG is failed, so we can ignore them
        // - if randomness is being generated, so we can process them
        if dkg_failed || randomness_round.is_some() {
            self.load_and_process_deferred_transactions_for_randomness(
                &mut output,
                &mut previously_deferred_tx_digests,
                &mut sequenced_randomness_transactions,
            )?;
        }

        // Add ConsensusRound deferred tx back into the sequence.
        for tx in deferred_txs
            .into_iter()
            .flat_map(|(_, txs)| txs.into_iter())
        {
            if tx
                .0
                .is_user_tx_with_randomness(self.randomness_state_enabled())
            {
                sequenced_randomness_transactions.push(tx);
            } else {
                sequenced_transactions.push(tx);
            }
        }
        sequenced_transactions.extend(current_commit_sequenced_consensus_transactions);
        sequenced_randomness_transactions.extend(current_commit_sequenced_randomness_transactions);

        // Save roots for checkpoint generation. One set for most tx, one for randomness tx.
        let mut roots: BTreeSet<_> = system_transactions
            .iter()
            .chain(sequenced_transactions.iter())
            // no need to include end_of_publish_transactions here because they would be
            // filtered out below by `executable_transaction_digest` anyway
            .filter_map(|transaction| {
                transaction
                    .0
                    .transaction
                    .executable_transaction_digest()
                    .map(TransactionKey::Digest)
            })
            .collect();
        let mut randomness_roots: BTreeSet<_> = sequenced_randomness_transactions
            .iter()
            .filter_map(|transaction| {
                transaction
                    .0
                    .transaction
                    .executable_transaction_digest()
                    .map(TransactionKey::Digest)
            })
            .collect();

        PostConsensusTxReorder::reorder(
            &mut sequenced_transactions,
            self.protocol_config.consensus_transaction_ordering(),
        );
        PostConsensusTxReorder::reorder(
            &mut sequenced_randomness_transactions,
            self.protocol_config.consensus_transaction_ordering(),
        );

        // Process new execution time observations for use by congestion control.
        let mut execution_time_estimator = self
            .execution_time_estimator
            .try_lock()
            .expect("should only ever be called from the commit handler thread");
        // It is ok to just release lock here as functions called by this one are the
        // only place that transition reconfig state, and this function itself is always
        // executed from consensus task.
        if !self
            .protocol_config()
            .ignore_execution_time_observations_after_certs_closed()
            || self
                .get_reconfig_state_read_lock_guard()
                .should_accept_consensus_certs()
        {
            for (
                key,
                ExecutionTimeObservation {
                    authority,
                    generation,
                    estimates,
                },
            ) in execution_time_observations
            {
                let Some(estimator) = execution_time_estimator.as_mut() else {
                    error!("dropping ExecutionTimeObservation from possibly-Byzantine authority {authority:?} sent when ExecutionTimeEstimate mode is not enabled");
                    continue;
                };
                let authority_index = self.committee.authority_index(&authority).unwrap();
                estimator.process_observations_from_consensus(
                    authority_index,
                    Some(generation),
                    &estimates,
                );
                output.insert_execution_time_observation(authority_index, generation, estimates);
                if self.protocol_config().record_time_estimate_processed() {
                    output.record_consensus_message_processed(key);
                }
            }
        }

        // We track transaction execution cost separately for regular transactions and transactions using randomness, since
        // they will be in different PendingCheckpoints.
        let shared_object_congestion_tracker = SharedObjectCongestionTracker::from_protocol_config(
            self.consensus_quarantine.read().load_initial_object_debts(
                self,
                consensus_commit_info.round,
                false,
                &sequenced_transactions,
            )?,
            self.protocol_config(),
            false,
        )?;
        let shared_object_using_randomness_congestion_tracker =
            SharedObjectCongestionTracker::from_protocol_config(
                self.consensus_quarantine.read().load_initial_object_debts(
                    self,
                    consensus_commit_info.round,
                    true,
                    &sequenced_randomness_transactions,
                )?,
                self.protocol_config(),
                true,
            )?;

        let sequenced_non_randomness_transactions: Vec<_> = system_transactions
            .into_iter()
            .chain(sequenced_transactions)
            .collect();

        let (
            verified_non_randomness_transactions,
            verified_randomness_transactions,
            notifications,
            lock,
            final_round,
            consensus_commit_prologue_root,
            assigned_versions,
        ) = self
            .process_consensus_transactions(
                &mut output,
                &sequenced_non_randomness_transactions,
                &sequenced_randomness_transactions,
                &end_of_publish_transactions,
                checkpoint_service,
                cache_reader,
                consensus_commit_info,
                indirect_state_observer,
                &mut roots,
                &mut randomness_roots,
                shared_object_congestion_tracker,
                shared_object_using_randomness_congestion_tracker,
                previously_deferred_tx_digests,
                randomness_manager.as_deref_mut(),
                dkg_failed,
                randomness_round,
                execution_time_estimator.as_ref(),
                authority_metrics,
            )
            .await?;
        self.process_user_signatures(
            verified_non_randomness_transactions
                .iter()
                .chain(verified_randomness_transactions.iter()),
        );
        output.record_consensus_commit_stats(consensus_stats.clone());

        // If this is the final round, record execution time observations for storage in the
        // end-of-epoch tx.
        if final_round {
            if let Some(estimator) = execution_time_estimator.as_mut() {
                self.end_of_epoch_execution_time_observations
                .set(estimator.take_observations())
                .expect(
                    "`stored_execution_time_observations` should only be set once at end of epoch",
                );
            }
            drop(execution_time_estimator); // make sure this is not used after `take_observations`
        }

        // Create pending checkpoints if we are still accepting tx.
        let should_accept_tx = if let Some(lock) = &lock {
            lock.should_accept_tx()
        } else {
            // It is ok to just release lock here as functions called by this one are the
            // only place that transition reconfig state, and this function itself is always
            // executed from consensus task. At this point if the lock was not already provided
            // above, we know we won't be transitioning state for this commit.
            self.get_reconfig_state_read_lock_guard().should_accept_tx()
        };
        let make_checkpoint = should_accept_tx || final_round;
        if make_checkpoint {
            let checkpoint_height =
                self.calculate_pending_checkpoint_height(consensus_commit_info.round);

            let mut non_randomness_roots: Vec<TransactionKey> = Vec::with_capacity(roots.len() + 1);

            if let Some(consensus_commit_prologue_root) = consensus_commit_prologue_root {
                if self
                    .protocol_config()
                    .prepend_prologue_tx_in_consensus_commit_in_checkpoints()
                {
                    // Put consensus commit prologue root at the beginning of the checkpoint roots.
                    non_randomness_roots.push(consensus_commit_prologue_root);
                } else {
                    roots.insert(consensus_commit_prologue_root);
                }
            }
            non_randomness_roots.extend(roots.into_iter());

            if let Some(randomness_round) = randomness_round {
                randomness_roots.insert(TransactionKey::RandomnessRound(
                    self.epoch(),
                    randomness_round,
                ));
            }

            // Determine whether to write pending checkpoint for user tx with randomness.
            // - If randomness is not generated for this commit, we will skip the
            //   checkpoint with the associated height. Therefore checkpoint heights may
            //   not be contiguous.
            // - Exception: if DKG fails, we always need to write out a PendingCheckpoint
            //   for randomness tx that are canceled.
            let should_write_random_checkpoint =
                randomness_round.is_some() || (dkg_failed && !randomness_roots.is_empty());

            let pending_checkpoint = PendingCheckpointV2::V2(PendingCheckpointV2Contents {
                roots: non_randomness_roots,
                details: PendingCheckpointInfo {
                    timestamp_ms: consensus_commit_info.timestamp,
                    last_of_epoch: final_round && !should_write_random_checkpoint,
                    checkpoint_height,
                },
            });
            self.write_pending_checkpoint(&mut output, &pending_checkpoint)?;

            if should_write_random_checkpoint {
                let pending_checkpoint = PendingCheckpointV2::V2(PendingCheckpointV2Contents {
                    roots: randomness_roots.into_iter().collect(),
                    details: PendingCheckpointInfo {
                        timestamp_ms: consensus_commit_info.timestamp,
                        last_of_epoch: final_round,
                        checkpoint_height: checkpoint_height + 1,
                    },
                });
                self.write_pending_checkpoint(&mut output, &pending_checkpoint)?;
            }
        }

        {
            let mut deferred_transactions =
                self.consensus_output_cache.deferred_transactions.lock();
            for deleted_deferred_key in output.get_deleted_deferred_txn_keys() {
                deferred_transactions.remove(&deleted_deferred_key);
            }
        }

        self.consensus_quarantine
            .write()
            .push_consensus_output(output, self)?;

        // Only after batch is written, notify checkpoint service to start building any new
        // pending checkpoints.
        if make_checkpoint {
            debug!(
                ?consensus_commit_info.round,
                "Notifying checkpoint service about new pending checkpoint(s)",
            );
            checkpoint_service.notify_checkpoint()?;
        }

        // Once commit processing is recorded, kick off randomness generation.
        if let Some(randomness_round) = randomness_round {
            let epoch = self.epoch();
            randomness_manager
                .as_ref()
                .expect("randomness manager should exist if randomness round is provided")
                .generate_randomness(epoch, randomness_round);
        }

        self.process_notifications(&notifications, &end_of_publish_transactions);

        if final_round {
            info!(
                epoch=?self.epoch(),
                // Accessing lock on purpose so that the compiler ensures
                // the lock is not yet dropped.
                lock=?lock.as_ref(),
                final_round=?final_round,
                "Notified last checkpoint"
            );
            self.record_end_of_message_quorum_time_metric();
        }

        let all_txns = [
            verified_non_randomness_transactions,
            verified_randomness_transactions,
        ]
        .concat();

        Ok((all_txns, assigned_versions))
    }

    fn calculate_pending_checkpoint_height(&self, consensus_round: u64) -> u64 {
        if self.randomness_state_enabled() {
            consensus_round * 2
        } else {
            consensus_round
        }
    }

    // Adds the consensus commit prologue transaction to the beginning of input `transactions` to update
    // the system clock used in all transactions in the current consensus commit.
    // Returns the root of the consensus commit prologue transaction if it was added to the input.
    fn add_consensus_commit_prologue_transaction(
        &self,
        output: &mut ConsensusCommitOutput,
        transactions: &mut VecDeque<Schedulable<VerifiedExecutableTransaction>>,
        consensus_commit_info: &ConsensusCommitInfo,
        indirect_state_observer: IndirectStateObserver,
        cancelled_txns: &BTreeMap<TransactionDigest, CancelConsensusCertificateReason>,
    ) -> SuiResult<Option<TransactionKey>> {
        {
            if consensus_commit_info.skip_consensus_commit_prologue_in_test {
                return Ok(None);
            }
        }

        let mut version_assignment = Vec::new();
        let mut shared_input_next_version = HashMap::new();
        for txn in transactions.iter() {
            let key = txn.key();
            match key.as_digest().and_then(|d| cancelled_txns.get(d)) {
                Some(CancelConsensusCertificateReason::CongestionOnObjects(_))
                | Some(CancelConsensusCertificateReason::DkgFailed) => {
                    assert_reachable!("cancelled transactions");
                    let assigned_versions = SharedObjVerManager::assign_versions_for_certificate(
                        self,
                        txn,
                        &mut shared_input_next_version,
                        cancelled_txns,
                    );
                    version_assignment.push((*key.unwrap_digest(), assigned_versions));
                }
                None => {}
            }
        }

        fail_point_arg!(
            "additional_cancelled_txns_for_tests",
            |additional_cancelled_txns: Vec<(
                TransactionDigest,
                Vec<(ConsensusObjectSequenceKey, SequenceNumber)>
            )>| {
                version_assignment.extend(additional_cancelled_txns);
            }
        );

        let transaction = consensus_commit_info.create_consensus_commit_prologue_transaction(
            self.epoch(),
            self.protocol_config(),
            version_assignment,
            consensus_commit_info,
            indirect_state_observer,
        );
        let consensus_commit_prologue_root = match self.process_consensus_system_transaction(&transaction) {
            ConsensusCertificateResult::SuiTransaction(processed_tx) => {
                transactions.push_front(Schedulable::Transaction(processed_tx.clone()));
                Some(processed_tx.key())
            }
            ConsensusCertificateResult::IgnoredSystem => None,
            _ => unreachable!("process_consensus_system_transaction returned unexpected ConsensusCertificateResult."),
        };

        output.record_consensus_message_processed(SequencedConsensusTransactionKey::System(
            *transaction.digest(),
        ));
        Ok(consensus_commit_prologue_root)
    }

    // Assigns shared object versions to transactions and updates the next shared object version state.
    // Shared object versions in cancelled transactions are assigned to special versions that will
    // cause the transactions to be cancelled in execution engine.
    fn process_consensus_transaction_shared_object_versions(
        &self,
        cache_reader: &dyn ObjectCacheRead,
        non_randomness_transactions: &[Schedulable],
        randomness_transactions: &[Schedulable],
        cancelled_txns: &BTreeMap<TransactionDigest, CancelConsensusCertificateReason>,
        output: &mut ConsensusCommitOutput,
    ) -> SuiResult<AssignedTxAndVersions> {
        let all_certs = non_randomness_transactions
            .iter()
            .chain(randomness_transactions.iter());

        let ConsensusSharedObjVerAssignment {
            shared_input_next_versions,
            assigned_versions,
        } = SharedObjVerManager::assign_versions_from_consensus(
            self,
            cache_reader,
            all_certs,
            cancelled_txns,
        )?;

        output.set_next_shared_object_versions(shared_input_next_versions);
        Ok(assigned_versions)
    }

    pub fn get_highest_pending_checkpoint_height(&self) -> CheckpointHeight {
        self.consensus_quarantine
            .read()
            .get_highest_pending_checkpoint_height()
            .unwrap_or_default()
    }

    // Caller is not required to set ExecutionIndices with the right semantics in
    // VerifiedSequencedConsensusTransaction.
    // Also, ConsensusStats and hash will not be updated in the db with this function, unlike in
    // process_consensus_transactions_and_commit_boundary().
    pub async fn process_consensus_transactions_for_tests<C: CheckpointServiceNotify>(
        self: &Arc<Self>,
        transactions: Vec<SequencedConsensusTransaction>,
        checkpoint_service: &Arc<C>,
        cache_reader: &dyn ObjectCacheRead,
        authority_metrics: &Arc<AuthorityMetrics>,
        skip_consensus_commit_prologue_in_test: bool,
    ) -> SuiResult<(Vec<Schedulable>, AssignedTxAndVersions)> {
        self.process_consensus_transactions_and_commit_boundary(
            transactions,
            &ExecutionIndicesWithStats::default(),
            checkpoint_service,
            cache_reader,
            &ConsensusCommitInfo::new_for_test(
                if self.randomness_state_enabled() {
                    self.get_highest_pending_checkpoint_height() / 2 + 1
                } else {
                    self.get_highest_pending_checkpoint_height() + 1
                },
                0,
                Some(Duration::from_millis(80)),
                skip_consensus_commit_prologue_in_test,
            ),
            IndirectStateObserver::new(),
            authority_metrics,
        )
        .await
    }

    pub fn assign_shared_object_versions_for_tests(
        self: &Arc<Self>,
        cache_reader: &dyn ObjectCacheRead,
        transactions: &[VerifiedExecutableTransaction],
    ) -> SuiResult<AssignedTxAndVersions> {
        let mut output = ConsensusCommitOutput::new(0);
        let transactions: Vec<_> = transactions
            .iter()
            .cloned()
            .map(Schedulable::Transaction)
            .collect();
        let assigned_versions = self.process_consensus_transaction_shared_object_versions(
            cache_reader,
            &transactions,
            &[],
            &BTreeMap::new(),
            &mut output,
        )?;
        let mut batch = self.db_batch()?;
        output.set_default_commit_stats_for_testing();
        output.write_to_batch(self, &mut batch)?;
        batch.write()?;
        Ok(assigned_versions)
    }

    fn process_notifications(
        &self,
        notifications: &[SequencedConsensusTransactionKey],
        end_of_publish: &[VerifiedSequencedConsensusTransaction],
    ) {
        for key in notifications
            .iter()
            .cloned()
            .chain(end_of_publish.iter().map(|tx| tx.0.transaction.key()))
        {
            self.consensus_notify_read.notify(&key, &());
        }
    }

    /// Depending on the type of the VerifiedSequencedConsensusTransaction wrappers,
    /// - Verify and initialize the state to execute the certificates.
    ///   Return VerifiedCertificates for each executable certificate
    /// - Or update the state for checkpoint or epoch change protocol.
    #[instrument(level = "debug", skip_all)]
    #[allow(clippy::type_complexity)]
    pub(crate) async fn process_consensus_transactions<C: CheckpointServiceNotify>(
        &self,
        output: &mut ConsensusCommitOutput,
        non_randomness_transactions: &[VerifiedSequencedConsensusTransaction],
        randomness_transactions: &[VerifiedSequencedConsensusTransaction],
        end_of_publish_transactions: &[VerifiedSequencedConsensusTransaction],
        checkpoint_service: &Arc<C>,
        cache_reader: &dyn ObjectCacheRead,
        consensus_commit_info: &ConsensusCommitInfo,
        mut indirect_state_observer: IndirectStateObserver,
        non_randomness_roots: &mut BTreeSet<TransactionKey>,
        randomness_roots: &mut BTreeSet<TransactionKey>,
        mut shared_object_congestion_tracker: SharedObjectCongestionTracker,
        mut shared_object_using_randomness_congestion_tracker: SharedObjectCongestionTracker,
        previously_deferred_tx_digests: HashMap<TransactionDigest, DeferralKey>,
        mut randomness_manager: Option<&mut RandomnessManager>,
        dkg_failed: bool,
        randomness_round: Option<RandomnessRound>,
        execution_time_estimator: Option<&ExecutionTimeEstimator>,
        authority_metrics: &Arc<AuthorityMetrics>,
    ) -> SuiResult<(
        Vec<Schedulable>,                      // non-randomness transactions to schedule
        Vec<Schedulable>,                      // randomness transactions to schedule
        Vec<SequencedConsensusTransactionKey>, // keys to notify as complete
        Option<RwLockWriteGuard<ReconfigState>>,
        bool,                   // true if final round
        Option<TransactionKey>, // consensus commit prologue root
        AssignedTxAndVersions,
    )> {
        let _scope = monitored_scope("ConsensusCommitHandler::process_consensus_transactions");

        if randomness_round.is_some() {
            assert!(!dkg_failed); // invariant check
        }

        let mut notifications =
            Vec::with_capacity(non_randomness_transactions.len() + randomness_transactions.len());

        let mut deferred_txns: BTreeMap<DeferralKey, Vec<VerifiedSequencedConsensusTransaction>> =
            BTreeMap::new();
        let mut cancelled_txns: BTreeMap<TransactionDigest, CancelConsensusCertificateReason> =
            BTreeMap::new();

        fail_point_arg!(
            "initial_congestion_tracker",
            |tracker: SharedObjectCongestionTracker| {
                info!(
                    "Initialize shared_object_congestion_tracker to  {:?}",
                    tracker
                );
                shared_object_congestion_tracker = tracker;
            }
        );

        let mut randomness_state_updated = false;
        let mut verified_non_randomness_certificates =
            VecDeque::with_capacity(non_randomness_transactions.len() + 1);
        let mut verified_randomness_certificates =
            VecDeque::with_capacity(randomness_transactions.len() + 1);

        if let Some(round) = randomness_round {
            verified_randomness_certificates
                .push_back(Schedulable::RandomnessStateUpdate(self.epoch(), round));
        }

        if self.accumulators_enabled() {
            let checkpoint_height =
                self.calculate_pending_checkpoint_height(consensus_commit_info.round);

            let non_random_settlement =
                Schedulable::AccumulatorSettlement(self.epoch(), checkpoint_height);
            non_randomness_roots.insert(non_random_settlement.key());
            verified_non_randomness_certificates.push_back(non_random_settlement);

            if randomness_round.is_some() {
                let randomness_settlement =
                    Schedulable::AccumulatorSettlement(self.epoch(), checkpoint_height + 1);
                randomness_roots.insert(randomness_settlement.key());
                verified_randomness_certificates.push_back(randomness_settlement);
            }
        }

        for entry in non_randomness_transactions
            .iter()
            .map(Either::Left)
            .chain(randomness_transactions.iter().map(Either::Right))
        {
            let (tx, execution_cost, verified_certificates) = match entry {
                Either::Left(tx) => (
                    tx,
                    &mut shared_object_congestion_tracker,
                    &mut verified_non_randomness_certificates,
                ),
                Either::Right(tx) => (
                    tx,
                    &mut shared_object_using_randomness_congestion_tracker,
                    &mut verified_randomness_certificates,
                ),
            };
            let key = tx.0.transaction.key();
            let mut ignored = false;
            let mut filter_roots = false;
            match self
                .process_consensus_transaction(
                    output,
                    tx,
                    checkpoint_service,
                    consensus_commit_info,
                    &mut indirect_state_observer,
                    &previously_deferred_tx_digests,
                    randomness_manager.as_deref_mut(),
                    dkg_failed,
                    randomness_round.is_some(),
                    execution_cost,
                    execution_time_estimator,
                    authority_metrics,
                )
                .await?
            {
                ConsensusCertificateResult::SuiTransaction(cert) => {
                    notifications.push(key.clone());
                    verified_certificates.push_back(Schedulable::Transaction(cert));
                }
                ConsensusCertificateResult::Deferred(deferral_key) => {
                    // Note: record_consensus_message_processed() must be called for this
                    // cert even though we are not processing it now!
                    deferred_txns
                        .entry(deferral_key)
                        .or_default()
                        .push(tx.clone());
                    filter_roots = true;
                    if tx.0.transaction.is_executable_transaction() {
                        // Notify consensus adapter that the consensus handler has received the transaction.
                        notifications.push(key.clone());
                    }
                }
                ConsensusCertificateResult::Cancelled((cert, reason)) => {
                    notifications.push(key.clone());
                    assert!(cancelled_txns.insert(*cert.digest(), reason).is_none());
                    verified_certificates.push_back(Schedulable::Transaction(cert));
                }
                ConsensusCertificateResult::RandomnessConsensusMessage => {
                    randomness_state_updated = true;
                    notifications.push(key.clone());
                }
                ConsensusCertificateResult::ConsensusMessage => notifications.push(key.clone()),
                ConsensusCertificateResult::IgnoredSystem => {
                    filter_roots = true;
                }
                // Note: ignored external transactions must not be recorded as processed. Otherwise
                // they may not get reverted after restart during epoch change.
                ConsensusCertificateResult::Ignored => {
                    ignored = true;
                    filter_roots = true;
                }
            }
            if !ignored {
                output.record_consensus_message_processed(key.clone());
            }
            if filter_roots {
                if let Some(txn_key) =
                    tx.0.transaction
                        .executable_transaction_digest()
                        .map(TransactionKey::Digest)
                {
                    non_randomness_roots.remove(&txn_key);
                    randomness_roots.remove(&txn_key);
                }
            }
        }

        let commit_has_deferred_txns = !deferred_txns.is_empty();
        let mut total_deferred_txns = 0;
        {
            let mut deferred_transactions =
                self.consensus_output_cache.deferred_transactions.lock();
            for (key, txns) in deferred_txns.into_iter() {
                total_deferred_txns += txns.len();
                deferred_transactions.insert(key, txns.clone());
                output.defer_transactions(key, txns);
            }
        }

        authority_metrics
            .consensus_handler_deferred_transactions
            .inc_by(total_deferred_txns as u64);
        authority_metrics
            .consensus_handler_cancelled_transactions
            .inc_by(cancelled_txns.len() as u64);
        authority_metrics
            .consensus_handler_max_object_costs
            .with_label_values(&["regular_commit"])
            .set(shared_object_congestion_tracker.max_cost() as i64);
        authority_metrics
            .consensus_handler_max_object_costs
            .with_label_values(&["randomness_commit"])
            .set(shared_object_using_randomness_congestion_tracker.max_cost() as i64);

        let object_debts =
            shared_object_congestion_tracker.accumulated_debts(consensus_commit_info);
        let randomness_object_debts = shared_object_using_randomness_congestion_tracker
            .accumulated_debts(consensus_commit_info);
        if let Some(tx_object_debts) = self.tx_object_debts.get() {
            if let Err(e) = tx_object_debts.try_send(
                object_debts
                    .iter()
                    .map(|(id, _)| *id)
                    .chain(randomness_object_debts.iter().map(|(id, _)| *id))
                    .collect(),
            ) {
                info!("failed to send updated object debts to ExecutionTimeObserver: {e:?}");
            }
        }
        output.set_congestion_control_object_debts(object_debts);
        output.set_congestion_control_randomness_object_debts(randomness_object_debts);

        if randomness_state_updated {
            if let Some(randomness_manager) = randomness_manager.as_mut() {
                randomness_manager
                    .advance_dkg(output, consensus_commit_info.round)
                    .await?;
            }
        }

        // Add the consensus commit prologue transaction to the beginning of `verified_non_randomness_certificates`.
        let consensus_commit_prologue_root = self.add_consensus_commit_prologue_transaction(
            output,
            &mut verified_non_randomness_certificates,
            consensus_commit_info,
            indirect_state_observer,
            &cancelled_txns,
        )?;

        let verified_non_randomness_certificates: Vec<_> =
            verified_non_randomness_certificates.into();
        let verified_randomness_certificates: Vec<_> = verified_randomness_certificates.into();

        let assigned_tx_and_versions = self.process_consensus_transaction_shared_object_versions(
            cache_reader,
            &verified_non_randomness_certificates,
            &verified_randomness_certificates,
            &cancelled_txns,
            output,
        )?;

        let (lock, final_round) = self.process_end_of_publish_transactions_and_reconfig(
            output,
            end_of_publish_transactions,
            commit_has_deferred_txns,
        )?;

        Ok((
            verified_non_randomness_certificates,
            verified_randomness_certificates,
            notifications,
            lock,
            final_round,
            consensus_commit_prologue_root,
            assigned_tx_and_versions,
        ))
    }

    fn process_end_of_publish_transactions_and_reconfig(
        &self,
        output: &mut ConsensusCommitOutput,
        transactions: &[VerifiedSequencedConsensusTransaction],
        commit_has_deferred_txns: bool,
    ) -> SuiResult<(
        Option<RwLockWriteGuard<ReconfigState>>,
        bool, // true if final round
    )> {
        let mut lock = None;

        for transaction in transactions {
            let VerifiedSequencedConsensusTransaction(SequencedConsensusTransaction {
                transaction,
                ..
            }) = transaction;

            if let SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::EndOfPublish(authority),
                ..
            }) = transaction
            {
                debug!(
                    "Received EndOfPublish for epoch {} from {:?}",
                    self.committee.epoch,
                    authority.concise()
                );

                // It is ok to just release lock here as this function is the only place that transition into RejectAllCerts state
                // And this function itself is always executed from consensus task
                let collected_end_of_publish = if lock.is_none()
                    && self
                        .get_reconfig_state_read_lock_guard()
                        .should_accept_consensus_certs()
                {
                    output.insert_end_of_publish(*authority);
                    self.end_of_publish.try_lock()
                        .expect("No contention on Authority::end_of_publish as it is only accessed from consensus handler")
                        .insert_generic(*authority, ()).is_quorum_reached()
                    // end_of_publish lock is released here.
                } else {
                    // If we past the stage where we are accepting consensus certificates we also don't record end of publish messages
                    debug!("Ignoring end of publish message from validator {:?} as we already collected enough end of publish messages", authority.concise());
                    false
                };

                if collected_end_of_publish {
                    assert!(lock.is_none());
                    debug!(
                        "Collected enough end_of_publish messages for epoch {} with last message from validator {:?}",
                        self.committee.epoch,
                        authority.concise(),
                    );
                    let mut l = self.get_reconfig_state_write_lock_guard();
                    l.close_all_certs();
                    output.store_reconfig_state(l.clone());
                    // Holding this lock until end of process_consensus_transactions_and_commit_boundary() where we write batch to DB
                    lock = Some(l);
                };
                // Important: we actually rely here on fact that ConsensusHandler panics if its
                // operation returns error. If some day we won't panic in ConsensusHandler on error
                // we need to figure out here how to revert in-memory state of .end_of_publish
                // and .reconfig_state when write fails.
                output.record_consensus_message_processed(transaction.key());
            } else {
                panic!(
                    "process_end_of_publish_transactions_and_reconfig called with non-end-of-publish transaction"
                );
            }
        }

        // Determine if we're ready to advance reconfig state to RejectAllTx.
        let is_reject_all_certs = if let Some(lock) = &lock {
            lock.is_reject_all_certs()
        } else {
            // It is ok to just release lock here as this function is the only place that
            // transitions into RejectAllTx state, and this function itself is always
            // executed from consensus task.
            self.get_reconfig_state_read_lock_guard()
                .is_reject_all_certs()
        };

        if !is_reject_all_certs || !self.deferred_transactions_empty() || commit_has_deferred_txns {
            // Don't end epoch until all deferred transactions are processed.
            if is_reject_all_certs {
                debug!(
                    "Blocking end of epoch on deferred transactions, from previous commits?={}, from this commit?={commit_has_deferred_txns}",
                    !self.deferred_transactions_empty(),
                );
            }
            return Ok((lock, false));
        }

        // Acquire lock to advance state if we don't already have it.
        let mut lock = lock.unwrap_or_else(|| self.get_reconfig_state_write_lock_guard());
        lock.close_all_tx();
        output.store_reconfig_state(lock.clone());
        Ok((Some(lock), true))
    }

    #[instrument(level = "trace", skip_all)]
    async fn process_consensus_transaction<C: CheckpointServiceNotify>(
        &self,
        output: &mut ConsensusCommitOutput,
        transaction: &VerifiedSequencedConsensusTransaction,
        checkpoint_service: &Arc<C>,
        commit_info: &ConsensusCommitInfo,
        indirect_state_observer: &mut IndirectStateObserver,
        previously_deferred_tx_digests: &HashMap<TransactionDigest, DeferralKey>,
        mut randomness_manager: Option<&mut RandomnessManager>,
        dkg_failed: bool,
        generating_randomness: bool,
        shared_object_congestion_tracker: &mut SharedObjectCongestionTracker,
        execution_time_estimator: Option<&ExecutionTimeEstimator>,
        authority_metrics: &Arc<AuthorityMetrics>,
    ) -> SuiResult<ConsensusCertificateResult> {
        let _scope = monitored_scope("ConsensusCommitHandler::process_consensus_transaction");

        let VerifiedSequencedConsensusTransaction(SequencedConsensusTransaction {
            certificate_author_index: _,
            certificate_author,
            consensus_index,
            transaction,
        }) = transaction;
        let tracking_id = transaction.get_tracking_id();

        match &transaction {
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::CertifiedTransaction(certificate),
                ..
            }) => {
                if certificate.epoch() != self.epoch() {
                    // Epoch has changed after this certificate was sequenced, ignore it.
                    debug!(
                        "Certificate epoch ({:?}) doesn't match the current epoch ({:?})",
                        certificate.epoch(),
                        self.epoch()
                    );
                    return Ok(ConsensusCertificateResult::Ignored);
                }
                // Safe because signatures are verified when consensus called into SuiTxValidator::validate_batch.
                let certificate = VerifiedCertificate::new_unchecked(*certificate.clone());
                let transaction = VerifiedExecutableTransaction::new_from_certificate(certificate);

                self.process_consensus_user_transaction(
                    transaction,
                    certificate_author,
                    commit_info,
                    indirect_state_observer,
                    tracking_id,
                    previously_deferred_tx_digests,
                    dkg_failed,
                    generating_randomness,
                    shared_object_congestion_tracker,
                    execution_time_estimator,
                    authority_metrics,
                )
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::CheckpointSignature(info),
                ..
            }) => {
                // We usually call notify_checkpoint_signature in SuiTxValidator, but that step can
                // be skipped when a batch is already part of a certificate, so we must also
                // notify here.
                checkpoint_service.notify_checkpoint_signature(self, info)?;
                Ok(ConsensusCertificateResult::ConsensusMessage)
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::EndOfPublish(_),
                ..
            }) => {
                // these are partitioned earlier
                panic!("process_consensus_transaction called with end-of-publish transaction");
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::CapabilityNotification(capabilities),
                ..
            }) => {
                let authority = capabilities.authority;
                if self
                    .get_reconfig_state_read_lock_guard()
                    .should_accept_consensus_certs()
                {
                    debug!(
                        "Received CapabilityNotification from {:?}",
                        authority.concise()
                    );
                    self.record_capabilities(capabilities)?;
                } else {
                    debug!(
                        "Ignoring CapabilityNotification from {:?} because of end of epoch",
                        authority.concise()
                    );
                }
                Ok(ConsensusCertificateResult::ConsensusMessage)
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::CapabilityNotificationV2(capabilities),
                ..
            }) => {
                let authority = capabilities.authority;
                if self
                    .get_reconfig_state_read_lock_guard()
                    .should_accept_consensus_certs()
                {
                    debug!(
                        "Received CapabilityNotificationV2 from {:?}",
                        authority.concise()
                    );
                    self.record_capabilities_v2(capabilities)?;
                } else {
                    debug!(
                        "Ignoring CapabilityNotificationV2 from {:?} because of end of epoch",
                        authority.concise()
                    );
                }
                Ok(ConsensusCertificateResult::ConsensusMessage)
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::NewJWKFetched(authority, jwk_id, jwk),
                ..
            }) => {
                if self
                    .get_reconfig_state_read_lock_guard()
                    .should_accept_consensus_certs()
                {
                    self.record_jwk_vote(
                        output,
                        consensus_index.last_committed_round,
                        *authority,
                        jwk_id,
                        jwk,
                    )?;
                } else {
                    debug!(
                        "Ignoring NewJWKFetched from {:?} because of end of epoch",
                        authority.concise()
                    );
                }
                Ok(ConsensusCertificateResult::ConsensusMessage)
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::RandomnessStateUpdate(_, _),
                ..
            }) => {
                // These are always generated as System transactions (handled below).
                panic!("process_consensus_transaction called with external RandomnessStateUpdate");
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::RandomnessDkgMessage(authority, bytes),
                ..
            }) => {
                if self.get_reconfig_state_read_lock_guard().should_accept_tx() {
                    if let Some(randomness_manager) = randomness_manager.as_mut() {
                        debug!(
                            "Received RandomnessDkgMessage from {:?}",
                            authority.concise()
                        );
                        match bcs::from_bytes(bytes) {
                            Ok(message) => randomness_manager.add_message(authority, message)?,
                            Err(e) => {
                                warn!(
                                    "Failed to deserialize RandomnessDkgMessage from {:?}: {e:?}",
                                    authority.concise()
                                );
                            }
                        }
                    } else {
                        debug!(
                            "Ignoring RandomnessDkgMessage from {:?} because randomness is not enabled",
                            authority.concise()
                        );
                    }
                } else {
                    debug!(
                        "Ignoring RandomnessDkgMessage from {:?} because of end of epoch",
                        authority.concise()
                    );
                }
                Ok(ConsensusCertificateResult::RandomnessConsensusMessage)
            }
            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::RandomnessDkgConfirmation(authority, bytes),
                ..
            }) => {
                if self.get_reconfig_state_read_lock_guard().should_accept_tx() {
                    if let Some(randomness_manager) = randomness_manager.as_mut() {
                        debug!(
                            "Received RandomnessDkgConfirmation from {:?}",
                            authority.concise()
                        );
                        match bcs::from_bytes(bytes) {
                            Ok(message) => {
                                randomness_manager.add_confirmation(output, authority, message)?
                            }
                            Err(e) => {
                                warn!(
                                        "Failed to deserialize RandomnessDkgConfirmation from {:?}: {e:?}",
                                        authority.concise(),
                                    );
                            }
                        }
                    } else {
                        debug!(
                            "Ignoring RandomnessDkgMessage from {:?} because randomness is not enabled",
                            authority.concise()
                        );
                    }
                } else {
                    debug!(
                        "Ignoring RandomnessDkgMessage from {:?} because of end of epoch",
                        authority.concise()
                    );
                }
                Ok(ConsensusCertificateResult::RandomnessConsensusMessage)
            }

            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::ExecutionTimeObservation(_),
                ..
            }) => {
                // These are partitioned earlier.
                fatal!("process_consensus_transaction called with ExecutionTimeObservation transaction");
            }

            SequencedConsensusTransactionKind::External(ConsensusTransaction {
                kind: ConsensusTransactionKind::UserTransaction(tx),
                ..
            }) => {
                // Ignore consensus certified user transaction if Mysticeti fastpath is not enabled.
                if !self.protocol_config().mysticeti_fastpath() {
                    return Ok(ConsensusCertificateResult::Ignored);
                }
                // Safe because transactions are certified by consensus.
                let tx = VerifiedTransaction::new_unchecked(*tx.clone());
                // TODO(fastpath): accept position in consensus, after plumbing consensus round, authority index, and transaction index here.
                let transaction =
                    VerifiedExecutableTransaction::new_from_consensus(tx, self.epoch());

                self.process_consensus_user_transaction(
                    transaction,
                    certificate_author,
                    commit_info,
                    indirect_state_observer,
                    tracking_id,
                    previously_deferred_tx_digests,
                    dkg_failed,
                    generating_randomness,
                    shared_object_congestion_tracker,
                    execution_time_estimator,
                    authority_metrics,
                )
            }
            SequencedConsensusTransactionKind::System(system_transaction) => {
                Ok(self.process_consensus_system_transaction(system_transaction))
            }
        }
    }

    fn process_consensus_system_transaction(
        &self,
        system_transaction: &VerifiedExecutableTransaction,
    ) -> ConsensusCertificateResult {
        if !self.get_reconfig_state_read_lock_guard().should_accept_tx() {
            debug!(
                "Ignoring system transaction {:?} because of end of epoch",
                system_transaction.digest()
            );
            return ConsensusCertificateResult::IgnoredSystem;
        }

        // If needed we can support owned object system transactions as well...
        assert!(system_transaction.is_consensus_tx());
        ConsensusCertificateResult::SuiTransaction(system_transaction.clone())
    }

    fn process_consensus_user_transaction(
        &self,
        transaction: VerifiedExecutableTransaction,
        block_author: &AuthorityPublicKeyBytes,
        commit_info: &ConsensusCommitInfo,
        indirect_state_observer: &mut IndirectStateObserver,
        tracking_id: u64,
        previously_deferred_tx_digests: &HashMap<TransactionDigest, DeferralKey>,
        dkg_failed: bool,
        generating_randomness: bool,
        shared_object_congestion_tracker: &mut SharedObjectCongestionTracker,
        execution_time_estimator: Option<&ExecutionTimeEstimator>,
        authority_metrics: &Arc<AuthorityMetrics>,
    ) -> SuiResult<ConsensusCertificateResult> {
        let _scope = monitored_scope("ConsensusCommitHandler::process_consensus_user_transaction");

        if self.has_sent_end_of_publish(block_author)?
            && !previously_deferred_tx_digests.contains_key(transaction.digest())
        {
            // This can not happen with valid authority
            // With some edge cases consensus might sometimes resend previously seen certificate after EndOfPublish
            // However this certificate will be filtered out before this line by `consensus_message_processed` call in `verify_consensus_transaction`
            // If we see some new certificate here it means authority is byzantine and sent certificate after EndOfPublish (or we have some bug in ConsensusAdapter)
            warn!("[Byzantine authority] Authority {:?} sent a new, previously unseen transaction {:?} after it sent EndOfPublish message to consensus", block_author.concise(), transaction.digest());
            return Ok(ConsensusCertificateResult::Ignored);
        }

        debug!(
            ?tracking_id,
            tx_digest = ?transaction.digest(),
            "handle_consensus_transaction UserTransaction",
        );

        if !self
            .get_reconfig_state_read_lock_guard()
            .should_accept_consensus_certs()
            && !previously_deferred_tx_digests.contains_key(transaction.digest())
        {
            debug!(
                "Ignoring consensus transaction {:?} because of end of epoch",
                transaction.digest()
            );
            return Ok(ConsensusCertificateResult::Ignored);
        }

        let tx_cost = shared_object_congestion_tracker.get_tx_cost(
            execution_time_estimator,
            &transaction,
            indirect_state_observer,
        );

        let deferral_info = self.should_defer(
            tx_cost,
            &transaction,
            commit_info,
            dkg_failed,
            generating_randomness,
            previously_deferred_tx_digests,
            shared_object_congestion_tracker,
        );

        if let Some((deferral_key, deferral_reason)) = deferral_info {
            debug!(
                "Deferring consensus certificate for transaction {:?} until {:?}",
                transaction.digest(),
                deferral_key
            );

            let deferral_result = match deferral_reason {
                DeferralReason::RandomnessNotReady => {
                    // Always defer transaction due to randomness not ready.
                    ConsensusCertificateResult::Deferred(deferral_key)
                }
                DeferralReason::SharedObjectCongestion(congested_objects) => {
                    authority_metrics
                        .consensus_handler_congested_transactions
                        .inc();
                    if transaction_deferral_within_limit(
                        &deferral_key,
                        self.protocol_config()
                            .max_deferral_rounds_for_congestion_control(),
                    ) {
                        ConsensusCertificateResult::Deferred(deferral_key)
                    } else {
                        // Cancel the transaction that has been deferred for too long.
                        debug!(
                            "Cancelling consensus transaction {:?} with deferral key {:?} due to congestion on objects {:?}",
                            transaction.digest(),
                            deferral_key,
                            congested_objects
                        );
                        ConsensusCertificateResult::Cancelled((
                            transaction,
                            CancelConsensusCertificateReason::CongestionOnObjects(
                                congested_objects,
                            ),
                        ))
                    }
                }
            };
            return Ok(deferral_result);
        }

        if dkg_failed
            && self.randomness_state_enabled()
            && transaction.transaction_data().uses_randomness()
        {
            debug!(
                "Canceling randomness-using transaction {:?} because DKG failed",
                transaction.digest(),
            );
            return Ok(ConsensusCertificateResult::Cancelled((
                transaction,
                CancelConsensusCertificateReason::DkgFailed,
            )));
        }

        // This certificate will be scheduled. Update object execution cost.
        shared_object_congestion_tracker.bump_object_execution_cost(tx_cost, &transaction);

        Ok(ConsensusCertificateResult::SuiTransaction(transaction))
    }

    pub(crate) fn write_pending_checkpoint(
        &self,
        output: &mut ConsensusCommitOutput,
        checkpoint: &PendingCheckpointV2,
    ) -> SuiResult {
        assert!(
            !self.pending_checkpoint_exists(&checkpoint.height())?,
            "Duplicate pending checkpoint notification at height {:?}",
            checkpoint.height()
        );

        debug!(
            checkpoint_commit_height = checkpoint.height(),
            "Pending checkpoint has {} roots",
            checkpoint.roots().len(),
        );
        trace!(
            checkpoint_commit_height = checkpoint.height(),
            "Transaction roots for pending checkpoint: {:?}",
            checkpoint.roots()
        );

        output.insert_pending_checkpoint(checkpoint.clone());

        Ok(())
    }

    pub fn get_pending_checkpoints(
        &self,
        last: Option<CheckpointHeight>,
    ) -> SuiResult<Vec<(CheckpointHeight, PendingCheckpointV2)>> {
        Ok(self
            .consensus_quarantine
            .read()
            .get_pending_checkpoints(last))
    }

    pub fn pending_checkpoint_exists(&self, index: &CheckpointHeight) -> SuiResult<bool> {
        Ok(self
            .consensus_quarantine
            .read()
            .pending_checkpoint_exists(index))
    }

    pub fn process_constructed_checkpoint(
        &self,
        commit_height: CheckpointHeight,
        content_info: NonEmpty<(CheckpointSummary, CheckpointContents)>,
    ) {
        let mut consensus_quarantine = self.consensus_quarantine.write();
        for (position_in_commit, (summary, transactions)) in content_info.into_iter().enumerate() {
            let sequence_number = summary.sequence_number;
            let summary = BuilderCheckpointSummary {
                summary,
                checkpoint_height: Some(commit_height),
                position_in_commit,
            };

            consensus_quarantine.insert_builder_summary(sequence_number, summary, transactions);
        }

        // Because builder can run behind state sync, the data may be immediately ready to be committed.
        consensus_quarantine
            .commit(self)
            .expect("commit cannot fail");
    }

    /// Register genesis checkpoint in builder DB
    pub fn put_genesis_checkpoint_in_builder(
        &self,
        summary: &CheckpointSummary,
        contents: &CheckpointContents,
    ) -> SuiResult<()> {
        let sequence = summary.sequence_number;
        for transaction in contents.iter() {
            let digest = transaction.transaction;
            debug!(
                "Manually inserting genesis transaction in checkpoint DB: {:?}",
                digest
            );
            self.tables()?
                .builder_digest_to_checkpoint
                .insert(&digest, &sequence)?;
        }
        let builder_summary = BuilderCheckpointSummary {
            summary: summary.clone(),
            checkpoint_height: None,
            position_in_commit: 0,
        };
        self.tables()?
            .builder_checkpoint_summary_v2
            .insert(summary.sequence_number(), &builder_summary)?;
        Ok(())
    }

    pub fn last_built_checkpoint_builder_summary(
        &self,
    ) -> SuiResult<Option<BuilderCheckpointSummary>> {
        if let Some(summary) = self.consensus_quarantine.read().last_built_summary() {
            return Ok(Some(summary.clone()));
        }

        Ok(self
            .tables()?
            .builder_checkpoint_summary_v2
            .reversed_safe_iter_with_bounds(None, None)?
            .next()
            .transpose()?
            .map(|(_, s)| s))
    }

    pub fn last_built_checkpoint_summary(
        &self,
    ) -> SuiResult<Option<(CheckpointSequenceNumber, CheckpointSummary)>> {
        if let Some(BuilderCheckpointSummary { summary, .. }) =
            self.consensus_quarantine.read().last_built_summary()
        {
            let seq = *summary.sequence_number();
            debug!(
                "returning last_built_summary from consensus quarantine: {:?}",
                seq
            );
            Ok(Some((seq, summary.clone())))
        } else {
            let seq = self
                .tables()?
                .builder_checkpoint_summary_v2
                .reversed_safe_iter_with_bounds(None, None)?
                .next()
                .transpose()?
                .map(|(seq, s)| (seq, s.summary));
            debug!(
                "returning last_built_summary from builder_checkpoint_summary_v2: {:?}",
                seq
            );
            Ok(seq)
        }
    }

    pub fn get_built_checkpoint_summary(
        &self,
        sequence: CheckpointSequenceNumber,
    ) -> SuiResult<Option<CheckpointSummary>> {
        if let Some(BuilderCheckpointSummary { summary, .. }) =
            self.consensus_quarantine.read().get_built_summary(sequence)
        {
            return Ok(Some(summary.clone()));
        }

        Ok(self
            .tables()?
            .builder_checkpoint_summary_v2
            .get(&sequence)?
            .map(|s| s.summary))
    }

    pub(crate) fn get_lowest_non_genesis_checkpoint_summary(
        &self,
    ) -> SuiResult<Option<CheckpointSummary>> {
        for result in self
            .tables()?
            .builder_checkpoint_summary_v2
            .safe_iter_with_bounds(None, None)
        {
            let (seq, bcs) = result?;
            if seq > 0 {
                return Ok(Some(bcs.summary));
            }
        }
        Ok(None)
    }

    pub fn builder_included_transactions_in_checkpoint<'a>(
        &self,
        digests: impl Iterator<Item = &'a TransactionDigest>,
    ) -> SuiResult<Vec<bool>> {
        let digests: Vec<_> = digests.cloned().collect();
        let tables = self.tables()?;
        Ok(do_fallback_lookup(
            &digests,
            |digest| {
                let consensus_quarantine = self.consensus_quarantine.read();
                if consensus_quarantine.included_transaction_in_checkpoint(digest) {
                    CacheResult::Hit(true)
                } else {
                    CacheResult::Miss
                }
            },
            |remaining| {
                tables
                    .builder_digest_to_checkpoint
                    .multi_contains_keys(remaining)
                    .expect("db error")
            },
        ))
    }

    pub fn get_last_checkpoint_signature_index(&self) -> SuiResult<u64> {
        Ok(self
            .tables()?
            .pending_checkpoint_signatures
            .reversed_safe_iter_with_bounds(None, None)?
            .next()
            .transpose()?
            .map(|((_, index), _)| index)
            .unwrap_or_default())
    }

    pub fn insert_checkpoint_signature(
        &self,
        checkpoint_seq: CheckpointSequenceNumber,
        index: u64,
        info: &CheckpointSignatureMessage,
    ) -> SuiResult<()> {
        Ok(self
            .tables()?
            .pending_checkpoint_signatures
            .insert(&(checkpoint_seq, index), info)?)
    }

    pub(crate) fn record_epoch_pending_certs_process_time_metric(&self) {
        if let Some(epoch_close_time) = *self.epoch_close_time.read() {
            self.metrics
                .epoch_pending_certs_processed_time_since_epoch_close_ms
                .set(epoch_close_time.elapsed().as_millis() as i64);
        }
    }

    pub fn record_end_of_message_quorum_time_metric(&self) {
        if let Some(epoch_close_time) = *self.epoch_close_time.read() {
            self.metrics
                .epoch_end_of_publish_quorum_time_since_epoch_close_ms
                .set(epoch_close_time.elapsed().as_millis() as i64);
        }
    }

    pub(crate) fn report_epoch_metrics_at_last_checkpoint(&self, stats: EpochStats) {
        if let Some(epoch_close_time) = *self.epoch_close_time.read() {
            self.metrics
                .epoch_last_checkpoint_created_time_since_epoch_close_ms
                .set(epoch_close_time.elapsed().as_millis() as i64);
        }
        info!(epoch=?self.epoch(), "Epoch statistics: checkpoint_count={:?}, transaction_count={:?}, total_gas_reward={:?}", stats.checkpoint_count, stats.transaction_count, stats.total_gas_reward);
        self.metrics
            .epoch_checkpoint_count
            .set(stats.checkpoint_count as i64);
        self.metrics
            .epoch_transaction_count
            .set(stats.transaction_count as i64);
        self.metrics
            .epoch_total_gas_reward
            .set(stats.total_gas_reward as i64);
    }

    pub fn record_epoch_reconfig_start_time_metric(&self) {
        if let Some(epoch_close_time) = *self.epoch_close_time.read() {
            self.metrics
                .epoch_reconfig_start_time_since_epoch_close_ms
                .set(epoch_close_time.elapsed().as_millis() as i64);
        }
    }

    fn record_reconfig_halt_duration_metric(&self) {
        if let Some(epoch_close_time) = *self.epoch_close_time.read() {
            self.metrics
                .epoch_validator_halt_duration_ms
                .set(epoch_close_time.elapsed().as_millis() as i64);
        }
    }

    pub(crate) fn record_epoch_first_checkpoint_creation_time_metric(&self) {
        self.metrics
            .epoch_first_checkpoint_created_time_since_epoch_begin_ms
            .set(self.epoch_open_time.elapsed().as_millis() as i64);
    }

    pub fn record_is_safe_mode_metric(&self, safe_mode: bool) {
        self.metrics.is_safe_mode.set(safe_mode as i64);
    }

    pub fn record_checkpoint_builder_is_safe_mode_metric(&self, safe_mode: bool) {
        if safe_mode {
            // allow tests to inject a panic here.
            fail_point!("record_checkpoint_builder_is_safe_mode_metric");
        }
        self.metrics
            .checkpoint_builder_advance_epoch_is_safe_mode
            .set(safe_mode as i64)
    }

    fn record_epoch_total_duration_metric(&self) {
        self.metrics
            .epoch_total_duration
            .set(self.epoch_open_time.elapsed().as_millis() as i64);
    }

    pub(crate) fn update_authenticator_state(&self, update: &AuthenticatorStateUpdate) {
        info!("Updating authenticator state: {:?}", update);
        for active_jwk in &update.new_active_jwks {
            let ActiveJwk { jwk_id, jwk, .. } = active_jwk;
            self.signature_verifier.insert_jwk(jwk_id, jwk);
        }
    }

    pub fn clear_signature_cache(&self) {
        self.signature_verifier.clear_signature_cache();
    }

    pub(crate) fn check_all_executed_transactions_in_checkpoint(&self) {
        let uncheckpointed_transactions = self
            .consensus_output_cache
            .get_uncheckpointed_transactions();

        if uncheckpointed_transactions.is_empty() {
            info!("Verified that all executed transactions are in a checkpoint");
            return;
        }

        // TODO: should this be debug_fatal? Its potentially very serious in that it could
        // indicate that we broke the checkpoint inclusion guarantee, but we won't be able to
        // do anything about it if it happens.
        fatal!(
            "The following transactions were neither reverted nor checkpointed: {:?}",
            uncheckpointed_transactions
        );
    }

    pub(crate) fn set_consensus_tx_status(
        &self,
        position: ConsensusPosition,
        status: ConsensusTxStatus,
    ) {
        if let Some(cache) = self.consensus_tx_status_cache.as_ref() {
            cache.set_transaction_status(position, status);
        }
    }

    pub(crate) fn set_rejection_vote_reason(&self, position: ConsensusPosition, reason: &SuiError) {
        if let Some(tx_reject_reason_cache) = self.tx_reject_reason_cache.as_ref() {
            tx_reject_reason_cache.set_rejection_vote_reason(position, reason);
        }
    }

    pub(crate) fn get_rejection_vote_reason(
        &self,
        position: ConsensusPosition,
    ) -> Option<SuiError> {
        if let Some(tx_reject_reason_cache) = self.tx_reject_reason_cache.as_ref() {
            tx_reject_reason_cache.get_rejection_vote_reason(position)
        } else {
            None
        }
    }

    /// Only used by admin API
    pub async fn get_estimated_tx_cost(&self, tx: &TransactionData) -> Option<u64> {
        self.execution_time_estimator
            .lock()
            .await
            .as_ref()
            .map(|estimator| estimator.get_estimate(tx).as_micros() as u64)
    }

    pub async fn get_consensus_tx_cost_estimates(
        &self,
    ) -> Vec<(ExecutionTimeObservationKey, ConsensusObservations)> {
        self.execution_time_estimator
            .lock()
            .await
            .as_ref()
            .map(|estimator| estimator.get_observations())
            .unwrap_or_default()
    }
}

impl ExecutionComponents {
    fn new(
        protocol_config: &ProtocolConfig,
        store: Arc<dyn BackingPackageStore + Send + Sync>,
        metrics: Arc<ResolverMetrics>,
        // Keep this as a parameter for possible future use
        _expensive_safety_check_config: &ExpensiveSafetyCheckConfig,
    ) -> Self {
        let silent = true;
        let executor = sui_execution::executor(protocol_config, silent, None)
            .expect("Creating an executor should not fail here");

        let module_cache = Arc::new(SyncModuleCache::new(ResolverWrapper::new(
            store,
            metrics.clone(),
        )));
        Self {
            executor,
            module_cache,
            metrics,
        }
    }

    pub(crate) fn metrics(&self) -> Arc<ResolverMetrics> {
        self.metrics.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockDetailsWrapper {
    V1(TransactionDigest),
}

impl LockDetailsWrapper {
    pub fn migrate(self) -> Self {
        // TODO: when there are multiple versions, we must iteratively migrate from version N to
        // N+1 until we arrive at the latest version
        self
    }

    // Always returns the most recent version. Older versions are migrated to the latest version at
    // read time, so there is never a need to access older versions.
    pub fn inner(&self) -> &LockDetails {
        match self {
            Self::V1(v1) => v1,

            // can remove #[allow] when there are multiple versions
            #[allow(unreachable_patterns)]
            _ => panic!("lock details should have been migrated to latest version at read time"),
        }
    }
    pub fn into_inner(self) -> LockDetails {
        match self {
            Self::V1(v1) => v1,

            // can remove #[allow] when there are multiple versions
            #[allow(unreachable_patterns)]
            _ => panic!("lock details should have been migrated to latest version at read time"),
        }
    }
}

pub type LockDetails = TransactionDigest;

impl From<LockDetails> for LockDetailsWrapper {
    fn from(details: LockDetails) -> Self {
        // always use latest version.
        LockDetailsWrapper::V1(details)
    }
}
