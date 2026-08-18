//! Token-level `macro_rules!`-style macro system for TMDL.
//!
//! Runs between `lex()` and `parse()`, rewriting `Vec<Spanned<Token>>` in place.
//! Everything downstream (parser, sema, typeck, codegen) is untouched.

use std::collections::HashMap;

use chumsky::error::Rich;

use crate::lexer::Token;
use crate::{Span, Spanned};

/// Diagnostic shape shared with sema/typeck: (file name, rich error).
pub type Diag = (String, Rich<'static, String, Span>);

const RECURSION_LIMIT: usize = 64;
const SIZE_LIMIT: usize = 1_000_000;
const NESTING_LIMIT: usize = 128;
const MATCH_STEP_BUDGET: usize = 1_000_000;

fn diag(file: &str, span: Span, msg: impl Into<String>) -> Diag {
    (file.to_string(), Rich::custom(span, msg.into()))
}

/// Arena for strings synthesized by `${concat(...)}`. Wraps `typed_arena::Arena`
/// so results live as long as the borrowed arena, avoiding `Box::leak` (the
/// fuzzer flags leaks).
pub struct StringArena(typed_arena::Arena<String>);

impl Default for StringArena {
    fn default() -> Self {
        Self::new()
    }
}

impl StringArena {
    pub fn new() -> Self {
        StringArena(typed_arena::Arena::new())
    }

    fn alloc(&self, s: String) -> &str {
        self.0.alloc(s).as_str()
    }
}

// --- token trees ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Delim {
    Paren,
    Bracket,
    Brace,
}

impl Delim {
    fn open(self) -> Token<'static> {
        match self {
            Delim::Paren => Token::LParen,
            Delim::Bracket => Token::LBracket,
            Delim::Brace => Token::LBrace,
        }
    }

    fn close(self) -> Token<'static> {
        match self {
            Delim::Paren => Token::RParen,
            Delim::Bracket => Token::RBracket,
            Delim::Brace => Token::RBrace,
        }
    }

    fn from_open(t: &Token) -> Option<Delim> {
        match t {
            Token::LParen => Some(Delim::Paren),
            Token::LBracket => Some(Delim::Bracket),
            Token::LBrace => Some(Delim::Brace),
            _ => None,
        }
    }

    fn from_close(t: &Token) -> Option<Delim> {
        match t {
            Token::RParen => Some(Delim::Paren),
            Token::RBracket => Some(Delim::Bracket),
            Token::RBrace => Some(Delim::Brace),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum TokenTree<'s> {
    Leaf(Spanned<Token<'s>>),
    Group {
        delim: Delim,
        open: Span,
        close: Span,
        trees: Vec<TokenTree<'s>>,
    },
}

impl<'s> TokenTree<'s> {
    fn span(&self) -> Span {
        match self {
            TokenTree::Leaf((_, s)) => *s,
            TokenTree::Group { open, close, .. } => (open.start..close.end).into(),
        }
    }
}

/// Build a token-tree forest from a flat token stream. `<`/`>` are ordinary
/// leaves, never delimiters (needed for `bits<8>`).
fn build_trees<'s>(
    file: &str,
    tokens: Vec<Spanned<Token<'s>>>,
    diags: &mut Vec<Diag>,
) -> Option<Vec<TokenTree<'s>>> {
    // Stack of (delim, open span, accumulated children).
    let mut stack: Vec<(Delim, Span, Vec<TokenTree<'s>>)> = vec![];
    let mut top: Vec<TokenTree<'s>> = vec![];

    for (tok, span) in tokens {
        if let Some(d) = Delim::from_open(&tok) {
            if stack.len() >= NESTING_LIMIT {
                diags.push(diag(file, span, "delimiter nesting too deep"));
                return None;
            }
            stack.push((d, span, std::mem::take(&mut top)));
            top = vec![];
        } else if let Some(d) = Delim::from_close(&tok) {
            let Some((od, open, parent)) = stack.pop() else {
                diags.push(diag(file, span, "unbalanced delimiter"));
                return None;
            };
            if od != d {
                diags.push(diag(file, span, "unbalanced delimiter"));
                return None;
            }
            let group = TokenTree::Group {
                delim: od,
                open,
                close: span,
                trees: std::mem::replace(&mut top, parent),
            };
            top.push(group);
        } else {
            top.push(TokenTree::Leaf((tok, span)));
        }
    }

    if let Some((_, open, _)) = stack.last() {
        diags.push(diag(file, *open, "unbalanced delimiter"));
        return None;
    }
    Some(top)
}

fn flatten<'s>(trees: &[TokenTree<'s>], out: &mut Vec<Spanned<Token<'s>>>) {
    for t in trees {
        match t {
            TokenTree::Leaf(l) => out.push(l.clone()),
            TokenTree::Group {
                delim,
                open,
                close,
                trees,
            } => {
                out.push((delim.open(), *open));
                flatten(trees, out);
                out.push((delim.close(), *close));
            }
        }
    }
}

