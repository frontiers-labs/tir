//! The selection automaton's tables, built from small rule lists, and the
//! rules generated from them, run on small graphs.

use smallvec::{smallvec, SmallVec};
use tir::backend::isel::automaton::{
    chain, Access, Arg, Automaton, Builder, Child, Described, Form, Held, Operand, Pattern, Row,
    Seed, Step, Synthesized, Tree,
};
use tir::backend::isel::edges::Cases;
use tir::backend::isel::select::{declare, Rules, Select, DEPTH, REGION, USES};
use tir::backend::isel::ImmRange;
use tir::sem::node::{int_carrier, WIDEST_RESULT};
use tir::sem::{template_node, IrOp, Kind, Prov, SemNode, SemPayload, SymKind};
use tir::TypeId;
use tir_adt::APInt;
use tir_egraph::{Engine, NoExterns, Ref, Vocabulary};

use Operand::{Immediate, Literal, Register};

// The seeds: a 32-bit and a 64-bit register, and the zeros of the constants
// 32, 64 and 4 bits wide.
const R32: Pattern = 0;
const R64: Pattern = 1;
const C32: Pattern = 2;
const C64: Pattern = 3;
const C4: Pattern = 4;

fn seeds() -> Vec<Seed> {
    let register = |ty| Seed::Register(TypeId::from_number(ty));
    let constant = Seed::Constant;
    vec![
        register(1),
        register(2),
        constant(32),
        constant(64),
        constant(4),
    ]
}

// The operations the rules are over, by instance number, and their labels.
const ADD: u32 = 0;
const MUL: u32 = 1;
const LOAD: u32 = 2;
const DIV: u32 = 3;
const REM: u32 = 4;
const NOT: u32 = 5;
const SUB: u32 = 6;
const WIDE: u32 = 7;
const STORE: u32 = 8;
const LANEWISE: u32 = 9;

fn described() -> Vec<Option<Described>> {
    described_as(&mut |label| label)
}

// The operations with the label `named` gives each.
fn described_as(named: &mut dyn FnMut(u32) -> u32) -> Vec<Option<Described>> {
    let operands = |count: u8, (register, constant): (Pattern, Pattern)| -> SmallVec<[Child; 5]> {
        let operand = |operand| Child::Operand {
            operand,
            width: 32,
            register,
            constant: Some(constant),
        };
        (0..count).map(operand).collect()
    };
    let mut plain = |label: u32, count: u8| Described {
        label: named(label),
        carrier: 32,
        children: operands(count, (R32, C32)),
        access: None,
        offsets: 0,
        negated: 0,
    };
    let add = Described {
        offsets: 0b11,
        ..plain(10, 2)
    };
    // A read of four bytes: the address, the bytes, the address space, the
    // state. A write has the value after the bytes.
    let fixed = |value| Child::Fixed {
        value,
        width: 4,
        constant: C4,
    };
    let address = operands(1, (R32, C32))[0];
    let read = |operand| Child::Operand {
        operand,
        width: 32,
        register: R32,
        constant: None,
    };
    let load = Described {
        children: smallvec![address, fixed(4), fixed(0), read(1)],
        access: Some(Access {
            state: 3,
            writes: false,
        }),
        ..plain(12, 1)
    };
    let store = Described {
        children: smallvec![address, fixed(4), read(1), fixed(0), read(2)],
        access: Some(Access {
            state: 4,
            writes: true,
        }),
        ..plain(17, 1)
    };
    let sub = Described {
        negated: 0b10,
        ..plain(16, 2)
    };
    // A division with the label of the 32-bit one over 64-bit operands.
    let wide = Described {
        children: operands(2, (R64, C64)),
        ..plain(13, 2)
    };
    // A sum of two vectors: the operands, then the lane count.
    let mut lanewise = plain(18, 2);
    lanewise.children.push(Child::Lanes {
        width: 64,
        constant: C64,
    });
    let rest = [
        plain(13, 2),
        plain(14, 2),
        plain(15, 1),
        sub,
        wide,
        store,
        lanewise,
    ];
    [add, plain(11, 2), load]
        .into_iter()
        .chain(rest)
        .map(Some)
        .collect()
}

fn row(instance: u32, operands: &[Operand]) -> Row {
    Row {
        instance,
        operands: operands.iter().copied().collect(),
    }
}

// A rule of one instruction, form `form`, over its operands in order.
fn rule(rows: Vec<Row>, form: u32, operands: u8) -> Synthesized {
    let tree: Tree = smallvec![Step {
        form,
        args: (0..operands).map(Arg::Operand).collect(),
    }];
    Synthesized {
        rows: rows.into_iter().collect(),
        tree,
        value: Arg::Step(0),
        cost: 16 + form,
        fields: SmallVec::new(),
        port: 0,
        results: 1,
        effect: false,
        branch: None,
    }
}

fn build(rules: &[Synthesized]) -> Automaton {
    Automaton::build(
        rules,
        &described(),
        seeds(),
        &mut Vocabulary::new(tir::Context::with_default_dialects().names()),
    )
}

fn rows(table: &tir_relational::Words) -> Vec<Vec<u32>> {
    let cell = |row: usize| (0..table.width()).map(move |column| table.column(column)[row]);
    (0..table.len()).map(|row| cell(row).collect()).collect()
}

