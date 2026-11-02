// The program file: little-endian u32 words. After the magic and the
// version comes a string table, then the sorts, tables, primitives, indexes
// and rulesets, each a count and its items, then the trie nodes: the word
// each starts at, then the nodes. Names are positions in the string table.
// A load validates the borrowed code before the interpreter reads it in place.

use std::collections::HashMap;
use std::fmt;

use tir_adt::FxBuildHasher;

use super::code::{Action, ActionRef, Bind, Binds, Node, NodeRef, Reg, Slice, Step, StepRef};
use crate::ast::Literal;
use crate::{Database, ExternId, F64, Merge, Shape, SortId, SortKind, TableId, TableKind, Value};

const MAGIC: [u8; 4] = *b"TEGG";
const VERSION: u32 = 9;
// Merges nest at most this deep, so decoding one cannot exhaust the stack.
const MERGE_DEPTH: u32 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SortClass {
    Eq,
    Base,
    Container,
}

#[derive(Clone)]
pub(crate) struct SortDecl {
    pub(crate) name: String,
    pub(crate) class: SortClass,
}

// A constant as a literal of its sort, so a load interns it into the
// database it binds to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Const {
    Lit(Literal),
    BigInt(String),
    BigRat(String, String),
    // The values of a container, in the order its sort lists them.
    Elems(Vec<Const>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConstDecl {
    pub(crate) sort: u32,
    pub(crate) value: Const,
}

// A merge names tables, primitives and constants by their position in the
// file; so does the default.
#[derive(Clone)]
pub(crate) struct TableDecl {
    pub(crate) name: String,
    pub(crate) kind: TableKind,
    pub(crate) columns: Vec<u32>,
    pub(crate) keys: usize,
    pub(crate) merge: Option<Merge>,
    pub(crate) shapes: Vec<Shape>,
    pub(crate) symmetries: Vec<Vec<usize>>,
    pub(crate) cost: Option<u64>,
    pub(crate) unextractable: bool,
    pub(crate) default: Option<u32>,
}

#[derive(Clone)]
pub(crate) struct ExternDecl {
    pub(crate) name: String,
    pub(crate) inputs: Vec<u32>,
    pub(crate) output: u32,
    pub(crate) required: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexDecl {
    pub(crate) table: u32,
    pub(crate) order: Vec<usize>,
}

// The constants are registers 0, 1, ... at the roots. A combined ruleset
// has no code; it runs the rulesets it names in one iteration.
#[derive(Clone)]
pub(crate) struct RulesetDecl {
    pub(crate) name: String,
    pub(crate) roots: Vec<u32>,
    // The roots of the run that finds every row new: one variant per rule.
    pub(crate) first: Vec<u32>,
    // The indexes the code of the ruleset reads, which a search refreshes.
    pub(crate) used: Vec<u32>,
    // The name of each rule: a load leaves them in the string table, see
    // `Code::rules`.
    pub(crate) rules: Vec<String>,
    pub(crate) consts: Vec<Literal>,
    // Constants a primitive computes from those before, each the
    // register after the literals and the calls before it.
    pub(crate) calls: Vec<(u32, Vec<u32>)>,
    pub(crate) members: Vec<String>,
    // Whether the rules match on the first run only: they read no table.
    pub(crate) once: bool,
    // Whether the rules match in the colors only.
    pub(crate) colored: bool,
}

#[derive(Clone)]
pub(crate) struct ProgramFile {
    pub(crate) sorts: Vec<SortDecl>,
    pub(crate) consts: Vec<ConstDecl>,
    pub(crate) tables: Vec<TableDecl>,
    pub(crate) externs: Vec<ExternDecl>,
    pub(crate) indexes: Vec<IndexDecl>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) rulesets: Vec<RulesetDecl>,
}

/// Why [`Program::from_bytes`](crate::Program::from_bytes) failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The file is of another version of the format.
    Version { found: u32, expected: u32 },
    /// The bytes are no program file, or one cut short or corrupted.
    Malformed(&'static str),
    /// The database has the sort as another kind, or lacks a sort the
    /// program cannot declare.
    SortMismatch { name: String },
    /// The database has the table with other columns, keys or merge.
    TableMismatch { name: String },
    /// The database has no primitive of this name and signature.
    MissingExtern { name: String, signature: String },
    /// The bound primitive does not admit a context required by the code.
    ExternContext {
        name: String,
        context: crate::Context,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version { found, expected } => {
                write!(f, "program file version {found}, expected {expected}")
            }
            Self::Malformed(what) => write!(f, "malformed program file: {what}"),
            Self::SortMismatch { name } => write!(f, "sort `{name}` does not match"),
            Self::TableMismatch { name } => write!(f, "table `{name}` does not match"),
            Self::MissingExtern { name, signature } => {
                write!(f, "no primitive `{name}` with signature {signature}")
            }
            Self::ExternContext { name, context } => {
                write!(f, "primitive `{name}` requires a {context:?} context")
            }
        }
    }
}

impl std::error::Error for LoadError {}

// Maps the tables, primitives and constants a merge names, in order.
pub(crate) fn map_merge(
    merge: &Merge,
    table: &mut impl FnMut(TableId) -> TableId,
    prim: &mut impl FnMut(ExternId) -> ExternId,
    konst: &mut impl FnMut(Value) -> Value,
) -> Merge {
    let args = |args: &[Merge], table: &mut _, prim: &mut _, konst: &mut _| -> Vec<Merge> {
        args.iter()
            .map(|arg| map_merge(arg, table, prim, konst))
            .collect()
    };
    match merge {
        Merge::Const(value) => Merge::Const(konst(*value)),
        Merge::Prim(p, a) => Merge::Prim(prim(*p), args(a, table, prim, konst)),
        Merge::Function(t, a) => Merge::Function(table(*t), args(a, table, prim, konst)),
        other => other.clone(),
    }
}

