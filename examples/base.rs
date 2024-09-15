use crossbeam::channel;
use distributed_rate_limiter::{
    RateLimitRequest, RateLimiter, RateLimiterBuilder, RequestData, Sender,
};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

struct Node {
    id: u64,
    rate_limiter: RateLimiter,
    sync_channel: channel::Sender<Vec<HashMap<Sender, (u64, Instant)>>>,
}

impl Node {
    fn new(id: u64, total_nodes: u64) -> Self {
        let (rate_limiter, sync_channel) = RateLimiterBuilder::new(id)
            .initial(100) // Start with 100 credits
            .refill(29) // Refill 50 credits
            .max_payer(40) // Allow 1000 credits for payers
            .max(300) // Allow 1000 credits in total
            .interval(Duration::from_secs(1)) // Every second
            .build(total_nodes)
            .unwrap();

        Self {
            id,
            rate_limiter,
            sync_channel,
        }
    }

    // Simulates syncing with other nodes by getting their rate limiter states
    fn sync_with_others(&self, nodes: &[Node]) -> Vec<HashMap<Sender, (u64, Instant)>> {
        // Ensure we only pick nodes that are not ourselves.
        let states = nodes
            .iter()
            .filter(|n| n.id != self.id)
            .map(|n| n.rate_limiter.get_state())
            .collect();
        // We simulate the fact that we don't know the order of the nodes communicating with us.
        // Helps with testing.
        let permuted_states = permute_states(states);
        self.sync_channel.send(permuted_states.clone()).unwrap();
        permuted_states
    }
}

fn main() {
    // Create 10 nodes
    let mut nodes: Vec<Node> = (0..10).map(|id| Node::new(id, 10)).collect();

    // Simulate some usage and syncing
    for i in 0..3 {
        eprintln!("Round {}", i + 1);

        for i in 0..10 {
            // Try to use 30 credits
            match nodes[i]
                .rate_limiter
                .check_rate_limit_blocking(RateLimitRequest {
                    request_data: RequestData::Query { limit: 300 },
                    sender: Sender::Unknown(Ipv4Addr::new(127, 0, 0, 1)),
                }) {
                Ok(_) => eprintln!("Node {} successfully used 30 credits", nodes[i].id),
                Err(_) => eprintln!("Node {} failed to acquire credits", nodes[i].id),
            }

            // Get states from other nodes
            let other_states = nodes[i].sync_with_others(&nodes);
            eprintln!(
                "Node {} synced with {} other nodes",
                nodes[i].id,
                other_states.len()
            );
        }

        // Wait a bit between rounds
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn permute_states(
    mut states: Vec<HashMap<Sender, (u64, Instant)>>,
) -> Vec<HashMap<Sender, (u64, Instant)>> {
    use rand::Rng;
    // FIXME: Alter code later to not use thread_rng.
    #[allow(deprecated)]
    let mut rng = rand::thread_rng();

    for i in (1..states.len()).rev() {
        // Generate random index between 0 and i inclusive
        let j = rng.random_range(0..=i);
        // Swap elements at indices i and j
        states.swap(i, j);
    }

    states
}