fn count_tokens(trees: &[TokenTree]) -> usize {
    trees
        .iter()
        .map(|t| match t {
            TokenTree::Leaf(_) => 1,
            TokenTree::Group { trees, .. } => 2 + count_tokens(trees),
        })
        .sum()
}

// --- macro definitions ------------------------------------------------------

#[derive(Clone, Debug)]
enum FragKind {
    Ident,
    Literal,
    Tt,
}

#[derive(Clone, Debug)]
enum MatcherNode<'s> {
    Token(Token<'s>),
    Group {
        delim: Delim,
        sub: Vec<MatcherNode<'s>>,
    },
    Frag {
        name: String,
        kind: FragKind,
    },
    Repeat {
        sub: Vec<MatcherNode<'s>>,
        sep: Option<Token<'s>>,
        plus: bool,
    },
}

#[derive(Clone, Debug)]
enum TransNode<'s> {
    Token(Token<'s>),
    Group {
        delim: Delim,
        sub: Vec<TransNode<'s>>,
    },
    Var(String),
    Repeat {
        sub: Vec<TransNode<'s>>,
        sep: Option<Token<'s>>,
    },
    Concat(Vec<TransNode<'s>>),
}

struct MacroArm<'s> {
    matcher: Vec<MatcherNode<'s>>,
    transcription: Vec<TransNode<'s>>,
}

struct MacroDef<'s> {
    arms: Vec<MacroArm<'s>>,
    def_file: String,
    def_span: Span,
}

#[derive(Default)]
pub struct MacroTable<'s> {
    macros: HashMap<String, MacroDef<'s>>,
}

impl<'s> MacroTable<'s> {
    pub fn new() -> Self {
        MacroTable {
            macros: HashMap::new(),
        }
    }
}

// --- collecting macro definitions -------------------------------------------

/// Parse and remove `macro NAME { ... }` items from `tokens`, registering them
/// in `table`. Returns the token stream with the definitions stripped.
pub fn collect_macros<'s>(
    file: &str,
    tokens: Vec<Spanned<Token<'s>>>,
    table: &mut MacroTable<'s>,
    diags: &mut Vec<Diag>,
) -> Vec<Spanned<Token<'s>>> {
    let Some(trees) = build_trees(file, tokens, diags) else {
        return vec![];
    };

    let mut kept: Vec<TokenTree<'s>> = vec![];
    let mut i = 0;
    while i < trees.len() {
        if matches!(&trees[i], TokenTree::Leaf((Token::KwMacro, _))) {
            let def_span = trees[i].span();
            let name = match trees.get(i + 1) {
                Some(TokenTree::Leaf((Token::Identifier(n), _))) => n.to_string(),
                _ => {
                    diags.push(diag(file, def_span, "expected macro name after `macro`"));
                    return vec![];
                }
            };
            let body = match trees.get(i + 2) {
                Some(TokenTree::Group {
                    delim: Delim::Brace,
                    trees,
                    ..
                }) => trees,
                _ => {
                    diags.push(diag(file, def_span, "expected `{ ... }` macro body"));
                    return vec![];
                }
            };
            match parse_macro_def(file, def_span, body, diags) {
                Some(def) => {
                    if let Some(prev) = table.macros.get(&name) {
                        diags.push(diag(
                            file,
                            def_span,
                            format!(
                                "duplicate macro `{}` (already defined in {})",
                                name, prev.def_file
                            ),
                        ));
                    } else {
                        table.macros.insert(name, def);
                    }
                }
                None => return vec![],
            }
            i += 3;
        } else {
            kept.push(trees[i].clone());
            i += 1;
        }
    }

    let mut out = vec![];
    flatten(&kept, &mut out);
    out
}

