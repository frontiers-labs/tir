//! The two passes of the plan on rows a client would write for real IR
//! files: a rule set compiles offline against the schema, and the pass loads
//! the bytes into the database that holds the IR.

use std::collections::BTreeSet;

use tir_egglog::prelude::*;
use tir_egglog::{
    Database, Extractor, Merge, Program, Rules, Schedule, Signature, SortId, SortKind, TableId,
    Value,
};

// The IR relations of the plan's Context sketch.
struct Ir {
    db: Database,
    op: SortId,
    val: SortId,
    opcode: TableId,
    operand: TableId,
    result: TableId,
    param: TableId,
    attr: TableId,
    effect: TableId,
}

fn ir() -> Ir {
    let mut db = Database::new();
    let op = db.add_sort("Op", SortKind::Eq);
    let val = db.add_sort("Val", SortKind::Eq);
    let (int, string) = (db.sort("i64").unwrap(), db.sort("String").unwrap());
    let opcode = db.add_function("opcode", &[op], string, Merge::AssertEq);
    let operand = db.add_function("operand", &[op, int], val, Merge::Union);
    let result = db.add_function("result", &[op, int], val, Merge::Union);
    let param = db.add_function("param", &[op, int], val, Merge::Union);
    let attr = db.add_function("attr", &[op, string], int, Merge::AssertEq);
    let effect = db.add_relation("effect", &[op]);
    Ir {
        db,
        op,
        val,
        opcode,
        operand,
        result,
        param,
        attr,
        effect,
    }
}

impl Ir {
    // The rows of one operation: its opcode and operands.
    fn op(&mut self, opcode: &str, operands: &[Value]) -> Value {
        let op = self.db.fresh(self.op);
        let name = self.db.base(opcode.to_owned());
        self.db.insert(self.opcode, &[op, name]).unwrap();
        for (i, &value) in operands.iter().enumerate() {
            let i = self.db.base(i as i64);
            self.db.insert(self.operand, &[op, i, value]).unwrap();
        }
        op
    }

    // The first result of `op`.
    fn result(&mut self, op: Value) -> Value {
        let (zero, value) = (self.db.base(0i64), self.db.fresh(self.val));
        self.db.insert(self.result, &[op, zero, value]).unwrap();
        value
    }

    fn constant(&mut self, value: i64) -> Value {
        let op = self.op("constant", &[]);
        let (key, value) = (self.db.base("value".to_owned()), self.db.base(value));
        self.db.insert(self.attr, &[op, key, value]).unwrap();
        self.result(op)
    }
}

#[test]
fn dce_keeps_exactly_the_effects_and_what_they_read() {
    // Offline: the rules against the schema, with the pass table `live`.
    let mut offline = ir();
    let op = offline.op;
    offline.db.add_relation("live", &[op]);
    let mut rules = Rules::new();
    let db = &offline.db;
    rules
        .rule(db, "dce", facts![(effect o)], actions![(live o)])
        .unwrap();
    rules
        .rule(
            db,
            "dce",
            facts![(live user) (= v (operand user i)) (= v (result def j))],
            actions![(live def)],
        )
        .unwrap();
    let bytes = rules.compile(db).unwrap().to_bytes();

    // core/checks/DCE/dead-chain.tir, function @f.
    let mut ir = ir();
    let (fn_g, x) = (ir.db.fresh(ir.val), ir.db.fresh(ir.val));
    let three = ir.constant(3);
    let o3 = ir.op("addi", &[x, three]);
    ir.result(o3);
    let o4 = ir.op("func.call", &[fn_g, x]);
    ir.result(o4);
    let o5 = ir.op("func.return", &[x]);
    ir.db.insert(ir.effect, &[o4]).unwrap();
    ir.db.insert(ir.effect, &[o5]).unwrap();

    let mut dce = Program::from_bytes(&bytes, &mut ir.db).unwrap();
    dce.run(&mut ir.db, &Schedule::saturate("dce")).unwrap();
    let live = ir.db.table("live").unwrap();
    let live: BTreeSet<Value> = ir.db.rows(live).map(|row| row[0]).collect();
    assert_eq!(live, BTreeSet::from([o4, o5]));
    assert!(ir.db.lookup(ir.db.table("live").unwrap(), &[o3]).is_none());
}