#[derive(Default)]
struct Writer {
    words: Vec<u32>,
    strings: HashMap<String, u32, FxBuildHasher>,
    order: Vec<String>,
}

impl Writer {
    fn word(&mut self, word: impl TryInto<u32>) {
        let word = word
            .try_into()
            .ok()
            .expect("program file values fit in u32");
        self.words.push(word);
    }

    fn string(&mut self, s: &str) {
        let id = self.intern(s);
        self.word(id);
    }

    fn intern(&mut self, s: &str) -> u32 {
        let next = self.order.len() as u32;
        *self.strings.entry(s.to_owned()).or_insert_with(|| {
            self.order.push(s.to_owned());
            next
        })
    }

    fn list<T>(&mut self, items: &[T], mut item: impl FnMut(&mut Self, &T)) {
        self.word(items.len());
        items.iter().for_each(|x| item(self, x));
    }

    fn words(&mut self, items: &[u32]) {
        self.list(items, |w, &x| w.word(x));
    }

    // Words with no count before them.
    fn fixed(&mut self, words: &[u32]) {
        self.words.extend_from_slice(words);
    }

    fn merge(&mut self, merge: &Merge) {
        let (tag, head, args): (u32, u32, &[Merge]) = match merge {
            Merge::AssertEq => (0, 0, &[]),
            Merge::Union => (1, 0, &[]),
            Merge::Old => (2, 0, &[]),
            Merge::New => (3, 0, &[]),
            Merge::Const(value) => (4, value.raw(), &[]),
            Merge::Prim(prim, args) => (5, prim.index() as u32, args),
            Merge::Function(table, args) => (6, table.index() as u32, args),
        };
        self.word(tag);
        self.word(head);
        self.list(args, Self::merge);
    }

    fn constant(&mut self, value: &Const) {
        match value {
            Const::Lit(literal) => {
                self.word(0u32);
                self.literal(literal);
            }
            Const::BigInt(digits) => {
                self.word(1u32);
                self.string(digits);
            }
            Const::BigRat(num, den) => {
                self.word(2u32);
                self.string(num);
                self.string(den);
            }
            Const::Elems(elems) => {
                self.word(3u32);
                self.list(elems, Self::constant);
            }
        }
    }

    fn literal(&mut self, literal: &Literal) {
        let wide = |w: &mut Self, tag: u32, bits: u64| {
            w.word(tag);
            w.word(bits as u32);
            w.word((bits >> 32) as u32);
        };
        match literal {
            Literal::Int(i) => wide(self, 0, *i as u64),
            Literal::Float(x) => wide(self, 1, x.0.to_bits()),
            Literal::String(s) => {
                self.word(2u32);
                self.string(s);
            }
            Literal::Bool(b) => {
                self.word(3u32);
                self.word(u32::from(*b));
            }
            Literal::Unit => self.word(4u32),
            Literal::Arg(n) => {
                self.word(5u32);
                self.word(*n);
            }
        }
    }

    fn bind(&mut self, bind: Bind) {
        match bind {
            Bind::New(r) => self.word(r * 2),
            Bind::Same(r) => self.word(r * 2 + 1),
        }
    }

    fn step(&mut self, step: &Step) {
        let slice = |s: Slice| s as u32;
        match step {
            Step::Scan {
                table,
                slice: s,
                cols,
            } => {
                self.fixed(&[0, *table, slice(*s)]);
                self.list(cols, |w, &b| w.bind(b));
            }
            Step::Lookup {
                table,
                key,
                out,
                slice: s,
            } => {
                self.fixed(&[1, *table, slice(*s)]);
                self.words(key);
                self.word(u32::from(out.is_some()));
                out.iter().for_each(|&b| self.bind(b));
            }
            Step::Expand {
                index,
                key,
                outs,
                slice: s,
            } => {
                self.fixed(&[2, *index, slice(*s)]);
                self.words(key);
                self.list(outs, |w, &b| w.bind(b));
            }
            Step::Check { index, key } => {
                self.fixed(&[3, *index, 0]);
                self.words(key);
            }
            Step::Absent { index, key } => {
                self.fixed(&[6, *index, 0]);
                self.words(key);
            }
            Step::Same { a, b } => self.fixed(&[4, *a, *b]),
            Step::Prim {
                prim,
                context,
                args,
                out,
            } => {
                self.fixed(&[5, *prim, *context as u32]);
                self.words(args);
                self.bind(*out);
            }
        }
    }

    fn action(&mut self, action: &Action) {
        match action {
            Action::Entry { table, args, out } => {
                self.fixed(&[0, *table, *out]);
                self.words(args);
            }
            Action::Set { table, args } => self.tagged(1, *table, args),
            Action::Union { a, b } => self.fixed(&[2, *a, *b]),
            Action::Delete { table, args } => self.tagged(3, *table, args),
            Action::Subsume { table, args } => self.tagged(4, *table, args),
            Action::Call {
                prim,
                context,
                args,
            } => {
                self.fixed(&[5, *prim, *context as u32]);
                self.words(args);
            }
            Action::Panic { message } => {
                let message = self.intern(message);
                self.fixed(&[6, message, 0]);
            }
            Action::FindOrMake { rows, out } => {
                self.fixed(&[7, 0, *out]);
                self.list(rows, |w, (table, args)| {
                    w.word(*table);
                    w.words(args);
                });
            }
        }
    }

