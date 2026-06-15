//! A tiny C abstract syntax tree — only the constructs needed to drive a
//! simple integer function (parameters, local `int` variables, arithmetic and
//! `return`) down to IR. There are no types beyond `int`/`void`, no control
//! flow, and no pointers at the source level.
//!
//! The tree is stored in core's [`PostOrderDag`], the same cache-friendly,
//! post-order layout the semantic-expression graph uses: node *kinds* live in a
//! flat vector while the variable-sized payload (names, literals, types) sits in
//! a sparse side table keyed by node id. Children always precede their parent,
//! so the root is the last node.

use tir::graph::{Dag, NodeId, PostOrderDag};

pub type Ast = PostOrderDag<AstKind, AstLeaf>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CType {
    Int,
    Void,
}

/// The structural kind of an AST node. How its children are interpreted depends
/// solely on the kind; payload data lives in the matching [`AstLeaf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AstKind {
    /// Children: the translation unit's functions.
    TranslationUnit,
    /// Children: parameters, then body statements.
    Function,
    Param,
    /// Child: the optional initializer expression.
    Decl,
    /// Child: the assigned value expression.
    Assign,
    /// Child: the optional returned expression.
    Return,
    /// Children: left-hand side, right-hand side.
    Add,
    Sub,
    Mul,
    Var,
    Int,
}

/// Payload for the nodes that carry one. Indexed by node id through
/// [`Dag::get_leaf_data`]; structural nodes ([`AstKind::TranslationUnit`],
/// [`AstKind::Return`], the binary operators) have none.
#[derive(Debug, Clone, PartialEq)]
pub enum AstLeaf {
    Function { name: String, ret: CType },
    Param { name: String, ty: CType },
    Decl { name: String, ty: CType },
    Assign(String),
    Var(String),
    Int(i64),
}

/// Render the tree as an indented outline, used by the `--stage ast` output.
pub fn render(ast: &Ast) -> String {
    let mut out = String::new();
    if let Some(root) = ast.root() {
        render_node(ast, root, 0, &mut out);
    }
    out
}

fn render_node(ast: &Ast, id: NodeId, depth: usize, out: &mut String) {
    use std::fmt::Write;

    let label = match ast.get_node(id) {
        AstKind::TranslationUnit => "TranslationUnit".to_string(),
        AstKind::Function => match ast.get_leaf_data(id) {
            Some(AstLeaf::Function { name, ret }) => format!("Function {name:?} -> {ret:?}"),
            _ => unreachable!(),
        },
        AstKind::Param => match ast.get_leaf_data(id) {
            Some(AstLeaf::Param { name, ty }) => format!("Param {name:?}: {ty:?}"),
            _ => unreachable!(),
        },
        AstKind::Decl => match ast.get_leaf_data(id) {
            Some(AstLeaf::Decl { name, ty }) => format!("Decl {name:?}: {ty:?}"),
            _ => unreachable!(),
        },
        AstKind::Assign => match ast.get_leaf_data(id) {
            Some(AstLeaf::Assign(name)) => format!("Assign {name:?}"),
            _ => unreachable!(),
        },
        AstKind::Return => "Return".to_string(),
        AstKind::Add => "Add".to_string(),
        AstKind::Sub => "Sub".to_string(),
        AstKind::Mul => "Mul".to_string(),
        AstKind::Var => match ast.get_leaf_data(id) {
            Some(AstLeaf::Var(name)) => format!("Var {name:?}"),
            _ => unreachable!(),
        },
        AstKind::Int => match ast.get_leaf_data(id) {
            Some(AstLeaf::Int(value)) => format!("Int {value}"),
            _ => unreachable!(),
        },
    };

    writeln!(out, "{:indent$}{label}", "", indent = depth * 2).unwrap();
    for child in ast.children(id) {
        render_node(ast, child, depth + 1, out);
    }
}
