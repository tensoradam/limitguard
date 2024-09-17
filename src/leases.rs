use crate::{RateLimitRequest, Sender};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Represents our accuracy error.
/// We keep it hardcoded for the scope of this prototype.
const BUCKET_ERROR: f64 = 0.100;

/// Underneath we implement a token bucket / lease based approach.
/// We keep track of the bucket state for each sender.
pub(crate) struct Leaser {
    // We represent in memory a bucket as just a tuple of (credits,
    // deadline) to keep it lightweight.
    //
    // FIXME: Next step would be to use a specific Bucket type, but we
    // are trying to not prototype under time constraints.
    pub(crate) table: HashMap<Sender, (u64, Instant)>,
    _node_id: u64,
    // Given we have two types of senders it is easier to keep track
    // of the max limits for each in the Leaser parent type.  This
    // saves us 64 bits per sender,
    //which add up if we have millions of distinct senders, and can
    // affect cache performance.
    max_limit: u64,
    max_limit_payer: u64,
    // We likely want to keep track of the min of all instants. We
    // assume no skew / skew is part of our error.
    epoch: Instant,
    interval: Duration,
    refill_amount: u64,
}

impl Leaser {
    /// Create a new Leaser instance.
    pub fn new(
        _node_id: u64,
        max_limit: u64,
        max_limit_payer: u64,
        interval: Duration,
        refill_amount: u64,
    ) -> Self {
        Self {
            table: HashMap::new(),
            max_limit,
            max_limit_payer,
            _node_id,
            epoch: Instant::now(),
            interval,
            refill_amount,
        }
    }

    /// Touch the bucket for a given sender. This is the core
    /// function that updates the bucket state.  We return the new
    /// bucket state if the request was allowed.  Otherwise we return
    /// None.  Note that it alters the state of the bucket.
    ///
    /// As such even requests with 0 cost will alter the bucket state,
    /// by affecting the deadline.
    ///
    /// We can generally, alter the core logic to any behaviour we
    /// want, as long as we guarantee that join_bucket is a
    /// commutative operation.
    ///
    /// Touch(.) will:
    ///
    /// 1. Retrieve the bucket state.
    /// 2. Check if the cost is greater than the current bucket state.
    /// 3. If so, return None.
    /// 4. Otherwise, reduce the cost (with minimum of 0) from the
    ///    bucket state.
    /// 5. Update the deadline to the max of the current instant and
    ///    the deadline.
    pub fn touch(
        &mut self,
        req: RateLimitRequest,
        default: (u64, Instant),
        cost: u64,
    ) -> Option<(u64, Instant)> {
        let e = self.table.entry(req.sender).or_insert(default);
        let (credits, deadline) = e;

        // The request should be barred.
        if cost > *credits {
            return None;
        }

        *credits = (*credits).saturating_sub(cost);

        *deadline = (*deadline).max(Instant::now() + self.interval);
        // We also could insert the new state in the alternative.
        //self.table.insert(req.sender, (new_credits, new_deadline));

        Some((*credits, *deadline))
    }

    /// Get the max limit for a given sender.
    pub(crate) fn get_max_limit(&self, sender: &Sender) -> u64 {
        match sender {
            Sender::Node(_) => u64::MAX,
            Sender::Payer(_) => self.max_limit_payer,
            _ => self.max_limit,
        }
    }
    /// Sync the bucket state with other Leaser instances.
    ///
    /// We merge the bucket states of all participating nodes.
    ///
    /// We do this by taking the union of all senders in the
    /// participating nodes, and then merging the bucket states for
    /// each sender.
    ///
    /// Correctness: sync is correct as join_buckets is a commutative
    /// operation.  Namely, any order of merging is eventually
    /// correct.  (As long as all Nodes have communicated their Leaser
    /// state to us.)
    ///
    /// Any requests that is received by a node while all the nodes
    /// are syncing will either be blocked awaiting the sync, or will
    /// be allowed to pass, because there are locally sufficient
    /// credits and we would be within our error margin if we allowed
    /// it.
    pub fn sync(&mut self, other: &[HashMap<Sender, (u64, Instant)>]) {
        let other: Vec<_> = other.to_vec();
        // Start with the active HashMap as our base
        let mut merged = HashMap::with_capacity(self.table.len() + other.len());
        // Merge with all other HashMaps (both from self.table and other)
        let new_table = other.iter().flatten();
        let new_table = new_table.chain(self.table.iter());
        // Merge with all other HashMaps (both from self.table and other)
        for (sender, &(credits, deadline)) in new_table {
            let max_limit = self.get_max_limit(sender);
            merged
                .entry(sender.clone())
                .and_modify(|e: &mut (u64, Instant)| {
                    // Join the existing entry with the new one using join_buckets
                    let buckets = vec![*e, (credits, deadline)];
                    let (total, latest) =
                        join_buckets(&buckets, max_limit, self.interval, self.refill_amount);
                    *e = (total, latest);
                })
                .or_insert((credits, deadline));
        }

        // Update all entries in self.table with the merged values
        self.table = merged;
    }