    // A tag and a head, then the arguments.
    fn tagged(&mut self, tag: u32, head: u32, args: &[Reg]) {
        self.fixed(&[tag, head, 0]);
        self.words(args);
    }
}

// The words of `file`, as bytes.
pub(crate) fn encode(file: &ProgramFile) -> Vec<u8> {
    let mut w = Writer::default();
    w.list(&file.sorts, |w, sort| {
        w.string(&sort.name);
        w.word(sort.class as u32);
    });
    w.list(&file.consts, |w, decl| {
        w.word(decl.sort);
        w.constant(&decl.value);
    });
    w.list(&file.tables, |w, table| {
        w.string(&table.name);
        w.word(table.kind as u32);
        w.words(&table.columns);
        w.word(table.keys);
        w.word(u32::from(table.merge.is_some()));
        table.merge.iter().for_each(|merge| w.merge(merge));
        w.list(&table.shapes, |w, &shape| w.word(shape as u32));
        w.list(&table.symmetries, |w, order| {
            w.list(order, |w, &c| w.word(c))
        });
        let cost = table.cost.unwrap_or(u64::MAX);
        w.fixed(&[
            cost as u32,
            (cost >> 32) as u32,
            u32::from(table.unextractable),
        ]);
        w.word(u32::from(table.default.is_some()));
        w.word(table.default.unwrap_or(0));
    });
    w.list(&file.externs, |w, prim| {
        w.string(&prim.name);
        w.words(&prim.inputs);
        w.word(prim.output);
    });
    w.list(&file.indexes, |w, index| {
        w.word(index.table);
        w.list(&index.order, |w, &c| w.word(c));
    });
    w.word(file.nodes.len());
    w.list(&file.rulesets, |w, ruleset| {
        w.string(&ruleset.name);
        w.words(&ruleset.roots);
        w.words(&ruleset.first);
        w.words(&ruleset.used);
        w.list(&ruleset.rules, |w, rule| w.string(rule));
        w.list(&ruleset.consts, Writer::literal);
        w.list(&ruleset.calls, |w, (prim, args)| {
            w.word(*prim);
            w.words(args);
        });
        w.list(&ruleset.members, |w, member| w.string(member));
        w.word(u32::from(ruleset.once));
        w.word(u32::from(ruleset.colored));
    });
    let table = w.words.len();
    w.words.resize(table + file.nodes.len(), 0);
    for (at, node) in file.nodes.iter().enumerate() {
        w.words[table + at] = w.words.len() as u32;
        w.step(&node.step);
        w.list(&node.actions, Writer::action);
        w.words(&node.children);
        w.words(&node.rules);
    }
    let mut bytes = MAGIC.to_vec();
    bytes.extend(VERSION.to_le_bytes());
    bytes.extend((w.order.len() as u32).to_le_bytes());
    for s in &w.order {
        bytes.extend((s.len() as u32).to_le_bytes());
        bytes.extend(s.as_bytes());
        bytes.resize(bytes.len().next_multiple_of(4), 0);
    }
    bytes.extend(w.words.iter().flat_map(|word| word.to_le_bytes()));
    bytes
}

struct Reader<'a> {
    words: &'a [u32],
    at: usize,
    // The file as bytes, and where each string of the table is in it.
    bytes: &'a [u8],
    strings: &'a [(u32, u32)],
}

type Load<T> = Result<T, LoadError>;

impl<'a> Reader<'a> {
    fn word(&mut self) -> Load<u32> {
        let word = self.words.get(self.at).copied();
        self.at += 1;
        word.ok_or(LoadError::Malformed("truncated"))
    }

    // A word below `limit`.
    fn below(&mut self, limit: usize, what: &'static str) -> Load<u32> {
        let word = self.word()?;
        if (word as usize) < limit {
            Ok(word)
        } else {
            Err(LoadError::Malformed(what))
        }
    }

    fn string(&mut self) -> Load<String> {
        let at = self.below(self.strings.len(), "string")?;
        self.string_at(at).map(str::to_owned)
    }

