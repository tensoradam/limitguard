use distributed_rate_limiter::{
    RateLimitRequest, RateLimiterBuilder, RequestData, Sender,
};
use std::net::Ipv4Addr;
use std::time::Duration;

fn main() {
    // Create a rate limiter with 100 initial credits, refilling 10 per second.
    let (mut rate_limiter, _sync_channel) = RateLimiterBuilder::new(1)
        .initial(100)
        .refill(10)
        .interval(Duration::from_secs(1))
        .max(200)
        .max_payer(500)
        .build(1) // 1 participating node
        .unwrap();

    // Check rate limit for a query request from a regular user.
    let request = RateLimitRequest {
        request_data: RequestData::Query { limit: 50 }, // Costs 5 credits (50/10)
        sender: Sender::Unknown(Ipv4Addr::new(127, 0, 0, 1)),
    };

    match rate_limiter.check_rate_limit_blocking(request) {
        Ok(response) => {
            if response.approved {
                eprintln!("Request approved! Remaining credits: {}", response.remaining_credits);
            } else {
                eprintln!("Request denied. Remaining credits: {}", response.remaining_credits);
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }

    // Payer gets higher rate limits
    let payer_request = RateLimitRequest {
        request_data: RequestData::Publish { num_messages: 10 }, 
        sender: Sender::Payer(123),
    };

    // Node requests are always allowed
    let node_request = RateLimitRequest {
        request_data: RequestData::Subscribe { num_topics: 5 }, 
        sender: Sender::Node(456),
    };

    // Test the payer request.
    match rate_limiter.check_rate_limit_blocking(payer_request) {
        Ok(response) => {
            if response.approved {
                eprintln!("Payer request approved! Remaining credits: {}", response.remaining_credits);
            } else {
                eprintln!("Payer request denied. Remaining credits: {}", response.remaining_credits);
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }

    // Test the node request.
    match rate_limiter.check_rate_limit_blocking(node_request) {
        Ok(response) => {
            if response.approved {
                eprintln!("Node request approved! Remaining credits: {}", response.remaining_credits);
            } else {
                eprintln!("Node request denied. Remaining credits: {}", response.remaining_credits);
            }
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}
