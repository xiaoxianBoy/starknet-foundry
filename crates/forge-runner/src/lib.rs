use crate::compiled_runnable::{CompiledTestCrateRunnable, FuzzerConfig, TestCaseRunnable};
use crate::fuzzer::RandomFuzzer;
use crate::printing::print_test_result;
use crate::running::{run_fuzz_test, run_test};
use crate::test_case_summary::TestCaseSummary;
use crate::test_crate_summary::TestCrateSummary;
use anyhow::{anyhow, Result};

use cairo_lang_runner::RunnerError;
use cairo_lang_sierra::ids::ConcreteTypeId;
use cairo_lang_sierra::program::Function;

use futures::stream::FuturesUnordered;
use futures::StreamExt;

use build_trace_data::save_trace_data;
use profiler_api::run_profiler;
use smol_str::SmolStr;

use crate::forge_config::{ExecutionDataToSave, ForgeConfig, TestRunnerConfig};
use std::sync::Arc;
use test_case_summary::{AnyTestCaseSummary, Fuzzing};
use tokio::sync::mpsc::{channel, Sender};
use tokio::task::JoinHandle;
use universal_sierra_compiler_api::{compile_sierra_to_casm, AssembledProgramWithDebugInfo};

pub mod build_trace_data;
pub mod compiled_runnable;
pub mod expected_result;
pub mod forge_config;
pub mod profiler_api;
pub mod test_case_summary;
pub mod test_crate_summary;

mod fuzzer;
mod gas;
mod printing;
mod running;

pub const CACHE_DIR: &str = ".snfoundry_cache";

pub const BUILTINS: [&str; 8] = [
    "Pedersen",
    "RangeCheck",
    "Bitwise",
    "EcOp",
    "Poseidon",
    "SegmentArena",
    "GasBuiltin",
    "System",
];

/// Exit status of the runner
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum RunnerStatus {
    /// Runner exited without problems
    Default,
    /// Some test failed
    TestFailed,
    /// Runner did not run, e.g. when test cases got skipped
    DidNotRun,
}

pub trait TestCaseFilter {
    fn should_be_run(&self, test_case: &TestCaseRunnable) -> bool;
}

#[non_exhaustive]
pub enum TestCrateRunResult {
    Ok(TestCrateSummary),
    Interrupted(TestCrateSummary),
}

pub async fn run_tests_from_crate(
    tests: CompiledTestCrateRunnable,
    forge_config: Arc<ForgeConfig>,
    tests_filter: &impl TestCaseFilter,
) -> Result<TestCrateRunResult> {
    let sierra_program = &tests.sierra_program;
    let casm_program = Arc::new(compile_sierra_to_casm(sierra_program)?);

    let mut tasks = FuturesUnordered::new();
    let test_cases = tests.test_cases;
    // Initiate two channels to manage the `--exit-first` flag.
    // Owing to `cheatnet` fork's utilization of its own Tokio runtime for RPC requests,
    // test execution must occur within a `tokio::spawn_blocking`.
    // As `spawn_blocking` can't be prematurely cancelled (refer: https://dtantsur.github.io/rust-openstack/tokio/task/fn.spawn_blocking.html),
    // a channel is used to signal the task that test processing is no longer necessary.
    let (send, mut rec) = channel(1);

    for case in test_cases {
        let case_name = case.name.clone();

        if !tests_filter.should_be_run(&case) {
            tasks.push(tokio::task::spawn(async {
                // TODO TestCaseType should also be encoded in the test case definition
                Ok(AnyTestCaseSummary::Single(TestCaseSummary::Ignored {
                    name: case_name,
                }))
            }));
            continue;
        };

        let function = sierra_program
            .funcs
            .iter()
            .find(|f| f.id.debug_name.as_ref().unwrap().ends_with(&case_name))
            .ok_or(RunnerError::MissingFunction { suffix: case_name })?;

        let args = function_args(function, &BUILTINS);

        let case = Arc::new(case);
        let args: Vec<ConcreteTypeId> = args.into_iter().cloned().collect();

        tasks.push(choose_test_strategy_and_run(
            args,
            case,
            casm_program.clone(),
            forge_config.clone(),
            send.clone(),
        ));
    }

    let mut results = vec![];
    let mut interrupted = false;

    while let Some(task) = tasks.next().await {
        let result = task??;

        print_test_result(&result, forge_config.output_config.detailed_resources);
        maybe_save_execution_data(&result, forge_config.output_config.execution_data_to_save)?;

        if result.is_failed() && forge_config.test_runner_config.exit_first {
            interrupted = true;
            rec.close();
        }

        results.push(result);
    }

    let summary = TestCrateSummary {
        test_case_summaries: results,
        runner_exit_status: RunnerStatus::Default,
    };

    if interrupted {
        Ok(TestCrateRunResult::Interrupted(summary))
    } else {
        Ok(TestCrateRunResult::Ok(summary))
    }
}

