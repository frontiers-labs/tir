//! Lossless, persistent concrete syntax trees with shared green storage.
//!
//! Build with `GreenNodeBuilder::with_cache`, close the root, and call
//! `GreenNodeBuilder::finish_green`. `SyntaxNode::new_root` positions that green
//! root without traversing descendants. Owned handles retain their tree, and
//! replacement/splicing return a new root while preserving the original.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod builder;
mod green;
mod syntax;
mod text;

pub use builder::{BuildError, Checkpoint, GreenNodeBuilder};
pub use green::{GreenElement, GreenNode, GreenToken, NodeCache};
pub use syntax::*;
pub use text::{TextRange, TextSize};

/// Either a grammar node or a token, including trivia.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeOrToken<N, T> {
    /// A grammar node.
    Node(N),
    /// A losslessly preserved token.
    Token(T),
}