// The patterns the rows of `table` under a label, its children's states
// and their offset classes step to.
fn step(
    automaton: &Automaton,
    table: &[std::sync::Arc<tir_relational::Words>],
    label: u32,
    states: &[Pattern],
    classes: &[u32],
) -> Vec<Pattern> {
    let chains = states.iter().map(|&state| automaton.chains[state as usize]);
    let key: Vec<u32> = std::iter::once(label)
        .chain(chains)
        .chain(classes.iter().copied())
        .collect();
    let rows = rows(&table[states.len()]);
    let stepped = rows.iter().filter(|row| row[..key.len()] == key[..]);
    stepped.map(|row| row[key.len()]).collect()
}

fn form(automaton: &Automaton, pattern: Pattern) -> u32 {
    let accept = automaton.accept[pattern as usize].as_ref();
    accept.expect("the pattern accepts a tree").tree[0].form
}

#[test]
fn rules_with_a_common_prefix_share_its_pattern() {
    let under = || row(NOT, &[Register(0)]);
    let automaton = build(&[
        rule(vec![row(NOT, &[Register(0)])], 0, 1),
        rule(
            vec![row(DIV, &[Operand::Row(1), Register(1)]), under()],
            1,
            2,
        ),
        rule(
            vec![row(REM, &[Operand::Row(1), Register(1)]), under()],
            2,
            2,
        ),
    ]);
    let [not] = step(&automaton, &automaton.step, 15, &[R32], &[0])[..] else {
        panic!("one pattern for the row both rules fold");
    };
    assert_eq!(form(&automaton, not), 0);
    let [div] = step(&automaton, &automaton.step, 13, &[not, R32], &[1, 0])[..] else {
        panic!("the division over the folded row");
    };
    let [rem] = step(&automaton, &automaton.step, 14, &[not, R32], &[1, 0])[..] else {
        panic!("the remainder over the folded row");
    };
    assert_eq!((form(&automaton, div), form(&automaton, rem)), (1, 2));
    // The patterns are numbered after the seeds, and every state has the
    // label of its chain.
    assert_eq!((not, automaton.patterns()), (5, 8));
    let chains: std::collections::HashSet<u32> = automaton.chains.iter().copied().collect();
    assert_eq!(chains.len(), 8);
    // The register under the folded row is the form's child's child.
    let accept = automaton.accept[div as usize].as_ref().unwrap();
    assert_eq!(accept.operands[0].path.as_slice(), [0, 0]);
    assert_eq!(accept.operands[1].path.as_slice(), [1]);
}

#[test]
fn rows_of_two_labels_never_share_a_pattern() {
    // One instruction defines both values: the trees are the same.
    let result = |instance, port| Synthesized {
        port,
        results: 2,
        ..rule(vec![row(instance, &[Register(0), Register(1)])], 7, 2)
    };
    let automaton = build(&[result(DIV, 0), result(REM, 1)]);
    let div = step(&automaton, &automaton.step, 13, &[R32, R32], &[0, 0]);
    let rem = step(&automaton, &automaton.step, 14, &[R32, R32], &[0, 0]);
    assert_eq!((div.len(), rem.len()), (1, 1));
    assert_ne!(div, rem);
    let accept = |pattern: &[Pattern]| automaton.accept[pattern[0] as usize].clone().unwrap();
    assert_eq!((accept(&div).port, accept(&rem).port), (0, 1));
    assert_eq!(accept(&div).tree[0].form, accept(&rem).tree[0].form);
}

#[test]
fn one_label_over_two_operand_types_is_two_patterns() {
    let over = |instance, form| rule(vec![row(instance, &[Register(0), Register(1)])], form, 2);
    let automaton = build(&[over(DIV, 0), over(WIDE, 1)]);
    let narrow = step(&automaton, &automaton.step, 13, &[R32, R32], &[0, 0]);
    let wide = step(&automaton, &automaton.step, 13, &[R64, R64], &[0, 0]);
    assert_eq!(
        (form(&automaton, narrow[0]), form(&automaton, wide[0])),
        (0, 1)
    );
    assert!(automaton.shadowed.is_empty());
    assert!(step(&automaton, &automaton.step, 13, &[R32, R64], &[0, 0]).is_empty());
    // A constant is built into the register the rule reads, under the seed
    // of a constant of that width alone.
    let built = step(&automaton, &automaton.step, 13, &[R32, C32], &[0, 0]);
    assert_eq!(built, narrow);
    assert!(step(&automaton, &automaton.step, 13, &[R32, C64], &[0, 0]).is_empty());
}