fn parse_macro_def<'s>(
    file: &str,
    def_span: Span,
    body: &[TokenTree<'s>],
    diags: &mut Vec<Diag>,
) -> Option<MacroDef<'s>> {
    let mut arms = vec![];
    let mut i = 0;
    while i < body.len() {
        let matcher_group = match &body[i] {
            TokenTree::Group { trees, .. } => trees,
            _ => {
                diags.push(diag(
                    file,
                    body[i].span(),
                    "expected macro matcher `( ... )`",
                ));
                return None;
            }
        };
        if !matches!(body.get(i + 1), Some(TokenTree::Leaf((Token::FatArrow, _)))) {
            diags.push(diag(file, def_span, "expected `=>` in macro arm"));
            return None;
        }
        let trans_group = match body.get(i + 2) {
            Some(TokenTree::Group { trees, .. }) => trees,
            _ => {
                diags.push(diag(file, def_span, "expected transcription `{ ... }`"));
                return None;
            }
        };

        let matcher = parse_matcher(file, matcher_group, diags)?;
        let transcription = parse_transcription(file, trans_group, diags)?;
        arms.push(MacroArm {
            matcher,
            transcription,
        });

        i += 3;
        // Optional arm separator.
        if matches!(body.get(i), Some(TokenTree::Leaf((Token::Semicolon, _)))) {
            i += 1;
        }
    }

    if arms.is_empty() {
        diags.push(diag(file, def_span, "macro has no arms"));
        return None;
    }

    Some(MacroDef {
        arms,
        def_file: file.to_string(),
        def_span,
    })
}

fn parse_matcher<'s>(
    file: &str,
    trees: &[TokenTree<'s>],
    diags: &mut Vec<Diag>,
) -> Option<Vec<MatcherNode<'s>>> {
    let mut out = vec![];
    let mut i = 0;
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Leaf((Token::Dollar, span)) => match trees.get(i + 1) {
                Some(TokenTree::Group {
                    delim: Delim::Paren,
                    trees: sub_trees,
                    ..
                }) => {
                    let sub = parse_matcher(file, sub_trees, diags)?;
                    let (sep, plus, consumed) = parse_rep_tail(file, trees, i + 2, diags)?;
                    out.push(MatcherNode::Repeat { sub, sep, plus });
                    i += consumed;
                }
                Some(TokenTree::Leaf((Token::Identifier(name), _))) => {
                    // `$name : kind`
                    if !matches!(trees.get(i + 2), Some(TokenTree::Leaf((Token::Colon, _)))) {
                        diags.push(diag(file, *span, "expected `:` after `$` metavariable"));
                        return None;
                    }
                    let kind = match trees.get(i + 3) {
                        Some(TokenTree::Leaf((Token::Identifier(k), _))) => match *k {
                            "ident" => FragKind::Ident,
                            "literal" => FragKind::Literal,
                            "tt" => FragKind::Tt,
                            other => {
                                diags.push(diag(
                                    file,
                                    *span,
                                    format!("unknown fragment specifier `{}`", other),
                                ));
                                return None;
                            }
                        },
                        _ => {
                            diags.push(diag(file, *span, "expected fragment specifier"));
                            return None;
                        }
                    };
                    out.push(MatcherNode::Frag {
                        name: name.to_string(),
                        kind,
                    });
                    i += 4;
                }
                _ => {
                    diags.push(diag(
                        file,
                        *span,
                        "expected metavariable or `( ... )` after `$`",
                    ));
                    return None;
                }
            },
            TokenTree::Leaf((tok, _)) => {
                out.push(MatcherNode::Token(tok.clone()));
                i += 1;
            }
            TokenTree::Group { delim, trees, .. } => {
                let sub = parse_matcher(file, trees, diags)?;
                out.push(MatcherNode::Group { delim: *delim, sub });
                i += 1;
            }
        }
    }
    Some(out)
}