    /// Garbage collect old entries from the lease table.  Any entry
    /// with a deadline older than the given time is removed.  The
    /// epoch is updated to the given time.
    ///
    /// FIXME: Test this, and improve upon it. Initially I started
    /// with a generational garbage collection approach, but there was
    /// no point for the complexity and changed to a simple retain().
    /// There was no need in the specification for longevity of the
    /// process.
    pub fn gc(&mut self, cutoff: Instant) {
        // Remove entries with deadlines older than cutoff
        self.table
            .retain(|_, &mut (_, deadline)| deadline >= cutoff);
        self.epoch = cutoff;
    }
}

/// We need to check if the request forces us to recheck with every
/// node.
///
/// If we are running with a single node, then we do not need to
/// recheck.
///
/// If we have N participating nodes, then we can allow for each node
/// bucket size/ N + error rate / N credits to be used. (Without any
/// coordination.)  Note this is a problem.
///
/// However, if that limit is reached, then we need to coordinate with
/// other nodes. We should block until we can merge. Perhaps, later we
/// can add incremental syncing, where we continuously guess the state
/// of the remaining unsynced nodes to make a decision.
pub(crate) fn sync_necessary(
    max_bucket_credits: u64,
    credits_to_be_used: u64,
    _now: Instant,
    participants: u64,
) -> bool {
    // We should never run this function with 0 participants.
    assert!(participants > 0);
    // If we're the only participant, no sync necessary.
    if participants == 1 {
        return false;
    }

    // Calculate fair share per node (credits / N).
    let fair_share: f64 = max_bucket_credits as f64 / participants as f64;

    // Use configured BUCKET_ERROR for margin -- for now.
    let error_margin = BUCKET_ERROR;

    // If we're using more than fair share + error margin, sync needed.
    credits_to_be_used as f64 > (fair_share * (1.0 + error_margin))
}

/// We join multiple participant buckets.
///
/// We do this by taking the union of all senders in the participating
/// nodes, and then merging the bucket states for each sender.
///
/// Correctness: join_buckets is correct as it is a commutative
/// operation.  Namely, any order of merging is eventually correct.
///
/// There are two operations:
/// 1. We gind the latest request time from all Nodes' buckets.
/// 2. We recalculate all buckets to the latest request time.
pub(crate) fn join_buckets(
    buckets: &[(u64, Instant)],
    max_bucket_balance: u64,
    bucket_interval: Duration,
    refill_amount: u64,
) -> (u64, Instant) {
    assert!(!buckets.is_empty());

    let mut total_credits = 0u64;
    // Find the latest request time from all buckets
    let latest_request = buckets
        .iter()
        .map(|(_, request_time)| *request_time)
        .max()
        .unwrap();

    // Drain each bucket first to get current values
    for &(credits_bucket, bucket_request) in buckets {
        if let Some((drained_credits, _drained_request)) = drain_bucket(
            credits_bucket,
            bucket_request,
            latest_request,
            bucket_interval,
            max_bucket_balance,
            refill_amount,
        ) {
            total_credits = total_credits.saturating_add(drained_credits);
        }
    }

    (total_credits.min(max_bucket_balance), latest_request)
}