#[test]
fn an_immediate_a_literal_and_a_register_step_one_key_by_class() {
    let byte = ImmRange {
        width: 8,
        signed: true,
        align: 1,
        nonzero: false,
    };
    let held = Synthesized {
        fields: smallvec![(1, byte)],
        ..rule(vec![row(DIV, &[Register(0), Immediate(1)])], 1, 2)
    };
    let one = rule(vec![row(DIV, &[Register(0), Literal(1)])], 2, 1);
    let register = rule(vec![row(DIV, &[Register(0), Register(1)])], 0, 2);
    let automaton = build(&[register, held, one]);
    let key = [
        13,
        automaton.chains[R32 as usize],
        automaton.chains[C32 as usize],
        1,
    ];
    let table = rows(&automaton.imm[2]);
    let classes: Vec<&[u32]> = table
        .iter()
        .filter(|row| row[..4] == key)
        .map(|row| &row[4..])
        .collect();
    assert_eq!(
        classes,
        [
            // Any offset; a signed byte; the literal one.
            &[0, 32, 0, 0, 0, 0, 32, 0, 0][..],
            &[1, 32, 0, 0, 7, 1, 8, 0, 0],
            &[2, 32, 0, 1, 0, 0, 0, 0, 0],
        ]
    );
    let steps = |class| step(&automaton, &automaton.step, 13, &[R32, C32], &[0, class]);
    let forms: Vec<u32> = (0..3)
        .map(|class| form(&automaton, steps(class)[0]))
        .collect();
    assert_eq!(forms, [0, 1, 2]);
    assert_eq!(automaton.field[steps(0)[0] as usize], 0);
    assert_eq!(automaton.field[steps(1)[0] as usize], 0b10);
    let accept = automaton.accept[steps(1)[0] as usize].as_ref().unwrap();
    assert_eq!(accept.operands[1].held, Held::Constant);
    // A register at an offset is no constant: it steps the register rule
    // alone.
    let table = rows(&automaton.imm[2]);
    let key = [
        13,
        automaton.chains[R32 as usize],
        automaton.chains[R32 as usize],
        1,
    ];
    assert_eq!(table.iter().filter(|row| row[..4] == key).count(), 1);
}

// The read of the address `r0` in the state `r1`, and the product of `r2`
// and that read, folded.
fn load() -> Row {
    row(LOAD, &[Register(0), Register(1)])
}

fn folded_load() -> Vec<Row> {
    vec![row(MUL, &[Register(2), Operand::Row(1)]), load()]
}

#[test]
fn a_folded_access_steps_the_effect_table() {
    let automaton = build(&[rule(vec![load()], 0, 2), rule(folded_load(), 1, 3)]);
    // The bytes and the address space are literals of the label.
    let states = [R32, C4, C4, R32];
    let alone = step(&automaton, &automaton.step, 12, &states, &[0, 1, 1, 0]);
    let under = step(
        &automaton,
        &automaton.step_effect,
        12,
        &states,
        &[0, 1, 1, 0],
    );
    assert_eq!((alone.len(), under.len()), (1, 1));
    assert_ne!(alone, under);
    assert!(automaton.accept[under[0] as usize].is_none());
    assert_eq!(rows(&automaton.effects[4]), [[12, 3, 0]]);
    // The reader steps the table of the rows over a folded access, which
    // it reads where it is and at no offset from there.
    let states = [R32, under[0]];
    assert!(step(&automaton, &automaton.step, 11, &states, &[0, 1]).is_empty());
    let reader = step(&automaton, &automaton.step_over, 11, &states, &[0, 1]);
    assert_eq!(automaton.carried[reader[0] as usize], 0b10);
    let accept = automaton.accept[reader[0] as usize].as_ref().unwrap();
    assert_eq!(accept.operands[0].path.as_slice(), [1, 0]);
    let unfolded = [R32, alone[0]];
    assert!(step(&automaton, &automaton.step_over, 11, &unfolded, &[0, 1]).is_empty());
}

#[test]
fn a_value_read_twice_steps_the_tied_table() {
    let automaton = build(&[
        rule(vec![row(DIV, &[Register(0), Register(1)])], 0, 2),
        rule(vec![row(DIV, &[Register(0), Register(0)])], 1, 1),
    ]);
    let plain = step(&automaton, &automaton.step, 13, &[R32, R32], &[0, 0]);
    let tied = step(&automaton, &automaton.step_tied, 13, &[R32, R32], &[0, 0]);
    assert_eq!(
        (form(&automaton, plain[0]), form(&automaton, tied[0])),
        (0, 1)
    );
    assert!(automaton.ties[plain[0] as usize].is_empty());
    let [(first, again)] = &automaton.ties[tied[0] as usize][..] else {
        panic!("one pair of paths");
    };
    assert_eq!((first.as_slice(), again.as_slice()), (&[0][..], &[1][..]));
}

