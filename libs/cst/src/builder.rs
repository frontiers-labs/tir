use crate::{GreenElement, GreenNode, NodeCache, NodeOrToken, SyntaxTree, TextSize};
use std::{fmt, hash::Hash, rc::Rc};

/// A construction or coordinate error. Non-consuming operations preserve the
/// builder on error. Consuming finish operations discard it even on error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// A coordinate, generation, or storage limit was exceeded.
    Overflow,
    /// A range ended before it started.
    InvalidRange,
    /// The operation requires an open node.
    NoOpenNode,
    /// Finish was called with nodes still open.
    UnclosedNodes,
    /// Construction did not produce exactly one node root.
    InvalidRoot,
    /// A child splice range is outside the direct children.
    InvalidChildRange,
    /// The checkpoint belongs to another builder, a closed frame, or a removed boundary.
    InvalidCheckpoint,
}
impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Overflow => "CST coordinate or capacity overflow",
            Self::InvalidRange => "range start exceeds end",
            Self::NoOpenNode => "no open node",
            Self::UnclosedNodes => "unclosed nodes",
            Self::InvalidRoot => "expected exactly one complete node root",
            Self::InvalidChildRange => "invalid direct-child range",
            Self::InvalidCheckpoint => "invalid or stale checkpoint",
        })
    }
}
impl std::error::Error for BuildError {}

/// An opaque position in the current frame's completed children.
/// Wrapping preserves boundaries at or before the wrapping start. Boundaries
/// strictly after it are permanently invalidated. Closing the frame invalidates
/// all its checkpoints.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    owner: Rc<()>,
    frame: u64,
    boundary: u64,
    index: usize,
}
struct Frame<K> {
    kind: K,
    generation: u64,
    boundaries: Vec<u64>,
    start: usize,
    len: TextSize,
}

/// Stack-based construction of one lossless syntax tree.
/// Tokens require an open node. Non-consuming operations leave the builder
/// usable and unchanged on error. `finish` and `finish_green` consume it on
/// success or error.
pub struct GreenNodeBuilder<'cache, K> {
    cache: Cache<'cache, K>,
    owner: Rc<()>,
    frames: Vec<Frame<K>>,
    completed: Vec<GreenElement<K>>,
    generation: u64,
}
enum Cache<'a, K> {
    Owned(NodeCache<K>),
    Borrowed(&'a mut NodeCache<K>),
}
impl<K> Cache<'_, K> {
    fn get(&mut self) -> &mut NodeCache<K> {
        match self {
            Self::Owned(c) => c,
            Self::Borrowed(c) => c,
        }
    }
}

