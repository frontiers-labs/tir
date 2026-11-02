// The code of a program: a trie of steps per ruleset. Tables, indexes and
// primitives are positions in the program file, which a load maps to the
// database. A batch holds one column per register, and registers are bound in
// order: a step that binds a register `r` runs on a batch of `r` columns.

pub(crate) type Reg = u32;

// Which rows of a table a step reads. Old rows were written before the
// ruleset last ran and New ones since; none of these see subsumed rows but
// Any, which reads every row as a lookup in an action does: a key it does
// not find fails the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Slice {
    New,
    Old,
    All,
    Any,
}

// A column a step reads: into a new register, or one that must equal a
// register bound already.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Bind {
    New(Reg),
    Same(Reg),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    // Every row of the slice, for each binding.
    Scan {
        table: u32,
        slice: Slice,
        cols: Vec<Bind>,
    },
    // The row of a key, through the key map; `out` reads the output column.
    Lookup {
        table: u32,
        key: Vec<Reg>,
        out: Option<Bind>,
        slice: Slice,
    },
    // The rows whose index columns hold the key; `outs` reads the other
    // columns in ascending order.
    Expand {
        index: u32,
        key: Vec<Reg>,
        outs: Vec<Bind>,
        slice: Slice,
    },
    // Keeps the bindings whose key the index holds.
    Check {
        index: u32,
        key: Vec<Reg>,
    },
    // Keeps the bindings whose key the index lacks.
    Absent {
        index: u32,
        key: Vec<Reg>,
    },
    // Both operands must exist. A self-equality guards a hoisted premise.
    Same {
        a: Reg,
        b: Reg,
    },
    // Drops the bindings the primitive has no result for.
    Prim {
        prim: u32,
        context: crate::Context,
        args: Vec<Reg>,
        out: Bind,
    },
}

// Writes wait for the end of the iteration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    // The e-class of a constructor key: the one the table holds, else one
    // made earlier in the iteration, else a fresh one, bound to `out`.
    Entry {
        table: u32,
        args: Vec<Reg>,
        out: Reg,
    },
    // The keyed group whose rows are exactly `rows`, each a member table and
    // the cells after its group column, else one made with them, bound to
    // `out`.
    FindOrMake {
        rows: Vec<(u32, Vec<Reg>)>,
        out: Reg,
    },
    // Writes a row: every column, the output last.
    Set {
        table: u32,
        args: Vec<Reg>,
    },
    Union {
        a: Reg,
        b: Reg,
    },
    Delete {
        table: u32,
        args: Vec<Reg>,
    },
    Subsume {
        table: u32,
        args: Vec<Reg>,
    },
    // Calls a primitive for its effect.
    Call {
        prim: u32,
        context: crate::Context,
        args: Vec<Reg>,
    },
    Panic {
        message: String,
    },
}

// The step runs on the batch of the parent, then the actions on its result,
// then each child. `rules` are the rules whose query ends at this node, by
// position in the ruleset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) step: Step,
    pub(crate) actions: Vec<Action>,
    pub(crate) children: Vec<u32>,
    pub(crate) rules: Vec<u32>,
}

// The code as the program file holds it, read in place: each view borrows
// the words of a node rather than retaining a decoded copy.

// A list of binds as words: twice the register, plus one for `Same`.
#[derive(Clone, Copy)]
pub(crate) struct Binds<'a>(pub(crate) &'a [u32]);

impl<'a> Binds<'a> {
    pub(crate) fn len(self) -> usize {
        self.0.len()
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = Bind> + 'a {
        self.0.iter().map(|&word| bind(word))
    }
}

pub(crate) fn bind(word: u32) -> Bind {
    match word % 2 {
        0 => Bind::New(word / 2),
        _ => Bind::Same(word / 2),
    }
}

