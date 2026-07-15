/// Emit one `fn <machine>_model() -> tir::backend::sched::MachineModel` per TMDL
/// `machine` block. Each instruction's `unit` membership is resolved against the
/// machine's `bind`s at compile time into a concrete per-mnemonic scheduling class,
/// so the runtime lookup is a binary search. This is the static half of the
/// performance model: the same table feeds the compiler cost model and the
/// cycle-approximate simulator, so they cannot disagree.
fn emit_machine_models<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let unit_defaults = collect_unit_defaults(files);
    let scheduled = collect_scheduled(files, item_cache);

    let mut model_fns = Vec::new();
    let mut lookup_arms = Vec::new();
    let mut machine_names = Vec::new();
    for machine in files.iter().flat_map(|f| f.machines()) {
        let binds: HashMap<&str, &ast::UnitBind> =
            machine.binds.iter().map(|b| (b.unit.as_str(), b)).collect();
        let overrides: HashMap<&str, &ast::MachineOverride> = machine
            .overrides
            .iter()
            .map(|o| (o.instruction.as_str(), o))
            .collect();

        // Resolve each scheduled instruction to a concrete class on this machine. A
        // per-instruction `override` supersedes the `unit`-based resolution.
        let mut entries: Vec<(String, ResolvedClass)> = scheduled
            .iter()
            .map(|(name, mnemonic, units)| {
                let resolved = match overrides.get(name.as_str()) {
                    Some(ov) => resolve_spec(
                        ov.reads.as_deref(),
                        ov.writes.as_deref(),
                        ov.latency,
                        ov.throughput,
                        &ov.uses,
                        &machine.pipeline,
                    ),
                    None => resolve_sched_class(units, &binds, &unit_defaults, &machine.pipeline),
                };
                (mnemonic.clone(), resolved)
            })
            .collect();
        // Sorted + deduplicated by mnemonic for the runtime binary search.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);

        let sched_lits = entries.iter().map(|(mnem, c)| {
            let mnem_lit = proc_macro2::Literal::string(mnem);
            let lat_lit = proc_macro2::Literal::u16_unsuffixed(c.latency);
            let read_lit = proc_macro2::Literal::u16_unsuffixed(c.read_cycle);
            let rthr_lit = proc_macro2::Literal::u16_unsuffixed(c.rthroughput);
            let res_lits = c.resources.iter().map(|r| proc_macro2::Literal::string(r));
            quote! {
                (#mnem_lit, tir::backend::sched::InstrSchedClass {
                    latency: #lat_lit,
                    read_cycle: #read_lit,
                    rthroughput: #rthr_lit,
                    resources: &[#(#res_lits),*],
                })
            }
        });

        let pipeline_lits = machine.pipeline.iter().map(|p| {
            let name_lit = proc_macro2::Literal::string(&p.name);
            let prot_ts = protection_ts(p.protection);
            quote! {
                tir::backend::sched::PipelinePhase { name: #name_lit, protection: #prot_ts }
            }
        });

        let forward_lits = machine.forwards.iter().map(|f| {
            let from_lit = proc_macro2::Literal::string(&f.from);
            let to_lit = proc_macro2::Literal::string(&f.to);
            let lat_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(f.latency));
            quote! {
                tir::backend::sched::Forward { from: #from_lit, to: #to_lit, latency: #lat_lit }
            }
        });

        let resource_lits = machine.resources.iter().map(|r| {
            let name_lit = proc_macro2::Literal::string(&r.name);
            let units_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(r.units));
            quote! { tir::backend::sched::ProcUnit { name: #name_lit, units: #units_lit } }
        });

        let buffer_lits = machine.buffers.iter().map(|(name, size)| {
            let name_lit = proc_macro2::Literal::string(name);
            let size_lit = proc_macro2::Literal::u32_unsuffixed(clamp_u32(*size));
            quote! { tir::backend::sched::BufferSize { name: #name_lit, size: #size_lit } }
        });

        let reg_file_lits = machine.reg_files.iter().map(|(name, count)| {
            let name_lit = proc_macro2::Literal::string(name);
            let count_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(*count));
            quote! { tir::backend::sched::RegFile { name: #name_lit, count: #count_lit } }
        });

        let name_lit = proc_macro2::Literal::string(&machine.name);
        let issue_width_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(
            machine.issue_width.unwrap_or(1).max(1),
        ));
        let fn_ident = format_ident!("{}_model", to_snake_case(&machine.name));

        model_fns.push(quote! {
            pub fn #fn_ident() -> tir::backend::sched::MachineModel {
                tir::backend::sched::MachineModel {
                    name: #name_lit,
                    issue_width: #issue_width_lit,
                    resources: &[#(#resource_lits),*],
                    buffers: &[#(#buffer_lits),*],
                    pipeline: &[#(#pipeline_lits),*],
                    forwards: &[#(#forward_lits),*],
                    reg_files: &[#(#reg_file_lits),*],
                    sched: &[#(#sched_lits),*],
                }
            }
        });

        // Select by the machine name, and by its alias when one is declared, so
        // the tool-facing name lives in TMDL next to the machine.
        let mut keys = vec![machine.name.clone()];
        if let Some(alias) = &machine.alias {
            keys.push(alias.clone());
        }
        let machine_features = feature_slice(&machine.for_isas);
        let key_lits = keys.iter().map(|k| proc_macro2::Literal::string(k));
        let tool_name = proc_macro2::Literal::string(keys.last().unwrap());
        machine_names.push(quote! {
            if features_enabled(features, #machine_features) {
                names.push(#tool_name);
            }
        });
        lookup_arms.push(quote! {
            #(#key_lits)|* => features_enabled(features, #machine_features).then(#fn_ident)
        });
    }

    // A target with no `machine` models (e.g. a text-only pseudo-ISA) gets
    // trivial accessors so `features` and `names` are not spuriously unused.
    if machine_names.is_empty() {
        return Ok(quote! {
            /// No machine models are declared for this target.
            pub fn machine_model(_name: &str, _features: &[Feature]) -> Option<tir::backend::sched::MachineModel> {
                None
            }

            /// No machine models are declared for this target.
            pub fn machines(_features: &[Feature]) -> Vec<&'static str> {
                Vec::new()
            }
        });
    }

    Ok(quote! {
        #(#model_fns)*

        /// Resolve a machine by its TMDL name or alias. `None` when the name is
        /// unknown or the machine's `for [...]` clause is disjoint from `features`.
        pub fn machine_model(name: &str, features: &[Feature]) -> Option<tir::backend::sched::MachineModel> {
            match name {
                #(#lookup_arms,)*
                _ => None,
            }
        }

        /// Tool-facing names (alias preferred) of the machines compatible with `features`.
        pub fn machines(features: &[Feature]) -> Vec<&'static str> {
            let mut names = Vec::new();
            #(#machine_names)*
            names
        }
    })
}

/// Resource-agnostic `unit` defaults, keyed by name. Used both when a machine
/// does not bind a unit and to drive the machine-independent [`instruction_cost`].
fn collect_unit_defaults(files: &[ast::File]) -> HashMap<&str, &ast::SchedClassDecl> {
    files
        .iter()
        .flat_map(|f| f.count())
        .map(|u| (u.name.as_str(), u))
        .collect()
}

/// `(instruction name, mnemonic, units)` for every instruction carrying a
/// `schedule` block. The name keys per-instruction machine `override`s; the
/// mnemonic keys the runtime scheduling table.
fn collect_scheduled<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, String, Vec<String>)> {
    let mut scheduled = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        let Some(schedule) =
            crate::utils::resolve_effective_schedule_for_instruction(inst, item_cache)
        else {
            continue;
        };
        let resolved_params = resolve_params_for_instruction(inst, item_cache);
        let mnemonic = resolved_params
            .get("MNEMONIC")
            .and_then(|(_, v)| v.as_ref())
            .and_then(resolve_string)
            .or_else(|| {
                resolved_params
                    .get("OPNAME")
                    .and_then(|(_, v)| v.as_ref())
                    .and_then(resolve_string)
            });
        let Some(mnemonic) = mnemonic else {
            continue;
        };
        scheduled.push((inst.name.clone(), mnemonic, schedule.classes.clone()));
    }
    scheduled
}

/// Emit a machine-independent `instruction_cost(mnemonic) -> u32` derived from
/// each instruction's `unit` defaults (latency, falling back to 1). This is the
/// single source of truth the compiler cost model consults — most importantly the
/// instruction-selection `base_cost` (see `emit_instructions`) — so selection and
/// the simulator agree on relative instruction cost.
fn emit_instruction_cost<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let unit_defaults = collect_unit_defaults(files);
    let scheduled = collect_scheduled(files, item_cache);
    let empty_binds: HashMap<&str, &ast::UnitBind> = HashMap::new();

    let mut arms = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (_name, mnemonic, units) in &scheduled {
        if !seen.insert(mnemonic.clone()) {
            continue;
        }
        // Machine-independent: no machine binds and no pipeline, so this resolves
        // through the unit defaults to a scalar latency.
        let resolved = resolve_sched_class(units, &empty_binds, &unit_defaults, &[]);
        let m_lit = proc_macro2::Literal::string(mnemonic);
        let c_lit = proc_macro2::Literal::u32_unsuffixed(u32::from(resolved.latency));
        arms.push(quote! { #m_lit => #c_lit, });
    }

    let cost_body = if arms.is_empty() {
        quote! { 1 }
    } else {
        quote! {
            match mnemonic {
                #(#arms)*
                _ => 1,
            }
        }
    };

    Ok(quote! {
        /// Machine-independent instruction cost (a latency proxy) derived from TMDL
        /// `unit` defaults. The instruction-selection cost model consults this so it
        /// shares one source of truth with the simulator's per-machine model.
        pub fn instruction_cost(mnemonic: &str) -> u32 {
            let _ = mnemonic;
            #cost_body
        }
    })
}

/// The cycle offset (index) of a named pipeline phase within a machine's pipeline.
fn phase_cycle(pipeline: &[ast::PipelinePhase], name: &str) -> Option<u16> {
    pipeline
        .iter()
        .position(|p| p.name == name)
        .map(|i| i as u16)
}

/// The resolved scheduling cost of an instruction on one machine.
struct ResolvedClass {
    latency: u16,
    read_cycle: u16,
    rthroughput: u16,
    resources: Vec<String>,
}

/// Resolve one explicit timing spec (a `bind` or an `override`) to a class. Timing
/// is phase-based when it names `reads`/`writes` phases (cycles from the machine's
/// pipeline), else scalar (`latency = N` ≡ read at cycle 0, write at cycle N).
fn resolve_spec(
    reads: Option<&str>,
    writes: Option<&str>,
    latency: Option<i64>,
    throughput: Option<i64>,
    uses: &[String],
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let (rc, wc) = if reads.is_some() || writes.is_some() {
        let rc = reads.and_then(|p| phase_cycle(pipeline, p)).unwrap_or(0);
        let wc = writes
            .and_then(|p| phase_cycle(pipeline, p))
            .unwrap_or_else(|| rc.saturating_add(clamp_u16(latency.unwrap_or(1))));
        (rc, wc.max(rc))
    } else {
        (0, clamp_u16(latency.unwrap_or(1)))
    };
    ResolvedClass {
        latency: wc.saturating_sub(rc).max(1),
        read_cycle: rc,
        rthroughput: clamp_u16(throughput.unwrap_or(1)).max(1),
        resources: uses.to_vec(),
    }
}

/// Resolve an instruction's `unit` membership to a concrete class on one machine.
/// Precedence per unit: the machine's `bind` → the unit's resource-agnostic default
/// → the built-in `(latency 1, read 0)`. Across multiple units the result aggregates
/// conservatively: the highest-latency unit sets the latency/read-cycle, throughput
/// is the max, resources are unioned.
fn resolve_sched_class(
    units: &[String],
    binds: &HashMap<&str, &ast::UnitBind>,
    unit_defaults: &HashMap<&str, &ast::SchedClassDecl>,
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let mut latency: u16 = 0;
    let mut read_cycle: u16 = 0;
    let mut rthroughput: u16 = 0;
    let mut resources: Vec<String> = Vec::new();
    let mut chosen = false;

    for unit in units {
        let class = if let Some(b) = binds.get(unit.as_str()) {
            resolve_spec(
                b.reads.as_deref(),
                b.writes.as_deref(),
                b.latency,
                b.throughput,
                &b.uses,
                pipeline,
            )
        } else if let Some(d) = unit_defaults.get(unit.as_str()) {
            ResolvedClass {
                latency: clamp_u16(d.default_latency.unwrap_or(1)).max(1),
                read_cycle: 0,
                rthroughput: clamp_u16(d.default_throughput.unwrap_or(1)).max(1),
                resources: Vec::new(),
            }
        } else {
            ResolvedClass {
                latency: 1,
                read_cycle: 0,
                rthroughput: 1,
                resources: Vec::new(),
            }
        };

        for r in &class.resources {
            if !resources.iter().any(|e| e == r) {
                resources.push(r.clone());
            }
        }
        if !chosen || class.latency > latency {
            latency = class.latency;
            read_cycle = class.read_cycle;
            chosen = true;
        }
        rthroughput = rthroughput.max(class.rthroughput);
    }

    ResolvedClass {
        latency: latency.max(1),
        read_cycle,
        rthroughput: rthroughput.max(1),
        resources,
    }
}

/// The `tir::backend::sched::Protection` variant for an AST protection mode.
fn protection_ts(p: ast::Protection) -> proc_macro2::TokenStream {
    match p {
        ast::Protection::Protected => quote! { tir::backend::sched::Protection::Protected },
        ast::Protection::Unprotected => quote! { tir::backend::sched::Protection::Unprotected },
        ast::Protection::Hard => quote! { tir::backend::sched::Protection::Hard },
    }
}

fn clamp_u16(v: i64) -> u16 {
    v.clamp(0, u16::MAX as i64) as u16
}

fn clamp_u32(v: i64) -> u32 {
    v.clamp(0, u32::MAX as i64) as u32
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i != 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

