//! The workloads the three e-graph benchmarks share (`egraph` = TIR, `egg`,
//! `egglog`). Each binary `#[path]`-includes this file and builds its own
//! engine's graph and rules from the same data, so they saturate the same seed
//! graph with the same rules for the same number of iterations. Keep this file
//! free of engine types.
//!
//! The data under `workloads/` is what `fcc -O2` hands its saturation passes
//! while compiling CoreMark, Dhrystone and Whetstone, written by
//! `TIR_SAT_DUMP` (see `core/src/sem/workload.rs` for the format and for what
//! the dump erases).
#![allow(dead_code)]

pub struct Workload {
    pub name: &'static str,
    /// Iterations every engine runs. Chosen per workload so the graph grows by
    /// orders of magnitude without reaching a node limit, which the engines
    /// would hit at different points.
    pub iters: usize,
    /// Operator names; a node and a pattern name one by index.
    pub symbols: Vec<String>,
    pub seeds: Vec<Seed>,
    /// Classes of the seed graph, numbered densely.
    pub classes: usize,
    pub rules: Vec<RuleSpec>,
}

/// One step of building the seed graph, in an order where every child class
/// already holds a node.
pub struct Seed {
    pub class: u32,
    pub symbol: u32,
    pub children: Vec<u32>,
}

pub struct RuleSpec {
    pub name: String,
    pub lhs: Pattern,
    pub rhs: Pattern,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    Var(u32),
    Node(u32, Vec<Pattern>),
}

impl Pattern {
    pub fn vars(&self) -> u32 {
        match self {
            Pattern::Var(var) => var + 1,
            Pattern::Node(_, children) => children.iter().map(Pattern::vars).max().unwrap_or(0),
        }
    }

    /// The pattern as an s-expression over `symbols`, holes spelled `?vN`.
    pub fn sexp(&self, symbols: &[String], rename: &dyn Fn(&str) -> String) -> String {
        match self {
            Pattern::Var(var) => format!("?v{var}"),
            Pattern::Node(symbol, children) if children.is_empty() => {
                rename(&symbols[*symbol as usize])
            }
            Pattern::Node(symbol, children) => {
                let mut out = format!("({}", rename(&symbols[*symbol as usize]));
                for child in children {
                    out.push(' ');
                    out.push_str(&child.sexp(symbols, rename));
                }
                out.push(')');
                out
            }
        }
    }
}

const SOURCES: &[(&str, usize, &str)] = &[
    (
        "coremark-instcombine-1377",
        3,
        include_str!("workloads/coremark-instcombine-1377.wl"),
    ),
    (
        "coremark-instcombine-206",
        3,
        include_str!("workloads/coremark-instcombine-206.wl"),
    ),
    (
        "coremark-isel-2665",
        4,
        include_str!("workloads/coremark-isel-2665.wl"),
    ),
    (
        "coremark-isel-534",
        4,
        include_str!("workloads/coremark-isel-534.wl"),
    ),
    (
        "dhrystone-isel-2033",
        4,
        include_str!("workloads/dhrystone-isel-2033.wl"),
    ),
    (
        "whetstone-instcombine-1450",
        3,
        include_str!("workloads/whetstone-instcombine-1450.wl"),
    ),
    (
        "whetstone-isel-1597",
        4,
        include_str!("workloads/whetstone-isel-1597.wl"),
    ),
];

pub fn workloads() -> Vec<Workload> {
    SOURCES
        .iter()
        .map(|&(name, iters, text)| parse(name, iters, text))
        .collect()
}

#[derive(Default)]
struct Interner {
    symbols: Vec<String>,
    index: std::collections::HashMap<String, u32>,
}

impl Interner {
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.index.get(name) {
            return id;
        }
        let id = self.symbols.len() as u32;
        self.symbols.push(name.to_string());
        self.index.insert(name.to_string(), id);
        id
    }
}

fn parse_pattern(tokens: &[&str], pos: &mut usize, names: &mut Interner) -> Pattern {
    let token = tokens[*pos];
    *pos += 1;
    if let Some(var) = token.strip_prefix("?v") {
        return Pattern::Var(var.parse().expect("hole number"));
    }
    if token != "(" {
        return Pattern::Node(names.intern(token), Vec::new());
    }
    let symbol = names.intern(tokens[*pos]);
    *pos += 1;
    let mut children = Vec::new();
    while tokens[*pos] != ")" {
        children.push(parse_pattern(tokens, pos, names));
    }
    *pos += 1;
    Pattern::Node(symbol, children)
}

