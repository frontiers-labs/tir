/// Emit one `fn <machine>_model() -> tir::backend::sched::MachineModel` per TMDL
/// `machine` block. Each instruction's `unit` membership is resolved against the
/// machine's `bind`s at compile time into a concrete per-operation scheduling class,
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
        let resource_groups: HashMap<&str, &ast::ResourceExpr> = machine
            .resource_groups
            .iter()
            .map(|group| (group.name.as_str(), &group.resources))
            .collect();

        // Resolve each scheduled instruction to a concrete class on this machine. A
        // per-instruction `override` supersedes the `unit`-based resolution.
        let mut entries: Vec<(String, ResolvedClass)> = scheduled
            .iter()
            .map(|(name, operation, _mnemonic, units)| {
                let resolved = match overrides.get(name.as_str()) {
                    Some(ov) => resolve_spec(
                        ExplicitTimingSpec {
                            reads: ov.reads.as_deref(),
                            writes: ov.writes.as_deref(),
                            latency: ov.latency,
                            throughput: ov.throughput,
                            uses: &ov.uses,
                            uops: &ov.uops,
                            decode_uops: ov.decode_uops,
                            decoder: ov.decoder.as_deref(),
                            decode_cycles: ov.decode_cycles,
                            eliminated: ov.eliminated,
                            zero_idiom: ov.zero_idiom,
                        },
                        &resource_groups,
                        &machine.pipeline,
                    ),
                    None => resolve_sched_class(
                        units,
                        &binds,
                        &unit_defaults,
                        &resource_groups,
                        &machine.pipeline,
                    ),
                };
                (operation.clone(), resolved)
            })
            .collect();
        // Sorted + deduplicated by stable operation identity for runtime lookup.
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);

        let sched_lits = entries.iter().map(|(mnem, c)| {
            let mnem_lit = proc_macro2::Literal::string(mnem);
            let lat_lit = proc_macro2::Literal::u16_unsuffixed(c.latency);
            let read_lit = proc_macro2::Literal::u16_unsuffixed(c.read_cycle);
            let rthr_lit = proc_macro2::Literal::u16_unsuffixed(c.rthroughput);
            let res_lits = c.resources.iter().map(|r| proc_macro2::Literal::string(r));
            let uop_lits = c.uops.iter().map(|uop| {
                let route_lits = uop.routes.iter().map(|route| {
                    let use_lits = route.iter().map(|use_| {
                        let resource = proc_macro2::Literal::string(&use_.resource);
                        let cycles = proc_macro2::Literal::u16_unsuffixed(use_.cycles);
                        quote! {
                            tir::backend::sched::ResourceUse {
                                resource: #resource,
                                cycles: #cycles,
                            }
                        }
                    });
                    quote! {
                        tir::backend::sched::ResourceRoute {
                            resources: &[#(#use_lits),*],
                        }
                    }
                });
                quote! {
                    tir::backend::sched::MicroOp {
                        routes: &[#(#route_lits),*],
                    }
                }
            });
            let decode_uops = proc_macro2::Literal::u16_unsuffixed(c.decode_uops);
            let decoder = match &c.decoder {
                Some(decoder) => {
                    let decoder = proc_macro2::Literal::string(decoder);
                    quote! { Some(#decoder) }
                }
                None => quote! { None },
            };
            let decode_cycles = proc_macro2::Literal::u16_unsuffixed(c.decode_cycles);
            let eliminated = c.eliminated;
            let zero_idiom = c.zero_idiom;
            quote! {
                (#mnem_lit, tir::backend::sched::InstrSchedClass {
                    latency: #lat_lit,
                    read_cycle: #read_lit,
                    rthroughput: #rthr_lit,
                    resources: &[#(#res_lits),*],
                    uops: &[#(#uop_lits),*],
                    decode_uops: #decode_uops,
                    decoder: #decoder,
                    decode_cycles: #decode_cycles,
                    eliminated: #eliminated,
                    zero_idiom: #zero_idiom,
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
        let frontend = frontend_ts(machine.frontend.as_ref());

        // Fusion rules are declared over mnemonics; the engine matches
        // scheduling keys, so expand each mnemonic to every key that carries it.
        let keys_with_mnemonics = |mnemonics: &[String]| -> Vec<proc_macro2::Literal> {
            scheduled
                .iter()
                .filter(|(_, _, mnemonic, _)| mnemonics.iter().any(|m| m == mnemonic))
                .map(|(_, operation, _, _)| proc_macro2::Literal::string(operation))
                .collect()
        };
        let fusion_lits: Vec<_> = machine
            .fusions
            .iter()
            .map(|fusion| {
                let first = keys_with_mnemonics(&fusion.first);
                let second = keys_with_mnemonics(&fusion.second);
                quote! {
                    tir::backend::sched::FusionGroup {
                        first: &[#(#first),*],
                        second: &[#(#second),*],
                    }
                }
            })
            .collect();

        model_fns.push(quote! {
            pub fn #fn_ident() -> tir::backend::sched::MachineModel {
                tir::backend::sched::MachineModel {
                    name: #name_lit,
                    issue_width: #issue_width_lit,
                    frontend: #frontend,
                    resources: &[#(#resource_lits),*],
                    buffers: &[#(#buffer_lits),*],
                    pipeline: &[#(#pipeline_lits),*],
                    forwards: &[#(#forward_lits),*],
                    reg_files: &[#(#reg_file_lits),*],
                    sched: &[#(#sched_lits),*],
                    fusions: &[#(#fusion_lits),*],
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

/// `(instruction name, operation identity, mnemonic, units)` for every
/// instruction carrying a `schedule` block. The declaration name keys
/// per-instruction machine `override`s; operation identity keys the runtime
/// scheduling table; mnemonic remains the machine-independent cost key.
fn collect_scheduled<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, String, String, Vec<String>)> {
    let mut scheduled = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        let Some(schedule) =
            crate::utils::resolve_effective_schedule_for_instruction(inst, item_cache)
        else {
            continue;
        };
        let resolved_params = resolve_params_for_instruction(inst, item_cache);
        let operation = resolved_params
            .get("OPNAME")
            .and_then(|(_, v)| v.as_ref())
            .and_then(resolve_string)
            .or_else(|| {
                resolved_params
                    .get("MNEMONIC")
                    .and_then(|(_, v)| v.as_ref())
                    .and_then(resolve_string)
            });
        let Some(operation) = operation else {
            continue;
        };
        let mnemonic = resolved_params
            .get("MNEMONIC")
            .and_then(|(_, v)| v.as_ref())
            .and_then(resolve_string)
            .unwrap_or_else(|| operation.clone());
        scheduled.push((
            inst.name.clone(),
            operation,
            mnemonic,
            schedule.classes.clone(),
        ));
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
    let empty_groups: HashMap<&str, &ast::ResourceExpr> = HashMap::new();

    let mut arms = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (_name, _operation, mnemonic, units) in &scheduled {
        if !seen.insert(mnemonic.clone()) {
            continue;
        }
        // Machine-independent: no machine binds and no pipeline, so this resolves
        // through the unit defaults to a scalar latency.
        let resolved =
            resolve_sched_class(units, &empty_binds, &unit_defaults, &empty_groups, &[]);
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
    uops: Vec<ResolvedMicroOp>,
    decode_uops: u16,
    decoder: Option<String>,
    decode_cycles: u16,
    eliminated: bool,
    zero_idiom: bool,
}

#[derive(Clone)]
struct ResolvedMicroOp {
    routes: Vec<Vec<ResolvedResourceUse>>,
}

#[derive(Clone)]
struct ResolvedResourceUse {
    resource: String,
    cycles: u16,
}

struct ExplicitTimingSpec<'a> {
    reads: Option<&'a str>,
    writes: Option<&'a str>,
    latency: Option<i64>,
    throughput: Option<i64>,
    uses: &'a [String],
    uops: &'a [ast::MicroOp],
    decode_uops: Option<i64>,
    decoder: Option<&'a str>,
    decode_cycles: Option<i64>,
    eliminated: Option<bool>,
    zero_idiom: Option<bool>,
}

/// Resolve one explicit timing spec (a `bind` or an `override`) to a class. Timing
/// is phase-based when it names `reads`/`writes` phases (cycles from the machine's
/// pipeline), else scalar (`latency = N` ≡ read at cycle 0, write at cycle N).
fn resolve_spec(
    spec: ExplicitTimingSpec<'_>,
    resource_groups: &HashMap<&str, &ast::ResourceExpr>,
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let (rc, wc) = if spec.reads.is_some() || spec.writes.is_some() {
        let rc = spec
            .reads
            .and_then(|p| phase_cycle(pipeline, p))
            .unwrap_or(0);
        let wc = spec
            .writes
            .and_then(|p| phase_cycle(pipeline, p))
            .unwrap_or_else(|| rc.saturating_add(clamp_u16(spec.latency.unwrap_or(1))));
        (rc, wc.max(rc))
    } else {
        (0, clamp_u16(spec.latency.unwrap_or(1)))
    };
    let resolved_uops: Vec<_> = spec
        .uops
        .iter()
        .flat_map(|uop| {
            (0..uop.count.max(0)).map(|_| ResolvedMicroOp {
                routes: resolve_resource_expr(&uop.resources, resource_groups, None),
            })
        })
        .collect();
    ResolvedClass {
        latency: wc.saturating_sub(rc).max(1),
        read_cycle: rc,
        rthroughput: clamp_u16(spec.throughput.unwrap_or(1)).max(1),
        resources: spec.uses.to_vec(),
        decode_uops: clamp_u16(
            spec.decode_uops
                .unwrap_or(resolved_uops.len().max(1) as i64),
        )
        .max(1),
        decoder: spec.decoder.map(str::to_string),
        decode_cycles: clamp_u16(spec.decode_cycles.unwrap_or(1)).max(1),
        eliminated: spec.eliminated.unwrap_or(false),
        zero_idiom: spec.zero_idiom.unwrap_or(false),
        uops: resolved_uops,
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
    resource_groups: &HashMap<&str, &ast::ResourceExpr>,
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let mut latency: u16 = 0;
    let mut read_cycle: u16 = 0;
    let mut rthroughput: u16 = 0;
    let mut resources: Vec<String> = Vec::new();
    let mut uops = Vec::new();
    let mut decode_uops = 0u16;
    let mut decoder = None;
    let mut decode_cycles = 0u16;
    let mut eliminated = false;
    let mut zero_idiom = false;
    let mut chosen = false;

    for unit in units {
        let class = if let Some(b) = binds.get(unit.as_str()) {
            resolve_spec(
                ExplicitTimingSpec {
                    reads: b.reads.as_deref(),
                    writes: b.writes.as_deref(),
                    latency: b.latency,
                    throughput: b.throughput,
                    uses: &b.uses,
                    uops: &b.uops,
                    decode_uops: b.decode_uops,
                    decoder: b.decoder.as_deref(),
                    decode_cycles: b.decode_cycles,
                    eliminated: b.eliminated,
                    zero_idiom: b.zero_idiom,
                },
                resource_groups,
                pipeline,
            )
        } else if let Some(d) = unit_defaults.get(unit.as_str()) {
            ResolvedClass {
                latency: clamp_u16(d.default_latency.unwrap_or(1)).max(1),
                read_cycle: 0,
                rthroughput: clamp_u16(d.default_throughput.unwrap_or(1)).max(1),
                resources: Vec::new(),
                uops: Vec::new(),
                decode_uops: 1,
                decoder: None,
                decode_cycles: 1,
                eliminated: false,
                zero_idiom: false,
            }
        } else {
            ResolvedClass {
                latency: 1,
                read_cycle: 0,
                rthroughput: 1,
                resources: Vec::new(),
                uops: Vec::new(),
                decode_uops: 1,
                decoder: None,
                decode_cycles: 1,
                eliminated: false,
                zero_idiom: false,
            }
        };

        for r in &class.resources {
            if !resources.iter().any(|e| e == r) {
                resources.push(r.clone());
            }
        }
        uops.extend(class.uops);
        decode_uops = decode_uops.saturating_add(class.decode_uops);
        if decoder.is_none() {
            decoder = class.decoder;
        }
        decode_cycles = decode_cycles.max(class.decode_cycles);
        eliminated |= class.eliminated;
        zero_idiom |= class.zero_idiom;
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
        uops,
        decode_uops: decode_uops.max(1),
        decoder,
        decode_cycles: decode_cycles.max(1),
        eliminated,
        zero_idiom,
    }
}

fn frontend_ts(frontend: Option<&ast::Frontend>) -> proc_macro2::TokenStream {
    let Some(frontend) = frontend else {
        return quote! { None };
    };
    let bytes_per_cycle = proc_macro2::Literal::u16_unsuffixed(clamp_u16(
        frontend.fetch.bytes_per_cycle,
    ));
    let window_bytes =
        proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.fetch.window_bytes));
    let alignment = proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.fetch.alignment));
    let queue_bytes = proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.fetch.queue_bytes));
    let slots = frontend
        .decode
        .slots
        .iter()
        .map(|slot| proc_macro2::Literal::string(slot));
    let uops_per_cycle =
        proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.decode.uops_per_cycle));
    let queue_uops =
        proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.decode.queue_uops));
    let decoders = frontend.decode.decoders.iter().map(|decoder| {
        let name = proc_macro2::Literal::string(&decoder.name);
        let max_uops = proc_macro2::Literal::u16_unsuffixed(clamp_u16(
            decoder.max_uops_per_instruction,
        ));
        quote! {
            tir::backend::sched::Decoder {
                name: #name,
                max_uops_per_instruction: #max_uops,
            }
        }
    });
    let decoded_cache = match &frontend.decoded_cache {
        Some(cache) => {
            let sets = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.sets));
            let ways = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.ways));
            let line_bytes = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.line_bytes));
            let line_uops = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.line_uops));
            let deliver = proc_macro2::Literal::u16_unsuffixed(clamp_u16(
                cache.deliver_uops_per_cycle,
            ));
            quote! {
                Some(tir::backend::sched::DecodedCache {
                    sets: #sets,
                    ways: #ways,
                    line_bytes: #line_bytes,
                    line_uops: #line_uops,
                    deliver_uops_per_cycle: #deliver,
                })
            }
        }
        None => quote! { None },
    };
    quote! {
        Some(tir::backend::sched::Frontend {
            fetch: tir::backend::sched::FrontendFetch {
                bytes_per_cycle: #bytes_per_cycle,
                window_bytes: #window_bytes,
                alignment: #alignment,
                queue_bytes: #queue_bytes,
            },
            decode: tir::backend::sched::FrontendDecode {
                slots: &[#(#slots),*],
                uops_per_cycle: #uops_per_cycle,
                queue_uops: #queue_uops,
                decoders: &[#(#decoders),*],
            },
            decoded_cache: #decoded_cache,
        })
    }
}

fn resolve_resource_expr(
    expr: &ast::ResourceExpr,
    groups: &HashMap<&str, &ast::ResourceExpr>,
    occupancy: Option<u16>,
) -> Vec<Vec<ResolvedResourceUse>> {
    match expr {
        ast::ResourceExpr::Resource(name) => match groups.get(name.as_str()) {
            Some(group) => resolve_resource_expr(group, groups, occupancy),
            None => vec![vec![ResolvedResourceUse {
                resource: name.clone(),
                cycles: occupancy.unwrap_or(1).max(1),
            }]],
        },
        ast::ResourceExpr::Any(resources) => resources
            .iter()
            .flat_map(|resource| resolve_resource_expr(resource, groups, occupancy))
            .collect(),
        ast::ResourceExpr::All(resources) => resources.iter().fold(
            vec![Vec::new()],
            |routes, resource| {
                let rhs = resolve_resource_expr(resource, groups, occupancy);
                routes
                    .into_iter()
                    .flat_map(|lhs| {
                        rhs.iter().map(move |right| {
                            let mut route = lhs.clone();
                            route.extend(right.iter().cloned());
                            route
                        })
                    })
                    .collect()
            },
        ),
        ast::ResourceExpr::Occupied { resource, cycles } => resolve_resource_expr(
            resource,
            groups,
            occupancy.or(Some(clamp_u16(*cycles).max(1))),
        ),
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