/// Parse an optional single-token separator followed by `*` or `+` after a
/// `$( ... )` repetition group. Returns (separator, is_plus, tokens consumed
/// starting at `start`, including the group at `start - 1`... caller adds 2).
fn parse_rep_tail<'s>(
    file: &str,
    trees: &[TokenTree<'s>],
    start: usize,
    diags: &mut Vec<Diag>,
) -> Option<(Option<Token<'s>>, bool, usize)> {
    // `consumed` counts trees from the `$` position: `$` + group = 2, plus tail.
    let rep_kind = |t: &TokenTree<'s>| match t {
        TokenTree::Leaf((Token::Asterisk, _)) => Some(false),
        TokenTree::Leaf((Token::Plus, _)) => Some(true),
        _ => None,
    };

    if let Some(t) = trees.get(start) {
        if let Some(plus) = rep_kind(t) {
            return Some((None, plus, 3));
        }
        // Separator then `*`/`+`.
        if let TokenTree::Leaf((sep, sep_span)) = t {
            match trees.get(start + 1).and_then(rep_kind) {
                Some(plus) => return Some((Some(sep.clone()), plus, 4)),
                None => {
                    diags.push(diag(file, *sep_span, "expected `*` or `+` after separator"));
                    return None;
                }
            }
        }
    }
    diags.push(diag(
        file,
        trees
            .get(start)
            .map(|t| t.span())
            .unwrap_or_else(|| (0..0).into()),
        "expected `*` or `+` after `$( ... )`",
    ));
    None
}

fn parse_transcription<'s>(
    file: &str,
    trees: &[TokenTree<'s>],
    diags: &mut Vec<Diag>,
) -> Option<Vec<TransNode<'s>>> {
    let mut out = vec![];
    let mut i = 0;
    while i < trees.len() {
        match &trees[i] {
            TokenTree::Leaf((Token::Dollar, span)) => match trees.get(i + 1) {
                Some(TokenTree::Group {
                    delim: Delim::Paren,
                    trees: sub_trees,
                    ..
                }) => {
                    let sub = parse_transcription(file, sub_trees, diags)?;
                    let (sep, _plus, consumed) = parse_rep_tail(file, trees, i + 2, diags)?;
                    out.push(TransNode::Repeat { sub, sep });
                    i += consumed;
                }
                Some(TokenTree::Group {
                    delim: Delim::Brace,
                    trees: sub_trees,
                    ..
                }) => {
                    let concat = parse_concat(file, *span, sub_trees, diags)?;
                    out.push(concat);
                    i += 2;
                }
                Some(TokenTree::Leaf((Token::Identifier(name), _))) => {
                    out.push(TransNode::Var(name.to_string()));
                    i += 2;
                }
                _ => {
                    diags.push(diag(
                        file,
                        *span,
                        "expected metavariable, `( ... )` or `{ ... }` after `$`",
                    ));
                    return None;
                }
            },
            TokenTree::Leaf((tok, _)) => {
                out.push(TransNode::Token(tok.clone()));
                i += 1;
            }
            TokenTree::Group { delim, trees, .. } => {
                let sub = parse_transcription(file, trees, diags)?;
                out.push(TransNode::Group { delim: *delim, sub });
                i += 1;
            }
        }
    }
    Some(out)
}

fn parse_concat<'s>(
    file: &str,
    span: Span,
    trees: &[TokenTree<'s>],
    diags: &mut Vec<Diag>,
) -> Option<TransNode<'s>> {
    let ok = matches!(
        trees.first(),
        Some(TokenTree::Leaf((Token::Identifier("concat"), _)))
    );
    let args_group = match (ok, trees.get(1)) {
        (
            true,
            Some(TokenTree::Group {
                delim: Delim::Paren,
                trees: args,
                ..
            }),
        ) if trees.len() == 2 => args,
        _ => {
            diags.push(diag(file, span, "expected `${concat( ... )}`"));
            return None;
        }
    };

    // Comma-separated argument list, each a single transcription node.
    let mut args = vec![];
    for chunk in args_group.split(|t| matches!(t, TokenTree::Leaf((Token::Comma, _)))) {
        if chunk.is_empty() {
            continue;
        }
        let nodes = parse_transcription(file, chunk, diags)?;
        if nodes.len() != 1 {
            diags.push(diag(
                file,
                span,
                "each `concat` argument must be a single token",
            ));
            return None;
        }
        args.push(nodes.into_iter().next().unwrap());
    }
    if args.is_empty() {
        diags.push(diag(file, span, "`concat` needs at least one argument"));
        return None;
    }
    Some(TransNode::Concat(args))
}