/// Renumber holes densely in first-use order, so a rule's variable count is the
/// number of holes it has.
fn renumber(pattern: &mut Pattern, map: &mut Vec<u32>) {
    match pattern {
        Pattern::Var(var) => {
            let at = map.iter().position(|seen| seen == var).unwrap_or_else(|| {
                map.push(*var);
                map.len() - 1
            });
            *var = at as u32;
        }
        Pattern::Node(_, children) => children.iter_mut().for_each(|c| renumber(c, map)),
    }
}

fn parse(name: &'static str, iters: usize, text: &str) -> Workload {
    let mut names = Interner::default();
    let mut nodes: Vec<(u32, u32, Vec<u32>)> = Vec::new();
    let mut classes: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut rules = Vec::new();
    let dense = |raw: &str, classes: &mut std::collections::HashMap<u32, u32>| {
        let next = classes.len() as u32;
        *classes
            .entry(raw.parse().expect("class id"))
            .or_insert(next)
    };
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("n") => {
                let class = dense(words.next().expect("class"), &mut classes);
                let symbol = names.intern(words.next().expect("symbol"));
                let children = words.map(|word| dense(word, &mut classes)).collect();
                nodes.push((class, symbol, children));
            }
            Some("c") => {
                let symbol = names.intern(words.next().expect("symbol"));
                let node = |a, b| Pattern::Node(symbol, vec![Pattern::Var(a), Pattern::Var(b)]);
                rules.push(RuleSpec {
                    name: format!("comm-{}", names.symbols[symbol as usize]),
                    lhs: node(0, 1),
                    rhs: node(1, 0),
                });
            }
            Some("r") => {
                let name = words.next().expect("rule name").to_string();
                let rest = words.collect::<Vec<_>>().join(" ");
                let spaced = rest.replace('(', " ( ").replace(')', " ) ");
                let (lhs, rhs) = spaced.split_once("=>").expect("rule arrow");
                let side = |text: &str, names: &mut Interner| {
                    let tokens: Vec<&str> = text.split_whitespace().collect();
                    parse_pattern(&tokens, &mut 0, names)
                };
                let (mut lhs, mut rhs) = (side(lhs, &mut names), side(rhs, &mut names));
                let mut map = Vec::new();
                renumber(&mut lhs, &mut map);
                renumber(&mut rhs, &mut map);
                rules.push(RuleSpec { name, lhs, rhs });
            }
            _ => {}
        }
    }
    let seeds = order(nodes, classes.len(), &mut names);
    Workload {
        name,
        iters,
        symbols: names.symbols,
        seeds,
        classes: classes.len(),
        rules,
    }
}

/// Order the nodes so each one's children are classes an earlier node defined.
/// A class on a cycle gets an anchor leaf of its own to break it.
fn order(nodes: Vec<(u32, u32, Vec<u32>)>, classes: usize, names: &mut Interner) -> Vec<Seed> {
    let mut defined = vec![false; classes];
    let mut placed = vec![false; nodes.len()];
    let mut seeds = Vec::with_capacity(nodes.len());
    loop {
        let before = seeds.len();
        for (index, (class, symbol, children)) in nodes.iter().enumerate() {
            if !placed[index] && children.iter().all(|&child| defined[child as usize]) {
                placed[index] = true;
                seeds.push(Seed {
                    class: *class,
                    symbol: *symbol,
                    children: children.clone(),
                });
            }
        }
        // A pass runs to the end before a class it defined unblocks anything,
        // so definitions land only here.
        for seed in &seeds[before..] {
            defined[seed.class as usize] = true;
        }
        if placed.iter().all(|&done| done) {
            break;
        }
        if seeds.len() == before {
            let blocked = nodes
                .iter()
                .enumerate()
                .filter(|&(index, _)| !placed[index])
                .flat_map(|(_, (_, _, children))| children)
                .find(|&&child| !defined[child as usize])
                .copied()
                .expect("an unplaced node waits on an undefined class");
            defined[blocked as usize] = true;
            seeds.push(Seed {
                class: blocked,
                symbol: names.intern(&format!("anchor:{blocked}")),
                children: Vec::new(),
            });
        }
    }
    seeds
}
