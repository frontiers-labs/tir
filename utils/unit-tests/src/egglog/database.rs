use tir_egglog::{
    ContainerValue, Database, Error, Merge, Shape, Signature, SortId, SortKind, TableId, Value,
};

fn rows(db: &Database, table: TableId) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = db.rows(table).map(|row| row.values().collect()).collect();
    rows.sort();
    rows
}

fn node_db() -> (Database, SortId) {
    let mut db = Database::new();
    let node = db.add_sort("Node", SortKind::Eq);
    (db, node)
}

#[test]
fn relation_rows_are_inserted_looked_up_and_removed() {
    let (mut db, node) = node_db();
    let edge = db.add_relation("edge", &[node, node]);
    let [a, b, c] = [(); 3].map(|()| db.fresh(node));
    db.insert(edge, &[a, b]).unwrap();
    db.insert(edge, &[b, c]).unwrap();
    db.insert(edge, &[a, b]).unwrap();
    assert!(db.lookup(edge, &[a, b]).is_some());
    assert_eq!(db.lookup(edge, &[a, c]), None);
    assert_eq!(rows(&db, edge), [vec![a, b], vec![b, c]]);

    db.remove(edge, &[a, b]);
    assert_eq!(db.lookup(edge, &[a, b]), None);
    assert_eq!(rows(&db, edge), [vec![b, c]]);
}

#[test]
fn a_database_moves_to_another_thread_and_back() {
    let (mut db, node) = node_db();
    let edge = db.add_relation("edge", &[node, node]);
    let [a, b] = [(); 2].map(|()| db.fresh(node));
    let db = std::thread::spawn(move || {
        db.insert(edge, &[a, b]).unwrap();
        db
    })
    .join()
    .unwrap();
    assert_eq!(rows(&db, edge), [vec![a, b]]);
}

#[test]
fn names_arity_and_ids_are_checked() {
    let (mut db, node) = node_db();
    let edge = db.add_relation("edge", &[node, node]);
    assert_eq!(db.table("edge"), Ok(edge));
    assert_eq!(db.sort("Node"), Ok(node));
    assert_eq!(db.sort("Nope"), Err(Error::UnknownName("Nope".into())));
    let a = db.fresh(node);
    assert!(matches!(
        db.insert(edge, &[a]),
        Err(Error::Arity {
            expected: 2,
            found: 1,
            ..
        })
    ));
    let never_issued = Value::new(1000);
    assert!(matches!(
        db.insert(edge, &[a, never_issued]),
        Err(Error::Sort { column: 1, .. })
    ));
    assert!(matches!(
        db.set_shapes(edge, &[Shape::Dense]),
        Err(Error::Shape { .. })
    ));
}

#[test]
fn assert_eq_merge_rejects_a_second_output() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let f = db.add_function("f", &[int], int, Merge::AssertEq);
    let [one, two, three] = [1i64, 2, 3].map(|v| db.base(v));
    db.insert(f, &[one, two]).unwrap();
    db.insert(f, &[one, two]).unwrap();
    assert!(matches!(
        db.insert(f, &[one, three]),
        Err(Error::Conflict { .. })
    ));
    assert_eq!(db.lookup(f, &[one]), Some(two));
}

#[test]
fn union_merge_joins_the_outputs() {
    let (mut db, node) = node_db();
    let parent = db.add_function("parent", &[node], node, Merge::Union);
    let [a, b, c] = [(); 3].map(|()| db.fresh(node));
    db.insert(parent, &[a, b]).unwrap();
    db.insert(parent, &[a, c]).unwrap();
    db.rebuild().unwrap();
    assert_eq!(db.find(b), db.find(c));
    assert_eq!(rows(&db, parent), [vec![a, db.find(b)]]);
}

#[test]
fn primitive_merge_keeps_the_minimum() {
    let (mut db, node) = node_db();
    let int = db.sort("i64").unwrap();
    let signature = Signature {
        inputs: vec![int, int],
        output: int,
    };
    let min = db.add_primitive("min", signature, |ctx, args| {
        let (a, b) = (
            ctx.base_value::<i64>(args[0]),
            ctx.base_value::<i64>(args[1]),
        );
        Some(ctx.base(a.min(b)))
    });
    let cost = db.add_function(
        "cost",
        &[node],
        int,
        Merge::Prim(min, vec![Merge::Old, Merge::New]),
    );
    let x = db.fresh(node);
    for v in [5i64, 3, 7, -1 << 40, 4] {
        let v = db.base(v);
        db.insert(cost, &[x, v]).unwrap();
    }
    let best = db.lookup(cost, &[x]).unwrap();
    assert_eq!(db.base_value::<i64>(best), -1 << 40);
}