// --- matching ---------------------------------------------------------------

#[derive(Clone, Debug)]
enum Binding<'s> {
    Leaf(Vec<TokenTree<'s>>),
    Seq(Vec<Binding<'s>>),
}

type Bindings<'s> = HashMap<String, Binding<'s>>;

/// Threads a match-step budget through matching so pathological backtracking
/// aborts instead of running exponentially long.
struct MatchCtx {
    steps: usize,
}

impl MatchCtx {
    fn tick(&mut self) -> bool {
        self.steps += 1;
        self.steps <= MATCH_STEP_BUDGET
    }

    fn exhausted(&self) -> bool {
        self.steps > MATCH_STEP_BUDGET
    }
}

fn frag_names<'s>(nodes: &[MatcherNode<'s>], out: &mut Vec<String>) {
    for n in nodes {
        match n {
            MatcherNode::Frag { name, .. } => out.push(name.clone()),
            MatcherNode::Group { sub, .. } => frag_names(sub, out),
            MatcherNode::Repeat { sub, .. } => frag_names(sub, out),
            MatcherNode::Token(_) => {}
        }
    }
}

fn leaf_tok<'a, 's>(t: &'a TokenTree<'s>) -> Option<&'a Token<'s>> {
    match t {
        TokenTree::Leaf((tok, _)) => Some(tok),
        _ => None,
    }
}

/// Match `nodes` against `input[pos..]`; return the position after the match, or
/// `None`. Bindings accumulate into `out` (cloned on backtracking branches).
fn match_nodes<'s>(
    nodes: &[MatcherNode<'s>],
    input: &[TokenTree<'s>],
    pos: usize,
    out: &mut Bindings<'s>,
    ctx: &mut MatchCtx,
) -> Option<usize> {
    if !ctx.tick() {
        return None;
    }
    let Some((first, rest)) = nodes.split_first() else {
        return Some(pos);
    };
    match first {
        MatcherNode::Token(t) => {
            if leaf_tok(input.get(pos)?) == Some(t) {
                match_nodes(rest, input, pos + 1, out, ctx)
            } else {
                None
            }
        }
        MatcherNode::Group { delim, sub } => {
            if let TokenTree::Group {
                delim: d, trees, ..
            } = input.get(pos)?
            {
                if d != delim {
                    return None;
                }
                // No clone: a failed group aborts the whole arm, which retries
                // from a fresh `Bindings`, so partial writes to `out` are discarded.
                let end = match_nodes(sub, trees, 0, out, ctx)?;
                if end != trees.len() {
                    return None;
                }
                match_nodes(rest, input, pos + 1, out, ctx)
            } else {
                None
            }
        }
        MatcherNode::Frag { name, kind } => {
            let tree = input.get(pos)?;
            let ok = match kind {
                FragKind::Ident => matches!(leaf_tok(tree), Some(Token::Identifier(_))),
                FragKind::Literal => {
                    matches!(leaf_tok(tree), Some(Token::Number(_) | Token::StringLit(_)))
                }
                FragKind::Tt => true,
            };
            if !ok {
                return None;
            }
            out.insert(name.clone(), Binding::Leaf(vec![tree.clone()]));
            match_nodes(rest, input, pos + 1, out, ctx)
        }
        MatcherNode::Repeat { sub, sep, plus } => {
            let mut names = vec![];
            frag_names(sub, &mut names);
            let rep = Repeat {
                sub,
                sep,
                plus: *plus,
                names: &names,
                rest,
                input,
            };
            let mut iters: Vec<Bindings<'s>> = vec![];
            match_repeat(&rep, pos, out, &mut iters, ctx)
        }
    }
}

/// Invariant description of one `$( ... )*` repetition, threaded unchanged
/// through `match_repeat`'s greedy recursion.
struct Repeat<'a, 's> {
    sub: &'a [MatcherNode<'s>],
    sep: &'a Option<Token<'s>>,
    plus: bool,
    names: &'a [String],
    rest: &'a [MatcherNode<'s>],
    input: &'a [TokenTree<'s>],
}

