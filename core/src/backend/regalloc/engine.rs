use tir_egglog::{EGraph, Program, RawValues, Schedule, Value, Write};

use crate::{Context, OpId, PassError, TypeId};

const MAX_REPAIRS: usize = 64;
const JOURNALS: &[&str] = &[
    "RATransferNative",
    "RAGeneratedTemporary",
    "RAFrameSlot",
    "RAFrameJoinedState",
    "RAAllocationOrder",
    "RAFrameEntryState",
    "RAFrameMemory",
    "RAFrameSaveInstalled",
    "RAFrameInstalled",
    "RAAllocationPin",
    // These immutable constructor rows are referenced by journal outputs.
    "RAFS",
    "RATCopy",
];

pub(super) fn run(context: &Context, root: OpId) -> Result<(), PassError> {
    let memory = context.terms().expression(TypeId::STATE);
    let mut staged = context.clone_engine();
    allocate(&mut staged, root, memory).map_err(invalid_rules)?;
    Context::refresh_engine_indexes(&mut staged).map_err(invalid_rules)?;
    let previous = context.replace_engine(staged);
    if let Err(error) = crate::operation::verify_op_tree(context, root)
        .and_then(|()| crate::backend::verify_machine_ir(context, root))
    {
        context.replace_engine(previous);
        return Err(PassError::InvalidIR {
            pass: "register-allocation",
            error,
        });
    }
    Ok(())
}

fn invalid_rules(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(format!("register allocation: {error}"))
}