#[test]
fn function_merge_makes_a_missing_constructor_row() {
    let (mut db, node) = node_db();
    let pair = db.add_constructor("Pair", &[node, node], node);
    let both = db.add_function(
        "both",
        &[node],
        node,
        Merge::Function(pair, vec![Merge::Old, Merge::New]),
    );
    let [k, a, b] = [(); 3].map(|()| db.fresh(node));
    db.insert(both, &[k, a]).unwrap();
    db.insert(both, &[k, b]).unwrap();
    db.rebuild().unwrap();
    let made = db.lookup(pair, &[a, b]).expect("the merge made Pair(a, b)");
    assert_eq!(db.lookup(both, &[k]), Some(made));
}

#[test]
fn rebuild_restores_congruence() {
    let (mut db, node) = node_db();
    let add = db.add_constructor("Add", &[node, node], node);
    let neg = db.add_constructor("Neg", &[node], node);
    let [a, a2, b, c1, c2, d1, d2] = [(); 7].map(|()| db.fresh(node));
    db.insert(add, &[a, b, c1]).unwrap();
    db.insert(add, &[a2, b, c2]).unwrap();
    db.insert(neg, &[c1, d1]).unwrap();
    db.insert(neg, &[c2, d2]).unwrap();
    db.union(a, a2);
    db.rebuild().unwrap();
    assert_eq!(db.find(c1), db.find(c2));
    // Neg(c1) and Neg(c2) only meet once the Add rows have merged.
    assert_eq!(db.find(d1), db.find(d2));
    assert_eq!(rows(&db, add), [vec![db.find(a), b, db.find(c1)]]);
    assert_eq!(db.rows(neg).count(), 1);
}

#[test]
fn rows_stay_visible_through_many_overwrites_and_removals() {
    let mut db = Database::new();
    let int = db.sort("i64").unwrap();
    let latest = db.add_function("latest", &[int], int, Merge::New);
    let v = |db: &mut Database, x: i64| db.base(x);
    for round in 0..5 {
        for key in 0..100 {
            let (key, value) = (v(&mut db, key), v(&mut db, key * 10 + round));
            db.insert(latest, &[key, value]).unwrap();
        }
    }
    for key in (0..100).step_by(2) {
        let key = v(&mut db, key);
        db.remove(latest, &[key]);
    }
    let want: Vec<Vec<Value>> = (0..100)
        .filter(|key| key % 2 == 1)
        .map(|key| vec![v(&mut db, key), v(&mut db, key * 10 + 4)])
        .collect();
    assert_eq!(rows(&db, latest), want);
    for row in &want {
        assert_eq!(db.lookup(latest, &row[..1]), Some(row[1]));
    }
}

#[test]
fn a_subsumed_row_stays_for_lookup() {
    let (mut db, node) = node_db();
    let add = db.add_constructor("Add", &[node, node], node);
    // `a2` is the older id, so the union moves the row.
    let [a2, a, b, c] = [(); 4].map(|()| db.fresh(node));
    db.insert(add, &[a, b, c]).unwrap();
    db.subsume(add, &[a, b]);
    db.insert(add, &[a, b, c]).unwrap();
    db.union(a, a2);
    db.rebuild().unwrap();
    assert_eq!(db.lookup(add, &[a2, b]), Some(c));
    let subsumed: Vec<bool> = db.rows(add).map(|row| row.is_subsumed()).collect();
    assert_eq!(subsumed, [true]);
}

#[test]
fn clear_drops_every_row() {
    let (mut db, node) = node_db();
    let edge = db.add_relation("edge", &[node, node]);
    let [a, b] = [(); 2].map(|()| db.fresh(node));
    db.insert(edge, &[a, b]).unwrap();
    db.clear(edge);
    assert_eq!(db.rows(edge).count(), 0);
    assert_eq!(db.lookup(edge, &[a, b]), None);
    db.insert(edge, &[b, a]).unwrap();
    assert_eq!(rows(&db, edge), [vec![b, a]]);
}

