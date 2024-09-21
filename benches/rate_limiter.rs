use criterion::{black_box, criterion_group, criterion_main, Criterion};
use distributed_rate_limiter::{RateLimitRequest, RateLimiterBuilder, RequestData, Sender};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::runtime::Runtime;

fn create_test_request(request_type: RequestData, sender: Sender) -> RateLimitRequest {
    RateLimitRequest {
        request_data: request_type,
        sender,
    }
}

fn bench_rate_limiter(c: &mut Criterion) {
    let _rt = Runtime::new().unwrap();
    let (mut limiter, _) = RateLimiterBuilder::new(1)
        .initial(100) // Start with 100 credits
        .refill(50) // Refill 50 credits
        .interval(Duration::from_secs(1)) // Every second
        .max(100)
        .max_payer(100)
        .build(1)
        .unwrap();
    let mut group = c.benchmark_group("rate_limiter");

    // Benchmark different request types
    let query_request = create_test_request(RequestData::Query { limit: 100 }, Sender::Payer(1));

    let publish_request =
        create_test_request(RequestData::Publish { num_messages: 10 }, Sender::Node(1));

    let subscribe_request = create_test_request(
        RequestData::Subscribe { num_topics: 5 },
        Sender::Unknown(Ipv4Addr::new(127, 0, 0, 1)),
    );

    // Benchmark query requests
    group.bench_function("query_request", |b| {
        b.iter(|| black_box(limiter.check_rate_limit_blocking(query_request.clone())))
    });
    // Benchmark query requests (non-blocking)
    group.bench_function("query_request_non_blocking", |b| {
        b.iter(|| black_box(limiter.check_rate_limit_non_blocking(query_request.clone())))
    });
    // Benchmark publish requests (non-blocking)
    group.bench_function("publish_request_non_blocking", |b| {
        b.iter(|| black_box(limiter.check_rate_limit_non_blocking(publish_request.clone())))
    });

    // Benchmark subscribe requests (non-blocking)
    group.bench_function("subscribe_request_non_blocking", |b| {
        b.iter(|| black_box(limiter.check_rate_limit_non_blocking(subscribe_request.clone())))
    });

    // Benchmark concurrent requests
    group.bench_function("concurrent_requests", |b| {
        b.iter(|| {
            let requests = vec![
                query_request.clone(),
                publish_request.clone(),
                subscribe_request.clone(),
            ];
            black_box(
                requests
                    .into_iter()
                    .map(|req| limiter.check_rate_limit_non_blocking(req))
                    .collect::<Vec<_>>(),
            )
        });
    });

    group.finish();
}

criterion_group!(benches, bench_rate_limiter);
criterion_main!(benches);
