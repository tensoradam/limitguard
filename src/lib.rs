use crossbeam::channel;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Duration;
use std::time::Instant;

mod leases;
use crate::leases::Leaser;

pub struct RateLimiter {
    leases: Leaser,
    _initial: u64,
    refill: u64,
    interval: Duration,
    _max: Option<u64>,
    _max_payer: Option<u64>,
    // We only expose this to allow adding and removing participating
    // nodes on the fly. It is unsafe to change it during an incoming
    // request or a sync operation.
    pub participants: u64,
    sync_channel: channel::Receiver<Vec<HashMap<Sender, (u64, Instant)>>>,
}

// #[async_trait::async_trait]
impl RateLimiter {
    /// Check the rate limit. Returns true if the request is allowed, false otherwise.
    /// Does not block. It will not force the caller to wait, even if it means the request will be rejected.
    pub fn check_rate_limit_non_blocking(
        &mut self,
        request: RateLimitRequest,
    ) -> Result<RateLimitResponse, Box<dyn std::error::Error>> {
        self.check_rate_limit(request, false)
    }

    /// Check the rate limit. Returns true if the request is allowed, false otherwise.
    /// Blocks until the rate limit is checked.
    pub fn check_rate_limit_blocking(
        &mut self,
        request: RateLimitRequest,
    ) -> Result<RateLimitResponse, Box<dyn std::error::Error>> {
        self.check_rate_limit(request, true)
    }

    // As we are using channels we can relatively easily switch this
    // to an async function.  However, this is a library so avoiding
    // trying to make executor decisions or adding one as a
    // dependency.
    fn check_rate_limit(
        &mut self,
        request: RateLimitRequest,
        blocking: bool,
    ) -> Result<RateLimitResponse, Box<dyn std::error::Error>> {
        // Get current credits and deadline from leaser
        let sender = request.sender.clone();
        let now = Instant::now();
        // If the request is from a node, we always allow it to pass.
        // Usually we would want to use the same logic path for all
        // requests.  However, we suggest in this prototype this early
        // alternative path for nodes, to protect against any deadlock
        // logic bugs in the node code.  Which we treat as blackbox
        // here.
        if let Sender::Node(_) = request.sender {
            return Ok(RateLimitResponse {
                approved: true,
                remaining_credits: u32::MAX,
            });
        }
        // Extract credits needed from request.
        let credits_needed = match request.request_data {
            RequestData::Query { limit } => (limit / 10) as u64,
            RequestData::Publish { num_messages } => num_messages as u64,
            RequestData::Subscribe { num_topics } => num_topics as u64,
        };

        let max_bucket_balance = self.leases.get_max_limit(&sender);
        // Get current bucket state
        let &mut (current_credits, bucket_request) = self
            .leases
            .table
            .entry(sender.clone())
            .or_insert((max_bucket_balance, now));

        // Recalculate the new credit-bucket state to get current
        // available credits. We record that a new transaction has
        // occurred.  Note that because of our modification we do not
        // track how frequently (historically) requests occur or node
        // requests at all.
        let (current_credits, bucket_request) = leases::drain_bucket(
            current_credits,
            bucket_request,
            now,
            self.interval,
            max_bucket_balance,
            self.refill, // As we have simplified our bucket logic we
                         // should always succeed here.
        )
        .expect("Failed to drain bucket");

        self.leases
            .table
            .insert(sender.clone(), (current_credits, bucket_request));

        // Check if sync is necessary based on credits needed
        let sync_needed =
            leases::sync_necessary(max_bucket_balance, credits_needed, now, self.participants);

        if sync_needed && blocking {
            // We are keeping the syncing mechanism a bit simple for
            // this version.  We ideally want a communication protocol to
            // sync the buckets that is pushed and pull based. We
            // block until we get a new state.
            //
            // We ideally also want to use send_timeout to avoid
            // blocking indefinitely, and fail but as we simply
            // emulate locally this is fine for a first prototype.
            let new_state = {
                let mut new_state = self.sync_channel.recv().unwrap();
                while let Ok(state) = self.sync_channel.recv() {
                    new_state = state;
                }
                new_state
            };
            self.leases.sync(&new_state);
        }
        // Touch the lease to update state.
        if let Some((remaining, _next)) =
            self.leases
                .touch(request, (max_bucket_balance, now), credits_needed)
        {
            return Ok(RateLimitResponse {
                approved: true,
                remaining_credits: remaining as u32,
            });
        }

        // If we get here, request cannot be accommodated.
        Ok(RateLimitResponse {
            approved: false,
            remaining_credits: current_credits as u32,
        })
    }

    pub fn get_state(&self) -> HashMap<Sender, (u64, Instant)> {
        self.leases.table.clone()
    }

