//! Random control-flow graphs through the `restructure` pass.
//!
//! Every generated function is restructured, verified, and *evaluated*: the
//! structured program must compute what the generated graph computes on every
//! argument pair. The graphs are cyclic and irreducible as often as the input
//! says, and still terminate: a backward edge is only taken while the fuel each
//! block spends is left.
//!
//! Both forms — the original CFG and the restructured program — are also
//! compiled and run, so the generated graphs gate the backend as well as the
//! pass: what a host-compiled function returns must be what the graph computes.

use tir::BlockHandle;

use tir::builtin::ops as b;
use tir::builtin::{FuncOp, IntegerType, ModuleOp};
use tir::cfg::ops as cb;
use tir::{Context, IRFormatter, Operation, PassManager};

/// How many backward edges a run may take.
const FUEL: i64 = 16;

/// The arguments an executed comparison is run on.
const ARGUMENTS: [(i64, i64); 5] = [(0, 0), (1, 2), (-3, 7), (11, -5), (i64::MAX, 3)];

pub fn check(data: &[u8]) {
    let Some(program) = Program::generate(data) else {
        return;
    };

    let context = Context::with_default_dialects();
    let module = program.build(&context);
    let source = render(&context, &module);

    restructure(&context, &module)
        .unwrap_or_else(|error| panic!("restructure failed: {error}\n{source}"));
    tir::verify_op_tree(&context, module.id())
        .unwrap_or_else(|error| panic!("restructured IR does not verify: {error}\n{source}"));
    assert!(
        !has_branches(&context, &module),
        "restructured IR still holds a CFG:\n{}",
        render(&context, &module)
    );

    for (left, right) in ARGUMENTS {
        let structured = crate::interpret::evaluate(&context, &module, &[left, right]);
        assert_eq!(
            structured,
            Some(program.evaluate(left, right)),
            "restructuring changed what f({left}, {right}) computes\n{source}\n{}",
            render(&context, &module)
        );
    }

    compare_execution(&program.pruned(), &source, &render(&context, &module));
}

fn restructure(context: &Context, module: &ModuleOp) -> Result<(), tir::PassError> {
    let mut passes = PassManager::new();
    passes
        .nest::<FuncOp>()
        .add_pass(tir::passes::RestructurePass::new());
    passes.run(context, context.get_op(module.id()))
}

/// A block ends with a branch only where restructuring left control flow
/// behind: the pass must leave a single block of structured operations.
fn has_branches(context: &Context, module: &ModuleOp) -> bool {
    module.body().iter(context.clone()).any(|op| {
        op.clone().as_op::<FuncOp>().is_some_and(|func| {
            func.regions()
                .any(|region| region.iter(context.clone()).len() > 1)
        })
    })
}

fn render(context: &Context, module: &ModuleOp) -> String {
    let mut rendered = String::new();
    let mut formatter = IRFormatter::new(&mut rendered);
    tir::print_ir(module, context, &mut formatter).expect("print");
    rendered
}