fn match_repeat<'s>(
    rep: &Repeat<'_, 's>,
    pos: usize,
    out: &mut Bindings<'s>,
    iters: &mut Vec<Bindings<'s>>,
    ctx: &mut MatchCtx,
) -> Option<usize> {
    // Greedy: attempt one more iteration first.
    let sep_ok = if iters.is_empty() {
        Some(pos)
    } else if let Some(s) = rep.sep {
        if rep.input.get(pos).and_then(leaf_tok) == Some(s) {
            Some(pos + 1)
        } else {
            None
        }
    } else {
        Some(pos)
    };

    if let Some(start) = sep_ok {
        let mut it_map = Bindings::new();
        if let Some(end) = match_nodes(rep.sub, rep.input, start, &mut it_map, ctx) {
            // Progress guard: a zero-consuming iteration would recurse forever at
            // the same position, so stop rather than push another empty iteration.
            if end > pos {
                iters.push(it_map);
                if let Some(r) = match_repeat(rep, end, out, iters, ctx) {
                    return Some(r);
                }
                iters.pop();
            }
        }
    }

    // Stop repeating here.
    if rep.plus && iters.is_empty() {
        return None;
    }
    let mut merged = out.clone();
    for name in rep.names {
        let seq = iters
            .iter()
            .map(|m| m.get(name).cloned().unwrap_or(Binding::Leaf(vec![])))
            .collect();
        merged.insert(name.clone(), Binding::Seq(seq));
    }
    let r = match_nodes(rep.rest, rep.input, pos, &mut merged, ctx)?;
    *out = merged;
    Some(r)
}

// --- transcription ----------------------------------------------------------

struct Transcriber<'a, 's> {
    file: &'a str,
    arena: &'s StringArena,
    inv_span: Span,
    diags: Vec<Diag>,
}