#[test]
fn a_sum_with_a_constant_is_an_offset_of_its_reader_child() {
    let disp = ImmRange {
        width: 12,
        signed: true,
        align: 4,
        nonzero: false,
    };
    let sum = || row(ADD, &[Register(0), Immediate(1)]);
    let field = |rule: Synthesized| Synthesized {
        fields: smallvec![(1, disp)],
        ..rule
    };
    let read = row(LOAD, &[Operand::Row(1), Register(2)]);
    let displaced = field(rule(vec![read, sum()], 0, 3));
    let difference = field(rule(vec![row(SUB, &[Register(0), Immediate(1)])], 2, 2));
    let automaton = build(&[displaced, field(rule(vec![sum()], 1, 2)), difference]);
    // A function's graph has no row for the sum or the difference: nothing
    // steps on them, and their rules build a register at an offset, the
    // difference at the offsets whose negation its field holds.
    assert!(automaton.step[2].is_empty());
    let [adder, subtractor] = &automaton.adders[..] else {
        panic!("the sum and the difference are the builders");
    };
    assert_eq!((adder.base, adder.accept.tree[0].form), (R32, 1));
    assert_eq!(adder.class, [32, 0, 0, 11, 1, 12, 3, 0]);
    assert_eq!((subtractor.base, subtractor.accept.tree[0].form), (R32, 2));
    assert_eq!(subtractor.class, [32, 0, 0, 11, 0, 12, 3, 0]);
    assert_eq!(subtractor.accept.operands[1].held, Held::Negated);
    assert!(automaton.unkeyed.is_empty());
    let states = [R32, C4, C4, R32];
    let [load] = step(&automaton, &automaton.step, 12, &states, &[1, 1, 1, 0])[..] else {
        panic!("the read of a register at a displacement");
    };
    assert_eq!(automaton.field[load as usize], 0b111);
    let accept = automaton.accept[load as usize].as_ref().unwrap();
    let held: Vec<_> = accept.operands.iter().map(|bind| bind.held).collect();
    assert_eq!(held, [Held::Base, Held::Offset, Held::Register]);
    let paths: Vec<_> = accept.operands.iter().map(|bind| bind.path[0]).collect();
    assert_eq!(paths, [0, 0, 3]);
}

// An operation of the rules as a function's graph spells it: 32 bits wide,
// over its operands and, for the read, the two constants its label fixes.
fn operation(label: u32, ty: TypeId) -> SemNode {
    let (name, arity) = match label {
        10 => ("add", 2),
        11 => ("mul", 2),
        12 => ("load", 4),
        13 => ("div", 2),
        14 => ("rem", 2),
        15 => ("not", 1),
        17 => ("store", 5),
        18 => ("lanewise", 3),
        _ => ("sub", 2),
    };
    SemNode {
        kind: Kind::Ir(IrOp::template("builtin", name, Vec::new())),
        payload: None,
        ty: Some(ty),
        children: vec![tir_egraph::ClassId::from_raw(0); arity],
        prov: Prov::None,
        carrier: int_carrier(32),
    }
}

// A graph over the automaton of `rules`, with the rules generated from it:
// two registers, the state the function is entered in and the zeros of the
// constants, each with its seed chain.
struct Selecting {
    g: Engine<SemNode>,
    automaton: Automaton,
    rules: Rules,
    cases: Cases,
    // The type of every operation, and the context it is interned in.
    word: TypeId,
    _context: tir::Context,
    x: Ref,
    y: Ref,
    entered: Ref,
}

impl Selecting {
    fn new(rules: &[Synthesized]) -> Self {
        let context = tir::Context::with_default_dialects();
        let word = tir::builtin::IntegerType::new(&context, 32);
        let mut vocabulary = Vocabulary::new(context.names());
        let described = described_as(&mut |label| vocabulary.intern(&operation(label, word)).0);
        let mut automaton = Automaton::build(rules, &described, seeds(), &mut vocabulary);
        // Any 32-bit constant is built by one instruction that costs three.
        automaton.forms.push(Form {
            rule: 0,
            port: 0,
            symbols: Vec::new(),
            ranges: Vec::new(),
            registers: Vec::new(),
            branch: None,
            cost: 3,
        });
        automaton.builders.push(Builder {
            form: 0,
            symbol: 0,
            range: ImmRange {
                width: 32,
                signed: true,
                align: 1,
                nonzero: false,
            },
            shift: 0,
            register: None,
            shifts: false,
            width: None,
            bits: 64,
            result: None,
            masks: false,
        });
        // Synthesis made an instance of every operation but the remainder.
        let instantiated = [
            "add", "div", "lanewise", "load", "mul", "not", "store", "sub",
        ];
        automaton.instantiated = instantiated.map(|name| ("builtin", name)).to_vec();
        let rules = Rules::of(&automaton);
        let mut g = Engine::over(vocabulary);
        for table in &rules.tables {
            g.add_words(table.clone());
        }
        declare(&mut g);
        let mut leaf = |serial| {
            let symbol = template_node(SymKind::Symbol, None, Some(word));
            g.add(SemNode {
                payload: Some(SemPayload::Opaque(serial)),
                ..symbol.carried(int_carrier(32))
            })
        };
        let (x, y, entered) = (leaf(0), leaf(1), leaf(2));
        let mut selecting = Selecting {
            g,
            automaton,
            rules,
            cases: Cases::default(),
            word,
            _context: context,
            x,
            y,
            entered,
        };
        let zeros = [(32, C32), (4, C4), (64, C64)];
        let zeros = zeros.map(|(width, seed)| (selecting.constant(width, 0), seed));
        for leaf in [x, y, entered] {
            selecting.g.raise_fact(DEPTH, leaf.class, 0);
            selecting.read(leaf);
        }
        for (class, seed) in zeros {
            let seeded = selecting.g.insert(chain(seed), &[class]);
            selecting.g.union(seeded, class).unwrap();
        }
        selecting
    }