fn allocate(engine: &mut EGraph, root: OpId, memory: Value) -> Result<(), tir_egglog::Error> {
    engine.update(|mut state| {
        state.add("Scope", i64::from(root.raw()))?;
        Ok(())
    })?;
    Context::refresh_engine_indexes(engine)?;
    let span = tir_egglog::span!();
    for &(name, presort, args) in crate::schema::REGALLOC_SORTS {
        if engine.database().schema().sort_named(name).is_none() {
            let declaration = presort.map(|kind| {
                (
                    kind.to_owned(),
                    args.iter()
                        .map(|arg| tir_egglog::ast::Expr::Var(span, (*arg).to_owned()))
                        .collect(),
                )
            });
            engine.declare_sort(name, &declaration, span)?;
        }
    }
    // Loading first installs the tables, including on a fresh Context.
    Program::from_static(crate::schema::REGALLOC_PROGRAM, engine.database_mut())
        .map_err(|error| failure(error.to_string()))?;
    for attempt in 0..=MAX_REPAIRS {
        if std::env::var_os("TIR_TIME_PASSES").is_some() {
            eprintln!("regalloc attempt {attempt}");
        }
        for table in crate::schema::REGALLOC_TABLES {
            if !JOURNALS.contains(table) {
                engine.clear_function(table)?;
            }
        }
        // Reset VM cursors after clearing tables; timestamps from the previous
        // candidate cannot describe a new allocation attempt.
        let mut program =
            Program::from_static(crate::schema::REGALLOC_PROGRAM, engine.database_mut())
                .map_err(|error| failure(error.to_string()))?;
        seed(engine, root, memory)?;
        phases(
            &mut program,
            engine,
            &[
                "ra-control-functions",
                "ra-control-inputs",
                "ra-structured-index",
                "ra-control-import",
                "ra-transfer-persist",
            ],
        )?;
        let functions = rows(engine, "RAFunction")?;
        let allocated = rows(engine, "RAAllocatedFunction")?;
        if functions.len() + allocated.len() != rows(engine, "RARequestedFunction")?.len() {
            return Err(failure(
                "allocation requires selected graphs or allocated CFGs",
            ));
        }
        if functions.is_empty() {
            if !allocated.is_empty() {
                for table in crate::schema::REGALLOC_TABLES {
                    engine.clear_function(table)?;
                }
                return Ok(());
            }
            return Err(failure("no selected graph function in allocation scope"));
        }
        phases(
            &mut program,
            engine,
            &["ra-structured-lanes", "ra-structured-plan"],
        )?;
        steps(
            &mut program,
            engine,
            &[
                "ra-structured-blocked",
                "ra-structured-ready",
                "ra-structured-step",
            ],
        )?;
        phases(
            &mut program,
            engine,
            &["ra-structured-emit", "ra-control-import"],
        )?;
        phases(
            &mut program,
            engine,
            &[
                "ra-control-normalize",
                "ra-bind-index",
                "ra-bind-count",
                "ra-bind-slots",
                "ra-bind-require",
                "ra-bind-fallback-index",
                "ra-bind-fallback",
                "ra-bind-require",
                "ra-bind-domain",
                "ra-abi-index",
                "ra-abi-bind",
                "ra-bind-check",
                "ra-bind-complete",
                "ra-control-pin",
                "ra-control-entry",
                "ra-control-reach",
                "ra-control-dom",
                "ra-control-demand",
                "ra-control-mandatory",
                "ra-control-mandatory-bad",
                "ra-control-mandatory-legal",
                "ra-control-mandatory-extrema",
                "ra-control-pin",
                "ra-control-demand",
                "ra-control-bad",
                "ra-control-legal",
                "ra-control-extrema",
                "ra-control-place",
                "ra-control-pred",
                "ra-control-seed",
            ],
        )?;
        steps(
            &mut program,
            engine,
            &[
                "ra-control-blocked",
                "ra-control-priority",
                "ra-control-ready",
                "ra-control-step",
            ],
        )?;
        phases(
            &mut program,
            engine,
            &[
                "ra-control-finish",
                "ra-control-check",
                "ra-control-complete",
                "ra-select-abi",
            ],
        )?;
        require_functions(engine, &functions, "RAControlComplete")?;
        require_functions(engine, &functions, "RABindingsComplete")?;
        phases(
            &mut program,
            engine,
            &[
                "ra-color-import",
                "ra-color-live",
                "ra-color-interfere",
                "ra-color-reject",
                "ra-color-domain",
                "ra-color-seed",
            ],
        )?;
        steps(
            &mut program,
            engine,
            &[
                "ra-color-blocked",
                "ra-color-priority",
                "ra-color-saturation",
                "ra-color-degree",
                "ra-color-ready",
                "ra-color-step",
            ],
        )?;
        phases(
            &mut program,
            engine,
            &[
                "ra-color-finish",
                "ra-color-check",
                "ra-color-complete",
                "ra-select-index",
                "ra-select-cost",
                "ra-select-best",
                "ra-select-choice",
            ],
        )?;
        require_functions(engine, &functions, "RASelectedCandidate")?;
        let candidates = rows(engine, "RASelectedCandidate")?;
        if candidates.len() != functions.len() {
            return Err(failure("candidate selection is not unique"));
        }
        if !rows(engine, "RAGraphFunction")?.is_empty() {
            phases(
                &mut program,
                engine,
                &[
                    "ra-publish-plan",
                    "ra-publish-check",
                    "ra-publish-ready",
                    "ra-publish-delete",
                    "ra-publish-install",
                ],
            )?;
            require_functions(engine, &rows(engine, "RAGraphFunction")?, "RAPublished")?;
            Context::refresh_engine_indexes(engine)?;
            continue;
        }
        phases(
            &mut program,
            engine,
            &[
                "ra-transfer-index",
                "ra-transfer-split-index",
                "ra-transfer-split",
                "ra-transfer-layout",
                "ra-transfer-emit",
                "ra-transfer-check",
                "ra-transfer-ready",
                "ra-transfer-delete",
                "ra-transfer-install",
            ],
        )?;
        if !rows(engine, "RATransferError")?.is_empty() {
            return Err(incomplete(engine, "RATransferReady")?);
        }
        if !rows(engine, "RATransferProgress")?.is_empty() {
            Context::refresh_engine_indexes(engine)?;
            continue;
        }
        phases(
            &mut program,
            engine,
            &[
                "ra-transfer-edge-index",
                "ra-transfer-edge-moves",
                "ra-transfer-edge-cycle",
                "ra-transfer-edge-plan",
            ],
        )?;
        steps(
            &mut program,
            engine,
            &[
                "ra-transfer-edge-blocked",
                "ra-transfer-edge-ready",
                "ra-transfer-edge-step",
            ],
        )?;
        phases(
            &mut program,
            engine,
            &[
                "ra-transfer-cycle-layout",
                "ra-transfer-cycle-store",
                "ra-transfer-frame-index",
                "ra-transfer-save-plan",
                "ra-transfer-adjust-index",
                "ra-transfer-adjust-plan",
                "ra-transfer-adjust",
                "ra-transfer-plan",
                "ra-transfer-memory-index",
                "ra-transfer-layout",
                "ra-transfer-memory-order",
                "ra-transfer-memory",
                "ra-transfer-memory-finish",
                "ra-transfer-recipe-index",
                "ra-transfer-recipe",
                "ra-transfer-address-index",
                "ra-transfer-address-plan",
                "ra-transfer-address-step",
                "ra-transfer-address-finish",
                "ra-transfer-layout",
                "ra-transfer-recipe-index",
                "ra-transfer-recipe",
                "ra-transfer-choose",
                "ra-transfer-emit",
                "ra-transfer-check",
                "ra-transfer-ready",
                "ra-transfer-delete",
                "ra-transfer-install",
            ],
        )?;
        if !rows(engine, "RATransferError")?.is_empty() {
            return Err(incomplete(engine, "RATransferReady")?);
        }
        if !rows(engine, "RATransferProgress")?.is_empty() {
            if attempt == MAX_REPAIRS {
                return Err(failure("repair limit reached"));
            }
            Context::refresh_engine_indexes(engine)?;
            continue;
        }
        phases(
            &mut program,
            engine,
            &["ra-transfer-frame-complete", "ra-transfer-verify-index"],
        )?;
        phases(
            &mut program,
            engine,
            &["ra-target-shapes", "ra-target-fields"],
        )?;
        phases(
            &mut program,
            engine,
            &["ra-transfer-verify-check", "ra-transfer-verify-complete"],
        )?;
        require_candidates(engine, &candidates, "RAColorComplete")?;
        require_candidates(engine, &candidates, "RATransferComplete")?;
        phases(
            &mut program,
            engine,
            &["ra-publish-plan", "ra-publish-check", "ra-publish-ready"],
        )?;
        require_candidates(engine, &candidates, "RAPublishReady")?;
        phases(
            &mut program,
            engine,
            &["ra-publish-delete", "ra-publish-install"],
        )?;
        require_candidates(engine, &candidates, "RAPublished")?;
        for table in crate::schema::REGALLOC_TABLES {
            engine.clear_function(table)?;
        }
        return Ok(());
    }
    Err(failure("repair limit reached"))
}