fn maybe_save_execution_data(
    result: &AnyTestCaseSummary,
    execution_data_to_save: ExecutionDataToSave,
) -> Result<()> {
    if let AnyTestCaseSummary::Single(TestCaseSummary::Passed {
        name, trace_data, ..
    }) = result
    {
        match execution_data_to_save {
            ExecutionDataToSave::Trace => {
                save_trace_data(name, trace_data)?;
            }
            ExecutionDataToSave::TraceAndProfile => {
                let trace_path = save_trace_data(name, trace_data)?;
                run_profiler(name, &trace_path)?;
            }
            ExecutionDataToSave::None => {}
        }
    }
    Ok(())
}

fn choose_test_strategy_and_run(
    args: Vec<ConcreteTypeId>,
    case: Arc<TestCaseRunnable>,
    casm_program: Arc<AssembledProgramWithDebugInfo>,
    forge_config: Arc<ForgeConfig>,
    send: Sender<()>,
) -> JoinHandle<Result<AnyTestCaseSummary>> {
    if args.is_empty() {
        tokio::task::spawn(async move {
            let res = run_test(
                case,
                casm_program,
                forge_config.test_runner_config.clone(),
                send,
            )
            .await??;
            Ok(AnyTestCaseSummary::Single(res))
        })
    } else {
        tokio::task::spawn(async move {
            let res = run_with_fuzzing(
                args,
                case,
                casm_program,
                forge_config.test_runner_config.clone(),
                send,
            )
            .await??;
            Ok(AnyTestCaseSummary::Fuzzing(res))
        })
    }
}

fn run_with_fuzzing(
    args: Vec<ConcreteTypeId>,
    case: Arc<TestCaseRunnable>,
    casm_program: Arc<AssembledProgramWithDebugInfo>,
    test_runner_config: Arc<TestRunnerConfig>,
    send: Sender<()>,
) -> JoinHandle<Result<TestCaseSummary<Fuzzing>>> {
    tokio::task::spawn(async move {
        if send.is_closed() {
            return Ok(TestCaseSummary::Skipped {});
        }

        let (fuzzing_send, mut fuzzing_rec) = channel(1);
        let args = args
            .iter()
            .map(|arg| {
                arg.debug_name
                    .as_ref()
                    .ok_or_else(|| anyhow!("Type {arg:?} does not have a debug name"))
                    .map(SmolStr::as_str)
            })
            .collect::<Result<Vec<_>>>()?;

        let (fuzzer_runs, fuzzer_seed) = match case.fuzzer_config {
            Some(FuzzerConfig {
                fuzzer_runs,
                fuzzer_seed,
            }) => (fuzzer_runs, fuzzer_seed),
            _ => (
                test_runner_config.fuzzer_runs,
                test_runner_config.fuzzer_seed,
            ),
        };
        let mut fuzzer = RandomFuzzer::create(fuzzer_seed, fuzzer_runs, &args)?;

        let mut tasks = FuturesUnordered::new();

        for _ in 1..=fuzzer_runs.get() {
            let args = fuzzer.next_args();

            tasks.push(run_fuzz_test(
                args,
                case.clone(),
                casm_program.clone(),
                test_runner_config.clone(),
                send.clone(),
                fuzzing_send.clone(),
            ));
        }

        let mut results = vec![];
        while let Some(task) = tasks.next().await {
            let result = task??;

            results.push(result.clone());

            if let TestCaseSummary::Failed { .. } = result {
                fuzzing_rec.close();
                break;
            }
        }

        let runs = u32::try_from(
            results
                .iter()
                .filter(|item| {
                    matches!(
                        item,
                        TestCaseSummary::Passed { .. } | TestCaseSummary::Failed { .. }
                    )
                })
                .count(),
        )?;

        let fuzzing_run_summary: TestCaseSummary<Fuzzing> = TestCaseSummary::from(results);

        if let TestCaseSummary::Passed { .. } = fuzzing_run_summary {
            // Because we execute tests parallel, it's possible to
            // get Passed after Skipped. To treat fuzzing a test as Passed
            // we have to ensure that all fuzzing subtests Passed
            if runs != fuzzer_runs.get() {
                return Ok(TestCaseSummary::Skipped {});
            };
        };

        Ok(fuzzing_run_summary)
    })
}

fn function_args<'a>(function: &'a Function, builtins: &[&str]) -> Vec<&'a ConcreteTypeId> {
    let builtins: Vec<_> = builtins
        .iter()
        .map(|builtin| Some(SmolStr::new(builtin)))
        .collect();

    function
        .signature
        .param_types
        .iter()
        .filter(|pt| !builtins.contains(&pt.debug_name))
        .collect()
}