    fn string_at(&self, at: u32) -> Load<&'a str> {
        let &(start, len) =
            (self.strings.get(at as usize)).ok_or(LoadError::Malformed("string"))?;
        let text = std::str::from_utf8(&self.bytes[start as usize..(start + len) as usize]);
        text.map_err(|_| LoadError::Malformed("string"))
    }

    // Every item takes a word at least, so a count past the rest of the file
    // is malformed.
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Load<T>) -> Load<Vec<T>> {
        let len = self.word()? as usize;
        if len > self.words.len() - self.at {
            return Err(LoadError::Malformed("truncated"));
        }
        (0..len).map(|_| item(self)).collect()
    }

    fn words(&mut self) -> Load<Vec<u32>> {
        self.word_slice().map(<[u32]>::to_vec)
    }

    fn word_slice(&mut self) -> Load<&'a [u32]> {
        let len = self.word()? as usize;
        let end = self.at.saturating_add(len);
        let words = self.words.get(self.at..end);
        let words = words.ok_or(LoadError::Malformed("truncated"))?;
        self.at = end;
        Ok(words)
    }

    fn columns(&mut self) -> Load<Vec<usize>> {
        self.list(|r| r.word().map(|c| c as usize))
    }

    fn merge(&mut self, depth: u32) -> Load<Merge> {
        if depth == MERGE_DEPTH {
            return Err(LoadError::Malformed("merge"));
        }
        let (tag, head) = (self.word()?, self.word()?);
        let args = self.list(|r| r.merge(depth + 1))?;
        let head_id = head as usize;
        Ok(match (tag, args.is_empty()) {
            (0, true) => Merge::AssertEq,
            (1, true) => Merge::Union,
            (2, true) => Merge::Old,
            (3, true) => Merge::New,
            (4, true) => Merge::Const(crate::Value::new(head)),
            (5, _) => Merge::Prim(ExternId::new(head_id), args),
            (6, _) => Merge::Function(TableId::new(head_id), args),
            _ => return Err(LoadError::Malformed("merge")),
        })
    }

    fn constant(&mut self, depth: u32) -> Load<Const> {
        if depth == MERGE_DEPTH {
            return Err(LoadError::Malformed("constant"));
        }
        Ok(match self.word()? {
            0 => Const::Lit(self.literal()?),
            1 => Const::BigInt(self.string()?),
            2 => Const::BigRat(self.string()?, self.string()?),
            3 => Const::Elems(self.list(|r| r.constant(depth + 1))?),
            _ => return Err(LoadError::Malformed("constant")),
        })
    }

    fn literal(&mut self) -> Load<Literal> {
        let wide = |r: &mut Self| -> Load<u64> {
            let (lo, hi) = (r.word()?, r.word()?);
            Ok(u64::from(hi) << 32 | u64::from(lo))
        };
        Ok(match self.word()? {
            0 => Literal::Int(wide(self)? as i64),
            1 => Literal::Float(F64(f64::from_bits(wide(self)?))),
            2 => Literal::String(self.string()?),
            3 => Literal::Bool(self.below(2, "bool")? == 1),
            4 => Literal::Unit,
            5 => Literal::Arg(self.word()?),
            _ => return Err(LoadError::Malformed("literal")),
        })
    }

    fn step(&mut self) -> Load<(StepRef<'a>, u32)> {
        let (tag, head) = (self.word()?, self.word()?);
        ensure(tag <= 6, "step")?;
        let extra = self.word()?;
        let slice = || {
            [Slice::New, Slice::Old, Slice::All, Slice::Any]
                .get(extra as usize)
                .copied()
                .ok_or(LoadError::Malformed("slice"))
        };
        let step = match tag {
            0 => StepRef::Scan {
                table: head,
                slice: slice()?,
                cols: Binds(self.word_slice()?),
            },
            1 => {
                let slice = slice()?;
                let key = self.word_slice()?;
                let out = if self.below(2, "lookup")? == 1 {
                    Some(super::code::bind(self.word()?))
                } else {
                    None
                };
                StepRef::Lookup {
                    table: head,
                    key,
                    out,
                    slice,
                }
            }
            2 => StepRef::Expand {
                index: head,
                slice: slice()?,
                key: self.word_slice()?,
                outs: Binds(self.word_slice()?),
            },
            3 => StepRef::Check {
                index: head,
                key: self.word_slice()?,
            },
            4 => StepRef::Same { a: head, b: extra },
            6 => StepRef::Absent {
                index: head,
                key: self.word_slice()?,
            },
            5 => {
                ensure((extra as usize) < crate::Context::ALL.len(), "context")?;
                let args = self.word_slice()?;
                let out = super::code::bind(self.word()?);
                StepRef::Prim {
                    prim: head,
                    args,
                    out,
                }
            }
            _ => return Err(LoadError::Malformed("step")),
        };
        Ok((step, extra))
    }

    fn action(&mut self) -> Load<(ActionRef<'a>, u32)> {
        let (tag, head, extra) = (self.word()?, self.word()?, self.word()?);
        if tag == 2 {
            return Ok((ActionRef::Union { a: head, b: extra }, extra));
        }
        if tag == 6 {
            self.string_at(head)?;
            return Ok((ActionRef::Panic { message: head }, extra));
        }
        if tag == 7 {
            let count = self.word()?;
            ensure(count as usize <= self.words.len() - self.at, "truncated")?;
            let start = self.at;
            for _ in 0..count {
                self.word()?;
                self.word_slice()?;
            }
            return Ok((
                ActionRef::FindOrMake {
                    rows: &self.words[start..self.at],
                    count,
                    out: extra,
                },
                extra,
            ));
        }
        let args = self.word_slice()?;
        let action = match tag {
            0 => ActionRef::Entry { table: head, args },
            1 => ActionRef::Set { table: head, args },
            3 => ActionRef::Delete { table: head, args },
            4 => ActionRef::Subsume { table: head, args },
            5 => {
                ensure((extra as usize) < crate::Context::ALL.len(), "context")?;
                ActionRef::Call { prim: head, args }
            }
            _ => return Err(LoadError::Malformed("action")),
        };
        Ok((action, extra))
    }
}

// The words of a program file: its own, or a static file's in place.
pub(crate) enum Storage {
    Owned(Vec<u32>),
    Static(&'static [u32]),
}

impl Storage {
    fn words(&self) -> &[u32] {
        match self {
            Storage::Owned(words) => words,
            Storage::Static(words) => words,
        }
    }
}

// The file as the words it is, which hold its bytes in order on the
// little-endian machines a program runs on.
const _: () = assert!(cfg!(target_endian = "little"));

fn as_bytes(words: &[u32]) -> &[u8] {
    // SAFETY: a `u32` is four initialized bytes and its alignment is above
    // that of a `u8`.
    unsafe { std::slice::from_raw_parts(words.as_ptr().cast(), words.len() * 4) }
}

// The code of a loaded program file. Execution reads the original node
// words in place after load-time validation.
pub(crate) struct Code {
    storage: Storage,
    // The first of the words after the string table.
    start: usize,
    strings: Vec<(u32, u32)>,
    // Where the node offsets are among those words.
    table: usize,
    // The name of each rule of each ruleset, a string.
    pub(crate) rules: Vec<Vec<u32>>,
}

impl Code {
    fn words(&self) -> &[u32] {
        &self.storage.words()[self.start..]
    }

