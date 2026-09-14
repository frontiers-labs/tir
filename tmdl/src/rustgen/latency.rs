fn emit_latency_cases(
    machines: &[Vec<ast::LatencyCase>],
    numeric_params: &HashMap<String, i64>,
    register_indices: &HashMap<(String, String), u32>,
    ctx: &RustBehaviorCtx<'_>,
) -> Result<Vec<proc_macro2::TokenStream>, TMDLError> {
    machines
        .iter()
        .map(|cases| {
            let cases = cases
                .iter()
                .map(|case| {
                    let mut graph = sem_expr_state::ValueGraph::new();
                    let condition = inline_let_bindings(&case.condition);
                    let symbols = condition
                        .lower_to_sema_with_isa(
                            &mut graph,
                            numeric_params,
                            ctx.isa_param_values,
                            register_indices,
                        )
                        .ok_or_else(|| {
                            TMDLError::Codegen("cannot lower latency condition".into())
                        })?;
                    let (max_sym, sources) = emit_sym_inits(
                        &symbols.variable_symbols,
                        &symbols.register_symbols,
                        &symbols.regnum_symbols,
                        ctx.ops,
                        ctx.isa_param_values,
                        ctx.reg_kinds,
                    );
                    let sym_count = proc_macro2::Literal::usize_unsuffixed(max_sym + 1);
                    let condition = lowered_value_offset(&graph, symbols.root);
                    let latency = proc_macro2::Literal::u16_unsuffixed(case.latency as u16);
                    Ok(quote! {
                        tir::backend::sched::LatencyCase {
                            env: &EXEC_ENV,
                            sym_count: #sym_count,
                            sources: &[#(#sources),*],
                            condition: #condition,
                            latency: #latency,
                        }
                    })
                })
                .collect::<Result<Vec<_>, TMDLError>>()?;
            Ok(quote! { &[#(#cases),*] })
        })
        .collect()
}
