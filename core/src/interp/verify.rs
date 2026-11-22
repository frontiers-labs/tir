use super::*;

/// The function, block and instruction containing an assembly error, when known.
#[derive(Debug, PartialEq, Eq)]
pub struct VerifyError {
    pub function: usize,
    pub block: Option<usize>,
    pub instruction: Option<usize>,
    pub reason: &'static str,
}

type Check<T = ()> = Result<T, &'static str>;

impl Builder {
    /// Verify every function, including unreachable blocks, before execution.
    pub fn build(self) -> Result<Program, VerifyError> {
        for (index, f) in self.functions.iter().enumerate() {
            let error = |block, instruction, reason| VerifyError {
                function: index,
                block,
                instruction,
                reason,
            };
            if f.result_width == 0 || f.widths.contains(&0) || f.blocks.is_empty() {
                return Err(error(
                    None,
                    None,
                    "expected nonzero widths and an entry block",
                ));
            }
            for edge in &f.edges {
                let target = f
                    .blocks
                    .get(edge.block as usize)
                    .ok_or_else(|| error(None, None, "invalid target block"))?;
                f.check_args(edge.args, f, target)
                    .map_err(|reason| error(None, None, reason))?;
            }
            for args in &f.args {
                if args.iter().any(|r| f.width(*r).is_err()) {
                    return Err(error(None, None, "invalid argument register"));
                }
            }
            for (b, block) in f.blocks.iter().enumerate() {
                let mut live = vec![false; f.widths.len()];
                for &r in &block.params {
                    let slot = live
                        .get_mut(r.0 as usize)
                        .ok_or_else(|| error(Some(b), None, "invalid parameter"))?;
                    if std::mem::replace(slot, true) {
                        return Err(error(Some(b), None, "duplicate parameter"));
                    }
                }
                if block.code.is_empty() {
                    return Err(error(Some(b), None, "empty block"));
                }
                for (pc, &inst) in block.code.iter().enumerate() {
                    let terminal = matches!(
                        inst,
                        Inst::Jump(_) | Inst::Branch(..) | Inst::Return(_) | Inst::Trap
                    );
                    if terminal != (pc + 1 == block.code.len()) {
                        return Err(error(
                            Some(b),
                            Some(pc),
                            "expected exactly one final terminator",
                        ));
                    }
                    if let Some(dst) = self
                        .check(f, &live, inst)
                        .map_err(|e| error(Some(b), Some(pc), e))?
                    {
                        live[dst.0 as usize] = true;
                    }
                }
            }
        }
        Ok(Program(self))
    }

    fn check(&self, f: &Function, live: &[bool], inst: Inst) -> Check<Option<Reg>> {
        let read = |r: Reg| {
            let w = f.width(r)?;
            live[r.0 as usize]
                .then_some(w)
                .ok_or("uninitialized register")
        };
        let read_args = |args: Args| -> Check {
            for &reg in f.args.get(args.0 as usize).ok_or("invalid argument list")? {
                read(reg)?;
            }
            Ok(())
        };
        let edge = |id: EdgeId| {
            let e = f.edges.get(id.0 as usize).ok_or("invalid edge index")?;
            read_args(e.args)
        };
        use Inst::*;
        let write = |dst, width| {
            same(f.width(dst)?, width)?;
            Ok(Some(dst))
        };
        match inst {
            Constant(d, k) => {
                let v = self.constants.get(k.0 as usize).ok_or("invalid constant")?;
                write(
                    d,
                    u32::try_from(v.width()).map_err(|_| "constant too wide")?,
                )
            }
            PoisonIf(d, v, c) => {
                same(read(c)?, 1)?;
                write(d, read(v)?)
            }
            Load(d, a) => {
                read(a)?;
                write(d, f.width(d)?)
            }
            Add(d, a, b) | Sub(d, a, b) | Eq(d, a, b) | Ult(d, a, b) => {
                let w = read(a)?;
                same(w, read(b)?)?;
                write(
                    d,
                    if matches!(inst, Eq(..) | Ult(..)) {
                        1
                    } else {
                        w
                    },
                )
            }
            Call(d, id, args) => {
                let target = self.functions.get(id.0 as usize).ok_or("invalid callee")?;
                read_args(args)?;
                f.check_args(
                    args,
                    target,
                    target.blocks.first().ok_or("missing callee entry")?,
                )?;
                write(d, target.result_width)
            }
            Require(r) => {
                same(read(r)?, 1)?;
                Ok(None)
            }
            Jump(id) => {
                edge(id)?;
                Ok(None)
            }
            Branch(r, t, e) => {
                same(read(r)?, 1)?;
                edge(t)?;
                edge(e)?;
                Ok(None)
            }
            Return(r) => {
                same(read(r)?, f.result_width)?;
                Ok(None)
            }
            Trap => Ok(None),
        }
    }
}

impl Function {
    fn check_args(&self, args: Args, target: &Function, block: &Block) -> Check {
        let args = self
            .args
            .get(args.0 as usize)
            .ok_or("invalid argument list")?;
        if args.len() != block.params.len() {
            return Err("argument count mismatch");
        }
        for (&src, &dst) in args.iter().zip(&block.params) {
            same(self.width(src)?, target.width(dst)?)?;
        }
        Ok(())
    }

    fn width(&self, r: Reg) -> Check<u32> {
        self.widths
            .get(r.0 as usize)
            .copied()
            .ok_or("invalid register")
    }
}

fn same(left: u32, right: u32) -> Check {
    (left == right).then_some(()).ok_or("width mismatch")
}