    pub(crate) fn node(&self, id: u32) -> NodeRef<'_> {
        let words = self.words();
        NodeRef::at(words, words[self.table + id as usize] as usize)
    }

    pub(crate) fn string(&self, at: u32) -> &str {
        let (start, len) = self.strings[at as usize];
        let bytes = &as_bytes(self.storage.words())[start as usize..(start + len) as usize];
        std::str::from_utf8(bytes).expect("a load checked the strings")
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        as_bytes(self.storage.words()).to_vec()
    }
}

// Reads a program file that lives as long as the program, in place.
pub(crate) fn decode_static(bytes: &'static [u8]) -> Load<(ProgramFile, Code)> {
    // SAFETY: any four bytes are a `u32`.
    let (head, words, rest) = unsafe { bytes.align_to::<u32>() };
    if head.is_empty() && rest.is_empty() {
        return load(Storage::Static(words));
    }
    decode(bytes)
}

// Parses a program file, copying its words.
pub(crate) fn decode(bytes: &[u8]) -> Load<(ProgramFile, Code)> {
    let (words, rest) = bytes.as_chunks::<4>();
    if !rest.is_empty() {
        return Err(LoadError::Malformed("truncated"));
    }
    load(Storage::Owned(
        (words.iter()).map(|&w| u32::from_le_bytes(w)).collect(),
    ))
}

fn load(storage: Storage) -> Load<(ProgramFile, Code)> {
    let all = storage.words();
    if all.len() < 3 || all[0] != u32::from_le_bytes(MAGIC) {
        return Err(LoadError::Malformed("not a program file"));
    }
    if all[1] != VERSION {
        let (found, expected) = (all[1], VERSION);
        return Err(LoadError::Version { found, expected });
    }
    let mut strings = Vec::with_capacity(all[2] as usize);
    let mut at = 3;
    for _ in 0..all[2] {
        let len = *all.get(at).ok_or(LoadError::Malformed("truncated"))?;
        at += 1;
        strings.push(((at * 4) as u32, len));
        at += (len as usize).div_ceil(4);
    }
    ensure(at <= all.len(), "truncated")?;
    let start = at;
    let mut r = Reader {
        words: &all[start..],
        at: 0,
        bytes: as_bytes(all),
        strings: &strings,
    };
    let sorts = r.list(|r| {
        let name = r.string()?;
        const CLASSES: [SortClass; 3] = [SortClass::Eq, SortClass::Base, SortClass::Container];
        let class = CLASSES[r.below(3, "sort kind")? as usize];
        Ok(SortDecl { name, class })
    })?;
    let consts = r.list(|r| {
        let sort = r.below(sorts.len(), "sort")?;
        let value = r.constant(0)?;
        Ok(ConstDecl { sort, value })
    })?;
    let tables = r.list(|r| {
        const KINDS: [TableKind; 3] = [
            TableKind::Relation,
            TableKind::Function,
            TableKind::Constructor,
        ];
        let name = r.string()?;
        let kind = KINDS[r.below(3, "table kind")? as usize];
        let columns = r.list(|r| r.below(sorts.len(), "sort"))?;
        let keys = r.word()? as usize;
        let merge = match r.below(2, "merge")? {
            1 => Some(r.merge(0)?),
            _ => None,
        };
        const SHAPES: [Shape; 2] = [Shape::Dense, Shape::Sparse];
        let shapes = r.list(|r| Ok(SHAPES[r.below(2, "shape")? as usize]))?;
        let symmetries = r.list(Reader::columns)?;
        let cost = u64::from(r.word()?) | u64::from(r.word()?) << 32;
        let unextractable = r.below(2, "unextractable")? == 1;
        let default = r.below(2, "default")? == 1;
        let index = r.word()?;
        let default = default.then_some(index);
        ensure(
            default.is_none_or(|c| (c as usize) < consts.len()),
            "default",
        )?;
        Ok(TableDecl {
            name,
            kind,
            columns,
            keys,
            merge,
            shapes,
            symmetries,
            cost: Some(cost).filter(|&cost| cost != u64::MAX),
            unextractable,
            default,
        })
    })?;
    let externs = r.list(|r| {
        Ok(ExternDecl {
            name: r.string()?,
            inputs: r.list(|r| r.below(sorts.len(), "sort"))?,
            output: r.below(sorts.len(), "sort")?,
            required: 0,
        })
    })?;
    let indexes = r.list(|r| {
        let table = r.below(tables.len(), "table")?;
        Ok(IndexDecl {
            table,
            order: r.columns()?,
        })
    })?;
    let nodes = r.word()? as usize;
    let mut rules = Vec::new();
    let rulesets = r.list(|r| {
        let name = r.string()?;
        let roots = r.list(|r| r.below(nodes, "node"))?;
        let first = r.list(|r| r.below(nodes, "node"))?;
        let used = r.list(|r| r.below(indexes.len(), "index"))?;
        rules.push(r.list(|r| r.below(r.strings.len(), "string"))?);
        Ok(RulesetDecl {
            name,
            roots,
            first,
            used,
            rules: Vec::new(),
            consts: r.list(Reader::literal)?,
            calls: r.list(|r| Ok((r.word()?, r.words()?)))?,
            members: r.list(Reader::string)?,
            once: r.below(2, "once")? == 1,
            colored: r.below(2, "colored")? == 1,
        })
    })?;
    let table = r.at;
    ensure(table + nodes <= r.words.len(), "truncated")?;
    let mut file = ProgramFile {
        sorts,
        consts,
        tables,
        externs,
        indexes,
        nodes: Vec::new(),
        rulesets,
    };
    // Public loads validate before the interpreter borrows the code.
    let offsets = &r.words[table..table + nodes];
    for &offset in offsets {
        r.at = offset as usize;
        r.step()?;
        r.list(|r| r.action().map(|_| ()))?;
        r.word_slice()?;
        r.word_slice()?;
    }
    ensure(
        offsets.is_empty() || r.at == r.words.len(),
        "trailing words",
    )?;
    for names in &rules {
        for &name in names {
            r.string_at(name)?;
        }
    }
    let required = check(&file, &mut r, offsets, &rules)?;
    for (decl, required) in file.externs.iter_mut().zip(required) {
        decl.required = required;
    }
    let code = Code {
        storage,
        start,
        strings,
        table,
        rules,
    };
    Ok((file, code))
}

