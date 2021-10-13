use std::fmt::Display;

mod blobs;
mod logs;

pub trait SimpleBench: Sized {
    type Config: BenchConfig;
    type GroupState;
    const BACKEND: &'static str;

    fn should_execute() -> bool {
        true
    }

    fn can_execute() -> bool {
        Self::should_execute()
    }

    fn run(group: &mut criterion::BenchmarkGroup<WallTime>, config: &Self::Config) {
        if !Self::can_execute() {
            return;
        }

        // When tracing is enabled, we output flamegraphs of the benchmarks.
        #[cfg(feature = "tracing")]
        {
            use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
            let fmt_layer = tracing_subscriber::fmt::Layer::default();

            let (flame_layer, _guard) = tracing_flame::FlameLayer::with_file(format!(
                "{}-{}.folded",
                Self::BACKEND,
                config
            ))
            .unwrap();
            let filter_layer = tracing_subscriber::EnvFilter::try_from_default_env()
                .or_else(|_| tracing_subscriber::EnvFilter::try_new("info"))
                .unwrap();

            let subscriber = tracing_subscriber::Registry::default()
                .with(flame_layer)
                .with(filter_layer)
                .with(fmt_layer);

            tracing::subscriber::with_default(subscriber, || bench.execute_iterations(config))
        }

        #[cfg(not(feature = "tracing"))]
        {
            Self::execute_iterations(group, config)
        }
    }

    fn initialize_group(
        config: &Self::Config,
        group_state: &<Self::Config as BenchConfig>::GroupState,
    ) -> Self::GroupState;

    fn initialize(
        group_state: &Self::GroupState,
        config: &Self::Config,
        config_group_state: &<Self::Config as BenchConfig>::GroupState,
    ) -> Result<Self, anyhow::Error>;

    fn execute_measured(&mut self, config: &Self::Config) -> Result<(), anyhow::Error>;

    fn execute_iterations(group: &mut BenchmarkGroup<WallTime>, config: &Self::Config) {
        let config_group_state = config.initialize_group();
        let group_state = Self::initialize_group(config, &config_group_state);
        group.throughput(config.throughput());
        group.bench_with_input(
            BenchmarkId::new(Self::BACKEND, config),
            config,
            |b, config| {
                let mut bench =
                    Self::initialize(&group_state, config, &config_group_state).unwrap();
                b.iter(|| bench.execute_measured(config))
            },
        );
    }
}

pub trait BenchConfig: Display {
    type GroupState;
    type State: Iterator<Item = Self::Batch>;
    type Batch;

    fn initialize_group(&self) -> Self::GroupState;

    fn initialize(&self, group_state: &Self::GroupState) -> Self::State;

    fn throughput(&self) -> Throughput;
}

pub trait NebariBenchmark {
    const BACKEND: &'static str;
    type Root: Root;
}

pub struct VersionedBenchmark;
pub struct UnversionedBenchmark;

impl NebariBenchmark for VersionedBenchmark {
    const BACKEND: &'static str = "nebari-versioned";

    type Root = VersionedTreeRoot;
}

impl NebariBenchmark for UnversionedBenchmark {
    const BACKEND: &'static str = "nebari";

    type Root = UnversionedTreeRoot;
}

use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, BenchmarkId, Criterion,
    Throughput,
};
use nebari::tree::{Root, UnversionedTreeRoot, VersionedTreeRoot};

fn all_benches(c: &mut Criterion) {
    blobs::benches(c);
    logs::benches(c);
}

criterion_group!(benches, all_benches);
criterion_main!(benches);
