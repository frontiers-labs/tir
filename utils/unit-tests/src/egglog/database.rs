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