// Everything a caller can observe of `edge`, `weight` and `Add` after the
// same writes, with every column given `shape`.
fn shaped_run(shape: Shape) -> Vec<Vec<Vec<Value>>> {
    let (mut db, node) = node_db();
    let int = db.sort("i64").unwrap();
    let edge = db.add_relation("edge", &[node, node]);
    let weight = db.add_function("weight", &[node], int, Merge::New);
    let add = db.add_constructor("Add", &[node, node], node);
    db.set_shapes(edge, &[shape; 2]).unwrap();
    db.set_shapes(weight, &[shape; 2]).unwrap();
    db.set_shapes(add, &[shape; 3]).unwrap();
    let by_source = db.add_index(edge, &[0]);
    let by_target = db.add_index(edge, &[1]);
    let by_args = db.add_index(add, &[0, 1]);
    let nodes: Vec<Value> = (0..40).map(|_| db.fresh(node)).collect();
    for i in 0..120usize {
        let (x, y) = (nodes[i * 7 % 40], nodes[i * 13 % 40]);
        db.insert(edge, &[x, y]).unwrap();
        let w = db.base(i as i64);
        db.insert(weight, &[x, w]).unwrap();
        db.insert(add, &[x, y, nodes[(i * 3 + 1) % 40]]).unwrap();
    }
    for i in 0..10 {
        db.union(nodes[i], nodes[i + 20]);
    }
    db.rebuild().unwrap();
    let canonical = |db: &Database, rows: Vec<Vec<Value>>| {
        let mut rows: Vec<Vec<Value>> = rows
            .into_iter()
            .map(|row| row.into_iter().map(|v| db.find(v)).collect())
            .collect();
        rows.sort();
        rows
    };
    let mut seen = Vec::new();
    for table in [edge, weight, add] {
        seen.push(canonical(&db, rows(&db, table)));
    }
    for &x in &nodes {
        let lookups = vec![db.lookup(weight, &[x]).into_iter().collect()];
        seen.push(canonical(&db, lookups));
        for (index, key) in [
            (by_source, vec![x]),
            (by_target, vec![x]),
            (by_args, vec![x, nodes[3]]),
        ] {
            let found = db
                .matching(index, &key)
                .map(|row| row.values().collect())
                .collect();
            seen.push(canonical(&db, found));
        }
    }
    seen
}

#[test]
fn dense_and_sparse_shapes_give_the_same_results() {
    let dense = shaped_run(Shape::Dense);
    assert!(
        dense.iter().skip(3).any(|found| !found.is_empty()),
        "the lookups find rows"
    );
    assert_eq!(dense, shaped_run(Shape::Sparse));
}

// A container that holds two e-class ids.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Pair(Value, Value);

impl ContainerValue for Pair {
    fn rebuild_contents(&mut self, find: &dyn Fn(Value) -> Value) -> bool {
        let rebuilt = Pair(find(self.0), find(self.1));
        let changed = rebuilt != *self;
        *self = rebuilt;
        changed
    }

    fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        [self.0, self.1].into_iter()
    }
}

#[test]
fn containers_become_equal_after_a_union() {
    let (mut db, node) = node_db();
    let pairs = db.add_sort("Pair", SortKind::container::<Pair>());
    let holds = db.add_relation("holds", &[pairs]);
    let [a, a2, b] = [(); 3].map(|()| db.fresh(node));
    let first = db.container(Pair(a, b));
    let second = db.container(Pair(a2, b));
    assert_eq!(db.container(Pair(a, b)), first);
    assert_ne!(first, second);
    db.insert(holds, &[first]).unwrap();
    db.insert(holds, &[second]).unwrap();
    db.union(a, a2);
    db.rebuild().unwrap();
    assert_eq!(db.find(first), db.find(second));
    assert_eq!(
        db.container_value::<Pair>(db.find(first)),
        Some(&Pair(db.find(a), b))
    );
    assert_eq!(rows(&db, holds), [vec![db.find(first)]]);
}