    pub fn gc(&mut self, epoch: Instant) {
        self.leases.gc(epoch);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Sender {
    Payer(u64),
    Node(u64),
    Unknown(Ipv4Addr),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RequestData {
    Query { limit: u32 },
    Publish { num_messages: u32 },
    Subscribe { num_topics: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitRequest {
    pub request_data: RequestData,
    pub sender: Sender,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitResponse {
    pub approved: bool,
    pub remaining_credits: u32,
}

/// Builder for configuring and creating a RateLimiter instance
#[derive(Debug, Default)]
pub struct RateLimiterBuilder {
    initial: Option<u64>,
    refill: Option<u64>,
    interval: Option<Duration>,
    max: Option<u64>,
    max_payer: Option<u64>,
    node_id: u64,
}

impl RateLimiterBuilder {
    /// Create a new RateLimiterBuilder with default values
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            ..Default::default()
        }
    }

    /// Set the initial number of credits.
    pub fn initial(mut self, initial: u64) -> Self {
        self.initial = Some(initial);
        self
    }

    /// Set the number of credits to refill per interval.
    pub fn refill(mut self, refill: u64) -> Self {
        self.refill = Some(refill);
        self
    }

    /// Set the refill interval duration.
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = Some(interval);
        self
    }

    /// Set the maximum number of credits allowed.
    pub fn max(mut self, max: u64) -> Self {
        self.max = Some(max);
        self
    }

    /// Set the maximum number of credits allowed for payers.
    pub fn max_payer(mut self, max_payer: u64) -> Self {
        self.max_payer = Some(max_payer);
        self
    }

    /// Build and return a new RateLimiter instance.
    #[allow(clippy::type_complexity)]
    pub fn build(
        self,
        participants: u64,
    ) -> Result<
        (
            RateLimiter,
            channel::Sender<Vec<HashMap<Sender, (u64, Instant)>>>,
        ),
        &'static str,
    > {
        if self.initial.is_none() {
            return Err("Initial credits must be set");
        }
        if self.refill.is_none() {
            return Err("Refill amount must be set");
        }
        if self.interval.is_none() {
            return Err("Refill interval must be set");
        }

        let (sender_channel, receiver_channel) = channel::unbounded();
        Ok((
            RateLimiter {
                _initial: self.initial.unwrap(),
                refill: self.refill.unwrap(),
                interval: self.interval.unwrap(),
                _max: self.max,
                _max_payer: self.max_payer,
                participants,
                leases: Leaser::new(
                    self.node_id,
                    self.max.expect("Max must be set"),
                    self.max_payer.expect("Max payer must be set"),
                    self.interval.expect("Interval must be set"),
                    self.refill.expect("Refill must be set"),
                ),
                sync_channel: receiver_channel,
            },
            sender_channel,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let request = RateLimitRequest {
            request_data: RequestData::Query { limit: 100 },
            sender: Sender::Payer(1),
        };

        let serialized = serde_json::to_string(&request).unwrap();
        let deserialized: RateLimitRequest = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.sender, Sender::Payer(1));
    }

    #[test]
    fn test_exceed_and_wait() {
        use std::thread::sleep;
        let (mut limiter, _sync_channel) = RateLimiterBuilder::new(1)
            .initial(100)
            .refill(10)
            .interval(Duration::from_millis(50))
            .max(20)
            .max_payer(100)
            .build(1)
            .unwrap();

        let request = RateLimitRequest {
            request_data: RequestData::Query { limit: 1000 },
            sender: Sender::Payer(1),
        };
        // TODO: Add more tests with syncing.
        //  sync_channel.send(vec![]).unwrap();

        // First request should succeed
        let response = limiter.check_rate_limit_blocking(request).unwrap();

        assert!(response.approved);
        assert_eq!(response.remaining_credits, 0);

        // Second request should fail - not enough credits
        let request2 = RateLimitRequest {
            request_data: RequestData::Query { limit: 800 },
            sender: Sender::Payer(1),
        };

        let response = limiter.check_rate_limit_blocking(request2.clone()).unwrap();
        assert!(!response.approved);
        assert_eq!(response.remaining_credits, 0);
        // Interleaving another request, that should also fail.
        // User request should fail - not enough credits
        let request_ip = RateLimitRequest {
            request_data: RequestData::Query { limit: 210 },
            sender: Sender::Unknown(Ipv4Addr::new(127, 0, 0, 1)),
        };

        let response = limiter
            .check_rate_limit_blocking(request_ip.clone())
            .unwrap();
        assert!(!response.approved);
        assert_eq!(response.remaining_credits, 20);

        // Wait for refill
        sleep(Duration::from_millis(500));

        // Request should now succeed after refill
        let response = limiter.check_rate_limit_blocking(request2.clone()).unwrap();

        assert!(response.approved);
        assert_eq!(response.remaining_credits, 10);
    }
}
