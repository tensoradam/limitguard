# A (credit-weighted) distributed rate limiter

## Summary

This project implements a distributed rate limiter for any network to protect against denial of service and abuse. The system enforces different rate limits based on client types (regular users, payers, and other nodes) and request types (query, publish, subscribe).

The implementation uses a credit-weighted token bucket approach that:
- Assigns different credit budgets to different client types
- Deducts credits based on request parameters
- Refills credits at regular intervals
- Coordinates across multiple nodes to maintain global rate limits
- Handles high availability configurations
- Maintains acceptable performance (under 5ms overhead)
- Allows for a 10% error margin in rate limiting

The system is designed to handle thousands of requests per second while ensuring that clients don't exceed their global rate limits across the entire network.

## Quick Example

```rust
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
        request_data: RequestData::Query { limit: 50 }, // Costs 5 credits (50/10).
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
        request_data: RequestData::Publish { num_messages: 10 }, // Costs 10 credits
        sender: Sender::Payer(123),
    };

    // Node requests are always allowed
    let node_request = RateLimitRequest {
        request_data: RequestData::Subscribe { num_topics: 5 }, // Costs 5 credits
        sender: Sender::Node(456),
    };
}
```

## Setup
We have to assume that all participating Nodes act honestly.
At any moment, the network has N active nodes. N may change over time.

> Each logical node (1 of the N) may be divided into multiple API services for load balancing and HA reasons.



## First thoughts
We can loosely divide the problem into two:
a) A (local) rate limiter; and
b) A distributed state syncing mechanism.

We want a system that works well locally, until it receives enough traffic that it needs to "pause" to coordinate with all nodes.
That coordination pause also acts as part of our rate limiting scheme.


# Review of Related Work

https://blog.cloudflare.com/counting-things-a-lot-of-different-things/

https://github.com/copyleftdev/viz-buckets



## Citations and Related Work

### Token Bucket and Leaky Bucket Algorithms

- Wong, F., & Tan, M. (2014). "A Survey of Distributed Rate Limiting Algorithms." ACM Computing Surveys.
  Comprehensive overview of distributed rate limiting approaches including token bucket variants.

- Raghavan, B., et al. (2007). "Cloud Control with Distributed Rate Limiting." SIGCOMM '07.
  Seminal paper introducing distributed rate limiting using a token bucket approach.

- Kulkarni, S., et al. (2015). "Global Rate Limiting in Cloud Platforms." USENIX ATC '15.
  Describes practical implementation of distributed rate limiting at scale.

### Distributed Systems

- Karger, D., & Ruhl, M. (2004). "Simple Efficient Load Balancing Algorithms for Peer-to-Peer Systems." SPAA '04.
  Discusses load balancing techniques applicable to distributed rate limiting.

- Adya, A., et al. (2016). "Centrifuge: Integrated Lease Management and Partitioning for Cloud Services." NSDI '16.
  Details lease-based coordination for distributed systems.

### Industry Implementations

- "Throttling API requests using Redis and Lua" (2014) - Instagram Engineering Blog
  Describes Instagram's token bucket implementation using Redis.

- "Rate Limiting at Scale" (2016) - Cloudflare Blog
  Details Cloudflare's distributed rate limiting architecture.

- "Global Rate Limiting" (2017) - Stripe Engineering Blog
  Explains Stripe's approach to distributed rate limiting across data centers.

### Alternative Approaches

- Zhou, W., et al. (2019). "Adaptive Rate Limiting via Continuous Learning." NSDI '19.
  Explores machine learning approaches to rate limiting.

- Krishnamurthy, B., et al. (2001). "On the Use and Performance of Content Distribution Networks." IMW '01.
  Discusses CDN-based rate limiting strategies.


## A token bucket approach

We focus on a (credit) token bucket approach a bit modified to disallow burst traffic.
Our other consideration is not to have to keep track of complex notions, such as slots.
And we want a lightweight representation that will ensure we use minimal memory space.

We setup a bucket with an initial credit budget.
Then upon an incoming request we subtract the require number of credits. (If there are enough.)
How do we protect against burst traffic though, without keeping track of slots.
We set a deadline, which is the next time we will refill the bucket.

Namely, starting after the deadline, we split time in windows based upon an interval parameter, and award each bucket with that many credits -- up to a miximum.
That allows us to *reward* spread out traffic, offers us some granularity over spiky traffic.
As we only refill the bucket with credits *after* the deadline has passed.
In particular, if a user makes 3 in succession requests, each costing about 1/3 of the credits in the bucket, would force us to coordinate with the other nodes prior to serving that user with more requests.