impl<'a, 's> Transcriber<'a, 's> {
    fn used_var_len(&mut self, nodes: &[TransNode<'s>], bindings: &Bindings<'s>) -> Option<usize> {
        let mut len: Option<usize> = None;
        self.collect_lengths(nodes, bindings, &mut len);
        len
    }

    fn collect_lengths(
        &mut self,
        nodes: &[TransNode<'s>],
        bindings: &Bindings<'s>,
        len: &mut Option<usize>,
    ) {
        for n in nodes {
            match n {
                TransNode::Var(name) => {
                    if let Some(Binding::Seq(seq)) = bindings.get(name) {
                        match len {
                            Some(l) if *l != seq.len() => {
                                self.diags.push(diag(
                                    self.file,
                                    self.inv_span,
                                    "repetition length mismatch between metavariables",
                                ));
                            }
                            _ => *len = Some(seq.len()),
                        }
                    }
                }
                TransNode::Concat(args) => self.collect_lengths(args, bindings, len),
                TransNode::Group { sub, .. } => self.collect_lengths(sub, bindings, len),
                // Recurse into nested repetitions: a metavar used there is still a
                // `Seq` at this scope, so its outer length drives this repetition.
                TransNode::Repeat { sub, .. } => self.collect_lengths(sub, bindings, len),
                TransNode::Token(_) => {}
            }
        }
    }

    fn transcribe(
        &mut self,
        nodes: &[TransNode<'s>],
        bindings: &Bindings<'s>,
        out: &mut Vec<Spanned<Token<'s>>>,
    ) {
        for n in nodes {
            match n {
                TransNode::Token(tok) => out.push((tok.clone(), self.inv_span)),
                TransNode::Group { delim, sub } => {
                    out.push((delim.open(), self.inv_span));
                    self.transcribe(sub, bindings, out);
                    out.push((delim.close(), self.inv_span));
                }
                TransNode::Var(name) => match bindings.get(name) {
                    Some(Binding::Leaf(trees)) => {
                        let mut flat = vec![];
                        flatten(trees, &mut flat);
                        for (t, _) in flat {
                            out.push((t, self.inv_span));
                        }
                    }
                    Some(Binding::Seq(_)) => self.diags.push(diag(
                        self.file,
                        self.inv_span,
                        format!("metavariable `{}` used without matching repetition", name),
                    )),
                    None => self.diags.push(diag(
                        self.file,
                        self.inv_span,
                        format!("unknown metavariable `{}`", name),
                    )),
                },
                TransNode::Repeat { sub, sep } => {
                    let Some(len) = self.used_var_len(sub, bindings) else {
                        self.diags.push(diag(
                            self.file,
                            self.inv_span,
                            "repetition contains no metavariables",
                        ));
                        continue;
                    };
                    for i in 0..len {
                        let narrowed = narrow(bindings, i);
                        self.transcribe(sub, &narrowed, out);
                        if let (true, Some(s)) = (i + 1 < len, sep) {
                            out.push((s.clone(), self.inv_span));
                        }
                    }
                }
                TransNode::Concat(args) => self.transcribe_concat(args, bindings, out),
            }
        }
    }

    fn transcribe_concat(
        &mut self,
        args: &[TransNode<'s>],
        bindings: &Bindings<'s>,
        out: &mut Vec<Spanned<Token<'s>>>,
    ) {
        let mut text = String::new();
        let mut first_kind = None;
        for (idx, arg) in args.iter().enumerate() {
            let Some((piece, kind)) = self.concat_arg(arg, bindings) else {
                return;
            };
            if idx == 0 {
                first_kind = Some(kind);
            }
            text.push_str(&piece);
        }
        let s = self.arena.alloc(text);
        let tok = match first_kind {
            Some(ConcatKind::Str) => Token::StringLit(s),
            Some(ConcatKind::Num) => Token::Number(s),
            _ => Token::Identifier(s),
        };
        out.push((tok, self.inv_span));
    }

    fn concat_arg(
        &mut self,
        arg: &TransNode<'s>,
        bindings: &Bindings<'s>,
    ) -> Option<(String, ConcatKind)> {
        let tok = match arg {
            TransNode::Token(t) => t.clone(),
            TransNode::Var(name) => match bindings.get(name) {
                Some(Binding::Leaf(trees)) if trees.len() == 1 => match &trees[0] {
                    TokenTree::Leaf((t, _)) => t.clone(),
                    _ => {
                        self.bad_concat();
                        return None;
                    }
                },
                _ => {
                    self.bad_concat();
                    return None;
                }
            },
            _ => {
                self.bad_concat();
                return None;
            }
        };
        match tok {
            Token::Identifier(s) => Some((s.to_string(), ConcatKind::Ident)),
            Token::Number(s) => Some((s.to_string(), ConcatKind::Num)),
            Token::StringLit(s) => Some((s.to_string(), ConcatKind::Str)),
            _ => {
                self.bad_concat();
                None
            }
        }
    }

    fn bad_concat(&mut self) {
        self.diags.push(diag(
            self.file,
            self.inv_span,
            "invalid `${concat}` argument (expected identifier, number or string)",
        ));
    }
}

enum ConcatKind {
    Ident,
    Num,
    Str,
}

/// Narrow every sequence binding to its `i`-th element for one repetition step.
fn narrow<'s>(bindings: &Bindings<'s>, i: usize) -> Bindings<'s> {
    bindings
        .iter()
        .map(|(k, v)| {
            let nv = match v {
                Binding::Seq(seq) => seq.get(i).cloned().unwrap_or(Binding::Leaf(vec![])),
                other => other.clone(),
            };
            (k.clone(), nv)
        })
        .collect()
}

// --- expansion --------------------------------------------------------------

/// Expand all macro invocations in `tokens` to a fixpoint. Expanded tokens carry
/// the invocation-site span.
pub fn expand<'s>(
    file: &str,
    tokens: Vec<Spanned<Token<'s>>>,
    table: &MacroTable<'s>,
    arena: &'s StringArena,
) -> (Vec<Spanned<Token<'s>>>, Vec<Diag>) {
    let mut diags = vec![];
    let Some(mut trees) = build_trees(file, tokens, &mut diags) else {
        return (vec![], diags);
    };

    let mut depth = 0;
    loop {
        let mut ctx = ExpandCtx {
            diags: &mut diags,
            changed: false,
            had_error: false,
        };
        let next = expand_trees(file, &trees, table, arena, &mut ctx);
        let changed = ctx.changed;
        let had_error = ctx.had_error;
        trees = next;
        if had_error {
            return (vec![], diags);
        }
        if !changed {
            break;
        }
        depth += 1;
        if depth > RECURSION_LIMIT {
            diags.push(diag(file, (0..0).into(), "macro recursion limit exceeded"));
            return (vec![], diags);
        }
        if count_tokens(&trees) > SIZE_LIMIT {
            diags.push(diag(
                file,
                (0..0).into(),
                "macro expansion size limit exceeded",
            ));
            return (vec![], diags);
        }
    }

    let mut out = vec![];
    flatten(&trees, &mut out);
    (out, diags)
}

/// Mutable state threaded through one expansion pass.
struct ExpandCtx<'a> {
    diags: &'a mut Vec<Diag>,
    changed: bool,
    had_error: bool,
}