#[derive(Clone, Copy)]
pub(crate) enum StepRef<'a> {
    Scan {
        table: u32,
        slice: Slice,
        cols: Binds<'a>,
    },
    Lookup {
        table: u32,
        key: &'a [Reg],
        out: Option<Bind>,
        slice: Slice,
    },
    Expand {
        index: u32,
        key: &'a [Reg],
        outs: Binds<'a>,
        slice: Slice,
    },
    Check {
        index: u32,
        key: &'a [Reg],
    },
    Absent {
        index: u32,
        key: &'a [Reg],
    },
    Same {
        a: Reg,
        b: Reg,
    },
    Prim {
        prim: u32,
        args: &'a [Reg],
        out: Bind,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum ActionRef<'a> {
    // Binds the next register, as an `Action::Entry` does.
    Entry {
        table: u32,
        args: &'a [Reg],
    },
    // Each row a member table, then the count and the registers of its cells.
    FindOrMake {
        rows: &'a [u32],
        count: u32,
        out: Reg,
    },
    Set {
        table: u32,
        args: &'a [Reg],
    },
    Union {
        a: Reg,
        b: Reg,
    },
    Delete {
        table: u32,
        args: &'a [Reg],
    },
    Subsume {
        table: u32,
        args: &'a [Reg],
    },
    Call {
        prim: u32,
        args: &'a [Reg],
    },
    // The message is a position in the string table.
    Panic {
        message: u32,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct NodeRef<'a> {
    pub(crate) step: StepRef<'a>,
    // The count and the words of the actions.
    actions: (u32, &'a [u32]),
    pub(crate) children: &'a [u32],
    pub(crate) rules: &'a [u32],
}

const SLICES: [Slice; 4] = [Slice::New, Slice::Old, Slice::All, Slice::Any];

// Reads words in order from a position.
struct Cursor<'a> {
    words: &'a [u32],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn word(&mut self) -> u32 {
        self.at += 1;
        self.words[self.at - 1]
    }

    fn list(&mut self) -> &'a [u32] {
        let len = self.word() as usize;
        self.at += len;
        &self.words[self.at - len..self.at]
    }
}

impl<'a> NodeRef<'a> {
    // The node at word `at`, as the writer of the program file laid it out.
    pub(crate) fn at(words: &'a [u32], at: usize) -> Self {
        let mut c = Cursor { words, at };
        let (tag, head, extra) = (c.word(), c.word(), c.word());
        let step = match tag {
            0 => StepRef::Scan {
                table: head,
                slice: SLICES[extra as usize],
                cols: Binds(c.list()),
            },
            1 => {
                let key = c.list();
                let out = (c.word() == 1).then(|| bind(c.word()));
                StepRef::Lookup {
                    table: head,
                    key,
                    out,
                    slice: SLICES[extra as usize],
                }
            }
            2 => StepRef::Expand {
                index: head,
                key: c.list(),
                outs: Binds(c.list()),
                slice: SLICES[extra as usize],
            },
            3 => StepRef::Check {
                index: head,
                key: c.list(),
            },
            6 => StepRef::Absent {
                index: head,
                key: c.list(),
            },
            4 => StepRef::Same { a: head, b: extra },
            _ => {
                let args = c.list();
                StepRef::Prim {
                    prim: head,
                    args,
                    out: bind(c.word()),
                }
            }
        };
        let count = c.word();
        let start = c.at;
        for _ in 0..count {
            action(&mut c);
        }
        let actions = (count, &words[start..c.at]);
        let children = c.list();
        let rules = c.list();
        NodeRef {
            step,
            actions,
            children,
            rules,
        }
    }

    pub(crate) fn actions(self) -> impl Iterator<Item = ActionRef<'a>> {
        let mut c = Cursor {
            words: self.actions.1,
            at: 0,
        };
        (0..self.actions.0).map(move |_| action(&mut c))
    }
}

fn action<'a>(c: &mut Cursor<'a>) -> ActionRef<'a> {
    let (tag, head, extra) = (c.word(), c.word(), c.word());
    match tag {
        2 => ActionRef::Union { a: head, b: extra },
        6 => ActionRef::Panic { message: head },
        7 => {
            let count = c.word();
            let start = c.at;
            for _ in 0..count {
                c.word();
                c.list();
            }
            ActionRef::FindOrMake {
                rows: &c.words[start..c.at],
                count,
                out: extra,
            }
        }
        _ => {
            let args = c.list();
            match tag {
                0 => ActionRef::Entry { table: head, args },
                1 => ActionRef::Set { table: head, args },
                3 => ActionRef::Delete { table: head, args },
                4 => ActionRef::Subsume { table: head, args },
                _ => ActionRef::Call { prim: head, args },
            }
        }
    }
}

// The member table and cell registers of each row of a `FindOrMake`.
pub(crate) fn rows(words: &[u32], count: u32) -> Vec<(u32, &[Reg])> {
    row_iter(words, count).collect()
}

pub(crate) fn row_iter(words: &[u32], count: u32) -> impl Iterator<Item = (u32, &[Reg])> {
    let mut c = Cursor { words, at: 0 };
    (0..count).map(move |_| (c.word(), c.list()))
}
