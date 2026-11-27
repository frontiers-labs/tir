use super::{Attr, Check, Failures};
use crate::{
    OpHandle, TypeArg, TypeId,
    builtin::{FnType, IndexType, TupleType, UnitType},
    ptr::PtrType,
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

    fn layout(&self, errors: &mut Failures) {
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
        let input = self.inputs.one(1);
        let output = self.outputs.one(0);
        match self.kind.as_str() {
            "builtin.bitcast" => {
                let w = width(self.inputs.one(0));
                errors.require(w.is_some() && w == width(output), "bitcast width");
            }
            "ptr.ptradd" if pointer.is_some() => {
                errors.require(width(input) == pointer, "pointer offset width")
            }
            "ptr.ptrdiff" if pointer.is_some() => {
                errors.require(width(output) == pointer, "pointer difference width")
            }
            "ptr.disjoint" => {
                errors.require(
                    input.is_some() && input == self.inputs.one(3),
                    "disjoint size types",
                );
                if pointer.is_some() {
                    errors.require(width(input) == pointer, "disjoint size width");
                }
            }
            _ => {}
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
        if self.kind.as_str() == "type.def"
            && let Some(owner) = self.ctx.parent_op(self.op.id)
        {
            errors.require(
                self.ctx
                    .operation_kind(owner)
                    .map(tir_egglog::NativeIdentity::as_str)
                    == Some("builtin.module"),
                "type definition outside module scope",
            );
        }
    }

    fn abi(&self, args: &[TypeId], errors: &mut Failures) {
        let pointer = |i| {
            args.get(i)
                .is_some_and(|t| PtrType::view(self.ctx, *t).is_some())
        };
        if let Some(v) = self.op.attr("argument_alignments") {
            errors.require(matches!(v,Attr::Array(entries) if entries.len() == args.len() && entries.iter().all(|x| match x {Attr::UInt(n)=>n.is_power_of_two(),_=>unsigned(x).is_some_and(u64::is_power_of_two)})), "argument alignment");
        }
        if self.kind.as_str() == "func.func"
            && let Some(v) = self.op.attr("noalias")
        {
            errors.require(matches!(v,Attr::Array(entries) if entries.iter().all(|x| unsigned(x).is_some_and(|i|pointer(i as usize)))),"noalias arguments");
        }
        if self.op.attr("result_address") == Some(Attr::Bool(true)) {
            errors.require(pointer(0), "result address");
        }
    }

    fn function(&self, errors: &mut Failures) {
        let ret = match self.op.attr("ret_type") {
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
        self.abi(&args, errors);
        let sig = self.outputs.one(0).and_then(|t| FnType::view(self.ctx, t));
        errors.require(
            sig.is_some_and(|s| s.params() == args && Some(s.ret()) == ret),
            "function signature",
        );
        if nodes {
            errors.require(
                ret.is_some_and(|t| {
                    yields == [t] || (yields.is_empty() && UnitType::view(self.ctx, t).is_some())
                }),
                "function yields",
            );
        }
    }

    pub(super) fn fields(&self, errors: &mut Failures) {
        self.metadata(errors);
        let a = self.inputs.one(0);
        let b = self.outputs.one(0);
        match self.kind.as_str() {
            "builtin.bitcast" | "ptr.ptrdiff" | "ptr.ptradd" | "ptr.disjoint" => {
                self.layout(errors)
            }
            "builtin.make_tuple" => {
                let tuple = b.and_then(|t| TupleType::view(self.ctx, t));
                errors.require(
                    tuple.is_some_and(|t| Some(t.elements()) == self.inputs.list(0)),
                    "tuple elements",
                );
            }
            "builtin.tuple_get" => {
                let element = a.and_then(|t| TupleType::view(self.ctx, t)).and_then(|t| {
                    let Attr::UInt(i) = self.op.attr("index")? else {
                        return None;
                    };
                    t.elements().get(i as usize).copied()
                });
                errors.require(element.is_some() && element == b, "tuple projection");
            }
            "state.join" | "state.split" => {
                let join = self.kind.as_str() == "state.join";
                let (single, many) = if join {
                    (&self.outputs, &self.inputs)
                } else {
                    (&self.inputs, &self.outputs)
                };
                let valid = single.complete
                    && single.list(0).as_deref() == Some(&[TypeId::STATE])
                    && many
                        .list(0)
                        .is_some_and(|v| !v.is_empty() && v.iter().all(|t| *t == TypeId::STATE));
                errors.require(valid, if join { "memory join" } else { "memory split" });
            }
            "ptr.load" => errors.require(
                b.and_then(|t| FnType::view(self.ctx, t)).is_none(),
                "load moves a function value through memory",
            ),
            "ptr.store" => errors.require(
                a.and_then(|t| FnType::view(self.ctx, t)).is_none(),
                "store moves a function value through memory",
            ),
            "func.func" => self.function(errors),
            "func.return" => {
                let ret = self
                    .scoped(|op| {
                        (self
                            .ctx
                            .operation_kind(op.id)
                            .map(tir_egglog::NativeIdentity::as_str)
                            == Some("func.func"))
                        .then(|| op.attr("ret_type"))
                    })
                    .flatten();
                let valid = match (ret, self.inputs.list(0)) {
                    (Some(Attr::Type(t)), Some(args)) => {
                        args == [t] || (args.is_empty() && UnitType::view(self.ctx, t).is_some())
                    }
                    _ => false,
                };
                errors.require(valid, "return type");
            }
            "func.call" => {
                let args = self.inputs.list(1);
                if let Some(args) = &args {
                    self.abi(args, errors);
                }
                let valid = a
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
                        matches && Some(sig.ret()) == b
                    });
                errors.require(valid, "call signature");
                if let Some(v) = self.op.attr("resources") {
                    errors.require(matches!(v,Attr::Array(xs) if xs.len()==1 && xs[0].as_str()==Some("memory")),"call resource summary");
                }
            }
            _ => {}
        }
    }
}