    // A value some operation the graph holds no row for defines: nothing
    // says how many writes it waits for.
    fn unordered(&mut self) -> Ref {
        let symbol = template_node(SymKind::Symbol, None, Some(self.word));
        let leaf = self.g.add(SemNode {
            payload: Some(SemPayload::Opaque(3)),
            ..symbol.carried(int_carrier(32))
        });
        self.read(leaf);
        leaf
    }

    // A result of an operation the graph does not describe, which reads
    // `read` and, where `changes`, changes a state it publishes.
    fn result(&mut self, changes: bool, read: &[Ref]) -> Ref {
        let node = SemNode {
            kind: Kind::Result { index: 0, changes },
            payload: Some(SemPayload::Input),
            ty: Some(self.word),
            children: vec![tir_egraph::ClassId::from_raw(0); read.len()],
            prov: Prov::None,
            carrier: int_carrier(32),
        };
        let result = self.g.insert(node, read);
        self.read(result);
        result
    }

    // Give `value` the chain of a 32-bit register, as a row that reads it
    // makes the seeder do.
    fn read(&mut self, value: Ref) {
        let seeded = self.g.insert(chain(R32), &[value]);
        self.g.union(seeded, value).unwrap();
    }

    fn constant(&mut self, width: u32, value: u64) -> Ref {
        self.g.add(SemNode::constant(
            APInt::new(u64::from(width), value),
            Prov::None,
        ))
    }

    fn row(&mut self, label: u32, operands: &[Ref]) -> Ref {
        self.g.insert(operation(label, self.word), operands)
    }

    // The read of four bytes at `address` in `state`.
    fn load(&mut self, address: Ref, state: Ref) -> Ref {
        let fixed = [self.constant(4, 4), self.constant(4, 0)];
        self.row(12, &[address, fixed[0], fixed[1], state])
    }

    // The state the write of `value` at `address` in `state` leaves.
    fn store(&mut self, address: Ref, value: Ref, state: Ref) -> Ref {
        let fixed = [self.constant(4, 4), self.constant(4, 0)];
        let written = self.row(17, &[address, fixed[0], value, fixed[1], state]);
        self.read(written);
        written
    }

    // What the use lists say of a read one operation reads, where it is
    // defined.
    fn read_once(&mut self, read: Ref) {
        self.read(read);
        self.g.raise_fact(USES, read.class, 1);
        self.g.raise_fact(REGION, read.class, 7);
    }

    fn saturate(&mut self) {
        self.g.rebuild();
        self.rules.order(&mut self.g, usize::MAX);
        let selecting = &self.rules.selecting;
        self.g.saturate_rules(selecting, &NoExterns, 8, usize::MAX);
    }

    fn select(&self) -> Select<'_> {
        Select {
            automaton: &self.automaton,
            rules: &self.rules,
            cases: &self.cases,
        }
    }

    // What `value` is extracted as: the instruction of the chain chosen for
    // its class, the children of that chain's form, and the price of it all.
    fn selected(&self, value: Ref) -> (u32, Vec<Ref>, u64) {
        let select = self.select();
        let g = &self.g;
        let extraction =
            g.extract_best(|class, node, operands, _| select.price(g, class, node, operands));
        let class = g.root(value.class);
        let row = extraction.row(class).expect("an extracted class");
        let Kind::State(pattern) = g.node(row).kind else {
            panic!("the class is extracted as {:?}", g.node(row).kind);
        };
        let formed = g.rows(g.children(row)[0].class).next().expect("a form");
        assert_eq!(g.node(formed).kind, Kind::Form(pattern));
        let price = extraction.cost(class).expect("a priced class");
        (
            form(&self.automaton, pattern),
            g.children(formed).to_vec(),
            price,
        )
    }
}

fn byte() -> ImmRange {
    ImmRange {
        width: 8,
        signed: true,
        align: 1,
        nonzero: false,
    }
}

#[test]
fn a_binary_row_steps_by_the_class_its_constant_fits_and_pays_for_an_offset() {
    let held = Synthesized {
        fields: smallvec![(1, byte())],
        ..rule(vec![row(DIV, &[Register(0), Immediate(1)])], 0, 2)
    };
    let register = rule(vec![row(DIV, &[Register(0), Register(1)])], 1, 2);
    let sum = Synthesized {
        fields: smallvec![(1, byte())],
        ..rule(vec![row(ADD, &[Register(0), Immediate(1)])], 2, 2)
    };
    let mut s = Selecting::new(&[held, register, sum]);
    let (x, y) = (s.x, s.y);
    let (seven, wide) = (s.constant(32, 7), s.constant(32, 300));
    let near = s.row(13, &[x, seven]);
    let far = s.row(13, &[x, wide]);
    let moved = s.row(13, &[Ref::new(x.class, 5), y]);
    s.saturate();
    // The immediate in its field is the instruction's own.
    assert_eq!(s.selected(near), (0, vec![x, seven], 16));
    // One too wide for the field is built into the register the other rule
    // reads.
    assert_eq!(s.selected(far), (1, vec![x, wide], 17 + 3));
    // A register at an offset no field holds: the instruction over the
    // reference, and the add that builds it.
    assert_eq!(
        s.selected(moved),
        (1, vec![Ref::new(x.class, 5), y], 17 + 18)
    );
}

