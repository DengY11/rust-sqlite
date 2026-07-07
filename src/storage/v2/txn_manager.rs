use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use crate::common::error::{DbError, Result};
use crate::common::types::{RowId, Value};
use crate::engine::txn::TransactionId;
use crate::sql::ast::{CompareOp, IsolationLevel};
use crate::storage::v2::page::PageId;

use super::tx_types::{
    PageWriteSetEntry, TxnSnapshot, TxnState, TxnStatus, TxnWriteSet, UndoRecord,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum PredicateLockKind {
    Table,
    Record(Vec<Value>),
    Gap {
        prefix: Vec<Value>,
        lower: Option<(CompareOp, Value)>,
        upper: Option<(CompareOp, Value)>,
    },
    NextKey {
        prefix: Vec<Value>,
        lower: Option<(CompareOp, Value)>,
        upper: Option<(CompareOp, Value)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PredicateLock {
    txn_id: TransactionId,
    table: String,
    index: Option<String>,
    kind: PredicateLockKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TableLockMode {
    IntentionRead,
    IntentionWrite,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TableLock {
    txn_id: TransactionId,
    table: String,
    mode: TableLockMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PredicateWriteKey {
    table: String,
    index_keys: Vec<(String, Vec<Value>)>,
}

#[derive(Debug)]
pub struct TxnManager {
    next_txn_id: u64,
    next_commit_ts: u64,
    next_row_ids: HashMap<String, u64>,
    txns: HashMap<TransactionId, TxnState>,
    table_locks: Vec<TableLock>,
    predicate_locks: Vec<PredicateLock>,
    table_wait_queues: HashMap<String, VecDeque<TransactionId>>,
    page_write_owners: HashMap<PageId, TransactionId>,
    page_write_wait_queues: HashMap<PageId, VecDeque<TransactionId>>,
    table_wait_blockers: HashMap<TransactionId, BTreeSet<TransactionId>>,
    predicate_write_wait_queues: HashMap<PredicateWriteKey, VecDeque<TransactionId>>,
    page_wait_blockers: HashMap<TransactionId, BTreeSet<TransactionId>>,
    predicate_wait_blockers: HashMap<TransactionId, BTreeSet<TransactionId>>,
    waits_for: HashMap<TransactionId, BTreeSet<TransactionId>>,
    lock_wait_notifier: Arc<(Mutex<u64>, Condvar)>,
}

impl TxnManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_next_txn_id(next_txn_id: u64) -> Self {
        Self {
            next_txn_id,
            next_commit_ts: next_txn_id.saturating_sub(1),
            next_row_ids: HashMap::new(),
            txns: HashMap::new(),
            table_locks: Vec::new(),
            predicate_locks: Vec::new(),
            table_wait_queues: HashMap::new(),
            page_write_owners: HashMap::new(),
            page_write_wait_queues: HashMap::new(),
            table_wait_blockers: HashMap::new(),
            predicate_write_wait_queues: HashMap::new(),
            page_wait_blockers: HashMap::new(),
            predicate_wait_blockers: HashMap::new(),
            waits_for: HashMap::new(),
            lock_wait_notifier: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    pub fn lock_wait_notifier(&self) -> Arc<(Mutex<u64>, Condvar)> {
        self.lock_wait_notifier.clone()
    }

    pub fn current_lock_wait_epoch(notifier: &Arc<(Mutex<u64>, Condvar)>) -> u64 {
        *notifier.0.lock().unwrap()
    }

    pub fn wait_for_lock_epoch_change(notifier: &Arc<(Mutex<u64>, Condvar)>, observed_epoch: u64) {
        let mut epoch = notifier.0.lock().unwrap();
        while *epoch == observed_epoch {
            epoch = notifier.1.wait(epoch).unwrap();
        }
    }

    pub fn begin(&mut self, isolation_level: IsolationLevel) -> TransactionId {
        let id = TransactionId(self.next_txn_id.max(1));
        self.next_txn_id = id.0 + 1;
        self.insert_txn(id, isolation_level);
        id
    }

    pub fn begin_with_id(
        &mut self,
        txn_id: TransactionId,
        isolation_level: IsolationLevel,
    ) -> Result<()> {
        if self.txns.contains_key(&txn_id) {
            return Err(DbError::txn(format!(
                "transaction {} is already registered",
                txn_id.0
            )));
        }

        self.next_txn_id = self.next_txn_id.max(txn_id.0.saturating_add(1)).max(1);
        self.insert_txn(txn_id, isolation_level);
        Ok(())
    }

    pub fn get(&self, txn_id: TransactionId) -> Result<&TxnState> {
        self.txns
            .get(&txn_id)
            .ok_or_else(|| DbError::txn(format!("transaction {} is not active", txn_id.0)))
    }

    pub fn get_mut(&mut self, txn_id: TransactionId) -> Result<&mut TxnState> {
        self.txns
            .get_mut(&txn_id)
            .ok_or_else(|| DbError::txn(format!("transaction {} is not active", txn_id.0)))
    }

    pub fn commit(&mut self, txn_id: TransactionId) -> Result<u64> {
        let commit_ts = self.reserve_commit_ts(txn_id)?;
        self.finalize_commit(txn_id, commit_ts)?;
        Ok(commit_ts)
    }

    pub fn reserve_commit_ts(&mut self, txn_id: TransactionId) -> Result<u64> {
        self.ensure_active(txn_id)?;
        let commit_ts = self.next_commit_ts + 1;
        self.next_commit_ts = commit_ts;
        Ok(commit_ts)
    }

    pub fn finalize_commit(&mut self, txn_id: TransactionId, commit_ts: u64) -> Result<()> {
        let txn = self.get_mut(txn_id)?;
        if txn.status != TxnStatus::Active {
            return Err(DbError::txn(format!(
                "transaction {} is not active",
                txn_id.0
            )));
        }
        txn.status = TxnStatus::Committed;
        txn.terminal_error = None;
        txn.commit_ts = Some(commit_ts);
        txn.purge_records = txn
            .undo_records
            .iter()
            .filter(|record| matches!(record, UndoRecord::DeleteRow { .. }))
            .cloned()
            .collect();
        Ok(())
    }

    pub fn abort(&mut self, txn_id: TransactionId) -> Result<()> {
        let txn = self.get_mut(txn_id)?;
        if txn.status != TxnStatus::Active {
            return Err(DbError::txn(format!(
                "transaction {} is not active",
                txn_id.0
            )));
        }
        txn.status = TxnStatus::Aborted;
        txn.terminal_error = None;
        Ok(())
    }

    #[must_use]
    pub fn has_active_transactions(&self) -> bool {
        self.txns
            .values()
            .any(|txn| txn.status == TxnStatus::Active)
    }

    pub fn refresh_snapshot(&mut self, txn_id: TransactionId) -> Result<TxnSnapshot> {
        let visible_up_to = self.next_commit_ts;
        let active_txns = self
            .txns
            .values()
            .filter(|txn| txn.status == TxnStatus::Active && txn.id != txn_id)
            .map(|txn| txn.id)
            .collect::<BTreeSet<_>>();
        let snapshot = TxnSnapshot {
            visible_up_to,
            active_txns,
        };
        let txn = self.get_mut(txn_id)?;
        txn.snapshot = snapshot.clone();
        Ok(snapshot)
    }

    pub fn snapshot(&self, txn_id: TransactionId) -> Result<TxnSnapshot> {
        Ok(self.get(txn_id)?.snapshot.clone())
    }

    pub fn purge_horizon(&self) -> u64 {
        self.txns
            .values()
            .filter(|txn| txn.status == TxnStatus::Active)
            .map(|txn| txn.snapshot.visible_up_to)
            .min()
            .unwrap_or(self.next_commit_ts)
    }

    pub fn purge_finished_transactions_up_to(&mut self, purge_horizon: u64) {
        self.txns.retain(|_, txn| match txn.status {
            TxnStatus::Active => true,
            TxnStatus::Committed => {
                txn.commit_ts
                    .is_none_or(|commit_ts| commit_ts > purge_horizon)
                    || !txn.purge_records.is_empty()
            }
            TxnStatus::Aborted => !txn.undo_records.is_empty() || txn.terminal_error.is_some(),
        });
    }

    pub fn purge_finished_transactions(&mut self) {
        let purge_horizon = self.purge_horizon();
        self.purge_finished_transactions_up_to(purge_horizon);
    }

    pub fn history_list_length(&self) -> usize {
        self.txns
            .values()
            .filter(|txn| txn.status == TxnStatus::Committed)
            .map(|txn| txn.purge_records.len())
            .sum()
    }

    pub fn isolation_level(&self, txn_id: TransactionId) -> Result<IsolationLevel> {
        Ok(self.get(txn_id)?.isolation_level)
    }

    pub fn sync_next_row_ids(&mut self, next_row_ids: &BTreeMap<String, u64>) {
        for (table, next_row_id) in next_row_ids {
            self.next_row_ids
                .entry(table.clone())
                .and_modify(|existing| *existing = (*existing).max(*next_row_id))
                .or_insert(*next_row_id);
        }
    }

    pub fn allocate_row_id(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        fallback_next_row_id: u64,
    ) -> Result<RowId> {
        self.ensure_active(txn_id)?;
        let next_row_id = self
            .next_row_ids
            .entry(table.to_string())
            .or_insert(fallback_next_row_id.max(1));
        *next_row_id = (*next_row_id).max(fallback_next_row_id.max(1));
        let row_id = RowId(*next_row_id);
        *next_row_id += 1;
        Ok(row_id)
    }

    pub fn acquire_table_read_lock(&mut self, txn_id: TransactionId, table: &str) -> Result<()> {
        self.acquire_table_intention_read(txn_id, table)?;
        self.acquire_predicate_lock(
            txn_id,
            PredicateLock {
                txn_id,
                table: table.to_string(),
                index: None,
                kind: PredicateLockKind::Table,
            },
        )
    }

    pub fn acquire_table_intention_read(
        &mut self,
        txn_id: TransactionId,
        table: &str,
    ) -> Result<()> {
        self.acquire_table_lock(txn_id, table, TableLockMode::IntentionRead)
    }

    pub fn acquire_table_intention_write(
        &mut self,
        txn_id: TransactionId,
        table: &str,
    ) -> Result<()> {
        self.acquire_table_lock(txn_id, table, TableLockMode::IntentionWrite)
    }

    pub fn acquire_table_exclusive(&mut self, txn_id: TransactionId, table: &str) -> Result<()> {
        self.acquire_table_lock(txn_id, table, TableLockMode::Exclusive)
    }

    pub fn acquire_exact_key_lock(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        index: &str,
        key: &[Value],
    ) -> Result<()> {
        self.acquire_record_lock(txn_id, table, index, key)
    }

    pub fn acquire_record_lock(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        index: &str,
        key: &[Value],
    ) -> Result<()> {
        self.acquire_predicate_lock(
            txn_id,
            PredicateLock {
                txn_id,
                table: table.to_string(),
                index: Some(index.to_string()),
                kind: PredicateLockKind::Record(key.to_vec()),
            },
        )
    }

    pub fn acquire_prefix_lock(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        index: &str,
        key_prefix: &[Value],
    ) -> Result<()> {
        self.acquire_next_key_lock(txn_id, table, index, key_prefix, None, None)
    }

    pub fn acquire_gap_lock(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        index: &str,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<()> {
        self.acquire_predicate_lock(
            txn_id,
            PredicateLock {
                txn_id,
                table: table.to_string(),
                index: Some(index.to_string()),
                kind: PredicateLockKind::Gap {
                    prefix: key_prefix.to_vec(),
                    lower: lower.map(|(op, value)| (op, value.clone())),
                    upper: upper.map(|(op, value)| (op, value.clone())),
                },
            },
        )
    }

    pub fn acquire_range_lock(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        index: &str,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<()> {
        self.acquire_next_key_lock(txn_id, table, index, key_prefix, lower, upper)
    }

    pub fn acquire_next_key_lock(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        index: &str,
        key_prefix: &[Value],
        lower: Option<(CompareOp, &Value)>,
        upper: Option<(CompareOp, &Value)>,
    ) -> Result<()> {
        self.acquire_predicate_lock(
            txn_id,
            PredicateLock {
                txn_id,
                table: table.to_string(),
                index: Some(index.to_string()),
                kind: PredicateLockKind::NextKey {
                    prefix: key_prefix.to_vec(),
                    lower: lower.map(|(op, value)| (op, value.clone())),
                    upper: upper.map(|(op, value)| (op, value.clone())),
                },
            },
        )
    }

    pub fn check_write_conflicts(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        index_keys: &[(String, Vec<Value>)],
    ) -> Result<()> {
        self.ensure_active(txn_id)?;
        let wait_key = predicate_write_key(table, index_keys);
        let blockers = self.conflicting_predicate_lock_txns(txn_id, table, index_keys)?;

        if blockers.is_empty() {
            let front_waiter = self
                .predicate_write_wait_queues
                .get(&wait_key)
                .and_then(|queue| queue.front().copied());
            if let Some(front_waiter) = front_waiter {
                if front_waiter != txn_id {
                    self.enqueue_predicate_waiter(wait_key, txn_id);
                    self.set_predicate_wait_edges(txn_id, BTreeSet::from([front_waiter]));
                    if self.has_wait_path(front_waiter, txn_id, &mut BTreeSet::new()) {
                        if self.resolve_deadlock(txn_id, front_waiter, |victim| {
                            format!(
                                "deadlock detected while waiting for predicate write on table {table}; victim transaction {} aborted",
                                victim.0
                            )
                        })? {
                            return self.check_write_conflicts(txn_id, table, index_keys);
                        }
                    }
                    return Err(DbError::txn(format!(
                        "predicate write wait on table {table}; transaction {} is ahead in queue",
                        front_waiter.0
                    )));
                }
            }

            self.dequeue_predicate_waiter(&wait_key, txn_id);
            self.clear_predicate_wait_edges(txn_id);
            return Ok(());
        }

        self.enqueue_predicate_waiter(wait_key, txn_id);
        self.set_predicate_wait_edges(txn_id, blockers.clone());
        for blocker in &blockers {
            if self.has_wait_path(*blocker, txn_id, &mut BTreeSet::new()) {
                if self.resolve_deadlock(txn_id, *blocker, |victim| {
                    format!(
                        "deadlock detected while writing table {table}; victim transaction {} aborted",
                        victim.0
                    )
                })? {
                    return self.check_write_conflicts(txn_id, table, index_keys);
                }
            }
        }
        Err(DbError::txn(format!(
            "serializable conflict on table {table}; waiting on transactions {}",
            blockers
                .iter()
                .map(|txn| txn.0.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )))
    }

    pub fn replace_page_write_set(
        &mut self,
        txn_id: TransactionId,
        page_writes: Vec<PageWriteSetEntry>,
    ) -> Result<()> {
        let txn = self.get_mut(txn_id)?;
        txn.write_set.touched_pages = page_writes.iter().map(|entry| entry.page_id).collect();
        txn.write_set.page_writes = page_writes;
        Ok(())
    }

    pub fn page_write_set(&self, txn_id: TransactionId) -> Result<&TxnWriteSet> {
        Ok(&self.get(txn_id)?.write_set)
    }

    pub fn acquire_page_write(&mut self, txn_id: TransactionId, page_id: PageId) -> Result<()> {
        match self.page_write_owners.get(&page_id).copied() {
            None => {
                self.ensure_active(txn_id)?;
                let front_waiter = self
                    .page_write_wait_queues
                    .get(&page_id)
                    .and_then(|queue| queue.front().copied());
                if let Some(front_waiter) = front_waiter {
                    if front_waiter != txn_id {
                        self.enqueue_page_waiter(page_id, txn_id);
                        self.set_page_wait_edges(txn_id, BTreeSet::from([front_waiter]));
                        if self.has_wait_path(front_waiter, txn_id, &mut BTreeSet::new()) {
                            if self.resolve_deadlock(txn_id, front_waiter, |victim| {
                                format!(
                                    "deadlock detected between transactions {} and {} on page {}; victim transaction {} aborted",
                                    txn_id.0, front_waiter.0, page_id.0, victim.0
                                )
                            })? {
                                return self.acquire_page_write(txn_id, page_id);
                            }
                        }
                        return Err(DbError::txn(format!(
                            "page write wait on page {}; transaction {} is ahead in queue",
                            page_id.0, front_waiter.0
                        )));
                    }
                }
                self.dequeue_front_waiter(page_id, txn_id);
                self.page_write_owners.insert(page_id, txn_id);
                self.clear_page_wait_edges(txn_id);
                Ok(())
            }
            Some(owner) if owner == txn_id => {
                self.dequeue_front_waiter(page_id, txn_id);
                self.clear_page_wait_edges(txn_id);
                Ok(())
            }
            Some(owner) => {
                self.ensure_active(txn_id)?;
                self.get(owner)?;
                self.enqueue_page_waiter(page_id, txn_id);
                self.set_page_wait_edges(txn_id, BTreeSet::from([owner]));
                if self.has_wait_path(owner, txn_id, &mut BTreeSet::new()) {
                    if self.resolve_deadlock(txn_id, owner, |victim| {
                        format!(
                            "deadlock detected between transactions {} and {} on page {}; victim transaction {} aborted",
                            txn_id.0, owner.0, page_id.0, victim.0
                        )
                    })? {
                        return self.acquire_page_write(txn_id, page_id);
                    }
                }
                Err(DbError::txn(format!(
                    "page write conflict on page {} held by transaction {}",
                    page_id.0, owner.0
                )))
            }
        }
    }

    pub fn release_transaction_resources(&mut self, txn_id: TransactionId) -> Result<()> {
        self.get(txn_id)?;
        self.table_locks.retain(|lock| lock.txn_id != txn_id);
        self.page_write_owners.retain(|_, owner| *owner != txn_id);
        self.remove_txn_from_table_wait_queues(txn_id);
        self.remove_txn_from_wait_queues(txn_id);
        self.remove_txn_from_predicate_wait_queues(txn_id);
        self.predicate_locks.retain(|lock| lock.txn_id != txn_id);
        self.table_wait_blockers.remove(&txn_id);
        self.page_wait_blockers.remove(&txn_id);
        self.predicate_wait_blockers.remove(&txn_id);
        self.waits_for.remove(&txn_id);
        self.remove_wait_dependency_on(txn_id);
        self.bump_lock_wait_epoch();
        Ok(())
    }

    pub fn status(&self, txn_id: TransactionId) -> Result<TxnStatus> {
        Ok(self.get(txn_id)?.status)
    }

    pub fn record_undo(&mut self, txn_id: TransactionId, record: UndoRecord) -> Result<()> {
        self.get_mut(txn_id)?.undo_records.push(record);
        Ok(())
    }

    pub fn undo_records(&self, txn_id: TransactionId) -> Result<&[UndoRecord]> {
        Ok(&self.get(txn_id)?.undo_records)
    }

    pub fn take_undo_records(&mut self, txn_id: TransactionId) -> Result<Vec<UndoRecord>> {
        let txn = self.get_mut(txn_id)?;
        let mut undo_records = std::mem::take(&mut txn.undo_records);
        undo_records.reverse();
        Ok(undo_records)
    }

    pub fn clear_terminal_error(&mut self, txn_id: TransactionId) -> Result<()> {
        self.get_mut(txn_id)?.terminal_error = None;
        Ok(())
    }

    pub fn planned_purge_batch(
        &self,
        purge_horizon: u64,
        limit: usize,
    ) -> Vec<(TransactionId, UndoRecord)> {
        if limit == 0 {
            return Vec::new();
        }

        let mut committed_txns = self
            .txns
            .values()
            .filter(|txn| {
                txn.status == TxnStatus::Committed
                    && txn
                        .commit_ts
                        .is_some_and(|commit_ts| commit_ts <= purge_horizon)
                    && !txn.purge_records.is_empty()
            })
            .collect::<Vec<_>>();
        committed_txns.sort_by_key(|txn| txn.commit_ts.unwrap_or(u64::MAX));

        let mut planned = Vec::new();
        for txn in committed_txns {
            for record in txn.purge_records.iter() {
                if planned.len() == limit {
                    return planned;
                }
                planned.push((txn.id, record.clone()));
            }
        }
        planned
    }

    pub fn complete_purge_batch(
        &mut self,
        purged_records: &[(TransactionId, UndoRecord)],
    ) -> Result<()> {
        for (txn_id, record) in purged_records {
            let txn = self.get_mut(*txn_id)?;
            let Some(front) = txn.purge_records.pop_front() else {
                return Err(DbError::storage(format!(
                    "missing purge record for transaction {}",
                    txn_id.0
                )));
            };
            if front != *record {
                return Err(DbError::storage(format!(
                    "purge record order mismatch for transaction {}",
                    txn_id.0
                )));
            }
        }
        Ok(())
    }

    fn insert_txn(&mut self, txn_id: TransactionId, isolation_level: IsolationLevel) {
        let snapshot = TxnSnapshot {
            visible_up_to: self.next_commit_ts,
            active_txns: self
                .txns
                .values()
                .filter(|txn| txn.status == TxnStatus::Active)
                .map(|txn| txn.id)
                .collect::<BTreeSet<_>>(),
        };
        self.txns.insert(
            txn_id,
            TxnState {
                id: txn_id,
                isolation_level,
                status: TxnStatus::Active,
                terminal_error: None,
                start_ts: txn_id.0,
                commit_ts: None,
                snapshot,
                write_set: TxnWriteSet::default(),
                undo_records: Vec::new(),
                purge_records: VecDeque::new(),
            },
        );
        self.table_wait_blockers.entry(txn_id).or_default();
        self.page_wait_blockers.entry(txn_id).or_default();
        self.predicate_wait_blockers.entry(txn_id).or_default();
        self.waits_for.entry(txn_id).or_default();
    }

    fn ensure_active(&self, txn_id: TransactionId) -> Result<()> {
        let txn = self.get(txn_id)?;
        if txn.status == TxnStatus::Active {
            Ok(())
        } else {
            Err(DbError::txn(txn.terminal_error.clone().unwrap_or_else(
                || format!("transaction {} is not active", txn_id.0),
            )))
        }
    }

    fn choose_deadlock_victim(
        &self,
        requester: TransactionId,
        blocker: TransactionId,
    ) -> Result<Option<TransactionId>> {
        let Some(mut cycle) = self.wait_path(blocker, requester) else {
            return Ok(None);
        };
        cycle.push(requester);
        cycle.sort();
        cycle.dedup();

        Ok(cycle.into_iter().min_by_key(|txn_id| {
            let txn = self.get(*txn_id).expect("cycle members must exist");
            let page_lock_count = self
                .page_write_owners
                .values()
                .filter(|owner| **owner == *txn_id)
                .count();
            let predicate_lock_count = self
                .predicate_locks
                .iter()
                .filter(|lock| lock.txn_id == *txn_id)
                .count();
            let table_lock_count = self
                .table_locks
                .iter()
                .filter(|lock| lock.txn_id == *txn_id)
                .count();
            let rollback_cost = txn.undo_records.len()
                + txn.write_set.page_writes.len()
                + page_lock_count
                + table_lock_count
                + predicate_lock_count;
            (
                rollback_cost,
                Reverse(txn.start_ts),
                if *txn_id == requester { 0usize } else { 1usize },
            )
        }))
    }

    fn wait_path(
        &self,
        current: TransactionId,
        target: TransactionId,
    ) -> Option<Vec<TransactionId>> {
        self.wait_path_with_visited(current, target, &mut BTreeSet::new())
    }

    fn wait_path_with_visited(
        &self,
        current: TransactionId,
        target: TransactionId,
        visited: &mut BTreeSet<TransactionId>,
    ) -> Option<Vec<TransactionId>> {
        if current == target {
            return Some(vec![current]);
        }
        if !visited.insert(current) {
            return None;
        }

        for next in self.waits_for.get(&current).into_iter().flatten().copied() {
            if let Some(mut path) = self.wait_path_with_visited(next, target, visited) {
                path.insert(0, current);
                return Some(path);
            }
        }
        None
    }

    fn resolve_deadlock(
        &mut self,
        requester: TransactionId,
        blocker: TransactionId,
        context: impl Fn(TransactionId) -> String,
    ) -> Result<bool> {
        let Some(victim) = self.choose_deadlock_victim(requester, blocker)? else {
            return Ok(false);
        };
        let message = context(victim);
        self.abort_deadlock_victim(victim, message.clone())?;
        if victim == requester {
            return Err(DbError::txn(message));
        }
        Ok(true)
    }

    fn acquire_predicate_lock(&mut self, txn_id: TransactionId, lock: PredicateLock) -> Result<()> {
        if self.isolation_level(txn_id)? != IsolationLevel::Serializable {
            return Ok(());
        }
        if !self
            .predicate_locks
            .iter()
            .any(|existing| *existing == lock)
        {
            self.predicate_locks.push(lock);
        }
        Ok(())
    }

    fn conflicting_predicate_lock_txns(
        &self,
        txn_id: TransactionId,
        table: &str,
        index_keys: &[(String, Vec<Value>)],
    ) -> Result<BTreeSet<TransactionId>> {
        let mut blockers = BTreeSet::new();
        for lock in &self.predicate_locks {
            if lock.txn_id == txn_id || lock.table != table {
                continue;
            }
            if self.get(lock.txn_id)?.status != TxnStatus::Active {
                continue;
            }
            if predicate_lock_conflicts(lock, index_keys) {
                blockers.insert(lock.txn_id);
            }
        }
        Ok(blockers)
    }

    fn acquire_table_lock(
        &mut self,
        txn_id: TransactionId,
        table: &str,
        mode: TableLockMode,
    ) -> Result<()> {
        self.ensure_active(txn_id)?;

        let blockers = self.conflicting_table_lock_txns(txn_id, table, mode)?;
        let table_key = table.to_string();
        if blockers.is_empty() {
            let front_waiter = self
                .table_wait_queues
                .get(&table_key)
                .and_then(|queue| queue.front().copied());
            if let Some(front_waiter) = front_waiter {
                if front_waiter != txn_id {
                    self.enqueue_table_waiter(table, txn_id);
                    self.set_table_wait_edges(txn_id, BTreeSet::from([front_waiter]));
                    if self.has_wait_path(front_waiter, txn_id, &mut BTreeSet::new()) {
                        if self.resolve_deadlock(txn_id, front_waiter, |victim| {
                            format!(
                                "deadlock detected while waiting for table lock on {table}; victim transaction {} aborted",
                                victim.0
                            )
                        })? {
                            return self.acquire_table_lock(txn_id, table, mode);
                        }
                    }
                    return Err(DbError::txn(format!(
                        "table lock wait on {table}; transaction {} is ahead in queue",
                        front_waiter.0
                    )));
                }
            }

            self.dequeue_table_waiter(table, txn_id);
            self.clear_table_wait_edges(txn_id);
            self.upsert_table_lock(txn_id, table, mode);
            return Ok(());
        }

        self.enqueue_table_waiter(table, txn_id);
        self.set_table_wait_edges(txn_id, blockers.clone());
        for blocker in &blockers {
            if self.has_wait_path(*blocker, txn_id, &mut BTreeSet::new()) {
                if self.resolve_deadlock(txn_id, *blocker, |victim| {
                    format!(
                        "deadlock detected while acquiring table lock on {table}; victim transaction {} aborted",
                        victim.0
                    )
                })? {
                    return self.acquire_table_lock(txn_id, table, mode);
                }
            }
        }

        Err(DbError::txn(format!(
            "table lock wait on {table}; waiting on transactions {}",
            blockers
                .iter()
                .map(|txn| txn.0.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )))
    }

    fn conflicting_table_lock_txns(
        &self,
        txn_id: TransactionId,
        table: &str,
        requested_mode: TableLockMode,
    ) -> Result<BTreeSet<TransactionId>> {
        let mut blockers = BTreeSet::new();
        for lock in &self.table_locks {
            if lock.txn_id == txn_id || lock.table != table {
                continue;
            }
            if self.get(lock.txn_id)?.status != TxnStatus::Active {
                continue;
            }
            if table_lock_modes_conflict(lock.mode, requested_mode) {
                blockers.insert(lock.txn_id);
            }
        }
        Ok(blockers)
    }

    fn upsert_table_lock(&mut self, txn_id: TransactionId, table: &str, mode: TableLockMode) {
        if let Some(existing) = self
            .table_locks
            .iter_mut()
            .find(|lock| lock.txn_id == txn_id && lock.table == table)
        {
            if mode > existing.mode {
                existing.mode = mode;
            }
        } else {
            self.table_locks.push(TableLock {
                txn_id,
                table: table.to_string(),
                mode,
            });
        }
    }

    fn enqueue_table_waiter(&mut self, table: &str, txn_id: TransactionId) {
        let queue = self.table_wait_queues.entry(table.to_string()).or_default();
        if !queue.contains(&txn_id) {
            queue.push_back(txn_id);
        }
    }

    fn dequeue_table_waiter(&mut self, table: &str, txn_id: TransactionId) {
        let should_remove = if let Some(queue) = self.table_wait_queues.get_mut(table) {
            if queue.front().copied() == Some(txn_id) {
                queue.pop_front();
            }
            queue.is_empty()
        } else {
            false
        };
        if should_remove {
            self.table_wait_queues.remove(table);
        }
    }

    fn remove_txn_from_table_wait_queues(&mut self, txn_id: TransactionId) {
        let tables = self
            .table_wait_queues
            .iter()
            .filter_map(|(table, queue)| queue.contains(&txn_id).then_some(table.clone()))
            .collect::<Vec<_>>();
        for table in tables {
            let should_remove = if let Some(queue) = self.table_wait_queues.get_mut(&table) {
                queue.retain(|queued_txn_id| *queued_txn_id != txn_id);
                queue.is_empty()
            } else {
                false
            };
            if should_remove {
                self.table_wait_queues.remove(&table);
            }
        }
    }

    fn set_table_wait_edges(&mut self, txn_id: TransactionId, blockers: BTreeSet<TransactionId>) {
        self.table_wait_blockers.insert(txn_id, blockers);
        self.refresh_waits_for(txn_id);
    }

    fn clear_table_wait_edges(&mut self, txn_id: TransactionId) {
        self.table_wait_blockers.insert(txn_id, BTreeSet::new());
        self.refresh_waits_for(txn_id);
    }

    fn enqueue_page_waiter(&mut self, page_id: PageId, txn_id: TransactionId) {
        let queue = self.page_write_wait_queues.entry(page_id).or_default();
        if !queue.contains(&txn_id) {
            queue.push_back(txn_id);
        }
    }

    fn dequeue_front_waiter(&mut self, page_id: PageId, txn_id: TransactionId) {
        let should_remove = if let Some(queue) = self.page_write_wait_queues.get_mut(&page_id) {
            if queue.front().copied() == Some(txn_id) {
                queue.pop_front();
            }
            queue.is_empty()
        } else {
            false
        };
        if should_remove {
            self.page_write_wait_queues.remove(&page_id);
        }
    }

    fn enqueue_predicate_waiter(&mut self, wait_key: PredicateWriteKey, txn_id: TransactionId) {
        let queue = self
            .predicate_write_wait_queues
            .entry(wait_key)
            .or_default();
        if !queue.contains(&txn_id) {
            queue.push_back(txn_id);
        }
    }

    fn dequeue_predicate_waiter(&mut self, wait_key: &PredicateWriteKey, txn_id: TransactionId) {
        let should_remove = if let Some(queue) = self.predicate_write_wait_queues.get_mut(wait_key)
        {
            if queue.front().copied() == Some(txn_id) {
                queue.pop_front();
            }
            queue.is_empty()
        } else {
            false
        };
        if should_remove {
            self.predicate_write_wait_queues.remove(wait_key);
        }
    }

    fn remove_txn_from_wait_queues(&mut self, txn_id: TransactionId) {
        let page_ids = self
            .page_write_wait_queues
            .iter()
            .filter_map(|(page_id, queue)| queue.contains(&txn_id).then_some(*page_id))
            .collect::<Vec<_>>();
        for page_id in page_ids {
            let should_remove = if let Some(queue) = self.page_write_wait_queues.get_mut(&page_id) {
                queue.retain(|queued_txn_id| *queued_txn_id != txn_id);
                queue.is_empty()
            } else {
                false
            };
            if should_remove {
                self.page_write_wait_queues.remove(&page_id);
            }
        }
    }

    fn remove_txn_from_predicate_wait_queues(&mut self, txn_id: TransactionId) {
        let wait_keys = self
            .predicate_write_wait_queues
            .iter()
            .filter_map(|(wait_key, queue)| queue.contains(&txn_id).then_some(wait_key.clone()))
            .collect::<Vec<_>>();
        for wait_key in wait_keys {
            let should_remove =
                if let Some(queue) = self.predicate_write_wait_queues.get_mut(&wait_key) {
                    queue.retain(|queued_txn_id| *queued_txn_id != txn_id);
                    queue.is_empty()
                } else {
                    false
                };
            if should_remove {
                self.predicate_write_wait_queues.remove(&wait_key);
            }
        }
    }

    fn set_page_wait_edges(&mut self, txn_id: TransactionId, blockers: BTreeSet<TransactionId>) {
        self.page_wait_blockers.insert(txn_id, blockers);
        self.refresh_waits_for(txn_id);
    }

    fn clear_page_wait_edges(&mut self, txn_id: TransactionId) {
        self.page_wait_blockers.insert(txn_id, BTreeSet::new());
        self.refresh_waits_for(txn_id);
    }

    fn set_predicate_wait_edges(
        &mut self,
        txn_id: TransactionId,
        blockers: BTreeSet<TransactionId>,
    ) {
        self.predicate_wait_blockers.insert(txn_id, blockers);
        self.refresh_waits_for(txn_id);
    }

    fn clear_predicate_wait_edges(&mut self, txn_id: TransactionId) {
        self.predicate_wait_blockers.insert(txn_id, BTreeSet::new());
        self.refresh_waits_for(txn_id);
    }

    fn refresh_waits_for(&mut self, txn_id: TransactionId) {
        let mut blockers = BTreeSet::new();
        blockers.extend(
            self.table_wait_blockers
                .get(&txn_id)
                .into_iter()
                .flatten()
                .copied(),
        );
        blockers.extend(
            self.page_wait_blockers
                .get(&txn_id)
                .into_iter()
                .flatten()
                .copied(),
        );
        blockers.extend(
            self.predicate_wait_blockers
                .get(&txn_id)
                .into_iter()
                .flatten()
                .copied(),
        );
        self.waits_for.insert(txn_id, blockers);
    }

    fn remove_wait_dependency_on(&mut self, blocker: TransactionId) {
        let txns = self
            .txns
            .keys()
            .copied()
            .filter(|txn_id| *txn_id != blocker)
            .collect::<Vec<_>>();
        for txn_id in txns {
            if let Some(table_blockers) = self.table_wait_blockers.get_mut(&txn_id) {
                table_blockers.remove(&blocker);
            }
            if let Some(page_blockers) = self.page_wait_blockers.get_mut(&txn_id) {
                page_blockers.remove(&blocker);
            }
            if let Some(predicate_blockers) = self.predicate_wait_blockers.get_mut(&txn_id) {
                predicate_blockers.remove(&blocker);
            }
            self.refresh_waits_for(txn_id);
        }
    }

    fn abort_deadlock_victim(&mut self, txn_id: TransactionId, message: String) -> Result<()> {
        {
            let txn = self.get_mut(txn_id)?;
            if txn.status == TxnStatus::Active {
                txn.status = TxnStatus::Aborted;
                txn.terminal_error = Some(message);
            }
        }
        self.release_transaction_resources(txn_id)
    }

    fn bump_lock_wait_epoch(&self) {
        let mut epoch = self.lock_wait_notifier.0.lock().unwrap();
        *epoch += 1;
        self.lock_wait_notifier.1.notify_all();
    }

    fn has_wait_path(
        &self,
        current: TransactionId,
        target: TransactionId,
        visited: &mut BTreeSet<TransactionId>,
    ) -> bool {
        if current == target {
            return true;
        }
        if !visited.insert(current) {
            return false;
        }
        self.waits_for
            .get(&current)
            .into_iter()
            .flatten()
            .copied()
            .any(|next| self.has_wait_path(next, target, visited))
    }
}

impl Default for TxnManager {
    fn default() -> Self {
        Self::with_next_txn_id(1)
    }
}

fn predicate_lock_conflicts(lock: &PredicateLock, index_keys: &[(String, Vec<Value>)]) -> bool {
    match &lock.kind {
        PredicateLockKind::Table => true,
        PredicateLockKind::Record(locked_key) => index_keys
            .iter()
            .any(|(index, key)| lock.index.as_deref() == Some(index.as_str()) && key == locked_key),
        PredicateLockKind::Gap {
            prefix,
            lower,
            upper,
        }
        | PredicateLockKind::NextKey {
            prefix,
            lower,
            upper,
        } => index_keys.iter().any(|(index, key)| {
            if lock.index.as_deref() != Some(index.as_str()) || !key.starts_with(prefix) {
                return false;
            }
            let Some(candidate) = key.get(prefix.len()) else {
                return false;
            };
            lower
                .as_ref()
                .is_none_or(|(op, value)| matches_compare(candidate, *op, value))
                && upper
                    .as_ref()
                    .is_none_or(|(op, value)| matches_compare(candidate, *op, value))
        }),
    }
}

fn table_lock_modes_conflict(existing: TableLockMode, requested: TableLockMode) -> bool {
    !matches!(
        (existing, requested),
        (TableLockMode::IntentionRead, TableLockMode::IntentionRead)
            | (TableLockMode::IntentionRead, TableLockMode::IntentionWrite)
            | (TableLockMode::IntentionWrite, TableLockMode::IntentionRead)
            | (TableLockMode::IntentionWrite, TableLockMode::IntentionWrite)
    )
}

fn predicate_write_key(table: &str, index_keys: &[(String, Vec<Value>)]) -> PredicateWriteKey {
    let mut normalized = index_keys.to_vec();
    normalized.sort_by(|(left_index, left_key), (right_index, right_key)| {
        left_index
            .cmp(right_index)
            .then_with(|| left_key.cmp(right_key))
    });
    PredicateWriteKey {
        table: table.to_string(),
        index_keys: normalized,
    }
}

fn matches_compare(left: &Value, op: CompareOp, right: &Value) -> bool {
    match op {
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
        CompareOp::Gt => compare_values(left, right).is_some_and(|ord| ord.is_gt()),
        CompareOp::Gte => compare_values(left, right).is_some_and(|ord| ord.is_gt() || ord.is_eq()),
        CompareOp::Lt => compare_values(left, right).is_some_and(|ord| ord.is_lt()),
        CompareOp::Lte => compare_values(left, right).is_some_and(|ord| ord.is_lt() || ord.is_eq()),
    }
}

fn compare_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
        (Value::Boolean(left), Value::Boolean(right)) => Some(left.cmp(right)),
        (Value::Integer(left), Value::Integer(right)) => Some(left.cmp(right)),
        (Value::Text(left), Value::Text(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::sql::ast::IsolationLevel;

    use super::*;

    #[test]
    fn txn_manager_allows_multiple_active_transactions() {
        let mut manager = TxnManager::new();

        let t1 = manager.begin(IsolationLevel::RepeatableRead);
        let t2 = manager.begin(IsolationLevel::ReadCommitted);

        assert_ne!(t1, t2);
        assert_eq!(manager.get(t1).unwrap().status, TxnStatus::Active);
        assert_eq!(manager.get(t2).unwrap().status, TxnStatus::Active);
    }

    #[test]
    fn txn_manager_refreshes_read_committed_snapshot() {
        let mut manager = TxnManager::new();
        let t1 = manager.begin(IsolationLevel::ReadCommitted);
        let t2 = manager.begin(IsolationLevel::ReadCommitted);

        let commit_ts = manager.commit(t2).unwrap();
        let snapshot = manager.refresh_snapshot(t1).unwrap();

        assert_eq!(snapshot.visible_up_to, commit_ts);
        assert!(!snapshot.active_txns.contains(&t2));
    }

    #[test]
    fn txn_manager_can_register_external_transaction_ids() {
        let mut manager = TxnManager::with_next_txn_id(10);

        manager
            .begin_with_id(TransactionId(12), IsolationLevel::Serializable)
            .unwrap();
        let next = manager.begin(IsolationLevel::ReadCommitted);

        assert_eq!(next, TransactionId(13));
        assert_eq!(
            manager.get(TransactionId(12)).unwrap().isolation_level,
            IsolationLevel::Serializable
        );
    }

    #[test]
    fn txn_manager_tracks_page_write_metadata() {
        let mut manager = TxnManager::new();
        let txn = manager.begin(IsolationLevel::Serializable);

        manager
            .replace_page_write_set(
                txn,
                vec![PageWriteSetEntry {
                    page_id: crate::storage::v2::page::PageId(3),
                    before_image: Some(vec![1, 2, 3]),
                    after_image: vec![4, 5, 6],
                }],
            )
            .unwrap();

        let write_set = manager.page_write_set(txn).unwrap();
        assert!(
            write_set
                .touched_pages
                .contains(&crate::storage::v2::page::PageId(3))
        );
        assert_eq!(write_set.page_writes.len(), 1);
        assert_eq!(write_set.page_writes[0].after_image, vec![4, 5, 6]);
    }

    #[test]
    fn gap_lock_blocks_insert_into_protected_range() {
        let mut manager = TxnManager::new();
        let reader = manager.begin(IsolationLevel::Serializable);
        let writer = manager.begin(IsolationLevel::ReadCommitted);

        manager
            .acquire_gap_lock(
                reader,
                "users",
                "idx_users_active_name",
                &[Value::Boolean(true)],
                Some((CompareOp::Gte, &Value::from("alice"))),
                Some((CompareOp::Lte, &Value::from("carol"))),
            )
            .unwrap();

        let error = manager
            .check_write_conflicts(
                writer,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap_err();

        assert!(error.to_string().contains("serializable conflict"));
    }

    #[test]
    fn page_write_lock_blocks_second_writer_on_same_page() {
        let mut manager = TxnManager::new();
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);

        manager
            .acquire_page_write(writer1, crate::storage::v2::page::PageId(9))
            .unwrap();

        let error = manager
            .acquire_page_write(writer2, crate::storage::v2::page::PageId(9))
            .unwrap_err();
        assert!(error.to_string().contains("page write conflict"));
    }

    #[test]
    fn page_write_wait_cycle_reports_deadlock() {
        let mut manager = TxnManager::new();
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);

        manager
            .acquire_page_write(writer1, crate::storage::v2::page::PageId(1))
            .unwrap();
        manager
            .acquire_page_write(writer2, crate::storage::v2::page::PageId(2))
            .unwrap();

        let first_wait = manager
            .acquire_page_write(writer1, crate::storage::v2::page::PageId(2))
            .unwrap_err();
        assert!(first_wait.to_string().contains("page write conflict"));

        let deadlock = manager
            .acquire_page_write(writer2, crate::storage::v2::page::PageId(1))
            .unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));
    }

    #[test]
    fn release_transaction_resources_frees_page_write_locks() {
        let mut manager = TxnManager::new();
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);

        manager
            .acquire_page_write(writer1, crate::storage::v2::page::PageId(4))
            .unwrap();
        manager.release_transaction_resources(writer1).unwrap();

        manager
            .acquire_page_write(writer2, crate::storage::v2::page::PageId(4))
            .unwrap();
    }

    #[test]
    fn page_write_wait_queue_grants_lock_in_fifo_order() {
        let mut manager = TxnManager::new();
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);
        let writer3 = manager.begin(IsolationLevel::ReadCommitted);
        let page_id = crate::storage::v2::page::PageId(5);

        manager.acquire_page_write(writer1, page_id).unwrap();

        let writer2_wait = manager.acquire_page_write(writer2, page_id).unwrap_err();
        assert!(writer2_wait.to_string().contains("page write conflict"));

        let writer3_wait = manager.acquire_page_write(writer3, page_id).unwrap_err();
        assert!(writer3_wait.to_string().contains("page write conflict"));

        manager.release_transaction_resources(writer1).unwrap();

        let writer3_still_waiting = manager.acquire_page_write(writer3, page_id).unwrap_err();
        assert!(
            writer3_still_waiting
                .to_string()
                .contains("page write wait")
        );

        manager.acquire_page_write(writer2, page_id).unwrap();
    }

    #[test]
    fn deadlock_aborts_requesting_page_writer_and_releases_its_locks() {
        let mut manager = TxnManager::new();
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);
        let left_page = crate::storage::v2::page::PageId(6);
        let right_page = crate::storage::v2::page::PageId(7);

        manager.acquire_page_write(writer1, left_page).unwrap();
        manager.acquire_page_write(writer2, right_page).unwrap();

        manager.acquire_page_write(writer1, right_page).unwrap_err();

        let deadlock = manager.acquire_page_write(writer2, left_page).unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));
        assert!(deadlock.to_string().contains("victim"));
        assert_eq!(manager.get(writer2).unwrap().status, TxnStatus::Aborted);

        manager.acquire_page_write(writer1, right_page).unwrap();
    }

    #[test]
    fn deadlock_prefers_lower_cost_victim_even_when_requester_finds_cycle() {
        let mut manager = TxnManager::new();
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);
        let left_page = crate::storage::v2::page::PageId(16);
        let right_page = crate::storage::v2::page::PageId(17);

        manager.acquire_page_write(writer1, left_page).unwrap();
        manager.acquire_page_write(writer2, right_page).unwrap();
        manager
            .record_undo(
                writer2,
                UndoRecord::PageWrite {
                    page_id: crate::storage::v2::page::PageId(99),
                },
            )
            .unwrap();
        manager
            .record_undo(
                writer2,
                UndoRecord::PageWrite {
                    page_id: crate::storage::v2::page::PageId(100),
                },
            )
            .unwrap();

        manager.acquire_page_write(writer1, right_page).unwrap_err();

        manager.acquire_page_write(writer2, left_page).unwrap();
        assert_eq!(manager.status(writer1).unwrap(), TxnStatus::Aborted);
        assert_eq!(manager.status(writer2).unwrap(), TxnStatus::Active);
    }

    #[test]
    fn predicate_write_wait_queue_grants_retry_in_fifo_order() {
        let mut manager = TxnManager::new();
        let reader = manager.begin(IsolationLevel::Serializable);
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);

        manager
            .acquire_gap_lock(
                reader,
                "users",
                "idx_users_active_name",
                &[Value::Boolean(true)],
                Some((CompareOp::Gte, &Value::from("alice"))),
                Some((CompareOp::Lte, &Value::from("carol"))),
            )
            .unwrap();

        let writer1_wait = manager
            .check_write_conflicts(
                writer1,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap_err();
        assert!(writer1_wait.to_string().contains("serializable conflict"));

        let writer2_wait = manager
            .check_write_conflicts(
                writer2,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap_err();
        assert!(writer2_wait.to_string().contains("serializable conflict"));

        manager.release_transaction_resources(reader).unwrap();

        let writer2_still_waiting = manager
            .check_write_conflicts(
                writer2,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap_err();
        assert!(
            writer2_still_waiting
                .to_string()
                .contains("predicate write wait")
        );

        manager
            .check_write_conflicts(
                writer1,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap();
    }

    #[test]
    fn deadlock_between_page_and_predicate_locks_aborts_victim() {
        let mut manager = TxnManager::new();
        let writer1 = manager.begin(IsolationLevel::ReadCommitted);
        let reader2 = manager.begin(IsolationLevel::Serializable);
        let page_id = crate::storage::v2::page::PageId(12);

        manager.acquire_page_write(writer1, page_id).unwrap();
        manager
            .acquire_gap_lock(
                reader2,
                "users",
                "idx_users_active_name",
                &[Value::Boolean(true)],
                Some((CompareOp::Gte, &Value::from("alice"))),
                Some((CompareOp::Lte, &Value::from("carol"))),
            )
            .unwrap();

        let predicate_wait = manager
            .check_write_conflicts(
                writer1,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap_err();
        assert!(predicate_wait.to_string().contains("serializable conflict"));

        let deadlock = manager.acquire_page_write(reader2, page_id).unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));
        assert!(deadlock.to_string().contains("victim"));
        assert_eq!(manager.status(reader2).unwrap(), TxnStatus::Aborted);

        manager
            .check_write_conflicts(
                writer1,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap();
    }

    #[test]
    fn deadlock_victim_abort_releases_predicate_locks_for_other_writers() {
        let mut manager = TxnManager::new();
        let reader1 = manager.begin(IsolationLevel::Serializable);
        let writer2 = manager.begin(IsolationLevel::ReadCommitted);
        let writer3 = manager.begin(IsolationLevel::ReadCommitted);
        let page_id = crate::storage::v2::page::PageId(13);

        manager
            .acquire_gap_lock(
                reader1,
                "users",
                "idx_users_active_name",
                &[Value::Boolean(true)],
                Some((CompareOp::Gte, &Value::from("alice"))),
                Some((CompareOp::Lte, &Value::from("carol"))),
            )
            .unwrap();
        manager.acquire_page_write(writer2, page_id).unwrap();

        manager
            .check_write_conflicts(
                writer2,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap_err();
        manager
            .record_undo(
                writer2,
                UndoRecord::PageWrite {
                    page_id: crate::storage::v2::page::PageId(130),
                },
            )
            .unwrap();

        let deadlock = manager.acquire_page_write(reader1, page_id).unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));
        assert_eq!(manager.status(reader1).unwrap(), TxnStatus::Aborted);

        let writer3_wait = manager
            .check_write_conflicts(
                writer3,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap_err();
        assert!(writer3_wait.to_string().contains("predicate write wait"));

        manager
            .check_write_conflicts(
                writer2,
                "users",
                &[(
                    "idx_users_active_name".to_string(),
                    vec![Value::Boolean(true), Value::from("bob")],
                )],
            )
            .unwrap();
    }

    #[test]
    fn txn_manager_tracks_fine_grained_undo_records_in_order() {
        let mut manager = TxnManager::new();
        let txn = manager.begin(IsolationLevel::RepeatableRead);

        manager
            .record_undo(
                txn,
                UndoRecord::InsertRow {
                    table: "users".to_string(),
                    row_id: crate::common::types::RowId(7),
                },
            )
            .unwrap();
        manager
            .record_undo(
                txn,
                UndoRecord::IndexInsert {
                    table: "users".to_string(),
                    index: "idx_users_email".to_string(),
                    row_id: crate::common::types::RowId(7),
                    key: vec![Value::from("alice@example.com")],
                },
            )
            .unwrap();

        let undo_records = manager.undo_records(txn).unwrap();
        assert_eq!(undo_records.len(), 2);
        assert!(matches!(
            &undo_records[0],
            UndoRecord::InsertRow { table, row_id }
                if table == "users" && *row_id == crate::common::types::RowId(7)
        ));
        assert!(matches!(
            &undo_records[1],
            UndoRecord::IndexInsert { index, .. } if index == "idx_users_email"
        ));
    }

    #[test]
    fn txn_manager_takes_undo_records_in_reverse_order_and_clears_them() {
        let mut manager = TxnManager::new();
        let txn = manager.begin(IsolationLevel::RepeatableRead);

        manager
            .record_undo(
                txn,
                UndoRecord::InsertRow {
                    table: "users".to_string(),
                    row_id: crate::common::types::RowId(1),
                },
            )
            .unwrap();
        manager
            .record_undo(
                txn,
                UndoRecord::IndexInsert {
                    table: "users".to_string(),
                    index: "idx_users_email".to_string(),
                    row_id: crate::common::types::RowId(1),
                    key: vec![Value::from("a@example.com")],
                },
            )
            .unwrap();

        let undo_records = manager.take_undo_records(txn).unwrap();
        assert_eq!(undo_records.len(), 2);
        assert!(matches!(
            &undo_records[0],
            UndoRecord::IndexInsert { index, .. } if index == "idx_users_email"
        ));
        assert!(matches!(
            &undo_records[1],
            UndoRecord::InsertRow { table, .. } if table == "users"
        ));
        assert!(manager.undo_records(txn).unwrap().is_empty());
    }

    #[test]
    fn purge_finished_transactions_keeps_history_newer_than_horizon() {
        let mut manager = TxnManager::new();
        let committed_old = manager.begin(IsolationLevel::ReadCommitted);
        let committed_new = manager.begin(IsolationLevel::ReadCommitted);

        let committed_old_ts = manager.commit(committed_old).unwrap();
        let committed_new_ts = manager.commit(committed_new).unwrap();

        manager.purge_finished_transactions_up_to(committed_old_ts);

        assert!(manager.get(committed_old).is_err());
        assert_eq!(
            manager.get(committed_new).unwrap().commit_ts,
            Some(committed_new_ts)
        );
    }

    #[test]
    fn history_list_length_counts_retained_finished_undo_logs() {
        let mut manager = TxnManager::new();
        let txn = manager.begin(IsolationLevel::ReadCommitted);

        manager
            .record_undo(
                txn,
                UndoRecord::DeleteRow {
                    table: "users".to_string(),
                    row_id: crate::common::types::RowId(1),
                    previous_bytes: vec![1],
                },
            )
            .unwrap();
        manager
            .record_undo(
                txn,
                UndoRecord::DeleteRow {
                    table: "users".to_string(),
                    row_id: crate::common::types::RowId(2),
                    previous_bytes: vec![2],
                },
            )
            .unwrap();

        let commit_ts = manager.commit(txn).unwrap();
        assert_eq!(manager.history_list_length(), 2);

        let planned_batch = manager.planned_purge_batch(commit_ts, 8);
        manager.complete_purge_batch(&planned_batch).unwrap();
        manager.purge_finished_transactions_up_to(commit_ts);
        assert_eq!(manager.history_list_length(), 0);
    }

    #[test]
    fn table_intention_locks_allow_is_and_ix_but_block_exclusive() {
        let mut manager = TxnManager::new();
        let reader = manager.begin(IsolationLevel::ReadCommitted);
        let writer = manager.begin(IsolationLevel::ReadCommitted);
        let ddl = manager.begin(IsolationLevel::ReadCommitted);

        manager
            .acquire_table_intention_read(reader, "users")
            .unwrap();
        manager
            .acquire_table_intention_write(writer, "users")
            .unwrap();

        let wait = manager.acquire_table_exclusive(ddl, "users").unwrap_err();
        assert!(wait.to_string().contains("table lock wait"));

        manager.release_transaction_resources(reader).unwrap();
        manager.release_transaction_resources(writer).unwrap();

        manager.acquire_table_exclusive(ddl, "users").unwrap();
    }

    #[test]
    fn table_lock_wait_cycle_reports_deadlock() {
        let mut manager = TxnManager::new();
        let txn1 = manager.begin(IsolationLevel::ReadCommitted);
        let txn2 = manager.begin(IsolationLevel::ReadCommitted);

        manager.acquire_table_exclusive(txn1, "users").unwrap();
        manager.acquire_table_exclusive(txn2, "orders").unwrap();

        let wait = manager.acquire_table_exclusive(txn1, "orders").unwrap_err();
        assert!(wait.to_string().contains("table lock"));

        let deadlock = manager.acquire_table_exclusive(txn2, "users").unwrap_err();
        assert!(deadlock.to_string().contains("deadlock"));
        assert_eq!(manager.status(txn2).unwrap(), TxnStatus::Aborted);
    }
}
