//! Lossless syntax node builders used by `parser::parse_lossless`.

use std::{cell::RefCell, ops::Range};

use tir_cst::{BuildError, GreenNode};

use crate::syntax::{Fragment, SyntaxCache, SyntaxInput, SyntaxKind};

pub(crate) struct Node<T> {
    pub(crate) value: T,
    pub(crate) range: Range<usize>,
}

pub(crate) type ExprNode = Node<Result<GreenNode<SyntaxKind>, BuildError>>;
pub(crate) type PatNode = ExprNode;
pub(crate) type DeclNode = ExprNode;

pub(crate) struct Context<'a> {
    pub(crate) input: SyntaxInput<'a>,
    pub(crate) cache: RefCell<&'a mut SyntaxCache>,
}

pub(crate) fn wrap(
    ctx: &Context<'_>,
    kind: SyntaxKind,
    range: Range<usize>,
    children: Vec<ExprNode>,
) -> ExprNode {
    let frags: Result<Vec<Fragment>, BuildError> = children
        .into_iter()
        .map(|node| {
            node.value.map(|green| Fragment {
                green,
                range: node.range,
            })
        })
        .collect();
    let value = frags.and_then(|children| {
        ctx.input
            .fragment(&mut ctx.cache.borrow_mut(), kind, range.clone(), children)
            .map(|fragment| fragment.green)
    });
    Node { value, range }
}