#[test]
fn a_row_folds_the_row_its_operand_was_stepped_as() {
    let under = || row(NOT, &[Register(0)]);
    let folded = vec![row(DIV, &[Operand::Row(1), Register(1)]), under()];
    let mut s = Selecting::new(&[
        rule(vec![under()], 0, 1),
        rule(folded, 1, 2),
        rule(vec![row(DIV, &[Register(0), Register(1)])], 9, 2),
    ]);
    let (x, y) = (s.x, s.y);
    let not = s.row(15, &[x]);
    let div = s.row(13, &[not, y]);
    s.saturate();
    let (alone, operands, _) = s.selected(not);
    assert_eq!((alone, operands), (0, vec![x]));
    // The reader's form is over the folded row's form, not over its value.
    let (reader, operands, price) = s.selected(div);
    assert_eq!((reader, operands.len(), price), (1, 2, 17));
    assert_eq!(operands[1], y);
    let inner = s.g.rows(operands[0].class).next().expect("the form");
    assert_eq!(s.g.children(inner).to_vec(), vec![x]);
    assert_ne!(s.g.find(operands[0]), s.g.find(not));
}

// The rules of a read, a product, a product of a register and a read
// folded, a negation and a write.
fn reading() -> Vec<Synthesized> {
    let write = row(STORE, &[Register(0), Register(1), Register(2)]);
    vec![
        rule(vec![load()], 0, 2),
        rule(folded_load(), 1, 3),
        rule(vec![row(MUL, &[Register(0), Register(1)])], 9, 2),
        rule(vec![row(NOT, &[Register(0)])], 4, 1),
        rule(vec![write], 5, 3),
    ]
}

// A read one reader folds, `mul(y, load(x))`, with what the use lists say of
// the read: how many read it, and the regions it is defined and read in.
fn folded_read(uses: u64, regions: &[u64]) -> u32 {
    let mut s = Selecting::new(&reading());
    let (x, y) = (s.x, s.y);
    let read = s.load(x, s.entered);
    let reader = s.row(11, &[y, read]);
    s.read(read);
    s.g.raise_fact(USES, read.class, uses);
    for &region in regions {
        s.g.raise_fact(REGION, read.class, region);
    }
    s.saturate();
    assert_eq!(s.selected(read).0, 0);
    s.selected(reader).0
}

#[test]
fn a_read_folds_into_its_one_reader_in_its_region() {
    assert_eq!(folded_read(1, &[7, 7]), 1);
}

#[test]
fn a_read_with_two_readers_or_one_in_another_region_is_not_folded() {
    assert_eq!(folded_read(2, &[7, 7, 7]), 9);
    assert_eq!(folded_read(1, &[7, 8]), 9);
}

// Two reads in sibling arms that are one class are each read once, and
// defined in two regions: the class holds no region, and nothing folds it.
#[test]
fn twin_reads_in_two_regions_are_not_folded() {
    assert_eq!(folded_read(1, &[7, 7, 8, 8]), 9);
}

#[test]
fn a_tied_pattern_steps_only_where_its_paths_hold_one_value() {
    let under = || row(NOT, &[Register(0)]);
    let squared = rule(vec![row(DIV, &[Register(0), Register(0)])], 0, 1);
    let plain = rule(vec![row(DIV, &[Register(0), Register(1)])], 1, 2);
    // One register read by the row and by the row it folds.
    let again = vec![row(MUL, &[Register(0), Operand::Row(1)]), under()];
    let apart = vec![
        row(MUL, &[Register(0), Operand::Row(1)]),
        row(NOT, &[Register(1)]),
    ];
    let sum = Synthesized {
        fields: smallvec![(1, byte())],
        ..rule(vec![row(ADD, &[Register(0), Immediate(1)])], 5, 2)
    };
    let mut s = Selecting::new(&[
        squared,
        plain,
        rule(vec![under()], 2, 1),
        rule(again, 3, 1),
        rule(apart, 4, 2),
        sum,
    ]);
    let (x, y) = (s.x, s.y);
    let same = s.row(13, &[x, x]);
    let other = s.row(13, &[x, y]);
    let shifted = s.row(13, &[x, Ref::new(x.class, 4)]);
    let not = s.row(15, &[x]);
    let read_twice = s.row(11, &[x, not]);
    let read_once = s.row(11, &[y, not]);
    s.saturate();
    assert_eq!(s.selected(same).0, 0);
    assert_eq!(s.selected(other).0, 1);
    // The register and the register four on are two values.
    assert_eq!(s.selected(shifted).0, 1);
    assert_eq!(s.selected(read_twice).0, 3);
    assert_eq!(s.selected(read_once).0, 4);
}

// `mul(not(load(y, state)), load(x, entered))`, the second read read once,
// where `state` is what `after` makes of the state the function is entered
// in: the instruction the product is selected as.
fn read_beside(after: impl FnOnce(&mut Selecting) -> Ref) -> u32 {
    let mut s = Selecting::new(&reading());
    let (x, y, entered) = (s.x, s.y, s.entered);
    let read = s.load(x, entered);
    s.read_once(read);
    let state = after(&mut s);
    let other = s.load(y, state);
    s.read(other);
    let negated = s.row(15, &[other]);
    s.read(negated);
    let reader = s.row(11, &[negated, read]);
    s.saturate();
    s.selected(reader).0
}