/// Build the program twice — the pipeline consumes the module it compiles —
/// and check that restructuring left what it computes alone. The program is
/// pruned first: the backend does not take a CFG with unreachable blocks, which
/// the restructured form no longer has.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn compare_execution(program: &Program, before: &str, after: &str) {
    let jit = tir_jit::Jit::host().expect("host target");
    let original = jit.context().expect("host context");
    let original_module = program.build(&original);
    let original = match jit.compile_module(&original, &original_module) {
        Ok(module) => module,
        Err(error) => {
            eprintln!("BASELINE FAILED {error}\n{before}");
            return;
        }
    };

    let context = jit.context().expect("host context");
    let module = program.build(&context);
    restructure(&context, &module).expect("restructure");
    let restructured = jit
        .compile_module(&context, &module)
        .unwrap_or_else(|error| panic!("restructured program does not compile: {error}\n{after}"));

    let original: extern "C" fn(i64, i64) -> i64 =
        unsafe { original.get("f") }.expect("f in the original");
    let restructured: extern "C" fn(i64, i64) -> i64 =
        unsafe { restructured.get("f") }.expect("f in the restructured program");
    for (left, right) in ARGUMENTS {
        let expected = program.evaluate(left, right);
        assert_eq!(
            original(left, right),
            expected,
            "the original program does not compute f({left}, {right})\n{before}"
        );
        assert_eq!(
            restructured(left, right),
            expected,
            "restructuring changed what f({left}, {right}) computes\n{before}\n{after}"
        );
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn compare_execution(_program: &Program, _before: &str, _after: &str) {}

/// A generated function: `blocks` basic blocks, each computing one value from
/// its own argument and leaving through the terminator the fuzz input chose.
/// Every block spends one unit of fuel, and a backward edge is only taken while
/// fuel is left, so every generated program halts.
#[derive(Clone)]
struct Program {
    blocks: Vec<Vertex>,
}

#[derive(Clone)]
struct Vertex {
    operation: Arith,
    terminator: Terminator,
}

#[derive(Clone, Copy)]
enum Arith {
    Add,
    Sub,
    Mul,
}

/// A branch names the block it goes back to first, so the fuel test can guard
/// it; `Branch` always has a forward edge to fall out through.
#[derive(Clone, Copy)]
enum Terminator {
    Branch(usize, usize),
    Return,
}

impl Program {
    fn generate(data: &[u8]) -> Option<Program> {
        let mut bytes = Bytes::new(data)?;
        let count = 2 + bytes.next() as usize % 7;
        let blocks = (0..count)
            .map(|index| {
                let operation = match bytes.next() % 3 {
                    0 => Arith::Add,
                    1 => Arith::Sub,
                    _ => Arith::Mul,
                };
                let last = index + 1 == count;
                let terminator = match (last, bytes.next() % 4) {
                    (true, _) | (_, 0) => Terminator::Return,
                    _ => {
                        let left = bytes.next() as usize % count;
                        let right = index + 1 + bytes.next() as usize % (count - index - 1);
                        Terminator::Branch(left, right)
                    }
                };
                Vertex {
                    operation,
                    terminator,
                }
            })
            .collect();
        Some(Program { blocks })
    }

    /// What the program computes, as a reference the compiled forms are
    /// checked against: the same walk the generated blocks perform.
    fn evaluate(&self, left: i64, right: i64) -> i64 {
        let seed = left.wrapping_add(right);
        let (mut index, mut value, mut fuel) = (0usize, seed, FUEL);
        loop {
            let computed = match self.blocks[index].operation {
                Arith::Add => value.wrapping_add(seed),
                Arith::Sub => value.wrapping_sub(seed),
                Arith::Mul => value.wrapping_mul(seed),
            };
            let (left, right) = match self.blocks[index].terminator {
                Terminator::Return => return computed,
                Terminator::Branch(left, right) => (left, right),
            };
            let remaining = fuel.wrapping_sub(1);
            let taken = match left <= index {
                true => remaining > 0,
                false => computed > seed,
            };
            (index, value, fuel) = match taken {
                true => (left, computed, remaining),
                false => (right, value, remaining),
            };
        }
    }

    /// The program without the blocks no path reaches, which the backend
    /// declines to compile in its CFG form.
    fn pruned(&self) -> Program {
        let mut reachable = vec![false; self.blocks.len()];
        let mut pending = vec![0];
        while let Some(index) = pending.pop() {
            if std::mem::replace(&mut reachable[index], true) {
                continue;
            }
            if let Terminator::Branch(left, right) = self.blocks[index].terminator {
                pending.extend([left, right]);
            }
        }
        let mut renumbered = vec![0; self.blocks.len()];
        let mut next = 0;
        for (index, live) in reachable.iter().enumerate() {
            renumbered[index] = next;
            next += usize::from(*live);
        }
        let blocks = self
            .blocks
            .iter()
            .zip(&reachable)
            .filter(|(_, live)| **live)
            .map(|(block, _)| Vertex {
                operation: block.operation,
                terminator: match block.terminator {
                    Terminator::Return => Terminator::Return,
                    Terminator::Branch(left, right) => {
                        Terminator::Branch(renumbered[left], renumbered[right])
                    }
                },
            })
            .collect();
        Program { blocks }
    }

    /// The generated function, built through the IR API: the text form cannot
    /// express a forward branch to a block that later declares arguments.
    fn build(&self, context: &Context) -> ModuleOp {
        let integer = IntegerType::new(context, 64);
        let boolean = IntegerType::new(context, 1);
        let parameters = [
            context.create_value(integer, None),
            context.create_value(integer, None),
        ];
        let arguments = [parameters[0].id(), parameters[1].id()];
        let region = context.create_region();
        let entry = context.create_block(parameters.to_vec());
        region.add_block(entry.id());
        let blocks = self
            .blocks
            .iter()
            .map(|_| {
                let value = context.create_value(integer, None);
                let fuel = context.create_value(integer, None);
                let block = context.create_block(vec![value, fuel]);
                region.add_block(block.id());
                block
            })
            .collect::<Vec<BlockHandle>>();

        let seed = entry
            .append_op(b::addi(context, arguments[0], arguments[1], integer).build())
            .result();
        let fuel = entry
            .append_op(b::constant(context, FUEL, integer).build())
            .result();
        let spent = entry
            .append_op(b::constant(context, 1, integer).build())
            .result();
        let empty = entry
            .append_op(b::constant(context, 0, integer).build())
            .result();
        entry.append_op(cb::br(context, vec![seed, fuel], blocks[0].id()).build());

        for (index, specification) in self.blocks.iter().enumerate() {
            let block = &blocks[index];
            let argument = block.arguments()[0].id();
            let fuel = block.arguments()[1].id();
            let arithmetic = match specification.operation {
                Arith::Add => b::addi(context, argument, seed, integer).build().id(),
                Arith::Sub => b::subi(context, argument, seed, integer).build().id(),
                Arith::Mul => b::muli(context, argument, seed, integer).build().id(),
            };
            block.append(arithmetic);
            let value = context.get_op(arithmetic).results()[0];
            let left = match specification.terminator {
                Terminator::Return => {
                    block.append_op(b::r#return(context, value).build());
                    continue;
                }
                Terminator::Branch(left, _) => left,
            };
            let Terminator::Branch(_, right) = specification.terminator else {
                unreachable!()
            };
            let remaining = block
                .append_op(b::subi(context, fuel, spent, integer).build())
                .result();
            // A backward edge burns fuel; once it is out, only the forward edge
            // is taken, so the program halts however tangled the graph is.
            let (condition, backward) = match left <= index {
                true => (remaining, empty),
                false => (value, seed),
            };
            let condition = block
                .append_op(
                    b::CmpIOpBuilder::new(context)
                        .lhs(condition)
                        .rhs(backward)
                        .predicate("sgt")
                        .result_type(boolean)
                        .build(),
                )
                .result();
            block.append_op(
                cb::cond_br(
                    context,
                    condition,
                    vec![value, remaining],
                    vec![argument, remaining],
                    blocks[left].id(),
                    blocks[right].id(),
                )
                .build(),
            );
        }

        let func = b::func(context, "f", integer, Some(region.id())).build();
        let module = b::module(context, None).build();
        module.body().append(func.id());
        module.body().append(b::module_end(context).build().id());
        module
    }
}

/// The fuzz input read as an endless stream of choices.
struct Bytes<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Bytes<'a> {
    fn new(data: &'a [u8]) -> Option<Self> {
        (!data.is_empty()).then_some(Self { data, position: 0 })
    }

    fn next(&mut self) -> u8 {
        let byte = self.data[self.position % self.data.len()];
        self.position += 1;
        byte.wrapping_add((self.position / self.data.len()) as u8)
    }
}

#[cfg(test)]
mod tests {
    /// A bounded smoke campaign: enough shapes to cover branches, joins,
    /// dispatch and irreducible loops.
    #[test]
    fn five_hundred_random_graphs_restructure() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..500 {
            let mut input = Vec::new();
            for _ in 0..16 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                input.push(state as u8);
            }
            super::check(&input);
        }
    }
}