/// One expansion pass: replace every invocation whose name is known, recursing
/// into non-invocation groups.
fn expand_trees<'s>(
    file: &str,
    trees: &[TokenTree<'s>],
    table: &MacroTable<'s>,
    arena: &'s StringArena,
    ctx: &mut ExpandCtx,
) -> Vec<TokenTree<'s>> {
    let mut out = vec![];
    let mut i = 0;
    while i < trees.len() {
        // Invocation shape: `ident ! group`.
        if let (
            TokenTree::Leaf((Token::Identifier(name), name_span)),
            Some(TokenTree::Leaf((Token::Bang, _))),
            Some(TokenTree::Group {
                trees: args, close, ..
            }),
        ) = (&trees[i], trees.get(i + 1), trees.get(i + 2))
        {
            let inv_span: Span = (name_span.start..close.end).into();
            if let Some(def) = table.macros.get(*name) {
                match expand_one(file, def, args, inv_span, arena, ctx.diags) {
                    Some(rep) => out.extend(rep),
                    None => ctx.had_error = true,
                }
                ctx.changed = true;
                i += 3;
                continue;
            } else {
                ctx.diags.push(diag(
                    file,
                    inv_span,
                    format!("invocation of unknown macro `{}`", name),
                ));
                ctx.had_error = true;
                i += 3;
                continue;
            }
        }

        match &trees[i] {
            TokenTree::Group {
                delim,
                open,
                close,
                trees: sub,
            } => {
                let new_sub = expand_trees(file, sub, table, arena, ctx);
                out.push(TokenTree::Group {
                    delim: *delim,
                    open: *open,
                    close: *close,
                    trees: new_sub,
                });
            }
            leaf => out.push(leaf.clone()),
        }
        i += 1;
    }
    out
}

fn expand_one<'s>(
    file: &str,
    def: &MacroDef<'s>,
    args: &[TokenTree<'s>],
    inv_span: Span,
    arena: &'s StringArena,
    diags: &mut Vec<Diag>,
) -> Option<Vec<TokenTree<'s>>> {
    let mut ctx = MatchCtx { steps: 0 };
    for arm in &def.arms {
        let mut bindings = Bindings::new();
        let matched = match_nodes(&arm.matcher, args, 0, &mut bindings, &mut ctx);
        if ctx.exhausted() {
            diags.push(diag(file, inv_span, "macro match step budget exceeded"));
            return None;
        }
        if let Some(end) = matched {
            if end != args.len() {
                continue;
            }
            let mut tr = Transcriber {
                file,
                arena,
                inv_span,
                diags: vec![],
            };
            let mut flat = vec![];
            tr.transcribe(&arm.transcription, &bindings, &mut flat);
            if !tr.diags.is_empty() {
                diags.extend(tr.diags);
                return None;
            }
            let mut tree_diags = vec![];
            let Some(trees) = build_trees(file, flat, &mut tree_diags) else {
                diags.extend(tree_diags);
                return None;
            };
            return Some(trees);
        }
    }
    diags.push(diag(
        &def.def_file,
        def.def_span,
        "no macro arm matched the invocation",
    ));
    // Report at invocation site too so the error points at the caller's file.
    diags.push(diag(file, inv_span, "no matching macro arm"));
    None
}