#[test]
fn a_read_is_not_folded_past_a_write_its_reader_waits_for() {
    // Both reads observe one state: the reader folds its own.
    assert_eq!(read_beside(|s| s.entered), 1);
    // The other operand reads the state a write leaves. Folded, the first
    // read would run after that write.
    assert_eq!(read_beside(|s| s.store(s.y, s.x, s.entered)), 9);
    // Nor is it folded beside a state nothing orders.
    assert_eq!(read_beside(Selecting::unordered), 9);
}

// `mul(other, load(x, entered))`, the read read once: the instruction the
// product is selected as.
fn read_with(other: impl FnOnce(&mut Selecting) -> Ref) -> u32 {
    let mut s = Selecting::new(&reading());
    let (x, entered) = (s.x, s.entered);
    let read = s.load(x, entered);
    s.read_once(read);
    let other = other(&mut s);
    let reader = s.row(11, &[other, read]);
    s.saturate();
    s.selected(reader).0
}

#[test]
fn a_read_is_not_folded_past_an_operation_that_changes_its_state() {
    // `m1 = call f(m0)`, and the other operand reads in `m1`.
    assert_eq!(read_beside(|s| s.result(true, &[s.y, s.entered])), 9);
    // The value the call returns follows what the call wrote as well.
    assert_eq!(read_with(|s| s.result(true, &[s.y, s.entered])), 9);
    // Two calls over one operand are two classes, each ordered.
    let (first, second) = {
        let mut s = Selecting::new(&reading());
        let read = [s.y, s.entered];
        (s.result(true, &read), s.result(true, &read))
    };
    assert_ne!(first, second);
    // An operation that changes no state leaves the read where it was.
    assert_eq!(read_beside(|s| s.result(false, &[s.y, s.entered])), 1);
    assert_eq!(read_with(|s| s.result(false, &[s.y, s.entered])), 1);
}

#[test]
fn a_read_is_not_folded_past_a_loop_or_a_gate_that_publishes_a_state() {
    // The state a loop is left in reads what it was entered on and what its
    // body yields, a write of the state at the head of an iteration.
    let stores = |s: &mut Selecting| {
        let head = s.result(false, &[s.entered]);
        let written = s.store(s.y, s.x, head);
        s.result(true, &[s.entered, written])
    };
    assert_eq!(read_beside(stores), 9);
    // A gate reads its predicate, the state forwarded to its arms and what
    // each arm yields. Its arms only read, yet the state it publishes and
    // the value it chooses follow the reads ahead of it as a write would.
    let gate = |s: &mut Selecting| s.result(true, &[s.x, s.entered, s.entered]);
    assert_eq!(read_beside(gate), 9);
    assert_eq!(read_with(gate), 9);
    // A gate that takes no state orders nothing.
    assert_eq!(read_with(|s| s.result(false, &[s.x, s.y])), 1);
}

// The state at the head of an iteration is as deep as what the loop is
// entered on, so a read of it folds beside a value computed before the loop
// and after a write there.
#[test]
fn a_read_in_a_loop_folds_beside_a_value_from_before_it() {
    let mut s = Selecting::new(&reading());
    let (x, y, entered) = (s.x, s.y, s.entered);
    let written = s.store(y, x, entered);
    let before = s.load(y, written);
    s.read(before);
    let outside = s.row(15, &[before]);
    s.read(outside);
    let head = s.result(false, &[written]);
    let read = s.load(x, head);
    s.read_once(read);
    let reader = s.row(11, &[outside, read]);
    s.saturate();
    assert_eq!(s.selected(reader).0, 1);
}

// A result is the operation it names, whatever the operation reads: an
// operand no instruction covers is not its price.
#[test]
fn a_result_costs_nothing_of_what_its_operation_reads() {
    let mut s = Selecting::new(&reading());
    let uncovered = s.row(13, &[s.x, s.y]);
    let result = s.result(true, &[uncovered, s.entered]);
    s.saturate();
    let (select, g) = (s.select(), &s.g);
    let extraction =
        g.extract_best(|class, node, operands, _| select.price(g, class, node, operands));
    assert_eq!(extraction.cost(g.root(uncovered.class)), Some(u64::MAX));
    assert_eq!(extraction.cost(g.root(result.class)), Some(0));
}

// An operation that reads more than one row holds keeps the rest in a row
// its results read last, and is ordered through it.
#[test]
fn a_result_is_ordered_through_the_operands_past_its_row() {
    let wide = |written: bool| {
        read_beside(|s| {
            let last = match written {
                true => s.store(s.y, s.x, s.entered),
                false => s.entered,
            };
            let rest = [s.y, last];
            let rest = s.g.insert(SemNode::operands(vec![s.y.class; 2]), &rest);
            let read: Vec<Ref> = std::iter::repeat_n(s.x, WIDEST_RESULT - 1)
                .chain([rest])
                .collect();
            s.result(false, &read)
        })
    };
    assert_eq!(wide(true), 9);
    assert_eq!(wide(false), 1);
}