#[test]
fn a_container_rebuilt_in_place_makes_its_rows_new() {
    let (mut db, node) = node_db();
    let pairs = db.add_sort("Pair", SortKind::container::<Pair>());
    let holds = db.add_relation("holds", &[pairs]);
    let [a, a2, b] = [(); 3].map(|()| db.fresh(node));
    let pair = db.container(Pair(a2, b));
    db.insert(holds, &[pair]).unwrap();
    db.rebuild().unwrap();
    let before = db.timestamp();
    assert_eq!(db.rows_since(holds, before).count(), 0);
    // No Pair(a, b) exists, so the container keeps its id but its contents
    // change: rules must see its row again.
    db.union(a, a2);
    db.rebuild().unwrap();
    assert_eq!(db.find(pair), pair);
    assert_eq!(db.container_value::<Pair>(pair), Some(&Pair(a, b)));
    let new: Vec<Value> = db.rows_since(holds, before).map(|row| row[0]).collect();
    assert_eq!(new, [pair]);
}

// Lookups through an index between edits see exactly the live rows a scan
// sees, also once the rows written since the index was built outnumber it.
#[test]
fn index_lookups_between_edits_match_a_scan() {
    let (mut db, node) = node_db();
    let slot = db.sort("i64").unwrap();
    let operands = db.add_function("operands", &[node, slot], node, Merge::New);
    let uses = db.add_index(operands, &[2]);
    let values: Vec<Value> = (0..8).map(|_| db.fresh(node)).collect();
    let mut keys: Vec<(Value, Value)> = Vec::new();
    let mut state = 7u64;
    let mut next = |len: usize| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 33) as usize % len
    };
    for step in 0..2_000 {
        match next(4) {
            0 | 1 => {
                let op = db.fresh(node);
                let at = db.base_values().get((step % 3) as i64);
                db.insert(operands, &[op, at, values[next(values.len())]])
                    .unwrap();
                keys.push((op, at));
            }
            2 if !keys.is_empty() => {
                let (op, at) = keys.swap_remove(next(keys.len()));
                db.remove(operands, &[op, at]);
            }
            _ if !keys.is_empty() => {
                let (op, at) = keys[next(keys.len())];
                db.insert(operands, &[op, at, values[next(values.len())]])
                    .unwrap();
            }
            _ => {}
        }
        let value = values[next(values.len())];
        let mut found: Vec<Vec<Value>> = (db.matching(uses, &[value]))
            .map(|row| row.values().collect())
            .collect();
        found.sort();
        let scanned: Vec<Vec<Value>> = rows(&db, operands)
            .into_iter()
            .filter(|row| row[2] == value)
            .collect();
        assert_eq!(found, scanned, "step {step}");
    }
}

// An ordered index gives the rows of a key in the order of its other
// columns, rows written after a lookup included.
#[test]
fn an_ordered_index_lists_a_group_by_position() {
    let (mut db, node) = node_db();
    let pos = db.sort("i64").unwrap();
    let held = db.add_relation("held", &[node, pos, node]);
    let in_order = db.add_ordered_index(held, &[0], &[1]);
    let block = db.fresh(node);
    let positions = [512i64, 0, 256, 768];
    let mut ops = Vec::new();
    for &p in &positions[..2] {
        let op = db.fresh(node);
        db.insert(held, &[block, db.base_values().get(p), op])
            .unwrap();
        ops.push((p, op));
    }
    let listed = |db: &mut Database| -> Vec<Value> {
        db.matching(in_order, &[block]).map(|row| row[2]).collect()
    };
    ops.sort();
    assert_eq!(
        listed(&mut db),
        ops.iter().map(|&(_, op)| op).collect::<Vec<_>>()
    );
    for &p in &positions[2..] {
        let op = db.fresh(node);
        db.insert(held, &[block, db.base_values().get(p), op])
            .unwrap();
        ops.push((p, op));
    }
    ops.sort();
    assert_eq!(
        listed(&mut db),
        ops.iter().map(|&(_, op)| op).collect::<Vec<_>>()
    );
}

