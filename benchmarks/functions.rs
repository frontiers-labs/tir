//! One function declaration feeds Criterion, Cachegrind, and result discovery.
#[cfg(feature = "nightly-cachegrind")]
use std::hint::black_box;
use std::time::Duration;

pub type Group<'a> = criterion::BenchmarkGroup<'a, criterion::measurement::WallTime>;

#[derive(Default)]
pub struct Settings {
    pub bytes: Option<u64>,
    pub samples: Option<usize>,
    pub warmup: Option<Duration>,
    pub measurement: Option<Duration>,
}

impl Settings {
    pub fn apply(&self, group: &mut Group<'_>) {
        if let Some(bytes) = self.bytes {
            group.throughput(criterion::Throughput::Bytes(bytes));
        }
        if let Some(samples) = self.samples {
            group.sample_size(samples);
        }
        if let Some(duration) = self.warmup {
            group.warm_up_time(duration);
        }
        if let Some(duration) = self.measurement {
            group.measurement_time(duration);
        }
    }
}

enum Runner<'a, 'b> {
    Native {
        group: &'a mut Group<'b>,
        name: &'a str,
    },
    #[cfg(feature = "nightly-cachegrind")]
    Cachegrind,
}

pub struct Bencher<'a, 'b> {
    runner: Runner<'a, 'b>,
    calls: usize,
}

impl<'a, 'b> Bencher<'a, 'b> {
    pub fn native(group: &'a mut Group<'b>, name: &'a str) -> Self {
        Self {
            runner: Runner::Native { group, name },
            calls: 0,
        }
    }

    #[cfg(feature = "nightly-cachegrind")]
    pub fn profile() -> Self {
        Self {
            runner: Runner::Cachegrind,
            calls: 0,
        }
    }

    /// Measure the operation, including destruction of its return value.
    pub fn iter<O>(&mut self, mut operation: impl FnMut() -> O) {
        self.calls += 1;
        match &mut self.runner {
            Runner::Native { group, name } => {
                group.bench_function(*name, |b| b.iter(&mut operation));
            }
            #[cfg(feature = "nightly-cachegrind")]
            Runner::Cachegrind => {
                gungraun::client_requests::cachegrind::start_instrumentation();
                black_box(operation());
                gungraun::client_requests::cachegrind::stop_instrumentation();
            }
        }
    }

    /// Exclude input preparation and output destruction from measurement.
    pub fn iter_batched<I, O>(
        &mut self,
        mut setup: impl FnMut() -> I,
        mut operation: impl FnMut(I) -> O,
    ) {
        self.calls += 1;
        match &mut self.runner {
            Runner::Native { group, name } => {
                group.bench_function(*name, |b| {
                    b.iter_batched(&mut setup, &mut operation, criterion::BatchSize::SmallInput);
                });
            }
            #[cfg(feature = "nightly-cachegrind")]
            Runner::Cachegrind => {
                let input = black_box(setup());
                gungraun::client_requests::cachegrind::start_instrumentation();
                let output = black_box(operation(input));
                gungraun::client_requests::cachegrind::stop_instrumentation();
                drop(output);
            }
        }
    }

    pub fn finish(self) {
        assert_eq!(
            self.calls, 1,
            "each benchmark must measure exactly one operation"
        );
    }
}

#[cfg(feature = "nightly-cachegrind")]
pub fn cachegrind_config() -> gungraun::LibraryBenchmarkConfig {
    use gungraun::{Cachegrind, LibraryBenchmarkConfig, ValgrindTool};
    let mut config = LibraryBenchmarkConfig::default();
    config
        .default_tool(ValgrindTool::Cachegrind)
        .tool(Cachegrind::with_args([
            "--instr-at-start=no",
            "--cache-sim=yes",
            "--branch-sim=yes",
            "--I1=32768,8,64",
            "--D1=32768,8,64",
            "--LL=8388608,16,64",
        ]));
    config
}

pub struct Case {
    pub function_name: &'static str,
    pub benchmark: &'static str,
    pub settings: Settings,
    pub run: fn(&mut Bencher<'_, '_>),
}

pub fn describe(cases: &[Case], compiler: &str, inputs: &[&str]) -> serde_json::Value {
    let cases: Vec<_> = cases
        .iter()
        .map(|case| {
            serde_json::json!({
                "function_name": case.function_name,
                "id": null,
                "benchmark": case.benchmark,
                "compiler": compiler,
            })
        })
        .collect();
    serde_json::json!({"kind": "function", "cases": cases, "inputs": inputs})
}

pub fn run_native(cases: &[Case]) {
    let mut criterion = criterion::Criterion::default().configure_from_args();
    let mut groups = std::collections::BTreeMap::<_, Vec<_>>::new();
    for case in cases {
        let (group, name) = case
            .benchmark
            .rsplit_once('/')
            .expect("benchmark name needs a group/case");
        groups.entry(group).or_default().push((name, case));
    }
    for (name, cases) in groups {
        let mut group = criterion.benchmark_group(name);
        for (name, case) in cases {
            case.settings.apply(&mut group);
            let mut bencher = Bencher::native(&mut group, name);
            (case.run)(&mut bencher);
            bencher.finish();
        }
        group.finish();
    }
    criterion.final_summary();
}

macro_rules! benchmarks {
    (@settings) => { $crate::functions::Settings::default() };
    (@settings $settings:expr) => { $settings };
    (@inputs) => { &[] as &[&str] };
    (@inputs $inputs:expr) => { $inputs };
    (compiler = $compiler:literal $(, inputs = $inputs:expr)?; $(
        $name:ident($label:literal $(, $settings:expr)?) |$bencher:ident| $body:block
    )+) => {
        $(fn $name($bencher: &mut $crate::functions::Bencher<'_, '_>) $body)+

        #[cfg(feature = "nightly-cachegrind")]
        mod profiling {
            use gungraun::{library_benchmark_group, main};
            $(
                #[gungraun::library_benchmark]
                fn $name() {
                    let mut bencher = $crate::functions::Bencher::profile();
                    $crate::$name(&mut bencher);
                    bencher.finish();
                }
            )+
            gungraun::library_benchmark_group!(name = benches, benchmarks = [$($name),+]);
            gungraun::main!(
                config = $crate::functions::cachegrind_config(),
                library_benchmark_groups = benches
            );
            pub fn run() { main(); }
        }

        fn main() {
            let cases = [$($crate::functions::Case {
                function_name: stringify!($name),
                benchmark: $label,
                settings: benchmarks!(@settings $($settings)?),
                run: $name,
            }),+];
            if std::env::args().any(|arg| arg == "--describe") {
                println!("{}", $crate::functions::describe(
                    &cases, $compiler, benchmarks!(@inputs $($inputs)?),
                ));
                return;
            }
            #[cfg(feature = "nightly-cachegrind")]
            profiling::run();
            #[cfg(not(feature = "nightly-cachegrind"))]
            $crate::functions::run_native(&cases);
        }
    };
}