fn ensure(ok: bool, what: &'static str) -> Load<()> {
    if ok {
        Ok(())
    } else {
        Err(LoadError::Malformed(what))
    }
}

// Checks what the decoder could not see item by item: arities, ids in
// merges and code, and that each ruleset's code is a tree binding its
// registers in order.
fn check(
    file: &ProgramFile,
    reader: &mut Reader<'_>,
    offsets: &[u32],
    rules: &[Vec<u32>],
) -> Load<Vec<u8>> {
    let mut required = vec![0; file.externs.len()];
    for table in &file.tables {
        let len = table.columns.len();
        let keys_ok = match table.kind {
            TableKind::Relation => {
                table.keys == len && table.merge.is_none() && table.default.is_none()
            }
            TableKind::Function => table.keys + 1 == len && table.merge.is_some(),
            TableKind::Constructor => {
                let output = table.columns.last().map(|&s| file.sorts[s as usize].class);
                table.keys + 1 == len
                    && table.merge == Some(Merge::Union)
                    && output == Some(SortClass::Eq)
                    && table.default.is_none()
            }
        };
        ensure(keys_ok && table.shapes.len() == len, "table")?;
        for order in &table.symmetries {
            let mut sorted = order.clone();
            sorted.sort_unstable();
            let fixed =
                table.kind == TableKind::Relation || order.get(table.keys) == Some(&table.keys);
            ensure(sorted.into_iter().eq(0..len) && fixed, "symmetry")?;
        }
        if let Some(merge) = &table.merge {
            check_merge(file, merge)?;
        }
    }
    for index in &file.indexes {
        let len = file.tables[index.table as usize].columns.len();
        let distinct = (0..index.order.len()).all(|i| !index.order[..i].contains(&index.order[i]));
        ensure(distinct && index.order.iter().all(|&c| c < len), "index")?;
    }
    for (id, &offset) in offsets.iter().enumerate() {
        reader.at = offset as usize;
        check_node(file, reader, &mut required)?;
        let children = reader.word_slice()?;
        ensure(children.iter().all(|&child| (child as usize) < id), "node")?;
        reader.word_slice()?;
    }
    for (i, ruleset) in file.rulesets.iter().enumerate() {
        let earlier = |name: &String| file.rulesets[..i].iter().any(|r| r.name == *name);
        ensure(ruleset.members.iter().all(earlier), "members")?;
    }
    let mut seen = vec![false; offsets.len()];
    for (ruleset, names) in file.rulesets.iter().zip(rules) {
        let width = (ruleset.consts.len() + ruleset.calls.len()) as u32;
        for (at, (prim, args)) in ruleset.calls.iter().enumerate() {
            let before = (ruleset.consts.len() + at) as u32;
            ensure((*prim as usize) < file.externs.len(), "call")?;
            required[*prim as usize] |= 1 << crate::Context::Pure as u32;
            ensure(
                file.externs[*prim as usize].inputs.len() == args.len(),
                "call",
            )?;
            ensure(args.iter().all(|&arg| arg < before), "call")?;
        }
        let mut query = QueryCheck {
            rules: names.len(),
            computed: ruleset.consts.len() as u32,
            present: vec![false; ruleset.calls.len()],
            used: vec![false; file.indexes.len()],
        };
        for &root in ruleset.roots.iter().chain(&ruleset.first) {
            walk(reader, offsets, root, width, &mut seen, &mut query)?;
        }
        ensure(
            query
                .used
                .iter()
                .enumerate()
                .all(|(at, &used)| !used || ruleset.used.contains(&(at as u32))),
            "used index",
        )?;
    }
    Ok(required)
}

fn check_merge(file: &ProgramFile, merge: &Merge) -> Load<()> {
    let args = match merge {
        Merge::Prim(prim, args) => {
            ensure(prim.index() < file.externs.len(), "merge")?;
            args
        }
        Merge::Function(table, args) => {
            ensure(table.index() < file.tables.len(), "merge")?;
            args
        }
        Merge::Const(value) => return ensure((value.raw() as usize) < file.consts.len(), "merge"),
        _ => return Ok(()),
    };
    args.iter().try_for_each(|arg| check_merge(file, arg))
}