Underneath we need to keep track a table of senders and buckets.
That is realized by the Leaser data structure.

For each sender request it finds or initializes a bucket.
After it recalculates the buckets correct credit balance via drain_bucket(.), it needs to decide if we need to coordinate across the system.
This is done via sync_necessary(.)

We calculate if granting the user request will be within our error margin if the user is making the same request pattern (or worse) with every node.
(This assumes that every node of course acts honestly, but that is the prerequisite of any rate limiting scheme.)


Here if we need to coordinate that means we need a data structure that will merge the same across all nodes.

When we sync, we try to rebuild a common global state.
Here, for speed of implementation and simplicity we err to assume the worst case for user traffic, and award minimal credits to the user.
(We do that by picking the latest deadline of all the buckets.)
Essentially, on sync, we are reconstructing a common bucket between all nodes.

For simplicity, in our prototype, when we need to sync, we sync the state for all senders, that is the whole Leaser structure.

In high availability mode of course, we can set, each bucket to 1/N over the global bucket limit and force the user to do the load balancing for us.
And we avoid all the need for syncing between nodes.

Finally, we need to check if the bucket has enough credits and subtract from the currect token amount the bucket holds.
We do that with touch(.).

### Removing Nodes from the system

While adding Nodes, effectively means altering the participants parameter to be more restrictive as to the requests taken, removing Nodes can be more problematic, if at the same time we are trying  to sync past bucket history.
If that affects the RateLimiter's accuracy beyond what is acceptable, we can effectively set a Node to deny all requests until a sync operation occurs then we can take it down.

FIXME: The code as is currently requires simply allows altering the (number of) `participants` variable.


# Credit Weights System

For the particular test library in question we need the

| Type | User Identification | Rate Limit |
| --- | --- | --- |
| Everyone else |  Base rate |
| Payers |  5X the Base rate |
| Other Node | Unlimited |


To keep our implementation simple and configurable, we simply allow upon construction of the identifier what the base rate of Payers and non payers is.
A sender that is a node is allowed

### Request Types

In this simplified model there are three types of requests, each of which cost a specific amount of credits based on the request parameters.

| Request Type | Credits |
| --- | --- |
| Query | The `limit` parameter divided by 10 (a request with a limit of 100 would cost 10 credits) |
| Publish | 100 credits per message published |
| Subscribe | 10 credits per topic in the request body |


# Alternative approaches

Alternative approaches would be to use express leases between nodes or keep track of a sliding interval.
Or granting leases directly to users, in the form of enriched bearer tokens.
However, we would have to change the way our reqwester works perhaps or add an extra endpoint to grant the lease.

If we had picked a leaky bucket implementation might become more complex due to the multiple base rates, which would require implementing a hierarchical leaky bucket.


The sliding interval is the most complex solution -- and computational complex -- as we have to keep track globally of each request.
We avoided that mainly due to time constraints.
Any solution that minimizes the need for coordination was more desirable.

Furthermore, we wanted to allow for effective budget usage.
That would count against a naive leaky bucket approach, with a fixed time frame.

# Error rate

## Adding participating nodes

Removing nodes is not a problem in our accuracy rate per se.
To keep the implementation simple we err on the side of freezing while awaiting to sync state between all participants.


# The 5ms problem
Performance outside the need of coordination with other nodes was not really a concern.
Given the lack of specification on the network topology we can not infer positively performance assumption related to network speed.

# Garbage Collection

An issue we have to address is garbage collection of the in-memory data structure leases.
For instance, if an IP queries a node, how long do we keep track of that IP
We err on the side of keeping it simple, and we simply introduce functionality to drop the past buckets.

An initial approach was offer a generational garbage collection.
That however, adds quickly more complexity than is necessary for a prototype.
The specification further, offers only a 5ms performance overhead.
Even at hundreds of thousands of sender entries dropped, we should not have a significant latency issue.


N.B. We can assume all protocol participants are offset by uniformly at random time windows, thus garbage collection between protocol participants should not interfere with the overall system responsiveness.

We can later improve this first implementation to offer stricter guarantees.

# High Availability

Even if locally per node we expose multiple logical endpooints this does not alter our design.
We can either provide the ratelimiter as a shared service in-process to each HTTP handling worker (via a channel or queue), or share it via a lock.

If we split a node locally across multiple processes, we might need to add extra logic or optimize accordingly.
As a first step though we should treat each local process as being its own logical RateLimiter participant.

# Nix / Development Environment

If the user has no local Rust environment, they can use Nix instead:

```shell
# Enter development shell
nix develop

# Build and run tests
nix build
```

Build and test via cargo.
```shell
cargo build
cargo test
```