fn plus(db: &mut Database) {
    let int = db.sort("i64").unwrap();
    let signature = Signature {
        inputs: vec![int, int],
        output: int,
    };
    db.add_primitive("+", signature, |ctx, args| {
        let (a, b) = (
            ctx.base_value::<i64>(args[0]),
            ctx.base_value::<i64>(args[1]),
        );
        a.checked_add(b).map(|sum| ctx.base(sum))
    });
}

fn instcombine_tables(ir: &mut Ir) {
    let (op, val) = (ir.op, ir.val);
    let int = ir.db.sort("i64").unwrap();
    ir.db.add_constructor("Add", &[val, val], val);
    ir.db.add_constructor("Const", &[int], val);
    ir.db.add_constructor("Param", &[op, int], val);
}

#[test]
fn instcombine_folds_the_offsets_of_two_adds() {
    let mut offline = ir();
    plus(&mut offline.db);
    instcombine_tables(&mut offline);
    let db = &offline.db;
    let mut rules = Rules::new();
    let (addi, constant, value) = (
        exprs::string("addi"),
        exprs::string("constant"),
        exprs::string("value"),
    );
    rules
        .rule(
            db,
            "seed",
            facts![(= (unquote addi) (opcode o)) (= a (operand o 0)) (= b (operand o 1)) (= r (result o 0))],
            actions![(union r (Add a b))],
        )
        .unwrap();
    rules
        .rule(
            db,
            "seed",
            facts![(= (unquote constant) (opcode o)) (= c (attr o (unquote value))) (= r (result o 0))],
            actions![(union r (Const c))],
        )
        .unwrap();
    rules
        .rule(
            db,
            "seed",
            facts![(= v (param f i))],
            actions![(union v (Param f i))],
        )
        .unwrap();
    rules
        .rewrite(
            db,
            "fold",
            expr!((Add (Add x (Const c1)) (Add y (Const c2)))),
            expr!((Add (Add x y) (Const (+ c1 c2)))),
        )
        .unwrap();
    let bytes = rules.compile(db).unwrap().to_bytes();

    // core/checks/InstCombine/add_offsets_fold.tir, function @f.
    let mut ir = ir();
    plus(&mut ir.db);
    let f = ir.db.fresh(ir.op);
    let params: Vec<Value> = (0..2)
        .map(|i| {
            let (i, value) = (ir.db.base(i as i64), ir.db.fresh(ir.val));
            ir.db.insert(ir.param, &[f, i, value]).unwrap();
            value
        })
        .collect();
    let (c3, c5) = (ir.constant(3), ir.constant(5));
    let add = |ir: &mut Ir, x, y| {
        let op = ir.op("addi", &[x, y]);
        ir.result(op)
    };
    let (a, b) = (add(&mut ir, params[0], c3), add(&mut ir, params[1], c5));
    let r = add(&mut ir, a, b);
    let ret = ir.op("func.return", &[r]);
    ir.db.insert(ir.effect, &[ret]).unwrap();

    let mut ic = Program::from_bytes(&bytes, &mut ir.db).unwrap();
    let schedule = Schedule::seq([Schedule::run("seed"), Schedule::saturate("fold")]);
    ic.run(&mut ir.db, &schedule).unwrap();

    let db = &mut ir.db;
    let table = |db: &Database, name: &str| db.table(name).unwrap();
    let (zero, one, eight) = (db.base(0i64), db.base(1i64), db.base(8i64));
    let x = db.entry(table(db, "Param"), &[f, zero]).unwrap();
    let y = db.entry(table(db, "Param"), &[f, one]).unwrap();
    let sum = db.entry(table(db, "Add"), &[x, y]).unwrap();
    let offset = db.entry(table(db, "Const"), &[eight]).unwrap();
    let folded = db.entry(table(db, "Add"), &[sum, offset]).unwrap();
    let returned = db.lookup(ir.operand, &[ret, zero]).unwrap();
    assert_eq!(db.find(returned), db.find(folded));

    // Each row and each value costs 1, so the folded sum (10) beats the
    // seeded one (13). The Op `f` has no constructor: a leaf `Op#id`.
    let (termdag, best) = Extractor::new(db).best(returned).unwrap();
    let f = format!("Op#{}", f.raw());
    let expected = format!("(Add (Add (Param {f} 0) (Param {f} 1)) (Const 8))");
    assert_eq!(termdag.to_string(best), expected);
}
