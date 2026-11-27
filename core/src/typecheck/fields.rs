use super::{Attr, Check, Failures};
use crate::{
    OpHandle, TypeArg, TypeId,
    builtin::{FnType, IndexType, ModuleOp, TupleType, UnitType},
    ptr::PtrType,
    schema::{ConstraintPort, TypeConstraint},
};

fn unsigned(attr: &Attr) -> Option<u64> {
    match attr {
        Attr::UInt(n) if *n <= i64::MAX as u64 => Some(*n),
        Attr::Int(n) => (*n).try_into().ok(),
        _ => None,
    }
}

impl Check<'_> {
    // ReaderAt supplies all enclosing regions. Inside nested scopes, the
    // applicable owner with the most enclosing regions is the nearest one.
    fn scoped<T>(&self, read: impl Fn(&OpHandle) -> Option<T>) -> Option<T> {
        if let Some(t) = read(&self.op) {
            return Some(t);
        }
        self.ctx
            .typecheck_readers(self.op.id)
            .into_iter()
            .filter_map(|r| self.ctx.region_parent_op(r))
            .filter_map(|id| Some((id, read(&self.ctx.get_op(id))?)))
            .max_by_key(|(id, _)| self.ctx.typecheck_readers(*id).len())
            .map(|(_, v)| v)
    }

    fn port_type(&self, port: ConstraintPort) -> Option<TypeId> {
        match port {
            ConstraintPort::Input(index) => self.inputs.one(index),
            ConstraintPort::Output(index) => self.outputs.one(index),
        }
    }

    pub(super) fn layout(&self, constraints: &[TypeConstraint], errors: &mut Failures) {
        if !constraints.iter().any(|constraint| {
            matches!(
                constraint,
                TypeConstraint::EqualWidth(..)
                    | TypeConstraint::PointerWidth(..)
                    | TypeConstraint::EqualType(..)
            )
        }) {
            return;
        }

        let pointer = self.scoped(|op| {
            let Attr::LayoutSpec(spec) = op.attr("layout.spec")? else {
                return None;
            };
            spec.entries.iter().find_map(|e| {
                if e.vars.is_empty()
                    && PtrType::view(self.ctx, e.key).is_some_and(|p| p.pointee().is_none())
                    && let TypeArg::Nat(width) = e.size
                {
                    Some(width)
                } else {
                    None
                }
            })
        });
        let index = self
            .scoped(|op| {
                let Attr::LayoutSpec(spec) = op.attr("layout.spec")? else {
                    return None;
                };
                unsigned(spec.option("index")?)
            })
            .or(pointer);
        let width = |ty: Option<TypeId>| {
            let ty = ty?;
            let terms = self.ctx.terms();
            if IndexType::view(self.ctx, ty).is_some() {
                index
            } else if let Some((e, m)) = terms.float_format(ty) {
                e.checked_add(m)?.checked_add(1)
            } else {
                terms.int_width(ty)
            }
        };
        for constraint in constraints {
            let (valid, label) = match *constraint {
                TypeConstraint::EqualWidth(lhs, rhs, label) => {
                    let lhs = width(self.port_type(lhs));
                    (lhs.is_some() && lhs == width(self.port_type(rhs)), label)
                }
                TypeConstraint::PointerWidth(port, label) => (
                    pointer.is_none() || width(self.port_type(port)) == pointer,
                    label,
                ),
                TypeConstraint::EqualType(lhs, rhs, label) => {
                    let lhs = self.port_type(lhs);
                    (lhs.is_some() && lhs == self.port_type(rhs), label)
                }
                _ => continue,
            };
            errors.require(valid, label);
        }
    }

    fn metadata(&self, errors: &mut Failures) {
        if let Some(attr) = self.op.attr("layout.spec") {
            if let Attr::LayoutSpec(spec) = attr {
                for key in ["stack_align", "native", "index", "cache_line"] {
                    if let Some(v) = spec.option(key) {
                        errors.require(unsigned(v).is_some(), key);
                    }
                }
                if let Some(v) = spec.option("endianness") {
                    errors.require(
                        matches!(v.as_str(), Some("big" | "little")),
                        "layout endianness",
                    );
                }
            } else {
                errors.require(false, "layout.spec must be a #layout.spec");
            }
        }
        if let Some(attr) = self.op.attr("target_env") {
            if let Attr::Dict(fields) = attr {
                for (key, label) in [("arch", "target_env arch"), ("cpu", "target_env cpu")] {
                    if let Some(v) = fields.get(key) {
                        errors.require(v.as_str().is_some(), label);
                    }
                }
                if let Some(v) = fields.get("features") {
                    errors.require(
                        matches!(v,Attr::Array(xs) if xs.iter().all(|x| x.as_str().is_some())),
                        "target_env features",
                    );
                }
            } else {
                errors.require(false, "target_env must be a dictionary");
            }
        }
        if self.schema.is_some_and(|s| s.dialect == "fp") {
            errors.require(
                ["semantics", "rounding", "contraction"]
                    .iter()
                    .all(|k| self.op.attr(k).is_none()),
                "floating-point policies belong in the computation type",
            );
        }
    }

    fn abi(&self, args: &[TypeId], definition: bool, errors: &mut Failures) {
        let pointer = |i| {
            args.get(i)
                .is_some_and(|t| PtrType::view(self.ctx, *t).is_some())
        };
        if let Some(v) = self.op.attr("argument_alignments") {
            let valid = match v {
                Attr::Array(entries) => {
                    entries.len() == args.len()
                        && entries.iter().all(|entry| match entry {
                            Attr::UInt(n) => n.is_power_of_two(),
                            _ => unsigned(entry).is_some_and(u64::is_power_of_two),
                        })
                }
                _ => false,
            };
            errors.require(valid, "argument alignment");
        }
        if definition && let Some(v) = self.op.attr("noalias") {
            let valid = match v {
                Attr::Array(entries) => entries.iter().all(|entry| {
                    unsigned(entry)
                        .and_then(|i| usize::try_from(i).ok())
                        .is_some_and(pointer)
                }),
                _ => false,
            };
            errors.require(valid, "noalias arguments");
        }
        if self.op.attr("result_address") == Some(Attr::Bool(true)) {
            errors.require(pointer(0), "result address");
        }
    }

    fn matches_return(&self, expected: TypeId, actual: &[TypeId]) -> bool {
        actual == [expected] || (actual.is_empty() && UnitType::view(self.ctx, expected).is_some())
    }

    fn function(&self, result: Option<TypeId>, ret_attr: &str, errors: &mut Failures) {
        let ret = match self.op.attr(ret_attr) {
            Some(Attr::Type(t)) => Some(t),
            _ => None,
        };
        let mut args = Vec::new();
        let mut yields = Vec::new();
        let mut nodes = false;
        for &r in &self.op.regions() {
            let region = self.ctx.get_region(r);
            if region.is_nodes() {
                nodes = true;
                args.extend(region.ports().iter().filter_map(|v| self.ty(v.id())));
                yields.extend(region.results().into_iter().filter_map(|v| self.ty(v)));
            } else if let Some(entry) = self.ctx.region_entry(r) {
                args.extend(
                    self.ctx
                        .block_arguments(entry)
                        .iter()
                        .filter_map(|v| self.ty(v.id())),
                );
            }
        }
        args.retain(|t| !self.ctx.is_state_type(*t));
        yields.retain(|t| !self.ctx.is_state_type(*t));
        self.abi(&args, true, errors);
        let sig = result.and_then(|t| FnType::view(self.ctx, t));
        errors.require(
            sig.is_some_and(|s| s.params() == args && Some(s.ret()) == ret),
            "function signature",
        );
        if nodes {
            errors.require(
                ret.is_some_and(|t| self.matches_return(t, &yields)),
                "function yields",
            );
        }
    }

    fn function_return(&self, value: Option<Vec<TypeId>>, errors: &mut Failures) {
        let ret = self
            .scoped(|op| {
                let kind = self.ctx.operation_kind(op.id)?;
                let schema = self.ctx.schema_for_kind(kind)?;
                let ret_attr =
                    schema
                        .type_constraints
                        .iter()
                        .find_map(|constraint| match constraint {
                            TypeConstraint::FunctionDefinition(_, attr) => Some(*attr),
                            _ => None,
                        })?;
                Some(op.attr(ret_attr))
            })
            .flatten();
        let valid = match (ret, value) {
            (Some(Attr::Type(t)), Some(values)) => self.matches_return(t, &values),
            _ => false,
        };
        errors.require(valid, "return type");
    }

    fn function_call(
        &self,
        callee: Option<TypeId>,
        args: Option<Vec<TypeId>>,
        result: Option<TypeId>,
        errors: &mut Failures,
    ) {
        if let Some(args) = &args {
            self.abi(args, false, errors);
        }
        let valid = callee
            .and_then(|t| FnType::view(self.ctx, t))
            .zip(args)
            .is_some_and(|(sig, args)| {
                let params = sig.params();
                let matches = match params.split_last() {
                    Some((&last, fixed)) if self.ctx.is_variadic_tail(last) => {
                        args.starts_with(fixed)
                    }
                    _ => args == params,
                };
                matches && Some(sig.ret()) == result
            });
        errors.require(valid, "call signature");
        if let Some(v) = self.op.attr("resources") {
            errors.require(
                matches!(v, Attr::Array(xs) if xs.len() == 1 && xs[0].as_str() == Some("memory")),
                "call resource summary",
            );
        }
    }

    pub(super) fn fields(&self, errors: &mut Failures) {
        self.metadata(errors);
        let Some(schema) = self.schema else { return };
        self.layout(schema.type_constraints, errors);
        let port_group = |port| match port {
            ConstraintPort::Input(i) => (&self.inputs, i),
            ConstraintPort::Output(i) => (&self.outputs, i),
        };
        let list = |port| {
            let (group, index) = port_group(port);
            group.list(index)
        };
        for constraint in schema.type_constraints {
            match *constraint {
                TypeConstraint::TupleElements(input, result) => {
                    let tuple = self
                        .port_type(result)
                        .and_then(|t| TupleType::view(self.ctx, t));
                    errors.require(
                        tuple.is_some_and(|t| Some(t.elements()) == list(input)),
                        "tuple elements",
                    );
                }
                TypeConstraint::TupleProjection(tuple, index, result) => {
                    let tuple = self
                        .port_type(tuple)
                        .and_then(|t| TupleType::view(self.ctx, t));
                    let element = tuple.and_then(|t| {
                        let Attr::UInt(i) = self.op.attr(index)? else {
                            return None;
                        };
                        t.elements().get(usize::try_from(i).ok()?).copied()
                    });
                    errors.require(
                        element.is_some() && element == self.port_type(result),
                        "tuple projection",
                    );
                }
                TypeConstraint::StateFan(single, many, label) => {
                    let (single_group, single_index) = port_group(single);
                    let valid = single_group.complete
                        && single_group.one(single_index) == Some(TypeId::STATE)
                        && list(many).is_some_and(|types| {
                            !types.is_empty() && types.iter().all(|t| *t == TypeId::STATE)
                        });
                    errors.require(valid, label);
                }
                TypeConstraint::NonFunction(port, label) => errors.require(
                    self.port_type(port)
                        .and_then(|t| FnType::view(self.ctx, t))
                        .is_none(),
                    label,
                ),
                TypeConstraint::FunctionDefinition(result, ret_attr) => {
                    self.function(self.port_type(result), ret_attr, errors);
                }
                TypeConstraint::FunctionReturn(value) => self.function_return(list(value), errors),
                TypeConstraint::FunctionCall(callee, args, result) => {
                    self.function_call(
                        self.port_type(callee),
                        list(args),
                        self.port_type(result),
                        errors,
                    );
                }
                TypeConstraint::ModuleScope(label) => {
                    errors.require(
                        self.ctx.parent_op(self.op.id).is_none_or(|owner| {
                            self.ctx.operation_kind(owner)
                                == Some(tir_egglog::NativeIdentity::of::<ModuleOp>())
                        }),
                        label,
                    );
                }
                _ => {}
            }
        }
    }
}