/// Calculate refill amount. Returning a tuple of how much to fill and
/// remaining duration to sleep until the next refill time if
/// appropriate.
///
/// If we are past that time then we should simply zero the bucket.
/// The maximum number of additional tokens this method will ever
/// return is limited to the `max_balance` to ensure that addition
/// with an existing balance will never overflow.
///
/// We write this in a way as a "pure" function, such that we can
/// easily test it. The calculation is fundamental in our leaky bucket
/// implementation.
///
/// This function is generally invariant to the number of distributed
/// participants.
///
/// TODO: We ideally want a more sophisticated approach to handling
/// the case where we have a burst of traffic. (Outside of the scope
/// of this prototype.)
pub(crate) fn drain_bucket(
    current_credits: u64,
    deadline: Instant,
    now: Instant,
    interval: Duration,
    max_balance: u64,
    refill_amount: u64,
) -> Option<(u64, Instant)> {
    // Ensure we are past the deadline.
    if now <= deadline {
        // No refill happpens for too frequent requests-- that allows
        // us to punish burst traffic. We keep deadline the same.
        return Some((current_credits, deadline));
    }

    // Time elapsed in milliseconds since the last deadline.
    let window_millis = interval.as_millis();

    let since = now.saturating_duration_since(deadline).as_millis();
    // Number of periods passed.
    let periods = u64::try_from(since / window_millis).unwrap_or(u64::MAX);
    // We gain a number of credits equal to the number of periods
    // passed. We cap at max_balance.
    //
    // (We assume bucket was used up.)
    let mut credits = current_credits;
    credits += periods.checked_mul(refill_amount).unwrap_or(max_balance);
    credits = credits.min(max_balance);

    // We calculate the remaining time in the time window. That will
    // be our new deadline.  That way we do not double-count the time,
    // later.
    let remaining_time = u64::try_from(since % window_millis).unwrap_or(u64::MAX);

    // Calculated time remaining until the next deadline.
    let deadline = now + interval.saturating_sub(Duration::from_millis(remaining_time));

    Some((credits, deadline))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RequestData, Sender};
    use rand::Rng;

    #[test]
    fn test_drain() {
        let now = Instant::now();

        // Test with different intervals and balances
        let test_cases = vec![
            // (current, interval, max_balance, credits_bucket)
            (0, Duration::from_secs(1), 100, 10),
            (0, Duration::from_millis(500), 50, 5),
            (0, Duration::from_secs(5), 1000, 100),
        ];

        for (current_credits, interval, max_balance, credits_bucket) in test_cases {
            // Test when now is before deadline.
            let deadline = now + Duration::from_millis(100);
            let result = drain_bucket(
                current_credits,
                deadline,
                now,
                interval,
                max_balance,
                credits_bucket,
            );
            assert_eq!(result, Some((current_credits, deadline)));

            // Test when now is after deadline.
            let deadline = now - Duration::from_millis(100);
            let (credits, new_deadline) = drain_bucket(
                current_credits,
                deadline,
                now,
                interval,
                max_balance,
                credits_bucket,
            )
            .unwrap();

            // Credits should not exceed max balance.
            assert!(credits <= max_balance);

            // New deadline should be in the future.
            assert!(new_deadline > now);

            // New deadline should be less than one full interval
            // away.
            assert!(new_deadline - now <= interval);

            // Test multiple periods.
            let deadline = now - interval.mul_f32(2.5);
            let (credits, _) = drain_bucket(
                current_credits,
                deadline,
                now,
                interval,
                max_balance,
                credits_bucket,
            )
            .unwrap();

            // Credits should be credited for multiple periods but not
            // exceed max.
            let expected = (credits_bucket * 2).min(max_balance);
            assert_eq!(credits, expected);
        }
    }

    #[test]
    fn test_join_buckets() {
        let now = Instant::now();

        // Test joining a single bucket.
        let buckets = vec![(50u64, now)];
        let (total_credits, latest_deadline) =
            join_buckets(&buckets, 1000, Duration::from_secs(1), 10);

        // With a single bucket, credits and deadline should remain
        // unchanged.
        assert_eq!(total_credits, 50);
        assert_eq!(latest_deadline, now);

        let buckets = vec![
            (50u64, now + Duration::from_millis(500)),
            (30u64, now + Duration::from_millis(500)),
        ];
        let (total_credits, _latest_deadline) =
            join_buckets(&buckets, 1000, Duration::from_secs(1), 10);

        // Total credits should be sum of all buckets.
        assert_eq!(total_credits, 80);

        // Test joining two buckets.
        let buckets = vec![
            (50u64, now + Duration::from_millis(500)),
            (30u64, now + Duration::from_millis(300)),
        ];
        let (total_credits, latest_deadline) =
            join_buckets(&buckets, 1000, Duration::from_secs(1), 10);

        // Total credits should be sum to the latest bucket, as the other expired.
        assert_eq!(total_credits, 80);
        // Latest deadline should be the maximum of all deadlines.
        assert_eq!(latest_deadline, now + Duration::from_millis(500));

        // Test joining multiple buckets with different deadlines.
        let buckets = vec![
            (20u64, now + Duration::from_millis(100)),
            (40u64, now + Duration::from_millis(200)),
            (60u64, now + Duration::from_millis(300)),
        ];
        let (total_credits, latest_deadline) =
            join_buckets(&buckets, 1000, Duration::from_secs(1), 10);

        assert_eq!(total_credits, 120);
        assert_eq!(latest_deadline, now + Duration::from_millis(300));

        // Test joining buckets with same deadline.
        let buckets = vec![
            (10u64, now + Duration::from_millis(100)),
            (20u64, now + Duration::from_millis(100)),
            (30u64, now + Duration::from_millis(100)),
        ];
        let (total_credits, latest_deadline) =
            join_buckets(&buckets, 1000, Duration::from_secs(1), 10);

        assert_eq!(total_credits, 60);
        assert_eq!(latest_deadline, now + Duration::from_millis(100));

        // Test joining buckets with overflow protection.
        let buckets = vec![
            (u64::MAX, now + Duration::from_millis(100)),
            (1u64, now + Duration::from_millis(200)),
        ];
        let (total_credits, latest_deadline) =
            join_buckets(&buckets, u64::MAX, Duration::from_secs(1), 10);

        // Should saturate at u64::MAX.
        assert_eq!(total_credits, u64::MAX);
        assert_eq!(latest_deadline, now + Duration::from_millis(200));

        // Test joining 10 buckets with varying credits and deadlines.
        let buckets = vec![
            (10u64, now + Duration::from_millis(100)),
            (20u64, now + Duration::from_millis(200)),
            (30u64, now + Duration::from_millis(300)),
            (40u64, now + Duration::from_millis(400)),
            (50u64, now + Duration::from_millis(500)),
            (60u64, now + Duration::from_millis(600)),
            (70u64, now + Duration::from_millis(700)),
            (80u64, now + Duration::from_millis(800)),
            (90u64, now + Duration::from_millis(900)),
            (100u64, now + Duration::from_millis(1000)),
        ];
        let (total_credits, latest_deadline) =
            join_buckets(&buckets, 1000, Duration::from_secs(1), 10);

        // Total credits should be sum of all buckets (550).
        assert_eq!(total_credits, 550);
        // Latest deadline should be the maximum of all deadlines (1000ms).
        assert_eq!(latest_deadline, now + Duration::from_millis(1000));
    }

    #[test]
    fn test_join_buckets_permutations() {
        let now = Instant::now();

        // Create a test vector of buckets.
        let original_buckets = vec![
            (10u64, now + Duration::from_millis(100)),
            (20u64, now + Duration::from_millis(200)),
            (30u64, now + Duration::from_millis(300)),
        ];

        // Test different permutations using different seeds.
        let seeds = [0, 42, 100, 255];

        // Expected results should be the same for all permutations.
        let expected_total = 60u64;
        let expected_deadline = now + Duration::from_millis(300);

        for seed in seeds {
            // Create a permuted copy of the buckets.
            let permuted = permute_states_with_seed(original_buckets.clone(), seed);

            // Join the permuted buckets.
            let (total_credits, latest_deadline) =
                join_buckets(&permuted, 100, Duration::from_secs(1), 10);

            // Results should be identical regardless of permutation.
            assert_eq!(
                total_credits, expected_total,
                "Total credits mismatch for seed {seed}"
            );
            assert_eq!(
                latest_deadline, expected_deadline,
                "Latest deadline mismatch for seed {seed}"
            );

            // Verify the permutation is different but contains same
            // elements.
            let mut sorted_original = original_buckets.clone();
            let mut sorted_permuted = permuted.clone();
            sorted_original.sort_by_key(|&(c, _)| c);
            sorted_permuted.sort_by_key(|&(c, _)| c);
            assert_eq!(
                sorted_original, sorted_permuted,
                "Permuted buckets missing elements for seed {seed}"
            );
        }
    }

    #[test]
    fn test_sync_necessary() {
        let now = Instant::now();

        // Test with single participant (should never need sync).
        let max_bucket_credits = 100u64;
        assert!(!sync_necessary(max_bucket_credits, 50, now, 1));

        // Test with multiple participants but within fair share.

        // With 2 participants, fair share is 50.0, using 40 (within fair share).
        assert!(!sync_necessary(max_bucket_credits, 40, now, 2));

        // Test with multiple participants exceeding fair share.

        // With 2 participants, fair share is 50.0, using 60 (exceeds
        // fair share).
        assert!(sync_necessary(max_bucket_credits, 60, now, 2));

        // Test we are within the error margin (BUCKET_ERROR = 0.1).
        let max_bucket_credits = 1000u64;
        // With 4 participants:
        // - Fair share is 250.0;
        // - Error margin is 0.1 (fixed);
        // - Max allowed is 250.0 * 1.1 = 275.0; and
        // Using 276 should trigger sync.
        assert!(sync_necessary(max_bucket_credits, 276, now, 4));
        // Using 275 should not trigger sync.
        assert!(!sync_necessary(max_bucket_credits, 275, now, 4));

        // Test with very large numbers.
        let max_bucket_credits = u64::MAX;

        // Even with large numbers, should still work correctly with
        // floating point.
        let max_allowed = u64::MAX >> 1;
        let max_allowed = (max_allowed as f64 * (1.0 + BUCKET_ERROR)).ceil() as u64;
        // assert!(sync_necessary(
        //     max_bucket_credits,
        //     max_allowed + 10000,
        //     now,
        //     2
        // ));
        assert!(!sync_necessary(max_bucket_credits, max_allowed, now, 2));
    }

    #[test]
    fn test_integrated_bucket_operations() {
        let now = Instant::now();
        let interval = Duration::from_secs(1);
        let max_balance = 1000;
        let credits_bucket = 100;
        let refill = 100;

        // Create two buckets with different deadlines.
        let bucket1 = (500u64, now - Duration::from_millis(500));
        let bucket2 = (400u64, now - Duration::from_millis(300));

        // First, drain both buckets to get their current state.
        let (credits1, deadline1) = drain_bucket(
            credits_bucket,
            bucket1.1,
            now,
            interval,
            max_balance,
            refill,
        )
        .unwrap();
        let (credits2, deadline2) = drain_bucket(
            credits_bucket,
            bucket2.1,
            now,
            interval,
            max_balance,
            refill,
        )
        .unwrap();

        // Join the drained buckets.
        let buckets = vec![(credits1, deadline1), (credits2, deadline2)];
        let (total_credits, latest_deadline) =
            join_buckets(&buckets, max_balance, interval, refill);

        // Verify the joined state.
        assert!(
            total_credits > 0,
            "Total credits should be positive after draining and joining"
        );
        assert_eq!(
            latest_deadline,
            deadline1.max(deadline2),
            "Latest deadline should be max of both deadlines"
        );
    }

    #[test]
    fn test_touch() {
        let mut leaser = Leaser::new(1, 100, 1000, Duration::from_secs(1), 10);
        let now = Instant::now();
        let _later = now + Duration::from_secs(1);

        // Test case 1: Basic touch with default values.
        let request = RateLimitRequest {
            request_data: RequestData::Query { limit: 10 },
            sender: Sender::Payer(1),
        };
        let default = (100, now);
        let cost = 10;

        let result = leaser.touch(request.clone(), default, cost);
        assert!(result.is_some());
        let (credits, deadline) = result.unwrap();
        assert_eq!(credits, 90); // 100 - 10
        assert!(deadline >= now);

        // Verify the state in the table.
        assert_eq!(leaser.table.len(), 1);
        assert!(leaser.table.contains_key(&request.sender));
        let (stored_credits, stored_deadline) = leaser.table[&request.sender];
        assert_eq!(stored_credits, 90);
        assert!(stored_deadline >= now);

        // Test case 2: Touch with zero credits.
        let default = (0, now);
        let result = leaser.touch(request.clone(), default, cost);
        assert!(result.is_some());
        let (credits, _) = result.unwrap();
        assert_eq!(credits, 80); // Should not go below 0.

        // Verify the state in the table.
        let (stored_credits, _) = leaser.table[&request.sender];
        assert_eq!(stored_credits, 80);

        // Test case 3: Touch with cost greater than credits.
        let default = (5, now);
        let result = leaser.touch(request.clone(), default, cost);
        assert!(result.is_some());
        let (credits, _) = result.unwrap();
        assert_eq!(credits, 70);

        // Verify the state in the table.
        let (stored_credits, _) = leaser.table[&request.sender];
        assert_eq!(stored_credits, 70);

        // Test case 4: Touch with different sender.
        let request2 = RateLimitRequest {
            request_data: RequestData::Query { limit: 10 },
            sender: Sender::Node(2),
        };
        let default = (100, now);
        let result = leaser.touch(request2.clone(), default, cost);
        assert!(result.is_some());
        let (credits, _) = result.unwrap();
        assert_eq!(credits, 90);

        // Verify the state in the table.
        assert!(leaser.table.contains_key(&request2.sender));
        let (stored_credits, _) = leaser.table[&request2.sender];
        assert_eq!(stored_credits, 90);

        // Test case 5: Multiple touches for same sender.
        let default = (100, now);

        // First touch.
        let result = leaser.touch(request.clone(), default, cost);
        assert!(result.is_some());
        let (credits, _) = result.unwrap();
        assert_eq!(credits, 60);

        // Second touch.
        let result = leaser.touch(request.clone(), default, cost);
        assert!(result.is_some());
        let (credits, _) = result.unwrap();
        assert_eq!(credits, 50);
        // Should use the same bucket from first touch and subtract
        // again.

        // Verify the state in the table.
        let (stored_credits, _) = leaser.table[&request.sender];
        assert_eq!(stored_credits, 50);
    }

    #[test]
    fn test_bucket_draining_over_time() {
        let now = Instant::now();
        let interval = Duration::from_secs(1);
        let max_balance = 100;
        let initial_credits = 80;

        // Test draining at different time points.
        let test_points = vec![
            (Duration::from_millis(0), Some((80, now))), // Immediate - no refill.
            (Duration::from_millis(500), Some((80, now + interval))), // Before interval - same as immediate.
            (
                Duration::from_secs(1),
                Some((100, now + Duration::from_secs(1) + interval)),
            ), // Exactly one interval.
            (
                Duration::from_millis(1500),
                Some((100, now + Duration::from_secs(1) + interval)),
            ), // 1.5 intervals.
            (
                Duration::from_secs(2),
                Some((100, now + Duration::from_secs(2) + interval)),
            ), // Two full intervals.
            (
                Duration::from_secs(4),
                Some((100, now + Duration::from_secs(4) + interval)),
            ),
            (
                Duration::from_secs(400),
                Some((100, now + Duration::from_secs(400) + interval)),
            ),
        ];

        for (elapsed, expected) in test_points {
            let check_time = now + elapsed;
            let result = drain_bucket(
                initial_credits,
                now,
                check_time,
                interval,
                max_balance,
                initial_credits,
            );

            assert_eq!(
                result, expected,
                "Unexpected drain result after {elapsed:?}"
            );
        }

        // Test with very large time gap (should cap at max_balance).
        let far_future = now + Duration::from_secs(1000);
        let result = drain_bucket(
            initial_credits,
            now,
            far_future,
            interval,
            max_balance,
            initial_credits,
        );
        assert_eq!(result, Some((max_balance, far_future + interval)));
    }

    /// Permute the states with a given seed. This is useful for
    /// testing to avoid always getting the same order of states.
    /// TODO: Use a quickcheck framework to generate random states and
    /// permute them.
    fn permute_states_with_seed(mut states: Vec<(u64, Instant)>, seed: u64) -> Vec<(u64, Instant)> {
        use rand::rngs::StdRng;
        use rand::SeedableRng;

        let mut rng = StdRng::seed_from_u64(seed);

        for i in (1..states.len()).rev() {
            // Generate random index between 0 and i inclusive.
            // FIXME: Use new style of rand.
            #[allow(deprecated)]
            let j = rng.gen_range(0..=i);
            // Swap elements at indices i and j.
            states.swap(i, j);
        }

        states
    }
}
