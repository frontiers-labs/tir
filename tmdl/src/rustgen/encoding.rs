// ---------------------------------------------------------------------------

/// One contiguous run of an integer operand's bits placed into the encoded
/// word: operand bits `[op_lo, op_lo + width)` land at word bits
/// `[word_lo, word_lo + width)`.
struct IntField {
    op_lo: u16,
    word_lo: u16,
    width: u16,
}

/// One shape's bit map, lowered from its arms: the bits it fixes and the runs
/// each operand contributes. Shared by the encoder and the decoder, which are
/// the same map read in opposite directions.
struct LoweredShape {
    const_word: u128,
    fixed_mask: u128,
    width_bytes: u8,
    // Insertion-ordered so generated code is stable across runs.
    reg_fields: Vec<(String, Vec<IntField>)>,
    int_fields: Vec<(String, Vec<IntField>)>,
}

fn encoding_mask(width: u16) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

fn emit_field_runs(fields: &[IntField]) -> proc_macro2::TokenStream {
    let runs = fields.iter().map(|f| {
        let op_lo = proc_macro2::Literal::u16_unsuffixed(f.op_lo);
        let word_lo = proc_macro2::Literal::u16_unsuffixed(f.word_lo);
        let width = proc_macro2::Literal::u16_unsuffixed(f.width);
        quote! {
            tir::backend::binary::FieldRun {
                op_lo: #op_lo,
                word_lo: #word_lo,
                width: #width,
            }
        }
    });
    quote! { &[#(#runs),*] }
}

/// Lower one shape's arms into its bit map. Errors on a value no bit map can
/// hold; the decoder treats that as "not invertible" rather than a failure.
fn lower_shape(
    inst: &ast::Instruction,
    shape: &EncodingShape,
    ops_map: &HashMap<String, Type>,
    resolved_params: &HashMap<String, (Type, Option<ast::Expr>)>,
) -> Result<LoweredShape, TMDLError> {
    let width_bytes = shape.width_bits.div_ceil(8);
    if width_bytes > 16 {
        return Err(TMDLError::Codegen(format!(
            "instruction '{}': encodings wider than 128 bits are not supported",
            inst.name
        )));
    }
    let mut lowered = LoweredShape {
        const_word: 0,
        fixed_mask: 0,
        width_bytes: width_bytes as u8,
        reg_fields: Vec::new(),
        int_fields: Vec::new(),
    };

    let push_field = |dst: &mut Vec<(String, Vec<IntField>)>, name: &str, field: IntField| match dst
        .iter_mut()
        .find(|(n, _)| n == name)
    {
        Some((_, fields)) => fields.push(field),
        None => dst.push((name.to_string(), vec![field])),
    };

    for arm in &shape.arms {
        let word_lo = arm.start;
        let width = arm.end.unwrap_or(arm.start) - arm.start + 1;
        let bad_value = || {
            TMDLError::Codegen(format!(
                "instruction '{}': unsupported encoding value at bits {}..{}",
                inst.name,
                arm.start,
                arm.end.unwrap_or(arm.start)
            ))
        };
        let (base, op_lo, width) = match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => {
                lowered.const_word |=
                    (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
                lowered.fixed_mask |= encoding_mask(width) << word_lo;
                continue;
            }
            ast::Expr::Ident(id) => (&id.name, 0, width),
            ast::Expr::Slice(slc) => match &*slc.base {
                ast::Expr::Ident(id) => (&id.name, slc.lo, width),
                _ => return Err(bad_value()),
            },
            ast::Expr::IndexAccess(idx) => match &*idx.base {
                ast::Expr::Ident(id) => (&id.name, idx.index, 1),
                _ => return Err(bad_value()),
            },
            _ => return Err(bad_value()),
        };
        let field = IntField {
            op_lo,
            word_lo,
            width,
        };
        match ops_map.get(base) {
            Some(Type::Struct(_)) => push_field(&mut lowered.reg_fields, base, field),
            Some(Type::Integer | Type::Bits(_)) => push_field(&mut lowered.int_fields, base, field),
            Some(_) => return Err(bad_value()),
            // Not an operand: an encoding parameter, which must hold a literal.
            None => match resolved_params.get(base) {
                Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
                    lowered.const_word |= (u128::from(parse_literal_value(li))
                        & encoding_mask(field.width))
                        << word_lo;
                    lowered.fixed_mask |= encoding_mask(field.width) << word_lo;
                }
                _ => {
                    return Err(TMDLError::Codegen(format!(
                        "instruction '{}': encoding parameter '{base}' has no literal value",
                        inst.name
                    )));
                }
            },
        }
    }
    Ok(lowered)
}

/// The runtime guard that selects `shape`, as a `tir::backend::binary::Guard`.
///
/// The tests themselves are read out of the encoding by `shapes::lower_guard`;
/// this renders them. Only tests the operands answer are left by then: the
/// shape expansion has already taken every branch the parameters decide.
fn emit_guard(
    inst: &ast::Instruction,
    shape: &EncodingShape,
    ctx: &crate::shapes::Context,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let predicate = crate::shapes::lower_guard(&shape.guard, ctx).map_err(|reason| {
        TMDLError::Codegen(format!("instruction '{}': {reason}", inst.name))
    })?;
    Ok(emit_predicate(&predicate))
}

fn emit_predicate(predicate: &crate::shapes::Predicate) -> proc_macro2::TokenStream {
    use crate::shapes::Predicate;
    match predicate {
        Predicate::Always => quote! { tir::backend::binary::Guard::True },
        Predicate::Not(inner) => {
            let inner = emit_predicate(inner);
            quote! { tir::backend::binary::Guard::Not(&#inner) }
        }
        Predicate::And(parts) => {
            let parts = parts.iter().map(emit_predicate);
            quote! { tir::backend::binary::Guard::And(&[#(#parts),*]) }
        }
        Predicate::Or(parts) => {
            let parts = parts.iter().map(emit_predicate);
            quote! { tir::backend::binary::Guard::Or(&[#(#parts),*]) }
        }
        Predicate::Bit { op, bit } => {
            let op = proc_macro2::Literal::string(op);
            let bit = proc_macro2::Literal::u16_unsuffixed(*bit);
            quote! { tir::backend::binary::Guard::Bit { op: #op, bit: #bit } }
        }
        Predicate::SliceEq { op, lo, hi, value } => {
            let op = proc_macro2::Literal::string(op);
            let lo = proc_macro2::Literal::u16_unsuffixed(*lo);
            let hi = proc_macro2::Literal::u16_unsuffixed(*hi);
            let value = proc_macro2::Literal::u128_unsuffixed(*value);
            quote! {
                tir::backend::binary::Guard::SliceEq { op: #op, lo: #lo, hi: #hi, value: #value }
            }
        }
        Predicate::Cmp {
            op,
            width,
            cmp_width,
            cmp,
            value,
        } => {
            let op = proc_macro2::Literal::string(op);
            let width = proc_macro2::Literal::u16_unsuffixed(*width);
            let cmp_width = proc_macro2::Literal::u16_unsuffixed(*cmp_width);
            let cmp = format_ident!("{}", cmp.name());
            let value = proc_macro2::Literal::i128_unsuffixed(*value);
            quote! {
                tir::backend::binary::Guard::Cmp {
                    op: #op,
                    width: #width,
                    cmp_width: #cmp_width,
                    cmp: tir::backend::binary::CmpOp::#cmp,
                    value: #value,
                }
            }
        }
        Predicate::Fits {
            op,
            width,
            bits,
            signed,
        } => {
            let op = proc_macro2::Literal::string(op);
            let width = proc_macro2::Literal::u16_unsuffixed(*width);
            let bits = proc_macro2::Literal::u16_unsuffixed(*bits);
            let variant = format_ident!("{}", if *signed { "SignedFits" } else { "UnsignedFits" });
            quote! {
                tir::backend::binary::Guard::#variant { op: #op, width: #width, bits: #bits }
            }
        }
    }
}

/// Compile an instruction's encoding shapes into an `ENCODE_*` spec: one fixed
/// bit map per shape, the guard that selects it, and the patch fields that
/// re-scatter a resolved fixup into it. The spec is interpreted by
/// `tir::backend::binary::encode_with`; the instruction-specific part is a
/// static data table.
/// Returns `None` when the instruction has no encoding.
fn emit_instruction_encoder(
    inst: &ast::Instruction,
    shapes: &[EncodingShape],
    ops_map: &HashMap<String, Type>,
    constraints: &HashMap<String, OperandConstraint>,
    resolved_params: &HashMap<String, (Type, Option<ast::Expr>)>,
    ctx: &crate::shapes::Context,
) -> Result<Option<proc_macro2::TokenStream>, TMDLError> {
    if shapes.is_empty() {
        return Ok(None);
    }

    let mut shape_specs = Vec::new();
    for shape in shapes {
        let lowered = lower_shape(inst, shape, ops_map, resolved_params)?;
        // Register operand: encode the allocated physical index; reject
        // anything that did not get one. Immediate operand: fit-check when the
        // field is narrower than 64 bits (immediates written in assembly may be
        // spelled signed or unsigned), then scatter. Symbol names and
        // branch-target blocks are not representable at encode time: their bits
        // stay zero and they are recorded as fixups instead.
        let mut spec_fields: Vec<proc_macro2::TokenStream> = Vec::new();
        for (name, fields) in &lowered.reg_fields {
            let name_lit = proc_macro2::Literal::string(name);
            let runs = emit_field_runs(fields);
            spec_fields.push(quote! {
                tir::backend::binary::EncodeField {
                    attr: #name_lit,
                    int_range: None,
                    align_mask: 0u128,
                    nonzero: false,
                    runs: #runs,
                }
            });
        }
        let mut patch_fields: Vec<proc_macro2::TokenStream> = Vec::new();
        for (name, fields) in &lowered.int_fields {
            let name_lit = proc_macro2::Literal::string(name);
            let runs = emit_field_runs(fields);
            let declared = match ops_map.get(name.as_str()) {
                Some(Type::Bits(n)) => Some(*n),
                _ => None,
            };
            // Any attribute value fits a full-width field, and the range
            // literals would overflow at 64 bits.
            let bounded = declared.filter(|n| *n < 64);
            let int_range = match bounded {
                Some(n) => {
                    let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
                    let max = proc_macro2::Literal::i64_suffixed(1i64 << n);
                    let umax = proc_macro2::Literal::u64_suffixed(1u64 << n);
                    quote! { Some((#min, #max, #umax)) }
                }
                None => quote! { None },
            };
            let constraint = constraints.get(name.as_str()).copied().unwrap_or_default();
            let align_mask = proc_macro2::Literal::u128_suffixed(u128::from(constraint.align - 1));
            let nonzero = constraint.nonzero;
            spec_fields.push(quote! {
                tir::backend::binary::EncodeField {
                    attr: #name_lit,
                    int_range: #int_range,
                    align_mask: #align_mask,
                    nonzero: #nonzero,
                    runs: #runs,
                }
            });
            // The value a patch scatters into an immediate field is a resolved
            // fixup (e.g. a pc-relative branch delta), which must fit the
            // operand as a signed quantity: an operand of no declared width has
            // no such bound and gets no patch, so a fixup in it is refused
            // rather than silently truncated. The operand bits the encoding
            // drops are the ones its `#[align]` declares zero (sema admits an
            // encoding that drops bits only with the matching alignment), so a
            // value with any of them set is not representable either.
            if declared.is_none() {
                continue;
            }
            let range = match bounded {
                Some(n) => {
                    let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
                    let max = proc_macro2::Literal::i64_suffixed(1i64 << (n - 1));
                    quote! { Some((#min, #max)) }
                }
                None => quote! { None },
            };
            let dropped_mask =
                proc_macro2::Literal::u128_suffixed(u128::from(constraint.align - 1));
            patch_fields.push(quote! {
                tir::backend::binary::PatchField {
                    attr: #name_lit,
                    range: #range,
                    dropped_mask: #dropped_mask,
                    runs: #runs,
                }
            });
        }

        let guard = emit_guard(inst, shape, ctx)?;
        let const_word = proc_macro2::Literal::u128_suffixed(lowered.const_word);
        let width_bytes = proc_macro2::Literal::u8_unsuffixed(lowered.width_bytes);
        shape_specs.push(quote! {
            tir::backend::binary::EncodeShape {
                guard: #guard,
                const_word: #const_word,
                width_bytes: #width_bytes,
                fields: &[#(#spec_fields),*],
                patch: &[#(#patch_fields),*],
            }
        });
    }

    let encode_spec_ident = format_ident!("ENCODE_{}", inst.name.to_uppercase());
    let (min, max) = width_range(shapes);
    let min_lit = proc_macro2::Literal::u8_unsuffixed(min);
    let max_lit = proc_macro2::Literal::u8_unsuffixed(max);
    Ok(Some(quote! {
        static #encode_spec_ident: tir::backend::binary::EncodeSpec =
            tir::backend::binary::EncodeSpec {
                shapes: &[#(#shape_specs),*],
                width_bytes: (#min_lit, #max_lit),
            };
    }))
}

/// The narrowest and widest encoding of an instruction, in bytes.
fn width_range(shapes: &[EncodingShape]) -> (u8, u8) {
    let widths = shapes
        .iter()
        .map(|shape| shape.width_bits.div_ceil(8) as u8);
    (
        widths.clone().min().unwrap_or(0),
        widths.max().unwrap_or(0),
    )
}

/// Compile an instruction's encoding shapes into a `DECODE_*` spec — the
/// inverse of [`emit_instruction_encoder`]. Given a 32-bit little-endian
/// instruction word the shared interpreter matches a shape's fixed opcode bits,
/// reconstructs each operand from its (possibly split) bit-fields, builds the
/// corresponding op in the `Context`, and returns its id.
///
/// Best-effort: returns `None` (no decoder emitted) for instructions without an
/// encoding, wider than the 32-bit fetch window, or using an encoding form this
/// generator cannot invert — so enabling decoding never breaks a backend's
/// build. The specificity the dispatcher orders by is the count of bits the
/// least specific shape fixes.
fn emit_instruction_decoder(
    inst: &ast::Instruction,
    shapes: &[EncodingShape],
    ops_map: &HashMap<String, Type>,
    resolved_params: &HashMap<String, (Type, Option<ast::Expr>)>,
    dialect: &str,
    op_name: &str,
) -> Option<(proc_macro2::TokenStream, proc_macro2::Ident, u32)> {
    if shapes.is_empty() || width_range(shapes).1 > 4 {
        return None;
    }

    let mut specificity = u32::MAX;
    // Most-specific-first within the instruction too: two shapes sema keeps
    // apart only by width both match a fetch window that holds the narrower
    // one, and the wider must be tried first.
    let mut shape_specs: Vec<(u32, proc_macro2::TokenStream)> = Vec::new();
    for shape in shapes {
        let lowered = lower_shape(inst, shape, ops_map, resolved_params).ok()?;
        let mut spec_fields: Vec<proc_macro2::TokenStream> = Vec::new();
        for (name, fields) in &lowered.reg_fields {
            let class = match ops_map.get(name) {
                Some(Type::Struct(c)) => c,
                _ => return None,
            };
            let name_lit = proc_macro2::Literal::string(name);
            let class_ident = format_ident!("{}", class);
            let runs = emit_field_runs(fields);
            spec_fields.push(quote! {
                tir::backend::binary::DecodeField {
                    attr: #name_lit,
                    kind: tir::backend::binary::DecodeFieldKind::Register(RegClass::#class_ident.id()),
                    runs: #runs,
                }
            });
        }
        for (name, fields) in &lowered.int_fields {
            let name_lit = proc_macro2::Literal::string(name);
            let runs = emit_field_runs(fields);
            spec_fields.push(quote! {
                tir::backend::binary::DecodeField {
                    attr: #name_lit,
                    kind: tir::backend::binary::DecodeFieldKind::Int,
                    runs: #runs,
                }
            });
        }
        specificity = specificity.min(lowered.fixed_mask.count_ones());
        let const_word = proc_macro2::Literal::u32_suffixed(lowered.const_word as u32);
        let fixed_mask = proc_macro2::Literal::u32_suffixed(lowered.fixed_mask as u32);
        shape_specs.push((
            lowered.fixed_mask.count_ones(),
            quote! {
                tir::backend::binary::DecodeShape {
                    fixed_mask: #fixed_mask,
                    const_word: #const_word,
                    fields: &[#(#spec_fields),*],
                }
            },
        ));
    }
    shape_specs.sort_by_key(|(fixed_bits, _)| std::cmp::Reverse(*fixed_bits));
    let shape_specs: Vec<proc_macro2::TokenStream> =
        shape_specs.into_iter().map(|(_, spec)| spec).collect();

    let spec_ident = format_ident!("DECODE_{}", inst.name.to_uppercase());
    let dialect_lit = proc_macro2::Literal::string(dialect);
    let op_name_lit = proc_macro2::Literal::string(op_name);
    // Sorted so generated code is stable across runs; the interpreter only
    // needs the set.
    let mut attr_names: Vec<&String> = ops_map.keys().collect();
    attr_names.sort();
    let attr_lits: Vec<proc_macro2::Literal> = attr_names
        .iter()
        .map(|n| proc_macro2::Literal::string(n))
        .collect();

    let spec = quote! {
        static #spec_ident: tir::backend::binary::DecodeSpec =
            tir::backend::binary::DecodeSpec {
                op: (#dialect_lit, #op_name_lit),
                shapes: &[#(#shape_specs),*],
                attrs: &[#(#attr_lits),*],
            };
    };

    Some((spec, spec_ident, specificity))
}