// The ids and arities of one node.
fn check_node(file: &ProgramFile, reader: &mut Reader<'_>, required: &mut [u8]) -> Load<()> {
    let table = |t: u32| {
        file.tables
            .get(t as usize)
            .ok_or(LoadError::Malformed("table"))
    };
    let index = |i: u32| {
        file.indexes
            .get(i as usize)
            .ok_or(LoadError::Malformed("index"))
    };
    let prim = |p: u32| {
        file.externs
            .get(p as usize)
            .ok_or(LoadError::Malformed("primitive"))
    };
    let (step, context) = reader.step()?;
    if let StepRef::Prim { prim: prim_id, .. } = step {
        prim(prim_id)?;
        required[prim_id as usize] |= 1 << context;
    }
    match &step {
        StepRef::Scan { table: t, cols, .. } => {
            ensure(table(*t)?.columns.len() == cols.len(), "scan")?
        }
        StepRef::Lookup {
            table: t, key, out, ..
        } => {
            let t = table(*t)?;
            ensure(
                key.len() == t.keys && out.is_some() == (t.keys < t.columns.len()),
                "lookup",
            )?;
        }
        StepRef::Expand {
            index: i,
            key,
            outs,
            ..
        } => {
            let i = index(*i)?;
            let len = file.tables[i.table as usize].columns.len();
            ensure(
                key.len() == i.order.len() && key.len() + outs.len() == len,
                "expand",
            )?;
        }
        StepRef::Check { index: i, key } | StepRef::Absent { index: i, key } => {
            ensure(index(*i)?.order.len() == key.len(), "check")?
        }
        StepRef::Same { .. } => {}
        StepRef::Prim { prim: p, args, .. } => {
            ensure(prim(*p)?.inputs.len() == args.len(), "prim")?
        }
    }
    let count = reader.word()?;
    for _ in 0..count {
        let (action, extra) = reader.action()?;
        let ok = match &action {
            ActionRef::Entry { table: t, args, .. } => {
                let t = table(*t)?;
                (t.kind == TableKind::Constructor || t.default.is_some()) && args.len() == t.keys
            }
            ActionRef::Set { table: t, args } => table(*t)?.columns.len() == args.len(),
            ActionRef::Delete { table: t, args } | ActionRef::Subsume { table: t, args } => {
                table(*t)?.keys == args.len()
            }
            ActionRef::Call { prim: p, args } => {
                ensure(
                    matches!(
                        crate::Context::ALL[extra as usize],
                        crate::Context::Write | crate::Context::Full
                    ),
                    "action context",
                )?;
                let decl = prim(*p)?;
                required[*p as usize] |= 1 << extra;
                decl.inputs.len() == args.len()
            }
            ActionRef::FindOrMake { rows, count, .. } => {
                let mut group = None;
                for (t, args) in super::code::row_iter(rows, *count) {
                    let t = table(t)?;
                    ensure(*group.get_or_insert(t.columns[0]) == t.columns[0], "group")?;
                    ensure(args.len() + 1 == t.columns.len(), "row")?;
                }
                group.is_some()
            }
            ActionRef::Union { .. } | ActionRef::Panic { .. } => true,
        };
        ensure(ok, "action")?;
    }
    Ok(())
}

struct QueryCheck {
    rules: usize,
    computed: u32,
    present: Vec<bool>,
    used: Vec<bool>,
}

// Checks that the registers a node reads are bound and the ones it binds come
// next in order, then its children.
fn walk(
    reader: &mut Reader<'_>,
    offsets: &[u32],
    id: u32,
    width: u32,
    seen: &mut [bool],
    query: &mut QueryCheck,
) -> Load<()> {
    ensure(
        !std::mem::replace(&mut seen[id as usize], true),
        "shared node",
    )?;
    reader.at = offsets[id as usize] as usize;
    let (step, _) = reader.step()?;
    let mut w = width;
    let mut proved = [None; 2];
    if let StepRef::Same { a, b } = step {
        ensure(a < w && b < w, "register")?;
        for (slot, reg) in [a, b].into_iter().enumerate() {
            if let Some(at) = reg.checked_sub(query.computed).map(|at| at as usize)
                && let Some(present) = query.present.get_mut(at)
                && !*present
            {
                *present = true;
                proved[slot] = Some(at);
            }
        }
    }
    let reads = |regs: &[Reg], w: u32| {
        ensure(regs.iter().all(|&r| r < w), "register")?;
        ensure(
            regs.iter().all(|&r| {
                r.checked_sub(query.computed)
                    .and_then(|at| query.present.get(at as usize))
                    .copied()
                    .unwrap_or(true)
            }),
            "computed register",
        )
    };
    let bind = |bind: Bind, w: &mut u32| match bind {
        Bind::New(r) if r == *w => {
            *w += 1;
            Ok(())
        }
        Bind::Same(r) => reads(&[r], *w),
        _ => Err(LoadError::Malformed("register")),
    };
    match &step {
        StepRef::Scan { cols, .. } => cols.iter().try_for_each(|b| bind(b, &mut w))?,
        StepRef::Lookup { key, out, .. } => {
            reads(key, w)?;
            out.iter().try_for_each(|&b| bind(b, &mut w))?;
        }
        StepRef::Expand {
            index, key, outs, ..
        } => {
            query.used[*index as usize] = true;
            reads(key, w)?;
            outs.iter().try_for_each(|b| bind(b, &mut w))?;
        }
        StepRef::Check { index, key } | StepRef::Absent { index, key } => {
            query.used[*index as usize] = true;
            reads(key, w)?;
        }
        StepRef::Same { a, b } => reads(&[*a, *b], w)?,
        StepRef::Prim { args, out, .. } => {
            reads(args, w)?;
            bind(*out, &mut w)?;
        }
    }
    let count = reader.word()?;
    for _ in 0..count {
        let (action, extra) = reader.action()?;
        match &action {
            ActionRef::Entry { args, .. } => {
                reads(args, w)?;
                bind(Bind::New(extra), &mut w)?;
            }
            ActionRef::Set { args, .. }
            | ActionRef::Delete { args, .. }
            | ActionRef::Subsume { args, .. }
            | ActionRef::Call { args, .. } => reads(args, w)?,
            ActionRef::Union { a, b } => reads(&[*a, *b], w)?,
            ActionRef::Panic { .. } => {}
            ActionRef::FindOrMake { rows, count, out } => {
                for (_, args) in super::code::row_iter(rows, *count) {
                    reads(args, w)?;
                }
                bind(Bind::New(*out), &mut w)?;
            }
        }
    }
    let children = reader.word_slice()?;
    let rules = reader.word_slice()?;
    ensure(rules.iter().all(|&r| (r as usize) < query.rules), "rule")?;
    for &child in children {
        walk(reader, offsets, child, w, seen, query)?;
    }
    for at in proved.into_iter().flatten() {
        query.present[at] = false;
    }
    Ok(())
}

