use std::collections::HashMap;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use neemata_proxy::bench::{
    BenchStickySessions, RouterConfig, resolve_route_for_bench, strip_first_path_segment_for_bench,
};

fn router_config(route_count: usize) -> RouterConfig {
    let mut subdomain_routes = HashMap::with_capacity(route_count);
    let mut path_routes = HashMap::with_capacity(route_count);

    for index in 0..route_count {
        subdomain_routes.insert(format!("app-{index}.test"), format!("app-{index}"));
        path_routes.insert(format!("path-{index}"), format!("app-{index}"));
    }

    RouterConfig {
        subdomain_routes,
        path_routes,
        default_app: Some("default".to_string()),
        apps: HashMap::new(),
    }
}

fn bench_route_resolution(c: &mut Criterion) {
    let config = router_config(1_000);
    let mut group = c.benchmark_group("router/route_resolution");

    group.bench_function("subdomain_hit", |b| {
        b.iter(|| {
            black_box(resolve_route_for_bench(
                black_box(&config),
                black_box(Some("app-500.test")),
                black_box(None),
            ))
        });
    });

    group.bench_function("path_hit", |b| {
        b.iter(|| {
            black_box(resolve_route_for_bench(
                black_box(&config),
                black_box(None),
                black_box(Some("path-500")),
            ))
        });
    });

    group.bench_function("default_hit", |b| {
        b.iter(|| {
            black_box(resolve_route_for_bench(
                black_box(&config),
                black_box(Some("missing.test")),
                black_box(Some("missing")),
            ))
        });
    });

    group.finish();
}

fn bench_path_rewrite(c: &mut Criterion) {
    let mut group = c.benchmark_group("router/path_rewrite");

    group.bench_function("borrowed_without_query", |b| {
        b.iter(|| {
            black_box(strip_first_path_segment_for_bench(
                black_box("/app/api/users"),
                black_box("app"),
            ))
        });
    });

    group.bench_function("owned_with_query", |b| {
        b.iter(|| {
            black_box(strip_first_path_segment_for_bench(
                black_box("/app/api/users?page=1&limit=25"),
                black_box("app"),
            ))
        });
    });

    group.bench_function("boundary_miss", |b| {
        b.iter(|| {
            black_box(strip_first_path_segment_for_bench(
                black_box("/application/api/users"),
                black_box("app"),
            ))
        });
    });

    group.finish();
}

fn bench_sticky_sessions(c: &mut Criterion) {
    let mut group = c.benchmark_group("router/sticky_sessions");

    group.bench_function("lookup_hit", |b| {
        let state = BenchStickySessions::new(20_000);
        state.bind("client-1");

        b.iter(|| black_box(state.lookup(black_box("client-1"))));
    });

    group.bench_function("bind", |b| {
        b.iter_batched(
            || BenchStickySessions::new(20_000),
            |state| {
                state.bind(black_box("client-1"));
                black_box(state);
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("generate_affinity_key", |b| {
        let state = BenchStickySessions::new(20_000);
        b.iter(|| black_box(state.generate_affinity_key()));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_route_resolution,
    bench_path_rewrite,
    bench_sticky_sessions
);
criterion_main!(benches);
