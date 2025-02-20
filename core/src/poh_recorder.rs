//! The `poh_recorder` module provides an object for synchronizing with Proof of History.
//! It synchronizes PoH, bank's register_tick and the ledger
//!
//! PohRecorder will send ticks or entries to a WorkingBank, if the current range of ticks is
//! within the specified WorkingBank range.
//!
//! For Ticks:
//! * tick must be > WorkingBank::min_tick_height && tick must be <= WorkingBank::max_tick_height
//!
//! For Entries:
//! * recorded entry must be >= WorkingBank::min_tick_height && entry must be < WorkingBank::max_tick_height
//!
use crate::blocktree::Blocktree;
use crate::entry::Entry;
use crate::leader_schedule_cache::LeaderScheduleCache;
use crate::poh::Poh;
use crate::result::{Error, Result};
use solana_runtime::bank::Bank;
use solana_sdk::hash::Hash;
use solana_sdk::poh_config::PohConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::timing;
pub use solana_sdk::timing::Slot;
use solana_sdk::transaction::Transaction;
use std::sync::mpsc::{channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const GRACE_TICKS_FACTOR: u64 = 2;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum PohRecorderError {
    InvalidCallingObject,
    MaxHeightReached,
    MinHeightNotReached,
}

pub type WorkingBankEntries = (Arc<Bank>, Vec<(Entry, u64)>);

#[derive(Clone)]
pub struct WorkingBank {
    pub bank: Arc<Bank>,
    pub min_tick_height: u64,
    pub max_tick_height: u64,
}

pub struct PohRecorder {
    pub poh: Arc<Mutex<Poh>>,
    tick_height: u64,
    clear_bank_signal: Option<SyncSender<bool>>,
    start_slot: Slot, // parent slot
    start_tick: u64,  // first tick this recorder will observe
    tick_cache: Vec<(Entry, u64)>,
    working_bank: Option<WorkingBank>,
    sender: Sender<WorkingBankEntries>,
    start_leader_at_tick: Option<u64>,
    last_leader_tick: u64, // zero if none
    grace_ticks: u64,
    id: Pubkey,
    blocktree: Arc<Blocktree>,
    leader_schedule_cache: Arc<LeaderScheduleCache>,
    poh_config: Arc<PohConfig>,
    ticks_per_slot: u64,
}

impl PohRecorder {
    fn clear_bank(&mut self) {
        if let Some(working_bank) = self.working_bank.take() {
            let bank = working_bank.bank;
            let next_leader_slot = self.leader_schedule_cache.next_leader_slot(
                &self.id,
                bank.slot(),
                &bank,
                Some(&self.blocktree),
            );
            assert_eq!(self.ticks_per_slot, bank.ticks_per_slot());
            let (start_leader_at_tick, last_leader_tick) = Self::compute_leader_slot_ticks(
                next_leader_slot,
                self.ticks_per_slot,
                self.grace_ticks,
            );
            self.start_leader_at_tick = start_leader_at_tick;
            self.last_leader_tick = last_leader_tick;
        }
        if let Some(ref signal) = self.clear_bank_signal {
            let _ = signal.try_send(true);
        }
    }

    pub fn would_be_leader(&self, within_next_n_ticks: u64) -> bool {
        self.has_bank()
            || self.start_leader_at_tick.map_or(false, |leader_tick| {
                let ideal_leader_tick = leader_tick.saturating_sub(self.grace_ticks);

                self.tick_height <= self.last_leader_tick
                    && self.tick_height >= ideal_leader_tick.saturating_sub(within_next_n_ticks)
            })
    }

    pub fn leader_after_slots(&self, slots: u64) -> Option<Pubkey> {
        self.leader_schedule_cache
            .slot_leader_at(self.tick_height / self.ticks_per_slot + slots, None)
    }

    pub fn next_slot_leader(&self) -> Option<Pubkey> {
        self.leader_after_slots(1)
    }

    pub fn bank(&self) -> Option<Arc<Bank>> {
        self.working_bank.clone().map(|w| w.bank)
    }

    pub fn has_bank(&self) -> bool {
        self.working_bank.is_some()
    }

    pub fn tick_height(&self) -> u64 {
        self.tick_height
    }

    pub fn ticks_per_slot(&self) -> u64 {
        self.ticks_per_slot
    }

    /// returns if leader tick has reached, how many grace ticks were afforded,
    ///   imputed leader_slot and self.start_slot
    /// reached_leader_tick() == true means "ready for a bank"
    pub fn reached_leader_tick(&self) -> (bool, u64, Slot, Slot) {
        trace!(
            "tick_height {}, start_tick {}, start_leader_at_tick {:?}, grace {}, has_bank {}",
            self.tick_height,
            self.start_tick,
            self.start_leader_at_tick,
            self.grace_ticks,
            self.has_bank()
        );

        let next_tick = self.tick_height + 1;

        if let Some(target_tick) = self.start_leader_at_tick {
            // we've reached target_tick OR poh was reset to run immediately
            if next_tick >= target_tick || self.start_tick + self.grace_ticks == target_tick {
                assert!(next_tick >= self.start_tick);
                let ideal_target_tick = target_tick.saturating_sub(self.grace_ticks);

                return (
                    true,
                    next_tick.saturating_sub(ideal_target_tick),
                    next_tick / self.ticks_per_slot,
                    self.start_slot,
                );
            }
        }
        (false, 0, next_tick / self.ticks_per_slot, self.start_slot)
    }

    // returns (start_leader_at_tick, last_leader_tick) given the next slot this
    //  recorder will lead
    fn compute_leader_slot_ticks(
        next_leader_slot: Option<Slot>,
        ticks_per_slot: u64,
        grace_ticks: u64,
    ) -> (Option<u64>, u64) {
        next_leader_slot
            .map(|slot| {
                (
                    Some(slot * ticks_per_slot + grace_ticks),
                    (slot + 1) * ticks_per_slot - 1,
                )
            })
            .unwrap_or((None, 0))
    }

    // synchronize PoH with a bank
    pub fn reset(&mut self, blockhash: Hash, start_slot: Slot, next_leader_slot: Option<Slot>) {
        self.clear_bank();
        let mut cache = vec![];
        {
            let mut poh = self.poh.lock().unwrap();
            info!(
                "reset poh from: {},{},{} to: {},{}",
                poh.hash, self.tick_height, self.start_slot, blockhash, start_slot
            );
            poh.reset(blockhash, self.poh_config.hashes_per_tick);
        }

        std::mem::swap(&mut cache, &mut self.tick_cache);

        self.start_slot = start_slot;
        self.start_tick = (start_slot + 1) * self.ticks_per_slot;
        self.tick_height = self.start_tick - 1;

        let (start_leader_at_tick, last_leader_tick) = Self::compute_leader_slot_ticks(
            next_leader_slot,
            self.ticks_per_slot,
            self.grace_ticks,
        );
        self.start_leader_at_tick = start_leader_at_tick;
        self.last_leader_tick = last_leader_tick;
    }

    pub fn set_working_bank(&mut self, working_bank: WorkingBank) {
        trace!("new working bank");
        assert_eq!(working_bank.bank.ticks_per_slot(), self.ticks_per_slot());
        self.working_bank = Some(working_bank);
    }
    pub fn set_bank(&mut self, bank: &Arc<Bank>) {
        let working_bank = WorkingBank {
            bank: bank.clone(),
            min_tick_height: bank.tick_height(),
            max_tick_height: bank.max_tick_height(),
        };
        self.set_working_bank(working_bank);
    }

    // Flush cache will delay flushing the cache for a bank until it past the WorkingBank::min_tick_height
    // On a record flush will flush the cache at the WorkingBank::min_tick_height, since a record
    // occurs after the min_tick_height was generated
    fn flush_cache(&mut self, tick: bool) -> Result<()> {
        // check_tick_height is called before flush cache, so it cannot overrun the bank
        // so a bank that is so late that it's slot fully generated before it starts recording
        // will fail instead of broadcasting any ticks
        let working_bank = self
            .working_bank
            .as_ref()
            .ok_or(Error::PohRecorderError(PohRecorderError::MaxHeightReached))?;
        if self.tick_height < working_bank.min_tick_height {
            return Err(Error::PohRecorderError(
                PohRecorderError::MinHeightNotReached,
            ));
        }
        if tick && self.tick_height == working_bank.min_tick_height {
            return Err(Error::PohRecorderError(
                PohRecorderError::MinHeightNotReached,
            ));
        }

        let entry_count = self
            .tick_cache
            .iter()
            .take_while(|x| x.1 <= working_bank.max_tick_height)
            .count();
        let send_result = if entry_count > 0 {
            trace!(
                "flush_cache: bank_slot: {} tick_height: {} max: {} sending: {}",
                working_bank.bank.slot(),
                working_bank.bank.tick_height(),
                working_bank.max_tick_height,
                entry_count,
            );
            let cache = &self.tick_cache[..entry_count];
            for t in cache {
                working_bank.bank.register_tick(&t.0.hash);
            }
            self.sender
                .send((working_bank.bank.clone(), cache.to_vec()))
        } else {
            Ok(())
        };
        if self.tick_height >= working_bank.max_tick_height {
            info!(
                "poh_record: max_tick_height {} reached, clearing working_bank {}",
                working_bank.max_tick_height,
                working_bank.bank.slot()
            );
            self.start_slot = working_bank.max_tick_height / self.ticks_per_slot;
            self.start_tick = (self.start_slot + 1) * self.ticks_per_slot;
            self.clear_bank();
        }
        if send_result.is_err() {
            info!("WorkingBank::sender disconnected {:?}", send_result);
            // revert the cache, but clear the working bank
            self.clear_bank();
        } else {
            // commit the flush
            let _ = self.tick_cache.drain(..entry_count);
        }

        Ok(())
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        let poh_entry = self.poh.lock().unwrap().tick();
        inc_new_counter_warn!(
            "poh_recorder-tick_lock_contention",
            timing::duration_as_ms(&now.elapsed()) as usize,
            0,
            1000
        );
        let now = Instant::now();
        if let Some(poh_entry) = poh_entry {
            self.tick_height += 1;
            trace!("tick {}", self.tick_height);

            if self.start_leader_at_tick.is_none() {
                inc_new_counter_warn!(
                    "poh_recorder-tick_overhead",
                    timing::duration_as_ms(&now.elapsed()) as usize,
                    0,
                    1000
                );
                return;
            }

            let entry = Entry {
                num_hashes: poh_entry.num_hashes,
                hash: poh_entry.hash,
                transactions: vec![],
            };

            self.tick_cache.push((entry, self.tick_height));
            let _ = self.flush_cache(true);
        }
        inc_new_counter_warn!(
            "poh_recorder-tick_overhead",
            timing::duration_as_ms(&now.elapsed()) as usize,
            0,
            1000
        );
    }

    pub fn record(
        &mut self,
        bank_slot: Slot,
        mixin: Hash,
        transactions: Vec<Transaction>,
    ) -> Result<()> {
        // Entries without transactions are used to track real-time passing in the ledger and
        // cannot be generated by `record()`
        assert!(!transactions.is_empty(), "No transactions provided");
        loop {
            self.flush_cache(false)?;

            let working_bank = self
                .working_bank
                .as_ref()
                .ok_or(Error::PohRecorderError(PohRecorderError::MaxHeightReached))?;
            if bank_slot != working_bank.bank.slot() {
                return Err(Error::PohRecorderError(PohRecorderError::MaxHeightReached));
            }

            let now = Instant::now();
            if let Some(poh_entry) = self.poh.lock().unwrap().record(mixin) {
                inc_new_counter_warn!(
                    "poh_recorder-record_lock_contention",
                    timing::duration_as_ms(&now.elapsed()) as usize,
                    0,
                    1000
                );
                let entry = Entry {
                    num_hashes: poh_entry.num_hashes,
                    hash: poh_entry.hash,
                    transactions,
                };
                self.sender
                    .send((working_bank.bank.clone(), vec![(entry, self.tick_height)]))?;
                return Ok(());
            }
            // record() might fail if the next PoH hash needs to be a tick.  But that's ok, tick()
            // and re-record()
            self.tick();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_clear_signal(
        tick_height: u64,
        last_entry_hash: Hash,
        start_slot: Slot,
        next_leader_slot: Option<Slot>,
        ticks_per_slot: u64,
        id: &Pubkey,
        blocktree: &Arc<Blocktree>,
        clear_bank_signal: Option<SyncSender<bool>>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        poh_config: &Arc<PohConfig>,
    ) -> (Self, Receiver<WorkingBankEntries>) {
        let poh = Arc::new(Mutex::new(Poh::new(
            last_entry_hash,
            poh_config.hashes_per_tick,
        )));
        let (sender, receiver) = channel();
        let grace_ticks = ticks_per_slot / GRACE_TICKS_FACTOR;
        let (start_leader_at_tick, last_leader_tick) =
            Self::compute_leader_slot_ticks(next_leader_slot, ticks_per_slot, grace_ticks);
        (
            Self {
                poh,
                tick_height,
                tick_cache: vec![],
                working_bank: None,
                sender,
                clear_bank_signal,
                start_slot,
                start_tick: tick_height + 1,
                start_leader_at_tick,
                last_leader_tick,
                grace_ticks,
                id: *id,
                blocktree: blocktree.clone(),
                leader_schedule_cache: leader_schedule_cache.clone(),
                ticks_per_slot,
                poh_config: poh_config.clone(),
            },
            receiver,
        )
    }

    /// A recorder to synchronize PoH with the following data structures
    /// * bank - the LastId's queue is updated on `tick` and `record` events
    /// * sender - the Entry channel that outputs to the ledger
    pub fn new(
        tick_height: u64,
        last_entry_hash: Hash,
        start_slot: Slot,
        next_leader_slot: Option<Slot>,
        ticks_per_slot: u64,
        id: &Pubkey,
        blocktree: &Arc<Blocktree>,
        leader_schedule_cache: &Arc<LeaderScheduleCache>,
        poh_config: &Arc<PohConfig>,
    ) -> (Self, Receiver<WorkingBankEntries>) {
        Self::new_with_clear_signal(
            tick_height,
            last_entry_hash,
            start_slot,
            next_leader_slot,
            ticks_per_slot,
            id,
            blocktree,
            None,
            leader_schedule_cache,
            poh_config,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocktree::{get_tmp_ledger_path, Blocktree};
    use crate::genesis_utils::{create_genesis_block, GenesisBlockInfo};
    use crate::test_tx::test_tx;
    use solana_sdk::hash::hash;
    use solana_sdk::timing::DEFAULT_TICKS_PER_SLOT;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn test_poh_recorder_no_zero_tick() {
        let prev_hash = Hash::default();
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");

            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                DEFAULT_TICKS_PER_SLOT,
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::default()),
                &Arc::new(PohConfig::default()),
            );
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 1);
            assert_eq!(poh_recorder.tick_cache[0].1, 1);
            assert_eq!(poh_recorder.tick_height, 1);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_tick_height_is_last_tick() {
        let prev_hash = Hash::default();
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");

            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                DEFAULT_TICKS_PER_SLOT,
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::default()),
                &Arc::new(PohConfig::default()),
            );
            poh_recorder.tick();
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 2);
            assert_eq!(poh_recorder.tick_cache[1].1, 2);
            assert_eq!(poh_recorder.tick_height, 2);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_reset_clears_cache() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                Hash::default(),
                0,
                Some(4),
                DEFAULT_TICKS_PER_SLOT,
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::default()),
                &Arc::new(PohConfig::default()),
            );
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 1);
            poh_recorder.reset(Hash::default(), 0, Some(4));
            assert_eq!(poh_recorder.tick_cache.len(), 0);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_clear() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let working_bank = WorkingBank {
                bank,
                min_tick_height: 2,
                max_tick_height: 3,
            };
            poh_recorder.set_working_bank(working_bank);
            assert!(poh_recorder.working_bank.is_some());
            poh_recorder.clear_bank();
            assert!(poh_recorder.working_bank.is_none());
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_tick_sent_after_min() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let working_bank = WorkingBank {
                bank: bank.clone(),
                min_tick_height: 2,
                max_tick_height: 3,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.tick();
            poh_recorder.tick();
            //tick height equal to min_tick_height
            //no tick has been sent
            assert_eq!(poh_recorder.tick_cache.last().unwrap().1, 2);
            assert!(entry_receiver.try_recv().is_err());

            // all ticks are sent after height > min
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_height, 3);
            assert_eq!(poh_recorder.tick_cache.len(), 0);
            let (bank_, e) = entry_receiver.recv().expect("recv 1");
            assert_eq!(e.len(), 3);
            assert_eq!(bank_.slot(), bank.slot());
            assert!(poh_recorder.working_bank.is_none());
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_tick_sent_upto_and_including_max() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            poh_recorder.tick();
            poh_recorder.tick();
            poh_recorder.tick();
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.last().unwrap().1, 4);
            assert_eq!(poh_recorder.tick_height, 4);

            let working_bank = WorkingBank {
                bank,
                min_tick_height: 2,
                max_tick_height: 3,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.tick();

            assert_eq!(poh_recorder.tick_height, 5);
            assert!(poh_recorder.working_bank.is_none());
            let (_, e) = entry_receiver.recv().expect("recv 1");
            assert_eq!(e.len(), 3);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_record_to_early() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let working_bank = WorkingBank {
                bank: bank.clone(),
                min_tick_height: 2,
                max_tick_height: 3,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.tick();
            let tx = test_tx();
            let h1 = hash(b"hello world!");
            assert!(poh_recorder
                .record(bank.slot(), h1, vec![tx.clone()])
                .is_err());
            assert!(entry_receiver.try_recv().is_err());
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_record_bad_slot() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let working_bank = WorkingBank {
                bank: bank.clone(),
                min_tick_height: 1,
                max_tick_height: 2,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 1);
            assert_eq!(poh_recorder.tick_height, 1);
            let tx = test_tx();
            let h1 = hash(b"hello world!");
            assert_matches!(
                poh_recorder.record(bank.slot() + 1, h1, vec![tx.clone()]),
                Err(Error::PohRecorderError(PohRecorderError::MaxHeightReached))
            );
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_record_at_min_passes() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let working_bank = WorkingBank {
                bank: bank.clone(),
                min_tick_height: 1,
                max_tick_height: 2,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 1);
            assert_eq!(poh_recorder.tick_height, 1);
            let tx = test_tx();
            let h1 = hash(b"hello world!");
            assert!(poh_recorder
                .record(bank.slot(), h1, vec![tx.clone()])
                .is_ok());
            assert_eq!(poh_recorder.tick_cache.len(), 0);

            //tick in the cache + entry
            let (_b, e) = entry_receiver.recv().expect("recv 1");
            assert_eq!(e.len(), 1);
            assert!(e[0].0.is_tick());
            let (_b, e) = entry_receiver.recv().expect("recv 2");
            assert!(!e[0].0.is_tick());
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_record_at_max_fails() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let working_bank = WorkingBank {
                bank: bank.clone(),
                min_tick_height: 1,
                max_tick_height: 2,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.tick();
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_height, 2);
            let tx = test_tx();
            let h1 = hash(b"hello world!");
            assert!(poh_recorder
                .record(bank.slot(), h1, vec![tx.clone()])
                .is_err());

            let (_bank, e) = entry_receiver.recv().expect("recv 1");
            assert_eq!(e.len(), 2);
            assert!(e[0].0.is_tick());
            assert!(e[1].0.is_tick());
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_cache_on_disconnect() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let working_bank = WorkingBank {
                bank,
                min_tick_height: 2,
                max_tick_height: 3,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.tick();
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_height, 2);
            drop(entry_receiver);
            poh_recorder.tick();
            assert!(poh_recorder.working_bank.is_none());
            assert_eq!(poh_recorder.tick_cache.len(), 3);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_reset_current() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                Hash::default(),
                0,
                Some(4),
                DEFAULT_TICKS_PER_SLOT,
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::default()),
                &Arc::new(PohConfig::default()),
            );
            poh_recorder.tick();
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 2);
            let hash = poh_recorder.poh.lock().unwrap().hash;
            poh_recorder.reset(hash, 0, Some(4));
            assert_eq!(poh_recorder.tick_cache.len(), 0);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_reset_with_cached() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                Hash::default(),
                0,
                Some(4),
                DEFAULT_TICKS_PER_SLOT,
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::default()),
                &Arc::new(PohConfig::default()),
            );
            poh_recorder.tick();
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 2);
            poh_recorder.reset(poh_recorder.tick_cache[0].0.hash, 0, Some(4));
            assert_eq!(poh_recorder.tick_cache.len(), 0);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_reset_to_new_value() {
        solana_logger::setup();

        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                Hash::default(),
                0,
                Some(4),
                DEFAULT_TICKS_PER_SLOT,
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::default()),
                &Arc::new(PohConfig::default()),
            );
            poh_recorder.tick();
            poh_recorder.tick();
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_cache.len(), 3);
            assert_eq!(poh_recorder.tick_height, 3);
            poh_recorder.reset(hash(b"hello"), 0, Some(4)); // parent slot 0 implies tick_height of 3
            assert_eq!(poh_recorder.tick_cache.len(), 0);
            poh_recorder.tick();
            assert_eq!(poh_recorder.tick_height, 4);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_reset_clear_bank() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                Hash::default(),
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );
            let working_bank = WorkingBank {
                bank,
                min_tick_height: 2,
                max_tick_height: 3,
            };
            poh_recorder.set_working_bank(working_bank);
            poh_recorder.reset(hash(b"hello"), 0, Some(4));
            assert!(poh_recorder.working_bank.is_none());
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    pub fn test_clear_signal() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let (sender, receiver) = sync_channel(1);
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new_with_clear_signal(
                0,
                Hash::default(),
                0,
                None,
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                Some(sender),
                &Arc::new(LeaderScheduleCache::default()),
                &Arc::new(PohConfig::default()),
            );
            poh_recorder.set_bank(&bank);
            poh_recorder.clear_bank();
            assert!(receiver.try_recv().is_ok());
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_poh_recorder_reset_start_slot() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let ticks_per_slot = 5;
            let GenesisBlockInfo {
                mut genesis_block, ..
            } = create_genesis_block(2);
            genesis_block.ticks_per_slot = ticks_per_slot;
            let bank = Arc::new(Bank::new(&genesis_block));

            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                Some(4),
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            let end_slot = 3;
            let max_tick_height = (end_slot + 1) * ticks_per_slot - 1;
            let working_bank = WorkingBank {
                bank: bank.clone(),
                min_tick_height: 1,
                max_tick_height,
            };

            poh_recorder.set_working_bank(working_bank);
            for _ in 0..max_tick_height {
                poh_recorder.tick();
            }

            let tx = test_tx();
            let h1 = hash(b"hello world!");
            assert!(poh_recorder
                .record(bank.slot(), h1, vec![tx.clone()])
                .is_err());
            assert!(poh_recorder.working_bank.is_none());
            // Make sure the starting slot is updated
            assert_eq!(poh_recorder.start_slot, end_slot);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_reached_leader_tick() {
        solana_logger::setup();

        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                None,
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            // Test that with no leader slot, we don't reach the leader tick
            assert_eq!(poh_recorder.reached_leader_tick().0, false);

            poh_recorder.reset(bank.last_blockhash(), 0, None);

            // Test that with no leader slot in reset(), we don't reach the leader tick
            assert_eq!(poh_recorder.reached_leader_tick().0, false);

            // Provide a leader slot 1 slot down
            poh_recorder.reset(bank.last_blockhash(), 0, Some(2));

            let init_ticks = poh_recorder.tick_height();

            // Send one slot worth of ticks
            for _ in 0..bank.ticks_per_slot() {
                poh_recorder.tick();
            }

            // Tick should be recorded
            assert_eq!(
                poh_recorder.tick_height(),
                init_ticks + bank.ticks_per_slot()
            );

            // Test that we don't reach the leader tick because of grace ticks
            assert_eq!(poh_recorder.reached_leader_tick().0, false);

            // reset poh now. we should immediately be leader
            poh_recorder.reset(bank.last_blockhash(), 1, Some(2));
            assert_eq!(poh_recorder.reached_leader_tick().0, true);
            assert_eq!(poh_recorder.reached_leader_tick().1, 0);

            // Now test that with grace ticks we can reach leader ticks
            // Set the leader slot 1 slot down
            poh_recorder.reset(bank.last_blockhash(), 1, Some(3));

            // Send one slot worth of ticks ("skips" slot 2)
            for _ in 0..bank.ticks_per_slot() {
                poh_recorder.tick();
            }

            // We are not the leader yet, as expected
            assert_eq!(poh_recorder.reached_leader_tick().0, false);

            // Send 1 less tick than the grace ticks
            for _ in 0..bank.ticks_per_slot() / GRACE_TICKS_FACTOR {
                poh_recorder.tick();
            }

            // We should be the leader now
            assert_eq!(poh_recorder.reached_leader_tick().0, true);
            assert_eq!(
                poh_recorder.reached_leader_tick().1,
                bank.ticks_per_slot() / GRACE_TICKS_FACTOR
            );

            // Let's test that correct grace ticks are reported
            // Set the leader slot 1 slot down
            poh_recorder.reset(bank.last_blockhash(), 2, Some(4));

            // send ticks for a slot
            for _ in 0..bank.ticks_per_slot() {
                poh_recorder.tick();
            }

            // We are not the leader yet, as expected
            assert_eq!(poh_recorder.reached_leader_tick().0, false);
            poh_recorder.reset(bank.last_blockhash(), 3, Some(4));

            // without sending more ticks, we should be leader now
            assert_eq!(poh_recorder.reached_leader_tick().0, true);
            assert_eq!(poh_recorder.reached_leader_tick().1, 0);

            // Let's test that if a node overshoots the ticks for its target
            // leader slot, reached_leader_tick() will return true, because it's overdue
            // Set the leader slot 1 slot down
            poh_recorder.reset(bank.last_blockhash(), 4, Some(5));

            // Send remaining ticks for the slot (remember we sent extra ticks in the previous part of the test)
            for _ in 0..4 * bank.ticks_per_slot() {
                poh_recorder.tick();
            }

            // We are overdue to lead
            assert_eq!(poh_recorder.reached_leader_tick().0, true);
        }
        Blocktree::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_would_be_leader_soon() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let blocktree =
                Blocktree::open(&ledger_path).expect("Expected to be able to open database ledger");
            let GenesisBlockInfo { genesis_block, .. } = create_genesis_block(2);
            let bank = Arc::new(Bank::new(&genesis_block));
            let prev_hash = bank.last_blockhash();
            let (mut poh_recorder, _entry_receiver) = PohRecorder::new(
                0,
                prev_hash,
                0,
                None,
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blocktree),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            // Test that with no leader slot, we don't reach the leader tick
            assert_eq!(
                poh_recorder.would_be_leader(2 * bank.ticks_per_slot()),
                false
            );

            poh_recorder.reset(bank.last_blockhash(), 0, None);

            assert_eq!(
                poh_recorder.would_be_leader(2 * bank.ticks_per_slot()),
                false
            );

            // We reset with leader slot after 3 slots
            poh_recorder.reset(bank.last_blockhash(), 0, Some(bank.slot() + 3));

            // Test that the node won't be leader in next 2 slots
            assert_eq!(
                poh_recorder.would_be_leader(2 * bank.ticks_per_slot()),
                false
            );

            // Test that the node will be leader in next 3 slots
            assert_eq!(
                poh_recorder.would_be_leader(3 * bank.ticks_per_slot()),
                true
            );

            assert_eq!(
                poh_recorder.would_be_leader(2 * bank.ticks_per_slot()),
                false
            );

            // If we set the working bank, the node should be leader within next 2 slots
            poh_recorder.set_bank(&bank);
            assert_eq!(
                poh_recorder.would_be_leader(2 * bank.ticks_per_slot()),
                true
            );
        }
    }
}