// A union in a large table rewrites exactly the rows of the merged ids: after
// each rebuild the rows are the model's rows with every id at its root.
#[test]
fn a_rebuild_after_few_unions_canonicalizes_every_row() {
    let (mut db, node) = node_db();
    let edge = db.add_relation("edge", &[node, node]);
    // Indexes on both id columns let the rebuild visit only the rows of the
    // merged ids.
    db.add_index(edge, &[0]);
    db.add_index(edge, &[1]);
    let nodes: Vec<Value> = (0..1_000).map(|_| db.fresh(node)).collect();
    let mut state = 11u64;
    let mut next = |len: usize| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 33) as usize % len
    };
    let mut model: Vec<(usize, usize)> = Vec::new();
    for _ in 0..5_000 {
        let (a, b) = (next(nodes.len()), next(nodes.len()));
        db.insert(edge, &[nodes[a], nodes[b]]).unwrap();
        model.push((a, b));
    }
    // The root of each node in the model: the least id of its class.
    let mut root: Vec<usize> = (0..nodes.len()).collect();
    let find = |root: &[usize], mut x: usize| {
        while root[x] != x {
            x = root[x];
        }
        x
    };
    for step in 0..200 {
        let (a, b) = (next(nodes.len()), next(nodes.len()));
        db.union(nodes[a], nodes[b]);
        let (a, b) = (find(&root, a), find(&root, b));
        root[a.max(b)] = a.min(b);
        db.rebuild().unwrap();
        let mut expected: Vec<Vec<Value>> = (model.iter())
            .map(|&(a, b)| vec![nodes[find(&root, a)], nodes[find(&root, b)]])
            .collect();
        expected.sort();
        expected.dedup();
        assert_eq!(rows(&db, edge), expected, "step {step}");
    }
}

// A kept index answers without write access between edits, after the table
// compacts its dead rows, and in a clone of the database.
#[test]
fn a_kept_index_answers_reads_between_edits_and_in_a_clone() {
    let (mut db, node) = node_db();
    let slot = db.sort("i64").unwrap();
    let operands = db.add_function("operands", &[node, slot], node, Merge::New);
    let uses = db.add_kept_index(operands, &[2], &[]);
    let values: Vec<Value> = (0..4).map(|_| db.fresh(node)).collect();
    let mut keys = Vec::new();
    let check = |db: &Database, step: usize| {
        for &value in &values {
            let mut found: Vec<Vec<Value>> = (db.listed(uses, &[value]))
                .map(|row| row.values().collect())
                .collect();
            found.sort();
            let scanned: Vec<Vec<Value>> = rows(db, operands)
                .into_iter()
                .filter(|row| row[2] == value)
                .collect();
            assert_eq!(found, scanned, "step {step}");
        }
    };
    for step in 0..600 {
        let op = db.fresh(node);
        let at = db.base_values().get(0i64);
        db.insert(operands, &[op, at, values[step % values.len()]])
            .unwrap();
        keys.push((op, at));
        // Remove most rows, so the dead ones outnumber the live and the
        // table renumbers its rows.
        if step % 3 != 0 {
            let (op, at) = keys.remove(0);
            db.remove(operands, &[op, at]);
        }
        check(&db, step);
    }
    let copy = db.clone();
    check(&copy, 600);
}

// A kept index by one column lists the rows of a key in position order while
// rows come and go, also once the table compacts its dead rows.
#[test]
fn a_kept_ordered_index_lists_a_group_by_position_through_edits() {
    let (mut db, node) = node_db();
    let pos = db.sort("i64").unwrap();
    let held = db.add_relation("held", &[node, pos, node]);
    let in_order = db.add_kept_index(held, &[0], &[1]);
    let block = db.fresh(node);
    let mut model: Vec<(i64, Value)> = Vec::new();
    let mut state = 5u64;
    let mut next = |len: usize| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (state >> 33) as usize % len
    };
    for step in 0..800 {
        if model.is_empty() || next(3) != 0 {
            let p = (next(1 << 20) * 2) as i64;
            if model.iter().any(|&(held, _)| held == p) {
                continue;
            }
            let op = db.fresh(node);
            db.insert(held, &[block, db.base_values().get(p), op])
                .unwrap();
            model.push((p, op));
        } else {
            let (p, op) = model.swap_remove(next(model.len()));
            db.remove(held, &[block, db.base_values().get(p), op]);
        }
        model.sort();
        let listed: Vec<Value> = db.listed(in_order, &[block]).map(|row| row[2]).collect();
        let expected: Vec<Value> = model.iter().map(|&(_, op)| op).collect();
        assert_eq!(listed, expected, "step {step}");
    }
}