// Maps the sorts, tables and primitives of `file` to `db` by name, and
// declares the Eq sorts and tables `db` lacks. Returns the ids of the
// tables and primitives.
pub(crate) fn bind(file: &ProgramFile, db: &mut Database) -> Load<(Vec<TableId>, Vec<ExternId>)> {
    let mut sorts: Vec<SortId> = Vec::new();
    for decl in &file.sorts {
        let class = |kind: SortKind| match kind {
            SortKind::Eq => SortClass::Eq,
            SortKind::Base(_) => SortClass::Base,
            SortKind::Container(_) => SortClass::Container,
        };
        let found = db.schema().sort_named(&decl.name);
        sorts.push(match found {
            Some(id) if class(db.schema().sort(id).kind()) == decl.class => id,
            None if decl.class == SortClass::Eq => db.add_sort(&decl.name, SortKind::Eq),
            _ => {
                let name = decl.name.clone();
                return Err(LoadError::SortMismatch { name });
            }
        });
    }
    let sort = |s: &u32| sorts[*s as usize];
    let consts: Vec<Value> = (file.consts.iter())
        .map(|decl| crate::sort::intern(db, sort(&decl.sort), &decl.value))
        .collect::<Option<_>>()
        .ok_or(LoadError::Malformed("constant"))?;
    let mut externs = Vec::new();
    for decl in &file.externs {
        let inputs: Vec<SortId> = decl.inputs.iter().map(sort).collect();
        let output = sort(&decl.output);
        let schema = db.schema();
        let found = schema.primitives(&decl.name).find(|&prim| {
            let signature = schema.primitive(prim).1;
            signature.inputs == inputs && signature.output == output
        });
        let Some(found) = found else {
            let names: Vec<&str> = inputs.iter().map(|&s| schema.sort(s).name()).collect();
            let output = schema.sort(output).name();
            let signature = format!("({}) {output}", names.join(" "));
            let name = decl.name.clone();
            return Err(LoadError::MissingExtern { name, signature });
        };
        externs.push(found);
    }
    // A table the database lacks gets the next id, in file order.
    let mut next = db.schema().tables().count();
    let tables: Vec<TableId> = (file.tables.iter())
        .map(|decl| {
            db.schema().table_named(&decl.name).unwrap_or_else(|| {
                next += 1;
                TableId::new(next - 1)
            })
        })
        .collect();
    for decl in &file.tables {
        let columns: Vec<SortId> = decl.columns.iter().map(sort).collect();
        let merge = decl.merge.as_ref().map(|merge| {
            let mut table = |t: TableId| tables[t.index()];
            let mut prim = |p: ExternId| externs[p.index()];
            let mut konst = |c: Value| consts[c.raw() as usize];
            map_merge(merge, &mut table, &mut prim, &mut konst)
        });
        let default = decl.default.map(|c| consts[c as usize]);
        if let Some(id) = db.schema().table_named(&decl.name) {
            let have = db.schema().table(id);
            let same = have.kind() == decl.kind
                && have.columns() == columns
                && have.key_arity() == decl.keys
                && have.merge() == merge.as_ref()
                && have.default_value() == default;
            if !same {
                let name = decl.name.clone();
                return Err(LoadError::TableMismatch { name });
            }
            continue;
        }
        let (keys, out) = (&columns[..decl.keys], columns.get(decl.keys).copied());
        let id = match (decl.kind, out, merge) {
            (TableKind::Function, Some(out), Some(merge)) => {
                db.add_function(&decl.name, keys, out, merge)
            }
            (TableKind::Constructor, Some(out), _) => db.add_constructor(&decl.name, keys, out),
            _ => db.add_relation(&decl.name, keys),
        };
        db.set_shapes(id, &decl.shapes)
            .expect("a checked table has a shape per column");
        if let Some(cost) = decl.cost {
            db.set_cost(id, cost);
        }
        db.set_unextractable(id, decl.unextractable);
        if let Some(default) = default {
            db.set_default(id, default);
        }
    }
    Ok((tables, externs))
}