impl<K: Copy + Eq + Hash> Default for GreenNodeBuilder<'_, K> {
    fn default() -> Self {
        Self::new()
    }
}
impl<'cache, K: Copy + Eq + Hash> GreenNodeBuilder<'cache, K> {
    /// Creates an empty builder with a private sharing cache.
    pub fn new() -> Self {
        Self {
            cache: Cache::Owned(NodeCache::new()),
            owner: Rc::new(()),
            frames: Vec::new(),
            completed: Vec::new(),
            generation: 0,
        }
    }
    /// Borrows a reusable cache for this build. Finished roots do not borrow it.
    pub fn with_cache(cache: &'cache mut NodeCache<K>) -> Self {
        Self {
            cache: Cache::Borrowed(cache),
            ..Self::new()
        }
    }
    /// Opens a grammar node. Only one root is permitted.
    pub fn start_node(&mut self, kind: K) -> Result<(), BuildError> {
        if self.frames.is_empty() && !self.completed.is_empty() {
            return Err(BuildError::InvalidRoot);
        }
        let generation = self.generation.checked_add(1).ok_or(BuildError::Overflow)?;
        self.frames.push(Frame {
            kind,
            generation,
            boundaries: vec![0],
            start: self.completed.len(),
            len: TextSize::ZERO,
        });
        self.generation = generation;
        Ok(())
    }
    /// Appends a token verbatim, including whitespace and comments.
    pub fn token(&mut self, kind: K, text: &str) -> Result<(), BuildError> {
        let frame = self.frames.last_mut().ok_or(BuildError::NoOpenNode)?;
        let width = TextSize::from_usize(text.len())?;
        let len = frame.len.checked_add(width)?;
        let generation = self.generation.checked_add(1).ok_or(BuildError::Overflow)?;
        let token = self.cache.get().token(kind, text)?;
        self.completed.push(NodeOrToken::Token(token));
        frame.len = len;
        frame.boundaries.push(generation);
        self.generation = generation;
        Ok(())
    }
    /// Closes the current node and appends it to its parent's children.
    pub fn finish_node(&mut self) -> Result<(), BuildError> {
        let frame = self.frames.last().ok_or(BuildError::NoOpenNode)?;
        let parent_len = if self.frames.len() > 1 {
            self.frames[self.frames.len() - 2]
                .len
                .checked_add(frame.len)?
        } else {
            frame.len
        };
        let generation = self.generation.checked_add(1).ok_or(BuildError::Overflow)?;
        let node = self
            .cache
            .get()
            .node(frame.kind, self.completed[frame.start..].iter().cloned())?;
        self.completed.truncate(frame.start);
        self.frames.pop();
        self.completed.push(NodeOrToken::Node(node));
        if let Some(parent) = self.frames.last_mut() {
            parent.len = parent_len;
            parent.boundaries.push(generation);
        }
        self.generation = generation;
        Ok(())
    }
    /// Records a boundary among the current node's completed children.
    pub fn checkpoint(&self) -> Result<Checkpoint, BuildError> {
        let frame = self.frames.last().ok_or(BuildError::NoOpenNode)?;
        Ok(Checkpoint {
            owner: self.owner.clone(),
            frame: frame.generation,
            boundary: frame.boundaries[self.completed.len() - frame.start],
            index: self.completed.len(),
        })
    }
    /// Opens a node around completed children since the checkpoint.
    /// Preserves enclosing checkpoints at or before this boundary, including
    /// this checkpoint. Permanently invalidates boundaries strictly after it.
    pub fn start_node_at(&mut self, checkpoint: Checkpoint, kind: K) -> Result<(), BuildError> {
        let frame = self.frames.last().ok_or(BuildError::InvalidCheckpoint)?;
        if !Rc::ptr_eq(&self.owner, &checkpoint.owner)
            || frame.generation != checkpoint.frame
            || checkpoint.index < frame.start
            || checkpoint.index > self.completed.len()
            || frame.boundaries.get(checkpoint.index - frame.start) != Some(&checkpoint.boundary)
        {
            return Err(BuildError::InvalidCheckpoint);
        }
        let generation = self.generation.checked_add(1).ok_or(BuildError::Overflow)?;
        let boundary_index = checkpoint.index - frame.start;
        let boundaries = frame.boundaries[boundary_index..].to_vec();
        let mut len = TextSize::ZERO;
        for child in &self.completed[checkpoint.index..] {
            len = len.checked_add(child.text_len())?;
        }
        if let Some(frame) = self.frames.last_mut() {
            frame.boundaries.truncate(boundary_index + 1);
            frame.len = TextSize::new(frame.len.get() - len.get());
        }
        self.frames.push(Frame {
            kind,
            generation,
            boundaries,
            start: checkpoint.index,
            len,
        });
        self.generation = generation;
        Ok(())
    }
    /// Appends a self-contained subtree or token from any cache.
    pub fn green_element(&mut self, element: GreenElement<K>) -> Result<(), BuildError> {
        let frame = self.frames.last_mut().ok_or(BuildError::NoOpenNode)?;
        let len = frame.len.checked_add(element.text_len())?;
        let generation = self.generation.checked_add(1).ok_or(BuildError::Overflow)?;
        self.completed.push(element);
        frame.boundaries.push(generation);
        frame.len = len;
        self.generation = generation;
        Ok(())
    }
    /// Finishes a green root. Convert it with `SyntaxNode::new_root` or
    /// `SyntaxTree::new`. The builder is consumed even on error.
    pub fn finish_green(mut self) -> Result<GreenNode<K>, BuildError> {
        if !self.frames.is_empty() {
            return Err(BuildError::UnclosedNodes);
        }
        if self.completed.len() != 1 {
            return Err(BuildError::InvalidRoot);
        }
        match self.completed.pop() {
            Some(NodeOrToken::Node(node)) => Ok(node),
            _ => Err(BuildError::InvalidRoot),
        }
    }
    /// Finishes an owned positioned tree.
    /// Consumes the builder even if construction is incomplete or freezing fails.
    pub fn finish(self) -> Result<SyntaxTree<K>, BuildError> {
        Ok(SyntaxTree::new(self.finish_green()?))
    }
}