#[test]
fn a_folded_row_is_read_at_an_offset_only_a_field_holds() {
    let displacement = ImmRange {
        width: 12,
        signed: true,
        align: 1,
        nonzero: false,
    };
    // The read of a negation at a displacement, and the sum that builds a
    // register at an offset.
    let displaced = vec![
        row(LOAD, &[Operand::Row(1), Register(2)]),
        row(ADD, &[Operand::Row(2), Immediate(1)]),
        row(NOT, &[Register(0)]),
    ];
    let displaced = Synthesized {
        fields: smallvec![(1, displacement)],
        ..rule(displaced, 2, 3)
    };
    let sum = Synthesized {
        fields: smallvec![(1, byte())],
        ..rule(vec![row(ADD, &[Register(0), Immediate(1)])], 3, 2)
    };
    let mut rules = reading();
    rules.extend([displaced, sum]);
    let mut s = Selecting::new(&rules);
    let (x, y, entered) = (s.x, s.y, s.entered);
    let read = s.load(x, entered);
    s.read_once(read);
    let moved = Ref::new(read.class, 5);
    let reader = s.row(11, &[y, moved]);
    let negated = s.row(15, &[x]);
    s.read(negated);
    let displaced = s.load(Ref::new(negated.class, 8), entered);
    s.saturate();
    // No register holds a folded read, so five is added to the one the
    // read alone leaves.
    let (product, operands, _) = s.selected(reader);
    assert_eq!((product, operands), (9, vec![y, moved]));
    // The displacement is the read's own field over the folded negation.
    let (read, operands, price) = s.selected(displaced);
    assert_eq!((read, operands[0].offset, price), (2, 8, 18));
    let folded = s.g.rows(operands[0].class).next().expect("the form");
    assert!(matches!(s.g.node(folded).kind, Kind::Form(_)));
}

#[test]
fn an_uncovered_operation_is_kept_only_where_synthesis_made_none_of_it() {
    let mut s = Selecting::new(&[rule(vec![row(NOT, &[Register(0)])], 0, 1)]);
    let (x, y) = (s.x, s.y);
    let instantiated = s.row(13, &[x, y]);
    let foreign = s.row(14, &[x, y]);
    s.saturate();
    let (select, g) = (s.select(), &s.g);
    let price = |value: Ref| {
        let row = g.rows(value.class).next().expect("the operation");
        select.price(g, value.class, g.node(row), &g.children(row))
    };
    assert_eq!((price(instantiated), price(foreign)), (u64::MAX, 0));
}

#[test]
fn a_vector_row_reads_its_lane_count_as_a_child() {
    let sum = rule(vec![row(LANEWISE, &[Register(0), Register(1)])], 6, 2);
    let mut s = Selecting::new(&[sum]);
    let (x, y) = (s.x, s.y);
    let lanes = [s.constant(64, 4), s.constant(64, 8)];
    let [four, eight] = lanes.map(|lanes| s.row(18, &[x, y, lanes]));
    s.saturate();
    // One pattern at any count, which the instruction is told: no register
    // is built for it.
    assert_eq!(s.selected(four), (6, vec![x, y, lanes[0]], 22));
    assert_eq!(s.selected(eight), (6, vec![x, y, lanes[1]], 22));
    let mut accepts = s.automaton.accept.iter().flatten();
    let accept = accepts.next().expect("the sum's tree");
    assert_eq!(accept.lanes.as_deref(), Some(&[2][..]));
}

// Two reads that are one class, each read once, are read twice: folded, the
// one access would run for each reader.
#[test]
fn reads_that_are_one_class_add_their_readers() {
    let mut s = Selecting::new(&reading());
    let (x, y) = (s.x, s.y);
    let read = s.load(x, s.entered);
    let reader = s.row(11, &[y, read]);
    s.read_once(read);
    s.g.raise_fact(USES, read.class, 1);
    s.saturate();
    assert_eq!(s.selected(reader).0, 9);
}

// `div(mul(y, load(x)), y)`, where one tree folds the product and the read
// in it: the instruction the quotient is selected as, given how many read
// the product.
fn folded_product(readers: u64) -> u32 {
    let nested = vec![
        row(DIV, &[Operand::Row(1), Register(3)]),
        row(MUL, &[Register(2), Operand::Row(2)]),
        load(),
    ];
    let mut rules = reading();
    rules.push(rule(nested, 7, 4));
    rules.push(rule(vec![row(DIV, &[Register(0), Register(1)])], 8, 2));
    let mut s = Selecting::new(&rules);
    let (x, y) = (s.x, s.y);
    let read = s.load(x, s.entered);
    s.read_once(read);
    let product = s.row(11, &[y, read]);
    s.read(product);
    s.g.raise_fact(USES, product.class, readers);
    s.g.raise_fact(REGION, product.class, 7);
    let quotient = s.row(13, &[product, y]);
    s.saturate();
    assert_eq!(s.selected(product).0, 1);
    s.selected(quotient).0
}

#[test]
fn a_row_holding_a_folded_read_folds_only_into_its_one_reader() {
    assert_eq!(folded_product(1), 7);
    // Read by another too, the product is computed once, with its read,
    // and the quotient reads it from a register.
    assert_eq!(folded_product(2), 8);
}