fn seed(engine: &mut EGraph, root: OpId, memory: Value) -> Result<(), tir_egglog::Error> {
    engine.update(|mut state| {
        let unsigned = state.add("Ctor", ("$attr.unsigned".to_owned(),))?;
        let signed = state.add("Ctor", ("$attr.signed".to_owned(),))?;
        let control = state.add("Ctor", ("son.control".to_owned(),))?;
        let nil = state.add("TNil", RawValues(vec![]))?;
        state.add("Apply", RawValues(vec![control, nil]))?;
        state.add("RAIntegerType", RawValues(vec![unsigned]))?;
        state.add("RASignedIntegerType", RawValues(vec![signed]))?;
        state.add("RAMemoryType", RawValues(vec![memory]))?;
        state.add("RARequest", (0_i64, i64::from(root.raw())))?;
        Ok(())
    })
}

fn phases(
    program: &mut Program,
    engine: &mut EGraph,
    names: &[&str],
) -> Result<(), tir_egglog::Error> {
    for name in names {
        let started = std::time::Instant::now();
        program.run(engine.database_mut(), &Schedule::saturate(*name), &[])?;
        if std::env::var_os("TIR_TIME_PASSES").is_some() {
            eprintln!("regalloc {name}: {:?}", started.elapsed());
        }
    }
    Ok(())
}

fn steps(
    program: &mut Program,
    engine: &mut EGraph,
    names: &[&str],
) -> Result<(), tir_egglog::Error> {
    let schedule = Schedule::saturate(Schedule::seq(names.iter().enumerate().map(
        |(index, name)| {
            if index + 1 == names.len() {
                Schedule::run(name)
            } else {
                Schedule::saturate(*name)
            }
        },
    )));
    let started = std::time::Instant::now();
    program.run(engine.database_mut(), &schedule, &[])?;
    if std::env::var_os("TIR_TIME_PASSES").is_some() {
        eprintln!("regalloc {}: {:?}", names.join("/"), started.elapsed());
    }
    Ok(())
}

fn rows(engine: &EGraph, name: &str) -> Result<Vec<Vec<Value>>, tir_egglog::Error> {
    let db = engine.database();
    Ok(db
        .rows(db.table(name)?)
        .map(|row| row.values().collect())
        .collect())
}

fn require_functions(
    engine: &EGraph,
    functions: &[Vec<Value>],
    name: &str,
) -> Result<(), tir_egglog::Error> {
    let completed = rows(engine, name)?;
    for function in functions {
        if !completed.iter().any(|row| row[..2] == function[..2]) {
            return Err(incomplete(engine, name)?);
        }
    }
    Ok(())
}

fn require_candidates(
    engine: &EGraph,
    candidates: &[Vec<Value>],
    name: &str,
) -> Result<(), tir_egglog::Error> {
    let completed = rows(engine, name)?;
    if candidates
        .iter()
        .any(|candidate| !completed.iter().any(|row| row[..3] == candidate[..3]))
    {
        return Err(incomplete(engine, name)?);
    }
    Ok(())
}

fn incomplete(engine: &EGraph, phase: &str) -> Result<tir_egglog::Error, tir_egglog::Error> {
    let mut errors = Vec::new();
    for (table, message) in [
        ("RAControlError", 2),
        ("RABindingError", 3),
        ("RAColorError", 4),
        ("RATransferError", 3),
        ("RATransferVerifyError", 4),
        ("RAPublishError", 3),
    ] {
        for row in rows(engine, table)? {
            errors.push(engine.value_to_base::<String>(row[message]));
        }
    }
    errors.sort();
    errors.dedup();
    Ok(failure(format!(
        "{phase} did not complete: {}",
        errors.join("; ")
    )))
}

fn failure(message: impl Into<String>) -> tir_egglog::Error {
    tir_egglog::Error::Unsupported(message.into())
}
