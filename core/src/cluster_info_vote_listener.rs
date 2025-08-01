use {
    crate::{
        banking_trace::BankingPacketSender,
        consensus::vote_stake_tracker::VoteStakeTracker,
        optimistic_confirmation_verifier::OptimisticConfirmationVerifier,
        replay_stage::DUPLICATE_THRESHOLD,
        result::{Error, Result},
        sigverify,
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Select, Sender},
    log::*,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_gossip::{
        cluster_info::{ClusterInfo, GOSSIP_SLEEP_MILLIS},
        crds::Cursor,
    },
    solana_hash::Hash,
    solana_ledger::blockstore::Blockstore,
    solana_measure::measure::Measure,
    solana_metrics::inc_new_counter_debug,
    solana_perf::packet::{self, PacketBatch},
    solana_pubkey::Pubkey,
    solana_rpc::{
        optimistically_confirmed_bank_tracker::{BankNotification, BankNotificationSenderConfig},
        rpc_subscriptions::RpcSubscriptions,
    },
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        bank_hash_cache::{BankHashCache, DumpedSlotSubscription},
        commitment::VOTE_THRESHOLD_SIZE,
        epoch_stakes::VersionedEpochStakes,
        root_bank_cache::RootBankCache,
        vote_sender_types::ReplayVoteReceiver,
    },
    solana_signature::Signature,
    solana_time_utils::AtomicInterval,
    solana_transaction::Transaction,
    solana_vote::{
        vote_parser::{self, ParsedVote},
        vote_transaction::VoteTransaction,
    },
    std::{
        cmp::max,
        collections::HashMap,
        iter::repeat,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex, RwLock,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

// Map from a vote account to the authorized voter for an epoch
pub type ThresholdConfirmedSlots = Vec<(Slot, Hash)>;
pub type VerifiedVoteTransactionsSender = Sender<Vec<Transaction>>;
pub type VerifiedVoteTransactionsReceiver = Receiver<Vec<Transaction>>;
pub type VerifiedVoteSender = Sender<(Pubkey, Vec<Slot>)>;
pub type VerifiedVoteReceiver = Receiver<(Pubkey, Vec<Slot>)>;
pub type GossipVerifiedVoteHashSender = Sender<(Pubkey, Slot, Hash)>;
pub type GossipVerifiedVoteHashReceiver = Receiver<(Pubkey, Slot, Hash)>;
pub type DuplicateConfirmedSlotsSender = Sender<ThresholdConfirmedSlots>;
pub type DuplicateConfirmedSlotsReceiver = Receiver<ThresholdConfirmedSlots>;

const THRESHOLDS_TO_CHECK: [f64; 2] = [DUPLICATE_THRESHOLD, VOTE_THRESHOLD_SIZE];

#[derive(Default)]
pub struct SlotVoteTracker {
    // Maps pubkeys that have voted for this slot
    // to whether or not we've seen the vote on gossip.
    // True if seen on gossip, false if only seen in replay.
    voted: HashMap<Pubkey, bool>,
    optimistic_votes_tracker: HashMap<Hash, VoteStakeTracker>,
    voted_slot_updates: Option<Vec<Pubkey>>,
    gossip_only_stake: u64,
}

impl SlotVoteTracker {
    pub(crate) fn get_voted_slot_updates(&mut self) -> Option<Vec<Pubkey>> {
        self.voted_slot_updates.take()
    }

    fn get_or_insert_optimistic_votes_tracker(&mut self, hash: Hash) -> &mut VoteStakeTracker {
        self.optimistic_votes_tracker.entry(hash).or_default()
    }
    pub(crate) fn optimistic_votes_tracker(&self, hash: &Hash) -> Option<&VoteStakeTracker> {
        self.optimistic_votes_tracker.get(hash)
    }
}

#[derive(Default)]
pub struct VoteTracker {
    // Map from a slot to a set of validators who have voted for that slot
    slot_vote_trackers: RwLock<HashMap<Slot, Arc<RwLock<SlotVoteTracker>>>>,
}

impl VoteTracker {
    fn get_or_insert_slot_tracker(&self, slot: Slot) -> Arc<RwLock<SlotVoteTracker>> {
        if let Some(slot_vote_tracker) = self.slot_vote_trackers.read().unwrap().get(&slot) {
            return slot_vote_tracker.clone();
        }
        let mut slot_vote_trackers = self.slot_vote_trackers.write().unwrap();
        slot_vote_trackers.entry(slot).or_default().clone()
    }

    pub(crate) fn get_slot_vote_tracker(&self, slot: Slot) -> Option<Arc<RwLock<SlotVoteTracker>>> {
        self.slot_vote_trackers.read().unwrap().get(&slot).cloned()
    }

    #[cfg(test)]
    pub(crate) fn insert_vote(&self, slot: Slot, pubkey: Pubkey) {
        let mut w_slot_vote_trackers = self.slot_vote_trackers.write().unwrap();

        let slot_vote_tracker = w_slot_vote_trackers.entry(slot).or_default();

        let mut w_slot_vote_tracker = slot_vote_tracker.write().unwrap();

        w_slot_vote_tracker.voted.insert(pubkey, true);
        if let Some(ref mut voted_slot_updates) = w_slot_vote_tracker.voted_slot_updates {
            voted_slot_updates.push(pubkey)
        } else {
            w_slot_vote_tracker.voted_slot_updates = Some(vec![pubkey]);
        }
    }

    fn purge_stale_state(&self, root_bank: &Bank) {
        // Purge any outdated slot data
        let new_root = root_bank.slot();
        self.slot_vote_trackers
            .write()
            .unwrap()
            .retain(|slot, _| *slot >= new_root);
    }

    fn progress_with_new_root_bank(&self, root_bank: &Bank) {
        self.purge_stale_state(root_bank);
    }
}

#[derive(Default)]
struct VoteProcessingTiming {
    gossip_txn_processing_time_us: u64,
    gossip_slot_confirming_time_us: u64,
    last_report: AtomicInterval,
}

const VOTE_PROCESSING_REPORT_INTERVAL_MS: u64 = 1_000;

impl VoteProcessingTiming {
    fn reset(&mut self) {
        self.gossip_txn_processing_time_us = 0;
        self.gossip_slot_confirming_time_us = 0;
    }

    fn update(&mut self, vote_txn_processing_time_us: u64, vote_slot_confirming_time_us: u64) {
        self.gossip_txn_processing_time_us += vote_txn_processing_time_us;
        self.gossip_slot_confirming_time_us += vote_slot_confirming_time_us;

        if self
            .last_report
            .should_update(VOTE_PROCESSING_REPORT_INTERVAL_MS)
        {
            datapoint_info!(
                "vote-processing-timing",
                (
                    "vote_txn_processing_us",
                    self.gossip_txn_processing_time_us as i64,
                    i64
                ),
                (
                    "slot_confirming_time_us",
                    self.gossip_slot_confirming_time_us as i64,
                    i64
                ),
            );
            self.reset();
        }
    }
}

pub struct ClusterInfoVoteListener {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl ClusterInfoVoteListener {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        exit: Arc<AtomicBool>,
        cluster_info: Arc<ClusterInfo>,
        verified_packets_sender: BankingPacketSender,
        vote_tracker: Arc<VoteTracker>,
        bank_forks: Arc<RwLock<BankForks>>,
        subscriptions: Option<Arc<RpcSubscriptions>>,
        verified_vote_sender: VerifiedVoteSender,
        gossip_verified_vote_hash_sender: GossipVerifiedVoteHashSender,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        bank_notification_sender: Option<BankNotificationSenderConfig>,
        duplicate_confirmed_slot_sender: DuplicateConfirmedSlotsSender,
    ) -> Self {
        let (verified_vote_transactions_sender, verified_vote_transactions_receiver) = unbounded();
        let listen_thread = {
            let exit = exit.clone();
            let mut root_bank_cache = RootBankCache::new(bank_forks.clone());
            Builder::new()
                .name("solCiVoteLstnr".to_string())
                .spawn(move || {
                    let _ = Self::recv_loop(
                        exit,
                        &cluster_info,
                        &mut root_bank_cache,
                        verified_packets_sender,
                        verified_vote_transactions_sender,
                    );
                })
                .unwrap()
        };

        let process_thread = Builder::new()
            .name("solCiProcVotes".to_string())
            .spawn(move || {
                let mut bank_hash_cache = BankHashCache::new(bank_forks);
                let dumped_slot_subscription = bank_hash_cache.dumped_slot_subscription();
                let _ = Self::process_votes_loop(
                    exit,
                    verified_vote_transactions_receiver,
                    vote_tracker,
                    &mut bank_hash_cache,
                    dumped_slot_subscription,
                    subscriptions.as_deref(),
                    gossip_verified_vote_hash_sender,
                    verified_vote_sender,
                    replay_votes_receiver,
                    blockstore,
                    bank_notification_sender,
                    duplicate_confirmed_slot_sender,
                );
            })
            .unwrap();

        Self {
            thread_hdls: vec![listen_thread, process_thread],
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        self.thread_hdls.into_iter().try_for_each(JoinHandle::join)
    }

    fn recv_loop(
        exit: Arc<AtomicBool>,
        cluster_info: &ClusterInfo,
        root_bank_cache: &mut RootBankCache,
        verified_packets_sender: BankingPacketSender,
        verified_vote_transactions_sender: VerifiedVoteTransactionsSender,
    ) -> Result<()> {
        let mut cursor = Cursor::default();
        while !exit.load(Ordering::Relaxed) {
            let votes = cluster_info.get_votes(&mut cursor);
            inc_new_counter_debug!("cluster_info_vote_listener-recv_count", votes.len());
            if !votes.is_empty() {
                let (vote_txs, packets) = Self::verify_votes(votes, root_bank_cache);
                verified_vote_transactions_sender.send(vote_txs)?;
                verified_packets_sender.send(BankingPacketBatch::new(packets))?;
            }
            sleep(Duration::from_millis(GOSSIP_SLEEP_MILLIS));
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn verify_votes(
        votes: Vec<Transaction>,
        root_bank_cache: &mut RootBankCache,
    ) -> (Vec<Transaction>, Vec<PacketBatch>) {
        let mut packet_batches = packet::to_packet_batches(&votes, 1);

        // Votes should already be filtered by this point.
        sigverify::ed25519_verify_cpu(
            &mut packet_batches,
            /*reject_non_vote=*/ false,
            votes.len(),
        );
        let root_bank = root_bank_cache.root_bank();
        let epoch_schedule = root_bank.epoch_schedule();
        votes
            .into_iter()
            .zip(packet_batches)
            .filter(|(_, packet_batch)| {
                // to_packet_batches() above splits into 1 packet long batches
                assert_eq!(packet_batch.len(), 1);
                !packet_batch.get(0).unwrap().meta().discard()
            })
            .filter_map(|(tx, packet_batch)| {
                let (vote_account_key, vote, ..) = vote_parser::parse_vote_transaction(&tx)?;
                let slot = vote.last_voted_slot()?;
                let epoch = epoch_schedule.get_epoch(slot);
                let authorized_voter = root_bank
                    .epoch_stakes(epoch)?
                    .epoch_authorized_voters()
                    .get(&vote_account_key)?;
                let mut keys = tx.message.account_keys.iter().enumerate();
                if !keys.any(|(i, key)| tx.message.is_signer(i) && key == authorized_voter) {
                    return None;
                }
                Some((tx, packet_batch))
            })
            .unzip()
    }

    #[allow(clippy::too_many_arguments)]
    fn process_votes_loop(
        exit: Arc<AtomicBool>,
        gossip_vote_txs_receiver: VerifiedVoteTransactionsReceiver,
        vote_tracker: Arc<VoteTracker>,
        bank_hash_cache: &mut BankHashCache,
        dumped_slot_subscription: DumpedSlotSubscription,
        subscriptions: Option<&RpcSubscriptions>,
        gossip_verified_vote_hash_sender: GossipVerifiedVoteHashSender,
        verified_vote_sender: VerifiedVoteSender,
        replay_votes_receiver: ReplayVoteReceiver,
        blockstore: Arc<Blockstore>,
        bank_notification_sender: Option<BankNotificationSenderConfig>,
        duplicate_confirmed_slot_sender: DuplicateConfirmedSlotsSender,
    ) -> Result<()> {
        let mut confirmation_verifier = OptimisticConfirmationVerifier::new(bank_hash_cache.root());
        let mut latest_vote_slot_per_validator = HashMap::new();
        let mut last_process_root = Instant::now();
        let duplicate_confirmed_slot_sender = Some(duplicate_confirmed_slot_sender);
        let mut vote_processing_time = Some(VoteProcessingTiming::default());
        loop {
            if exit.load(Ordering::Relaxed) {
                return Ok(());
            }

            let root_bank = bank_hash_cache.get_root_bank_and_prune_cache();
            if last_process_root.elapsed().as_millis() > DEFAULT_MS_PER_SLOT as u128 {
                let unrooted_optimistic_slots = confirmation_verifier
                    .verify_for_unrooted_optimistic_slots(&root_bank, &blockstore);
                // SlotVoteTracker's for all `slots` in `unrooted_optimistic_slots`
                // should still be available because we haven't purged in
                // `progress_with_new_root_bank()` yet, which is called below
                OptimisticConfirmationVerifier::log_unrooted_optimistic_slots(
                    &root_bank,
                    &vote_tracker,
                    &unrooted_optimistic_slots,
                );
                vote_tracker.progress_with_new_root_bank(&root_bank);
                last_process_root = Instant::now();
            }
            let confirmed_slots = Self::listen_and_confirm_votes(
                &gossip_vote_txs_receiver,
                &vote_tracker,
                &root_bank,
                subscriptions,
                &gossip_verified_vote_hash_sender,
                &verified_vote_sender,
                &replay_votes_receiver,
                &bank_notification_sender,
                &duplicate_confirmed_slot_sender,
                &mut vote_processing_time,
                &mut latest_vote_slot_per_validator,
                bank_hash_cache,
                &dumped_slot_subscription,
            );
            match confirmed_slots {
                Ok(confirmed_slots) => {
                    confirmation_verifier
                        .add_new_optimistic_confirmed_slots(confirmed_slots.clone(), &blockstore);
                }
                Err(e) => match e {
                    Error::RecvTimeout(RecvTimeoutError::Disconnected) => {
                        return Ok(());
                    }
                    Error::ReadyTimeout => (),
                    _ => {
                        error!("thread {:?} error {:?}", thread::current().name(), e);
                    }
                },
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn listen_and_confirm_votes(
        gossip_vote_txs_receiver: &VerifiedVoteTransactionsReceiver,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        subscriptions: Option<&RpcSubscriptions>,
        gossip_verified_vote_hash_sender: &GossipVerifiedVoteHashSender,
        verified_vote_sender: &VerifiedVoteSender,
        replay_votes_receiver: &ReplayVoteReceiver,
        bank_notification_sender: &Option<BankNotificationSenderConfig>,
        duplicate_confirmed_slot_sender: &Option<DuplicateConfirmedSlotsSender>,
        vote_processing_time: &mut Option<VoteProcessingTiming>,
        latest_vote_slot_per_validator: &mut HashMap<Pubkey, Slot>,
        bank_hash_cache: &mut BankHashCache,
        dumped_slot_subscription: &Mutex<bool>,
    ) -> Result<ThresholdConfirmedSlots> {
        let mut sel = Select::new();
        sel.recv(gossip_vote_txs_receiver);
        sel.recv(replay_votes_receiver);
        let mut remaining_wait_time = Duration::from_millis(200);
        while remaining_wait_time > Duration::ZERO {
            let start = Instant::now();
            // Wait for one of the receivers to be ready. `ready_timeout`
            // will return if channels either have something, or are
            // disconnected. `ready_timeout` can wake up spuriously,
            // hence the loop
            let _ = sel.ready_timeout(remaining_wait_time)?;

            // Should not early return from this point onwards until `process_votes()`
            // returns below to avoid missing any potential `optimistic_confirmed_slots`
            let gossip_vote_txs: Vec<_> = gossip_vote_txs_receiver.try_iter().flatten().collect();
            let replay_votes: Vec<_> = replay_votes_receiver.try_iter().collect();
            if !gossip_vote_txs.is_empty() || !replay_votes.is_empty() {
                return Ok(Self::filter_and_confirm_with_new_votes(
                    vote_tracker,
                    gossip_vote_txs,
                    replay_votes,
                    root_bank,
                    subscriptions,
                    gossip_verified_vote_hash_sender,
                    verified_vote_sender,
                    bank_notification_sender,
                    duplicate_confirmed_slot_sender,
                    vote_processing_time,
                    latest_vote_slot_per_validator,
                    bank_hash_cache,
                    dumped_slot_subscription,
                ));
            }
            remaining_wait_time = remaining_wait_time.saturating_sub(start.elapsed());
        }
        Ok(vec![])
    }

    #[allow(clippy::too_many_arguments)]
    fn track_new_votes_and_notify_confirmations(
        vote: VoteTransaction,
        vote_pubkey: &Pubkey,
        vote_transaction_signature: Signature,
        vote_tracker: &VoteTracker,
        root_bank: &Bank,
        rpc_subscriptions: Option<&RpcSubscriptions>,
        verified_vote_sender: &VerifiedVoteSender,
        gossip_verified_vote_hash_sender: &GossipVerifiedVoteHashSender,
        diff: &mut HashMap<Slot, HashMap<Pubkey, bool>>,
        new_optimistic_confirmed_slots: &mut ThresholdConfirmedSlots,
        is_gossip_vote: bool,
        bank_notification_sender: &Option<BankNotificationSenderConfig>,
        duplicate_confirmed_slot_sender: &Option<DuplicateConfirmedSlotsSender>,
        latest_vote_slot_per_validator: &mut HashMap<Pubkey, Slot>,
        bank_hash_cache: &mut BankHashCache,
        dumped_slot_subscription: &Mutex<bool>,
    ) {
        if vote.is_empty() {
            return;
        }

        // Hold lock for whole function to ensure hash consistency with bank_forks
        let mut slots_dumped = dumped_slot_subscription.lock().unwrap();
        let (last_vote_slot, last_vote_hash) = vote.last_voted_slot_hash().unwrap();

        let latest_vote_slot = latest_vote_slot_per_validator
            .entry(*vote_pubkey)
            .or_insert(0);

        let root = root_bank.slot();
        let mut is_new_vote = false;
        let vote_slots = vote.slots();

        let accumulate_intermediate_votes =
            if let Some(hash) = bank_hash_cache.hash(last_vote_slot, &mut slots_dumped) {
                // Only accumulate intermediates if we have replayed the same version being voted on, as
                // otherwise we cannot verify the ancestry or the hashes.
                // Note: this can only be performed on full tower votes, until deprecate_legacy_vote_ixs feature
                // is active we must check the transaction type.
                hash == last_vote_hash && vote.is_full_tower_vote()
            } else {
                // If we have not frozen the bank do not accumulate intermediate slots as we cannot verify
                // the hashes
                false
            };
        let mut get_hash = |slot: Slot| {
            (slot == last_vote_slot)
                .then_some(last_vote_hash)
                .or(bank_hash_cache.hash(slot, &mut slots_dumped))
        };

        // If slot is before the root, ignore it. Iterates from most recent vote slot to oldest.
        for slot in vote_slots.iter().filter(|slot| **slot > root).rev() {
            let slot = *slot;

            // if we don't have stake information, ignore it
            let epoch = root_bank.epoch_schedule().get_epoch(slot);
            let epoch_stakes = root_bank.epoch_stakes(epoch);
            if epoch_stakes.is_none() {
                continue;
            }
            let epoch_stakes = epoch_stakes.unwrap();

            // We always track the last vote slot for optimistic confirmation. If we have replayed
            // the same version of last vote slot that is being voted on, then we also track the
            // other votes in the proposed tower.
            if slot == last_vote_slot || accumulate_intermediate_votes {
                let vote_accounts = epoch_stakes.stakes().vote_accounts();
                let stake = vote_accounts.get_delegated_stake(vote_pubkey);
                let total_stake = epoch_stakes.total_stake();
                let Some(hash) = get_hash(slot) else {
                    // In this case the supposed ancestor of this vote is missing. This can happen
                    // if the ancestor has been pruned, or if this is a malformed vote. In either case
                    // we do not track this slot for optimistic confirmation.
                    continue;
                };

                // Fast track processing of the last slot in a vote transactions
                // so that notifications for optimistic confirmation can be sent
                // as soon as possible.
                let (reached_threshold_results, is_new) = Self::track_optimistic_confirmation_vote(
                    vote_tracker,
                    slot,
                    hash,
                    *vote_pubkey,
                    stake,
                    total_stake,
                );

                if is_gossip_vote && is_new && stake > 0 {
                    let _ = gossip_verified_vote_hash_sender.send((*vote_pubkey, slot, hash));
                }

                if reached_threshold_results[0] {
                    if let Some(sender) = duplicate_confirmed_slot_sender {
                        let _ = sender.send(vec![(slot, hash)]);
                    }
                }
                if reached_threshold_results[1] {
                    new_optimistic_confirmed_slots.push((slot, hash));
                    // Notify subscribers about new optimistic confirmation
                    if let Some(sender) = bank_notification_sender {
                        let dependency_work = sender
                            .dependency_tracker
                            .as_ref()
                            .map(|s| s.get_current_declared_work());
                        sender
                            .sender
                            .send((
                                BankNotification::OptimisticallyConfirmed(slot),
                                dependency_work,
                            ))
                            .unwrap_or_else(|err| {
                                warn!("bank_notification_sender failed: {err:?}")
                            });
                    }
                }

                if !is_new && !is_gossip_vote {
                    // By now:
                    // 1) The vote must have come from ReplayStage,
                    // 2) We've seen this vote from replay for this hash before
                    // (`track_optimistic_confirmation_vote()` will not set `is_new == true`
                    // for same slot different hash), so short circuit because this vote
                    // has no new information

                    // Note gossip votes will always be processed because those should be unique
                    // and we need to update the gossip-only stake in the `VoteTracker`.
                    break;
                }

                is_new_vote = is_new;
            }

            if slot < *latest_vote_slot {
                // Important that we filter after the `last_vote_slot` check, as even if this vote
                // is old, we still need to track optimistic confirmations.
                // However it is fine to filter the rest of the slots for the propagated check tracking below,
                // as the propagated check is able to roll up votes for descendants unlike optimistic confirmation.
                continue;
            }

            diff.entry(slot)
                .or_default()
                .entry(*vote_pubkey)
                .and_modify(|seen_in_gossip_previously| {
                    *seen_in_gossip_previously = *seen_in_gossip_previously || is_gossip_vote
                })
                .or_insert(is_gossip_vote);
        }

        *latest_vote_slot = max(*latest_vote_slot, last_vote_slot);

        if is_new_vote {
            if let Some(rpc_subscriptions) = rpc_subscriptions {
                rpc_subscriptions.notify_vote(*vote_pubkey, vote, vote_transaction_signature);
            }
            let _ = verified_vote_sender.send((*vote_pubkey, vote_slots));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn filter_and_confirm_with_new_votes(
        vote_tracker: &VoteTracker,
        gossip_vote_txs: Vec<Transaction>,
        replayed_votes: Vec<ParsedVote>,
        root_bank: &Bank,
        subscriptions: Option<&RpcSubscriptions>,
        gossip_verified_vote_hash_sender: &GossipVerifiedVoteHashSender,
        verified_vote_sender: &VerifiedVoteSender,
        bank_notification_sender: &Option<BankNotificationSenderConfig>,
        duplicate_confirmed_slot_sender: &Option<DuplicateConfirmedSlotsSender>,
        vote_processing_time: &mut Option<VoteProcessingTiming>,
        latest_vote_slot_per_validator: &mut HashMap<Pubkey, Slot>,
        bank_hash_cache: &mut BankHashCache,
        dumped_slot_subscription: &Mutex<bool>,
    ) -> ThresholdConfirmedSlots {
        let mut diff: HashMap<Slot, HashMap<Pubkey, bool>> = HashMap::new();
        let mut new_optimistic_confirmed_slots = vec![];

        // Process votes from gossip and ReplayStage
        let mut gossip_vote_txn_processing_time = Measure::start("gossip_vote_processing_time");
        let votes = gossip_vote_txs
            .iter()
            .filter_map(vote_parser::parse_vote_transaction)
            .zip(repeat(/*is_gossip:*/ true))
            .chain(replayed_votes.into_iter().zip(repeat(/*is_gossip:*/ false)));
        for ((vote_pubkey, vote, _switch_proof, signature), is_gossip) in votes {
            Self::track_new_votes_and_notify_confirmations(
                vote,
                &vote_pubkey,
                signature,
                vote_tracker,
                root_bank,
                subscriptions,
                verified_vote_sender,
                gossip_verified_vote_hash_sender,
                &mut diff,
                &mut new_optimistic_confirmed_slots,
                is_gossip,
                bank_notification_sender,
                duplicate_confirmed_slot_sender,
                latest_vote_slot_per_validator,
                bank_hash_cache,
                dumped_slot_subscription,
            );
        }
        gossip_vote_txn_processing_time.stop();
        let gossip_vote_txn_processing_time_us = gossip_vote_txn_processing_time.as_us();

        // Process all the slots accumulated from replay and gossip.
        let mut gossip_vote_slot_confirming_time = Measure::start("gossip_vote_slot_confirm_time");
        for (slot, mut slot_diff) in diff {
            let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
            {
                let r_slot_tracker = slot_tracker.read().unwrap();
                // Only keep the pubkeys we haven't seen voting for this slot
                slot_diff.retain(|pubkey, seen_in_gossip_above| {
                    let seen_in_gossip_previously = r_slot_tracker.voted.get(pubkey);
                    let is_new = seen_in_gossip_previously.is_none();
                    // `is_new_from_gossip` means we observed a vote for this slot
                    // for the first time in gossip
                    let is_new_from_gossip = !seen_in_gossip_previously.cloned().unwrap_or(false)
                        && *seen_in_gossip_above;
                    is_new || is_new_from_gossip
                });
            }
            let mut w_slot_tracker = slot_tracker.write().unwrap();
            if w_slot_tracker.voted_slot_updates.is_none() {
                w_slot_tracker.voted_slot_updates = Some(vec![]);
            }
            let mut gossip_only_stake = 0;
            let epoch = root_bank.epoch_schedule().get_epoch(slot);
            let epoch_stakes = root_bank.epoch_stakes(epoch);

            for (pubkey, seen_in_gossip_above) in slot_diff {
                if seen_in_gossip_above {
                    // By this point we know if the vote was seen in gossip above,
                    // it was not seen in gossip at any point in the past (if it was seen
                    // in gossip in the past, `is_new` would be false and it would have
                    // been filtered out above), so it's safe to increment the gossip-only
                    // stake
                    Self::sum_stake(&mut gossip_only_stake, epoch_stakes, &pubkey);
                }

                // From the `slot_diff.retain` earlier, we know because there are
                // no other writers to `slot_vote_tracker` that
                // `is_new || is_new_from_gossip`. In both cases we want to record
                // `is_new_from_gossip` for the `pubkey` entry.
                w_slot_tracker.voted.insert(pubkey, seen_in_gossip_above);
                w_slot_tracker
                    .voted_slot_updates
                    .as_mut()
                    .unwrap()
                    .push(pubkey);
            }

            w_slot_tracker.gossip_only_stake += gossip_only_stake
        }
        gossip_vote_slot_confirming_time.stop();
        let gossip_vote_slot_confirming_time_us = gossip_vote_slot_confirming_time.as_us();

        if let Some(ref mut vote_processing_time) = vote_processing_time {
            vote_processing_time.update(
                gossip_vote_txn_processing_time_us,
                gossip_vote_slot_confirming_time_us,
            )
        }
        new_optimistic_confirmed_slots
    }

    // Returns if the slot was optimistically confirmed, and whether
    // the slot was new
    fn track_optimistic_confirmation_vote(
        vote_tracker: &VoteTracker,
        slot: Slot,
        hash: Hash,
        pubkey: Pubkey,
        stake: u64,
        total_epoch_stake: u64,
    ) -> (Vec<bool>, bool) {
        let slot_tracker = vote_tracker.get_or_insert_slot_tracker(slot);
        // Insert vote and check for optimistic confirmation
        let mut w_slot_tracker = slot_tracker.write().unwrap();

        w_slot_tracker
            .get_or_insert_optimistic_votes_tracker(hash)
            .add_vote_pubkey(pubkey, stake, total_epoch_stake, &THRESHOLDS_TO_CHECK)
    }

    fn sum_stake(sum: &mut u64, epoch_stakes: Option<&VersionedEpochStakes>, pubkey: &Pubkey) {
        if let Some(stakes) = epoch_stakes {
            *sum += stakes.stakes().vote_accounts().get_delegated_stake(pubkey)
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_perf::packet,
        solana_pubkey::Pubkey,
        solana_rpc::optimistically_confirmed_bank_tracker::OptimisticallyConfirmedBank,
        solana_runtime::{
            bank::Bank,
            commitment::BlockCommitmentCache,
            genesis_utils::{
                self, create_genesis_config, GenesisConfigInfo, ValidatorVoteKeypairs,
            },
            vote_sender_types::ReplayVoteSender,
        },
        solana_signature::Signature,
        solana_signer::Signer,
        solana_vote::vote_transaction,
        solana_vote_program::vote_state::{TowerSync, Vote, MAX_LOCKOUT_HISTORY},
        std::{
            collections::BTreeSet,
            iter::repeat_with,
            sync::{atomic::AtomicU64, Arc},
        },
    };

    #[test]
    fn test_max_vote_tx_fits() {
        solana_logger::setup();
        let node_keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let tower_sync = TowerSync::new_from_slot(MAX_LOCKOUT_HISTORY as u64, Hash::default());
        let vote_tx = vote_transaction::new_tower_sync_transaction(
            tower_sync,
            Hash::default(),
            &node_keypair,
            &vote_keypair,
            &vote_keypair,
            Some(Hash::default()),
        );

        use bincode::serialized_size;
        info!("max vote size {}", serialized_size(&vote_tx).unwrap());

        let packet_batches = packet::to_packet_batches(&[vote_tx], 1); // panics if won't fit

        assert_eq!(packet_batches.len(), 1);
    }

    #[test]
    fn test_update_new_root() {
        let SetupComponents {
            vote_tracker, bank, ..
        } = setup();

        // Check outdated slots are purged with new root
        let new_voter = solana_pubkey::new_rand();
        // Make separate copy so the original doesn't count toward
        // the ref count, which would prevent cleanup
        let new_voter_ = new_voter;
        vote_tracker.insert_vote(bank.slot(), new_voter_);
        assert!(vote_tracker
            .slot_vote_trackers
            .read()
            .unwrap()
            .contains_key(&bank.slot()));
        let bank1 = Bank::new_from_parent(bank.clone(), &Pubkey::default(), bank.slot() + 1);
        vote_tracker.progress_with_new_root_bank(&bank1);
        assert!(!vote_tracker
            .slot_vote_trackers
            .read()
            .unwrap()
            .contains_key(&bank.slot()));

        // Check `keys` and `epoch_authorized_voters` are purged when new
        // root bank moves to the next epoch
        let current_epoch = bank.epoch();
        let new_epoch_slot = bank
            .epoch_schedule()
            .get_first_slot_in_epoch(current_epoch + 1);
        let new_epoch_bank = Bank::new_from_parent(bank, &Pubkey::default(), new_epoch_slot);
        vote_tracker.progress_with_new_root_bank(&new_epoch_bank);
    }

    #[test]
    fn test_update_new_leader_schedule_epoch() {
        let SetupComponents { bank, .. } = setup();

        // Check outdated slots are purged with new root
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let next_leader_schedule_epoch = leader_schedule_epoch + 1;
        let mut next_leader_schedule_computed = bank.slot();
        loop {
            next_leader_schedule_computed += 1;
            if bank.get_leader_schedule_epoch(next_leader_schedule_computed)
                == next_leader_schedule_epoch
            {
                break;
            }
        }
        assert_eq!(
            bank.get_leader_schedule_epoch(next_leader_schedule_computed),
            next_leader_schedule_epoch
        );
    }

    #[test]
    fn test_votes_in_range() {
        // Create some voters at genesis
        let stake_per_validator = 100;
        let SetupComponents {
            vote_tracker,
            validator_voting_keypairs,
            subscriptions,
            bank_forks,
            ..
        } = setup();
        let (votes_sender, votes_receiver) = unbounded();
        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();
        let mut latest_vote_slot_per_validator = HashMap::new();
        let mut bank_hash_cache = BankHashCache::new(bank_forks);

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );

        let bank0 = Bank::new_for_tests(&genesis_config);
        // Votes for slots less than the provided root bank's slot should not be processed
        let bank3 = Arc::new(Bank::new_from_parent(
            Arc::new(bank0),
            &Pubkey::default(),
            3,
        ));
        let vote_slots = vec![1, 2];
        send_vote_txs(
            vote_slots,
            vec![],
            &validator_voting_keypairs,
            None,
            &votes_sender,
            &replay_votes_sender,
        );
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_receiver,
            &vote_tracker,
            &bank3,
            Some(&subscriptions),
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
            &mut None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        )
        .unwrap();

        // Vote slots for slots greater than root bank's set of currently calculated epochs
        // are ignored
        let max_epoch = bank3.get_leader_schedule_epoch(bank3.slot());
        assert!(bank3.epoch_stakes(max_epoch).is_some());
        let unknown_epoch = max_epoch + 1;
        assert!(bank3.epoch_stakes(unknown_epoch).is_none());
        let first_slot_in_unknown_epoch = bank3
            .epoch_schedule()
            .get_first_slot_in_epoch(unknown_epoch);
        let vote_slots = vec![first_slot_in_unknown_epoch, first_slot_in_unknown_epoch + 1];
        send_vote_txs(
            vote_slots,
            vec![],
            &validator_voting_keypairs,
            None,
            &votes_sender,
            &replay_votes_sender,
        );
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_receiver,
            &vote_tracker,
            &bank3,
            Some(&subscriptions),
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
            &mut None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        )
        .unwrap();

        // Should be no updates since everything was ignored
        assert!(vote_tracker.slot_vote_trackers.read().unwrap().is_empty());
    }

    fn send_vote_txs(
        gossip_vote_slots: Vec<Slot>,
        replay_vote_slots: Vec<Slot>,
        validator_voting_keypairs: &[ValidatorVoteKeypairs],
        switch_proof_hash: Option<Hash>,
        votes_sender: &VerifiedVoteTransactionsSender,
        replay_votes_sender: &ReplayVoteSender,
    ) {
        let tower_sync = TowerSync::new_from_slots(gossip_vote_slots, Hash::default(), None);
        validator_voting_keypairs.iter().for_each(|keypairs| {
            let node_keypair = &keypairs.node_keypair;
            let vote_keypair = &keypairs.vote_keypair;
            let vote_tx = vote_transaction::new_tower_sync_transaction(
                tower_sync.clone(),
                Hash::default(),
                node_keypair,
                vote_keypair,
                vote_keypair,
                switch_proof_hash,
            );
            votes_sender.send(vec![vote_tx]).unwrap();
            let replay_vote = Vote::new(replay_vote_slots.clone(), Hash::default());
            // Send same vote twice, but should only notify once
            for _ in 0..2 {
                replay_votes_sender
                    .send((
                        vote_keypair.pubkey(),
                        VoteTransaction::from(replay_vote.clone()),
                        switch_proof_hash,
                        Signature::default(),
                    ))
                    .unwrap();
            }
        });
    }

    fn run_test_process_votes(hash: Option<Hash>) {
        // Create some voters at genesis
        let stake_per_validator = 100;
        let SetupComponents {
            vote_tracker,
            validator_voting_keypairs,
            subscriptions,
            bank_forks,
            ..
        } = setup();
        let (votes_txs_sender, votes_txs_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, gossip_verified_vote_hash_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();
        let mut latest_vote_slot_per_validator = HashMap::new();
        let mut bank_hash_cache = BankHashCache::new(bank_forks);

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );
        let bank0 = Bank::new_for_tests(&genesis_config);

        let gossip_vote_slots = vec![1, 2];
        let replay_vote_slots = vec![3, 4];
        send_vote_txs(
            gossip_vote_slots.clone(),
            replay_vote_slots.clone(),
            &validator_voting_keypairs,
            hash,
            &votes_txs_sender,
            &replay_votes_sender,
        );

        // Check that all the votes were registered for each validator correctly
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_txs_receiver,
            &vote_tracker,
            &bank0,
            Some(&subscriptions),
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
            &mut None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        )
        .unwrap();

        let mut gossip_verified_votes: HashMap<Slot, HashMap<Hash, Vec<Pubkey>>> = HashMap::new();
        for (pubkey, slot, hash) in gossip_verified_vote_hash_receiver.try_iter() {
            // send_vote_txs() will send each vote twice, but we should only get a notification
            // once for each via this channel
            let exists = gossip_verified_votes
                .get(&slot)
                .and_then(|slot_hashes| slot_hashes.get(&hash))
                .map(|slot_hash_voters| slot_hash_voters.contains(&pubkey))
                .unwrap_or(false);
            assert!(!exists);
            gossip_verified_votes
                .entry(slot)
                .or_default()
                .entry(hash)
                .or_default()
                .push(pubkey);
        }

        // Only the last vote in the `gossip_vote` set should count towards
        // the `voted_hash_updates` set. Important to note here that replay votes
        // should not count
        let last_gossip_vote_slot = *gossip_vote_slots.last().unwrap();
        assert_eq!(gossip_verified_votes.len(), 1);
        let slot_hashes = gossip_verified_votes.get(&last_gossip_vote_slot).unwrap();
        assert_eq!(slot_hashes.len(), 1);
        let slot_hash_votes = slot_hashes.get(&Hash::default()).unwrap();
        assert_eq!(slot_hash_votes.len(), validator_voting_keypairs.len());
        for voting_keypairs in &validator_voting_keypairs {
            let pubkey = voting_keypairs.vote_keypair.pubkey();
            assert!(slot_hash_votes.contains(&pubkey));
        }

        // Check that the received votes were pushed to other components
        // subscribing via `verified_vote_receiver`
        let all_expected_slots: BTreeSet<_> = gossip_vote_slots
            .clone()
            .into_iter()
            .chain(replay_vote_slots.clone())
            .collect();
        let mut pubkey_to_votes: HashMap<Pubkey, BTreeSet<Slot>> = HashMap::new();
        for (received_pubkey, new_votes) in verified_vote_receiver.try_iter() {
            let already_received_votes = pubkey_to_votes.entry(received_pubkey).or_default();
            for new_vote in new_votes {
                // `new_vote` should only be received once
                assert!(already_received_votes.insert(new_vote));
            }
        }
        assert_eq!(pubkey_to_votes.len(), validator_voting_keypairs.len());
        for keypairs in &validator_voting_keypairs {
            assert_eq!(
                *pubkey_to_votes
                    .get(&keypairs.vote_keypair.pubkey())
                    .unwrap(),
                all_expected_slots
            );
        }

        // Check the vote trackers were updated correctly
        for vote_slot in all_expected_slots {
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(vote_slot).unwrap();
            let r_slot_vote_tracker = slot_vote_tracker.read().unwrap();
            for voting_keypairs in &validator_voting_keypairs {
                let pubkey = voting_keypairs.vote_keypair.pubkey();
                assert!(r_slot_vote_tracker.voted.contains_key(&pubkey));
                assert!(r_slot_vote_tracker
                    .voted_slot_updates
                    .as_ref()
                    .unwrap()
                    .contains(&Arc::new(pubkey)));
                // Only the last vote in the stack of `gossip_vote` and `replay_vote_slots`
                // should count towards the `optimistic` vote set,
                let optimistic_votes_tracker =
                    r_slot_vote_tracker.optimistic_votes_tracker(&Hash::default());
                if vote_slot == *gossip_vote_slots.last().unwrap()
                    || vote_slot == *replay_vote_slots.last().unwrap()
                {
                    let optimistic_votes_tracker = optimistic_votes_tracker.unwrap();
                    assert!(optimistic_votes_tracker.voted().contains(&pubkey));
                    assert_eq!(
                        optimistic_votes_tracker.stake(),
                        stake_per_validator * validator_voting_keypairs.len() as u64
                    );
                } else {
                    assert!(optimistic_votes_tracker.is_none())
                }
            }
        }
    }

    #[test]
    fn test_process_votes1() {
        run_test_process_votes(None);
        run_test_process_votes(Some(Hash::default()));
    }

    #[test]
    fn test_process_votes2() {
        // Create some voters at genesis
        let SetupComponents {
            vote_tracker,
            validator_voting_keypairs,
            subscriptions,
            bank_forks,
            ..
        } = setup();

        // Create bank with the voters
        let stake_per_validator = 100;
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![stake_per_validator; validator_voting_keypairs.len()],
            );
        let bank0 = Bank::new_for_tests(&genesis_config);

        // Send some votes to process
        let (votes_txs_sender, votes_txs_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        let (verified_vote_sender, verified_vote_receiver) = unbounded();
        let (_replay_votes_sender, replay_votes_receiver) = unbounded();
        let mut latest_vote_slot_per_validator = HashMap::new();
        let mut bank_hash_cache = BankHashCache::new(bank_forks);

        let mut expected_votes = vec![];
        let num_voters_per_slot = 2;
        let bank_hash = Hash::default();
        for (i, keyset) in validator_voting_keypairs
            .chunks(num_voters_per_slot)
            .enumerate()
        {
            let validator_votes: Vec<_> = keyset
                .iter()
                .map(|keypairs| {
                    let node_keypair = &keypairs.node_keypair;
                    let vote_keypair = &keypairs.vote_keypair;
                    expected_votes.push((vote_keypair.pubkey(), vec![i as Slot + 1]));
                    let tower_sync =
                        TowerSync::new_from_slots(vec![(i as u64 + 1)], bank_hash, None);
                    vote_transaction::new_tower_sync_transaction(
                        tower_sync,
                        Hash::default(),
                        node_keypair,
                        vote_keypair,
                        vote_keypair,
                        None,
                    )
                })
                .collect();
            votes_txs_sender.send(validator_votes).unwrap();
        }

        // Read and process votes from channel `votes_receiver`
        ClusterInfoVoteListener::listen_and_confirm_votes(
            &votes_txs_receiver,
            &vote_tracker,
            &bank0,
            Some(&subscriptions),
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &replay_votes_receiver,
            &None,
            &None,
            &mut None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        )
        .unwrap();

        // Check that the received votes were pushed to other components
        // subscribing via a channel
        let received_votes: Vec<_> = verified_vote_receiver.try_iter().collect();
        assert_eq!(received_votes.len(), validator_voting_keypairs.len());
        for (expected_pubkey_vote, received_pubkey_vote) in
            expected_votes.iter().zip(received_votes.iter())
        {
            assert_eq!(expected_pubkey_vote, received_pubkey_vote);
        }

        // Check that all the votes were registered for each validator correctly
        for (i, keyset) in validator_voting_keypairs.chunks(2).enumerate() {
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(i as u64 + 1).unwrap();
            let r_slot_vote_tracker = &slot_vote_tracker.read().unwrap();
            for voting_keypairs in keyset {
                let pubkey = voting_keypairs.vote_keypair.pubkey();
                assert!(r_slot_vote_tracker.voted.contains_key(&pubkey));
                assert!(r_slot_vote_tracker
                    .voted_slot_updates
                    .as_ref()
                    .unwrap()
                    .contains(&Arc::new(pubkey)));
                // All the votes were single votes, so they should all count towards
                // the optimistic confirmation vote set
                let optimistic_votes_tracker = r_slot_vote_tracker
                    .optimistic_votes_tracker(&bank_hash)
                    .unwrap();
                assert!(optimistic_votes_tracker.voted().contains(&pubkey));
                assert_eq!(
                    optimistic_votes_tracker.stake(),
                    num_voters_per_slot as u64 * stake_per_validator
                );
            }
        }
    }

    fn run_test_process_votes3(switch_proof_hash: Option<Hash>) {
        let (votes_sender, votes_receiver) = unbounded();
        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        let (replay_votes_sender, replay_votes_receiver): (ReplayVoteSender, ReplayVoteReceiver) =
            unbounded();
        let mut latest_vote_slot_per_validator = HashMap::new();

        let vote_slot = 1;
        let vote_bank_hash = Hash::default();
        // Events:
        // 0: Send gossip vote
        // 1: Send replay vote
        // 2: Send both
        let ordered_events = vec![
            vec![0],
            vec![1],
            vec![0, 1],
            vec![1, 0],
            vec![2],
            vec![0, 1, 2],
            vec![1, 0, 2],
            vec![0, 1, 2, 0, 1, 2],
        ];
        for events in ordered_events {
            let SetupComponents {
                vote_tracker,
                bank,
                validator_voting_keypairs,
                subscriptions,
                bank_forks,
            } = setup();
            let mut bank_hash_cache = BankHashCache::new(bank_forks);
            let node_keypair = &validator_voting_keypairs[0].node_keypair;
            let vote_keypair = &validator_voting_keypairs[0].vote_keypair;
            for &e in &events {
                if e == 0 || e == 2 {
                    // Create vote transaction
                    let tower_sync =
                        TowerSync::new_from_slots(vec![(vote_slot)], vote_bank_hash, None);
                    let vote_tx = vote_transaction::new_tower_sync_transaction(
                        tower_sync,
                        Hash::default(),
                        node_keypair,
                        vote_keypair,
                        vote_keypair,
                        switch_proof_hash,
                    );
                    votes_sender.send(vec![vote_tx.clone()]).unwrap();
                }
                if e == 1 || e == 2 {
                    replay_votes_sender
                        .send((
                            vote_keypair.pubkey(),
                            VoteTransaction::from(Vote::new(vec![vote_slot], Hash::default())),
                            switch_proof_hash,
                            Signature::default(),
                        ))
                        .unwrap();
                }
                let _ = ClusterInfoVoteListener::listen_and_confirm_votes(
                    &votes_receiver,
                    &vote_tracker,
                    &bank,
                    Some(&subscriptions),
                    &gossip_verified_vote_hash_sender,
                    &verified_vote_sender,
                    &replay_votes_receiver,
                    &None,
                    &None,
                    &mut None,
                    &mut latest_vote_slot_per_validator,
                    &mut bank_hash_cache,
                    &Mutex::new(false),
                );
            }
            let slot_vote_tracker = vote_tracker.get_slot_vote_tracker(vote_slot).unwrap();
            let r_slot_vote_tracker = &slot_vote_tracker.read().unwrap();

            assert_eq!(
                r_slot_vote_tracker
                    .optimistic_votes_tracker(&vote_bank_hash)
                    .unwrap()
                    .stake(),
                100
            );
            if events == vec![1] {
                // Check `gossip_only_stake` is not incremented
                assert_eq!(r_slot_vote_tracker.gossip_only_stake, 0);
            } else {
                // Check that both the `gossip_only_stake` and `total_voted_stake` both
                // increased
                assert_eq!(r_slot_vote_tracker.gossip_only_stake, 100);
            }
        }
    }

    #[test]
    fn test_run_test_process_votes3() {
        run_test_process_votes3(None);
        run_test_process_votes3(Some(Hash::default()));
    }

    #[test]
    fn test_vote_tracker_references() {
        // Create some voters at genesis
        let validator_keypairs: Vec<_> =
            (0..2).map(|_| ValidatorVoteKeypairs::new_rand()).collect();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_keypairs,
                vec![100; validator_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let exit = Arc::new(AtomicBool::new(false));
        let bank_forks = BankForks::new_rw_arc(bank);
        let bank = bank_forks.read().unwrap().get(0).unwrap();
        let vote_tracker = VoteTracker::default();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            exit,
            max_complete_transaction_status_slot,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));
        let mut latest_vote_slot_per_validator = HashMap::new();
        let mut bank_hash_cache = BankHashCache::new(bank_forks);

        // Send a vote to process, should add a reference to the pubkey for that voter
        // in the tracker
        let validator0_keypairs = &validator_keypairs[0];
        let voted_slot = bank.slot() + 1;
        let vote_tx = vec![vote_transaction::new_tower_sync_transaction(
            // Must vote > root to be processed
            TowerSync::from(vec![(voted_slot, 1)]),
            Hash::default(),
            &validator0_keypairs.node_keypair,
            &validator0_keypairs.vote_keypair,
            &validator0_keypairs.vote_keypair,
            None,
        )];

        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vote_tx,
            // Add gossip vote for same slot, should not affect outcome
            vec![(
                validator0_keypairs.vote_keypair.pubkey(),
                VoteTransaction::from(Vote::new(vec![voted_slot], Hash::default())),
                None,
                Signature::default(),
            )],
            &bank,
            Some(&subscriptions),
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &None,
            &None,
            &mut None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        );

        // Setup next epoch
        let old_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let new_epoch = old_epoch + 1;

        // Test with votes across two epochs
        let first_slot_in_new_epoch = bank.epoch_schedule().get_first_slot_in_epoch(new_epoch);

        // Make 2 new votes in two different epochs for the same pubkey,
        // the ref count should go up by 3 * ref_count_per_vote
        // Add 1 vote through the replay channel for a different pubkey,
        // ref count should equal `current_ref_count` for that pubkey.
        let vote_txs: Vec<_> = [first_slot_in_new_epoch - 1, first_slot_in_new_epoch]
            .iter()
            .map(|slot| {
                vote_transaction::new_tower_sync_transaction(
                    // Must vote > root to be processed
                    TowerSync::from(vec![(*slot, 1)]),
                    Hash::default(),
                    &validator0_keypairs.node_keypair,
                    &validator0_keypairs.vote_keypair,
                    &validator0_keypairs.vote_keypair,
                    None,
                )
            })
            .collect();

        let new_root_bank =
            Bank::new_from_parent(bank, &Pubkey::default(), first_slot_in_new_epoch - 2);
        ClusterInfoVoteListener::filter_and_confirm_with_new_votes(
            &vote_tracker,
            vote_txs,
            vec![(
                validator_keypairs[1].vote_keypair.pubkey(),
                VoteTransaction::from(Vote::new(vec![first_slot_in_new_epoch], Hash::default())),
                None,
                Signature::default(),
            )],
            &new_root_bank,
            Some(&subscriptions),
            &gossip_verified_vote_hash_sender,
            &verified_vote_sender,
            &None,
            &None,
            &mut None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        );
    }

    struct SetupComponents {
        vote_tracker: Arc<VoteTracker>,
        bank: Arc<Bank>,
        validator_voting_keypairs: Vec<ValidatorVoteKeypairs>,
        subscriptions: Arc<RpcSubscriptions>,
        bank_forks: Arc<RwLock<BankForks>>,
    }

    fn setup() -> SetupComponents {
        let validator_voting_keypairs: Vec<_> =
            (0..10).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_voting_keypairs,
                vec![100; validator_voting_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let vote_tracker = VoteTracker::default();
        let exit = Arc::new(AtomicBool::new(false));
        let bank_forks = BankForks::new_rw_arc(bank);
        let bank = bank_forks.read().unwrap().get(0).unwrap();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            exit,
            max_complete_transaction_status_slot,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));

        SetupComponents {
            vote_tracker: Arc::new(vote_tracker),
            bank,
            validator_voting_keypairs,
            subscriptions,
            bank_forks,
        }
    }

    #[test]
    fn test_verify_votes_empty() {
        solana_logger::setup();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut root_bank_cache = RootBankCache::new(bank_forks);
        let votes = vec![];
        let (vote_txs, packets) =
            ClusterInfoVoteListener::verify_votes(votes, &mut root_bank_cache);
        assert!(vote_txs.is_empty());
        assert!(packets.is_empty());
    }

    fn verify_packets_len(packets: &[PacketBatch], ref_value: usize) {
        let num_packets: usize = packets.iter().map(|pb| pb.len()).sum();
        assert_eq!(num_packets, ref_value);
    }

    fn test_vote_tx(
        validator_vote_keypairs: Option<&ValidatorVoteKeypairs>,
        hash: Option<Hash>,
    ) -> Transaction {
        let other = ValidatorVoteKeypairs::new_rand();
        let validator_vote_keypair = validator_vote_keypairs.unwrap_or(&other);
        // TODO authorized_voter_keypair should be different from vote-keypair
        // but that is what create_genesis_... currently generates.
        vote_transaction::new_tower_sync_transaction(
            TowerSync::from(vec![(0, 1)]),
            Hash::default(),
            &validator_vote_keypair.node_keypair,
            &validator_vote_keypair.vote_keypair,
            &validator_vote_keypair.vote_keypair, // authorized_voter_keypair
            hash,
        )
    }

    fn run_test_verify_votes_1_pass(hash: Option<Hash>) {
        let voting_keypairs: Vec<_> = repeat_with(ValidatorVoteKeypairs::new_rand)
            .take(10)
            .collect();
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000, // mint_lamports
                &voting_keypairs,
                vec![100; voting_keypairs.len()], // stakes
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut root_bank_cache = RootBankCache::new(bank_forks);
        let vote_tx = test_vote_tx(voting_keypairs.first(), hash);
        let votes = vec![vote_tx];
        let (vote_txs, packets) =
            ClusterInfoVoteListener::verify_votes(votes, &mut root_bank_cache);
        assert_eq!(vote_txs.len(), 1);
        verify_packets_len(&packets, 1);
    }

    #[test]
    fn test_verify_votes_1_pass() {
        run_test_verify_votes_1_pass(None);
        run_test_verify_votes_1_pass(Some(Hash::default()));
    }

    fn run_test_bad_vote(hash: Option<Hash>) {
        let voting_keypairs: Vec<_> = repeat_with(ValidatorVoteKeypairs::new_rand)
            .take(10)
            .collect();
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000, // mint_lamports
                &voting_keypairs,
                vec![100; voting_keypairs.len()], // stakes
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut root_bank_cache = RootBankCache::new(bank_forks);
        let vote_tx = test_vote_tx(voting_keypairs.first(), hash);
        let mut bad_vote = vote_tx.clone();
        bad_vote.signatures[0] = Signature::default();
        let votes = vec![vote_tx.clone(), bad_vote, vote_tx];
        let (vote_txs, packets) =
            ClusterInfoVoteListener::verify_votes(votes, &mut root_bank_cache);
        assert_eq!(vote_txs.len(), 2);
        verify_packets_len(&packets, 2);
    }

    #[test]
    fn test_sum_stake() {
        let SetupComponents {
            bank,
            validator_voting_keypairs,
            ..
        } = setup();
        let vote_keypair = &validator_voting_keypairs[0].vote_keypair;
        let epoch_stakes = bank.epoch_stakes(bank.epoch()).unwrap();
        let mut gossip_only_stake = 0;

        ClusterInfoVoteListener::sum_stake(
            &mut gossip_only_stake,
            Some(epoch_stakes),
            &vote_keypair.pubkey(),
        );
        assert_eq!(gossip_only_stake, 100);
    }

    #[test]
    fn test_bad_vote() {
        run_test_bad_vote(None);
        run_test_bad_vote(Some(Hash::default()));
    }

    #[test]
    fn test_track_new_votes_filter() {
        let validator_keypairs: Vec<_> =
            (0..2).map(|_| ValidatorVoteKeypairs::new_rand()).collect();

        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &validator_keypairs,
                vec![100; validator_keypairs.len()],
            );
        let bank = Bank::new_for_tests(&genesis_config);
        let exit = Arc::new(AtomicBool::new(false));
        let bank_forks = BankForks::new_rw_arc(bank);
        let bank = bank_forks.read().unwrap().get(0).unwrap();
        let vote_tracker = VoteTracker::default();
        let optimistically_confirmed_bank =
            OptimisticallyConfirmedBank::locked_from_bank_forks_root(&bank_forks);
        let max_complete_transaction_status_slot = Arc::new(AtomicU64::default());
        let subscriptions = Arc::new(RpcSubscriptions::new_for_tests(
            exit,
            max_complete_transaction_status_slot,
            bank_forks.clone(),
            Arc::new(RwLock::new(BlockCommitmentCache::default())),
            optimistically_confirmed_bank,
        ));
        let mut latest_vote_slot_per_validator = HashMap::new();
        let mut bank_hash_cache = BankHashCache::new(bank_forks);

        let (verified_vote_sender, _verified_vote_receiver) = unbounded();
        let (gossip_verified_vote_hash_sender, _gossip_verified_vote_hash_receiver) = unbounded();
        let mut diff = HashMap::default();
        let mut new_optimistic_confirmed_slots = vec![];

        let validator0_keypairs = &validator_keypairs[0];
        let (vote_pubkey, vote, _, signature) =
            vote_parser::parse_vote_transaction(&vote_transaction::new_tower_sync_transaction(
                TowerSync::from(vec![(1, 3), (2, 2), (6, 1)]),
                Hash::default(),
                &validator0_keypairs.node_keypair,
                &validator0_keypairs.vote_keypair,
                &validator0_keypairs.vote_keypair,
                None,
            ))
            .unwrap();

        ClusterInfoVoteListener::track_new_votes_and_notify_confirmations(
            vote,
            &vote_pubkey,
            signature,
            &vote_tracker,
            &bank,
            Some(&subscriptions),
            &verified_vote_sender,
            &gossip_verified_vote_hash_sender,
            &mut diff,
            &mut new_optimistic_confirmed_slots,
            true, /* is gossip */
            &None,
            &None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        );
        assert_eq!(diff.keys().copied().sorted().collect_vec(), vec![1, 2, 6]);

        // Vote on a new slot, only those later than 6 should show up. 4 is skipped.
        diff.clear();
        let (vote_pubkey, vote, _, signature) =
            vote_parser::parse_vote_transaction(&vote_transaction::new_tower_sync_transaction(
                TowerSync::from(vec![(1, 6), (2, 5), (3, 4), (4, 3), (7, 2), (8, 1)]),
                Hash::default(),
                &validator0_keypairs.node_keypair,
                &validator0_keypairs.vote_keypair,
                &validator0_keypairs.vote_keypair,
                None,
            ))
            .unwrap();

        ClusterInfoVoteListener::track_new_votes_and_notify_confirmations(
            vote,
            &vote_pubkey,
            signature,
            &vote_tracker,
            &bank,
            Some(&subscriptions),
            &verified_vote_sender,
            &gossip_verified_vote_hash_sender,
            &mut diff,
            &mut new_optimistic_confirmed_slots,
            true, /* is gossip */
            &None,
            &None,
            &mut latest_vote_slot_per_validator,
            &mut bank_hash_cache,
            &Mutex::new(false),
        );
        assert_eq!(diff.keys().copied().sorted().collect_vec(), vec![7, 8]);
    }
}
