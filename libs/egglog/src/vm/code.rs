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
