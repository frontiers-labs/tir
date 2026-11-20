use crate::{
    BuildError, GreenElement, GreenNode, GreenToken, NodeCache, NodeOrToken, TextRange, TextSize,
};
use std::{
    fmt,
    hash::{Hash, Hasher},
    ops::Range,
    sync::Arc,
};

struct Occurrence<K> {
    green: GreenNode<K>,
    parent: Option<SyntaxNode<K>>,
    root: Arc<()>,
    index: usize,
    depth: usize,
    start: TextSize,
}
impl<K> Drop for Occurrence<K> {
    fn drop(&mut self) {
        let mut parent = self.parent.take();
        while let Some(node) = parent.take() {
            if let Some(mut occurrence) = Arc::into_inner(node.0) {
                parent = occurrence.parent.take();
            }
        }
    }
}

/// An owned positioned tree. Cloning retains the same occurrence identity.
/// A retained root or descendant remains valid after this wrapper is dropped.
pub struct SyntaxTree<K> {
    root: SyntaxNode<K>,
}
impl<K> Clone for SyntaxTree<K> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
        }
    }
}
impl<K> SyntaxTree<K> {
    /// Positions a green node at byte zero with a fresh occurrence identity.
    pub fn new(green: GreenNode<K>) -> Self {
        Self {
            root: SyntaxNode::new_root(green),
        }
    }
    /// Cheaply clones the owned root handle.
    pub fn root(&self) -> SyntaxNode<K> {
        self.root.clone()
    }
}

/// An owned node occurrence with a shared parent path, without reference cycles.
/// Cloning, parent and direct sibling navigation are O(1); equality and hashing
/// are O(depth). Independently navigated handles to the same path compare equal.
pub struct SyntaxNode<K>(Arc<Occurrence<K>>);
/// An owned token occurrence. Its parent path keeps text and navigation alive.
pub struct SyntaxToken<K> {
    parent: SyntaxNode<K>,
    index: usize,
    green: GreenToken<K>,
}
/// A positioned node or token with common navigation and coordinate operations.
pub type SyntaxElement<K> = NodeOrToken<SyntaxNode<K>, SyntaxToken<K>>;
impl<K> Clone for SyntaxNode<K> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<K> Clone for SyntaxToken<K> {
    fn clone(&self) -> Self {
        Self {
            parent: self.parent.clone(),
            index: self.index,
            green: self.green.clone(),
        }
    }
}
impl<K> PartialEq for SyntaxNode<K> {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) {
            return true;
        }
        if !Arc::ptr_eq(&self.0.root, &other.0.root) || self.0.depth != other.0.depth {
            return false;
        }
        let (mut left, mut right) = (Some(self), Some(other));
        while let (Some(a), Some(b)) = (left, right) {
            if a.0.index != b.0.index {
                return false;
            }
            left = a.0.parent.as_ref();
            right = b.0.parent.as_ref();
        }
        true
    }
}
impl<K> Eq for SyntaxNode<K> {}
impl<K> Hash for SyntaxNode<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::ptr::hash(Arc::as_ptr(&self.0.root), state);
        self.0.depth.hash(state);
        let mut node = Some(self);
        while let Some(n) = node {
            n.0.index.hash(state);
            node = n.0.parent.as_ref();
        }
    }
}
impl<K> PartialEq for SyntaxToken<K> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index && self.parent == other.parent
    }
}
impl<K> Eq for SyntaxToken<K> {}
impl<K> Hash for SyntaxToken<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parent.hash(state);
        self.index.hash(state);
    }
}

impl<K> SyntaxNode<K> {
    /// Creates an owned root in O(1), without traversing descendants. Root tokens
    /// are rejected by the `GreenNode` parameter type.
    pub fn new_root(green: GreenNode<K>) -> Self {
        Self(Arc::new(Occurrence {
            green,
            parent: None,
            root: Arc::new(()),
            index: 0,
            depth: 0,
            start: TextSize::ZERO,
        }))
    }
    /// Immutable shared green node underlying this occurrence.
    pub fn green(&self) -> &GreenNode<K> {
        &self.0.green
    }
    /// Half-open UTF-8 byte range.
    pub fn text_range(&self) -> TextRange {
        TextRange::at(self.0.start, self.green().text_len())
    }
    /// Immediate parent, absent for the root.
    pub fn parent(&self) -> Option<Self> {
        self.0.parent.clone()
    }
    /// Enclosing nodes from parent toward root, excluding this node.
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + use<K> {
        let mut next = self.parent();
        std::iter::from_fn(move || {
            let node = next.take()?;
            next = node.parent();
            Some(node)
        })
    }
    /// Owned root of this occurrence's tree. O(depth).
    pub fn root(&self) -> Self {
        self.ancestors().last().unwrap_or_else(|| self.clone())
    }
    fn child(&self, index: usize) -> SyntaxElement<K> {
        match &self.green().children()[index] {
            NodeOrToken::Node(green) => NodeOrToken::Node(Self(Arc::new(Occurrence {
                green: green.clone(),
                parent: Some(self.clone()),
                root: self.0.root.clone(),
                index,
                depth: self.0.depth + 1,
                start: TextSize::new(self.0.start.get() + self.green().offset(index).get()),
            }))),
            NodeOrToken::Token(green) => NodeOrToken::Token(SyntaxToken {
                parent: self.clone(),
                index,
                green: green.clone(),
            }),
        }
    }
    /// Direct children in source order. Each yielded node allocates one owned
    /// path link; tokens retain their existing parent. Descendants are untouched.
    pub fn children_with_tokens(&self) -> SyntaxElementChildren<K> {
        SyntaxElementChildren {
            parent: self.clone(),
            range: 0..self.green().children().len(),
        }
    }
    /// Direct grammar children in source order.
    pub fn children(&self) -> impl DoubleEndedIterator<Item = Self> + use<K> {
        self.children_with_tokens().filter_map(|e| match e {
            NodeOrToken::Node(n) => Some(n),
            NodeOrToken::Token(_) => None,
        })
    }
    /// First direct child, including tokens.
    pub fn first_child_or_token(&self) -> Option<SyntaxElement<K>> {
        self.children_with_tokens().next()
    }
    /// Last direct child, including tokens.
    pub fn last_child_or_token(&self) -> Option<SyntaxElement<K>> {
        self.children_with_tokens().next_back()
    }
    /// First direct grammar child.
    pub fn first_child(&self) -> Option<Self> {
        self.children().next()
    }
    /// Last direct grammar child.
    pub fn last_child(&self) -> Option<Self> {
        self.children().next_back()
    }
    /// Next direct sibling, including tokens. O(1).
    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement<K>> {
        let parent = self.0.parent.as_ref()?;
        (self.0.index + 1 < parent.green().children().len()).then(|| parent.child(self.0.index + 1))
    }
    /// Previous direct sibling, including tokens. O(1).
    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement<K>> {
        let parent = self.0.parent.as_ref()?;
        (self.0.index > 0).then(|| parent.child(self.0.index - 1))
    }
    /// Next grammar sibling, skipping tokens.
    pub fn next_sibling(&self) -> Option<Self> {
        node_sibling(self.next_sibling_or_token(), true)
    }
    /// Previous grammar sibling, skipping tokens.
    pub fn prev_sibling(&self) -> Option<Self> {
        node_sibling(self.prev_sibling_or_token(), false)
    }
    /// Iterative enter/leave traversal including this node and every token,
    /// bounded by this occurrence even when it has a parent.
    pub fn preorder_with_tokens(&self) -> Preorder<K> {
        Preorder {
            root: self.clone(),
            next: Some(WalkEvent::Enter(NodeOrToken::Node(self.clone()))),
        }
    }
    /// Iterative enter/leave traversal of grammar nodes, including this node.
    pub fn preorder(&self) -> impl Iterator<Item = WalkEvent<Self>> + use<K> {
        self.preorder_with_tokens().filter_map(|e| match e {
            WalkEvent::Enter(NodeOrToken::Node(n)) => Some(WalkEvent::Enter(n)),
            WalkEvent::Leave(NodeOrToken::Node(n)) => Some(WalkEvent::Leave(n)),
            _ => None,
        })
    }
    /// Grammar descendants in source preorder, excluding this node.
    pub fn descendants(&self) -> impl Iterator<Item = Self> + use<K> {
        self.preorder()
            .filter_map(|e| match e {
                WalkEvent::Enter(n) => Some(n),
                WalkEvent::Leave(_) => None,
            })
            .skip(1)
    }
    /// All descendants in source preorder, excluding this node.
    pub fn descendants_with_tokens(&self) -> impl Iterator<Item = SyntaxElement<K>> + use<K> {
        self.preorder_with_tokens()
            .filter_map(|e| match e {
                WalkEvent::Enter(n) => Some(n),
                WalkEvent::Leave(_) => None,
            })
            .skip(1)
    }
    /// First token in this subtree, including empty tokens. O(visited elements).
    pub fn first_token(&self) -> Option<SyntaxToken<K>> {
        self.descendants_with_tokens().find_map(|e| match e {
            NodeOrToken::Token(t) => Some(t),
            _ => None,
        })
    }
    /// Last token in this subtree, including empty tokens. Uses an iterative
    /// reverse walk, skipping empty grammar nodes without leaving the subtree.
    pub fn last_token(&self) -> Option<SyntaxToken<K>> {
        let mut stack = vec![self.children_with_tokens()];
        while let Some(children) = stack.last_mut() {
            match children.next_back() {
                Some(NodeOrToken::Token(t)) => return Some(t),
                Some(NodeOrToken::Node(n)) => stack.push(n.children_with_tokens()),
                None => {
                    stack.pop();
                }
            }
        }
        None
    }
}
impl<K: Copy> SyntaxNode<K> {
    /// Language-supplied kind.
    pub fn kind(&self) -> K {
        self.green().kind()
    }
}
fn node_sibling<K>(mut next: Option<SyntaxElement<K>>, forward: bool) -> Option<SyntaxNode<K>> {
    while let Some(e) = next {
        match e {
            NodeOrToken::Node(n) => return Some(n),
            NodeOrToken::Token(t) => {
                next = if forward {
                    t.next_sibling_or_token()
                } else {
                    t.prev_sibling_or_token()
                }
            }
        }
    }
    None
}
impl<K> SyntaxToken<K> {
    /// Immutable shared green token.
    pub fn green(&self) -> &GreenToken<K> {
        &self.green
    }
    /// Exact UTF-8 bytes, including trivia.
    pub fn text(&self) -> &str {
        self.green.text()
    }
    /// Half-open UTF-8 byte range.
    pub fn text_range(&self) -> TextRange {
        TextRange::at(
            TextSize::new(self.parent.0.start.get() + self.parent.green().offset(self.index).get()),
            self.green.text_len(),
        )
    }
    /// Immediate enclosing node. Tokens always have a parent.
    pub fn parent(&self) -> SyntaxNode<K> {
        self.parent.clone()
    }
    /// Enclosing nodes, from immediate parent through root.
    pub fn ancestors(&self) -> impl Iterator<Item = SyntaxNode<K>> + use<K> {
        std::iter::once(self.parent()).chain(self.parent.ancestors())
    }
    /// Next direct sibling, including tokens.
    pub fn next_sibling_or_token(&self) -> Option<SyntaxElement<K>> {
        (self.index + 1 < self.parent.green().children().len())
            .then(|| self.parent.child(self.index + 1))
    }
    /// Previous direct sibling, including tokens.
    pub fn prev_sibling_or_token(&self) -> Option<SyntaxElement<K>> {
        (self.index > 0).then(|| self.parent.child(self.index - 1))
    }
    /// Next token in source order, crossing enclosing nodes as needed.
    pub fn next_token(&self) -> Option<Self> {
        adjacent_token(NodeOrToken::Token(self.clone()), true)
    }
    /// Previous token in source order, crossing enclosing nodes as needed.
    pub fn prev_token(&self) -> Option<Self> {
        adjacent_token(NodeOrToken::Token(self.clone()), false)
    }
}
impl<K: Copy> SyntaxToken<K> {
    /// Language-supplied kind.
    pub fn kind(&self) -> K {
        self.green.kind()
    }
}
fn adjacent_token<K>(mut element: SyntaxElement<K>, forward: bool) -> Option<SyntaxToken<K>> {
    loop {
        let sibling = if forward {
            element.next_sibling_or_token()
        } else {
            element.prev_sibling_or_token()
        };
        if let Some(sibling) = sibling {
            match &sibling {
                NodeOrToken::Token(t) => return Some(t.clone()),
                NodeOrToken::Node(n) => {
                    if let Some(t) = if forward {
                        n.first_token()
                    } else {
                        n.last_token()
                    } {
                        return Some(t);
                    }
                }
            }
            element = sibling;
        } else {
            element = NodeOrToken::Node(element.parent()?);
        }
    }
}
impl<K> SyntaxElement<K> {
    /// Half-open UTF-8 byte range.
    pub fn text_range(&self) -> TextRange {
        match self {
            Self::Node(n) => n.text_range(),
            Self::Token(t) => t.text_range(),
        }
    }
    /// Parent node, absent only for a root node.
    pub fn parent(&self) -> Option<SyntaxNode<K>> {
        match self {
            Self::Node(n) => n.parent(),
            Self::Token(t) => Some(t.parent()),
        }
    }
    /// Shared position-independent green element.
    pub fn green(&self) -> GreenElement<K> {
        match self {
            Self::Node(n) => NodeOrToken::Node(n.green().clone()),
            Self::Token(t) => NodeOrToken::Token(t.green().clone()),
        }
    }
    /// Next direct sibling, including tokens.
    pub fn next_sibling_or_token(&self) -> Option<Self> {
        match self {
            Self::Node(n) => n.next_sibling_or_token(),
            Self::Token(t) => t.next_sibling_or_token(),
        }
    }
    /// Previous direct sibling, including tokens.
    pub fn prev_sibling_or_token(&self) -> Option<Self> {
        match self {
            Self::Node(n) => n.prev_sibling_or_token(),
            Self::Token(t) => t.prev_sibling_or_token(),
        }
    }
}
impl<K: Copy> SyntaxElement<K> {
    /// Language-supplied kind.
    pub fn kind(&self) -> K {
        match self {
            Self::Node(n) => n.kind(),
            Self::Token(t) => t.kind(),
        }
    }
}

/// Owned direct-child iterator. Token iteration allocates no path links.
pub struct SyntaxElementChildren<K> {
    parent: SyntaxNode<K>,
    range: Range<usize>,
}
impl<K> Iterator for SyntaxElementChildren<K> {
    type Item = SyntaxElement<K>;
    fn next(&mut self) -> Option<Self::Item> {
        self.range.next().map(|i| self.parent.child(i))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.range.size_hint()
    }
}
impl<K> DoubleEndedIterator for SyntaxElementChildren<K> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.range.next_back().map(|i| self.parent.child(i))
    }
}
impl<K> ExactSizeIterator for SyntaxElementChildren<K> {}
impl<K> std::iter::FusedIterator for SyntaxElementChildren<K> {}

/// Entry into or departure from an occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WalkEvent<T> {
    /// Before an element's children.
    Enter(T),
    /// After an element's children.
    Leave(T),
}
/// Owned iterative traversal bounded by its starting occurrence.
pub struct Preorder<K> {
    root: SyntaxNode<K>,
    next: Option<WalkEvent<SyntaxElement<K>>>,
}
impl<K> Iterator for Preorder<K> {
    type Item = WalkEvent<SyntaxElement<K>>;
    fn next(&mut self) -> Option<Self::Item> {
        let event = self.next.take()?;
        self.next = match &event {
            WalkEvent::Enter(e) => {
                let child = match e {
                    NodeOrToken::Node(n) => n.first_child_or_token(),
                    NodeOrToken::Token(_) => None,
                };
                Some(child.map_or_else(|| WalkEvent::Leave(e.clone()), WalkEvent::Enter))
            }
            WalkEvent::Leave(NodeOrToken::Node(n)) if Arc::ptr_eq(&n.0, &self.root.0) => None,
            WalkEvent::Leave(e) => e
                .next_sibling_or_token()
                .map(WalkEvent::Enter)
                .or_else(|| e.parent().map(|p| WalkEvent::Leave(NodeOrToken::Node(p)))),
        };
        Some(event)
    }
}
impl<K> std::iter::FusedIterator for Preorder<K> {}
impl<K> fmt::Display for SyntaxNode<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.green().fmt(f)
    }
}
impl<K> fmt::Display for SyntaxToken<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}
impl<K> fmt::Display for SyntaxTree<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.root.fmt(f)
    }
}
impl<K> fmt::Display for SyntaxElement<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Node(n) => n.fmt(f),
            Self::Token(t) => t.fmt(f),
        }
    }
}
impl<K: Copy + fmt::Debug> fmt::Debug for SyntaxNode<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxNode")
            .field("kind", &self.kind())
            .field("range", &self.text_range())
            .finish()
    }
}
impl<K: Copy + fmt::Debug> fmt::Debug for SyntaxToken<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxToken")
            .field("kind", &self.kind())
            .field("range", &self.text_range())
            .finish()
    }
}

/// A language's typed wrapper around an owned grammar node.
pub trait AstNode: Sized {
    /// Language-supplied syntax kind type.
    type Kind: Copy + Eq + Hash;
    /// Whether this wrapper accepts the kind.
    fn can_cast(kind: Self::Kind) -> bool;
    /// Takes ownership of a matching node, returning none for other kinds.
    fn cast(syntax: SyntaxNode<Self::Kind>) -> Option<Self>;
    /// Underlying positioned node.
    fn syntax(&self) -> &SyntaxNode<Self::Kind>;
}

impl<K: Copy + Eq + Hash> SyntaxElement<K> {
    /// Replaces only this occurrence and returns a new positioned ROOT.
    /// Rebuilds direct child arrays along the ancestor path, retaining all other
    /// green allocations. Neither this tree nor its handles are mutated. Cost is
    /// the sum of ancestor child counts, plus cache collision comparisons.
    /// A token cannot replace a root node. Cross-cache replacements are allowed.
    pub fn replace_with(
        &self,
        replacement: GreenElement<K>,
        cache: &mut NodeCache<K>,
    ) -> Result<SyntaxNode<K>, BuildError> {
        let mut current = self.clone();
        let mut replacement = replacement;
        while let Some(parent) = current.parent() {
            let index = match &current {
                Self::Node(n) => n.0.index,
                Self::Token(t) => t.index,
            };
            let children = parent
                .green()
                .children()
                .iter()
                .enumerate()
                .map(|(i, child)| {
                    if i == index {
                        replacement.clone()
                    } else {
                        child.clone()
                    }
                });
            replacement = NodeOrToken::Node(cache.node(parent.kind(), children)?);
            current = Self::Node(parent);
        }
        match replacement {
            NodeOrToken::Node(root) => Ok(SyntaxNode::new_root(root)),
            NodeOrToken::Token(_) => Err(BuildError::InvalidRoot),
        }
    }
}
impl<K: Copy + Eq + Hash> SyntaxNode<K> {
    /// Persistently replaces this node occurrence, returning the new ROOT.
    pub fn replace_with(
        &self,
        replacement: GreenNode<K>,
        cache: &mut NodeCache<K>,
    ) -> Result<Self, BuildError> {
        NodeOrToken::Node(self.clone()).replace_with(NodeOrToken::Node(replacement), cache)
    }
    /// Inserts, deletes or reorders a contiguous range of direct children.
    /// Returns a new ROOT, copying only this child array and ancestor arrays.
    /// Invalid ranges and byte-length overflow leave the original tree intact.
    pub fn splice_children(
        &self,
        range: Range<usize>,
        replacement: impl IntoIterator<Item = GreenElement<K>>,
        cache: &mut NodeCache<K>,
    ) -> Result<Self, BuildError> {
        let children = self.green().children();
        if range.start > range.end || range.end > children.len() {
            return Err(BuildError::InvalidChildRange);
        }
        let children = children[..range.start]
            .iter()
            .cloned()
            .chain(replacement)
            .chain(children[range.end..].iter().cloned());
        let node = cache.node(self.kind(), children)?;
        self.replace_with(node, cache)
    }
}
impl<K: Copy + Eq + Hash> SyntaxToken<K> {
    /// Persistently replaces only this token occurrence, returning the new ROOT.
    /// Changed token widths recompute ancestor lengths and later byte positions.
    pub fn replace_with(
        &self,
        replacement: GreenToken<K>,
        cache: &mut NodeCache<K>,
    ) -> Result<SyntaxNode<K>, BuildError> {
        NodeOrToken::Token(self.clone()).replace_with(NodeOrToken::Token(replacement), cache)
    }
}

/// Tokens adjacent to a byte offset. Zero-width tokens are excluded, so an
/// arbitrary run of empty tokens never hides either neighboring nonempty token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenAtOffset<T> {
    /// Outside the subtree or no nonempty tokens.
    None,
    /// Inside a token, or the sole neighbor at subtree start or EOF.
    Single(T),
    /// The token ending at the offset and the token beginning there, in order.
    Between(T, T),
}
impl<T> TokenAtOffset<T> {
    /// Chooses the token to the left when a boundary is ambiguous.
    pub fn left_biased(self) -> Option<T> {
        match self {
            Self::None => None,
            Self::Single(t) | Self::Between(t, _) => Some(t),
        }
    }
    /// Chooses the token to the right when a boundary is ambiguous.
    pub fn right_biased(self) -> Option<T> {
        match self {
            Self::None => None,
            Self::Single(t) | Self::Between(_, t) => Some(t),
        }
    }
}
impl<K> SyntaxNode<K> {
    fn token_on_side(&self, offset: TextSize, left: bool) -> Option<SyntaxToken<K>> {
        let mut node = self.clone();
        loop {
            let mut selected = None;
            for (index, child) in node.green().children().iter().enumerate() {
                let start = node.0.start.get() + node.green().offset(index).get();
                let end = start + child.text_len().get();
                let contains = if left {
                    start < offset.get() && offset.get() <= end
                } else {
                    start <= offset.get() && offset.get() < end
                };
                if contains {
                    selected = Some(node.child(index));
                    break;
                }
            }
            match selected? {
                NodeOrToken::Token(t) => return Some(t),
                NodeOrToken::Node(n) => node = n,
            }
        }
    }
    /// Looks up nonempty tokens at an absolute UTF-8 byte offset. Offsets need
    /// not be character boundaries. At EOF returns the left token; at start the
    /// right token; outside this subtree or in a zero-byte tree returns None.
    /// Scans direct siblings along at most two paths, never all descendants.
    pub fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken<K>> {
        let range = self.text_range();
        if offset < range.start() || offset > range.end() {
            return TokenAtOffset::None;
        }
        let left = self.token_on_side(offset, true);
        let right = self.token_on_side(offset, false);
        match (left, right) {
            (Some(a), Some(b)) if a != b => TokenAtOffset::Between(a, b),
            (Some(t), _) | (_, Some(t)) => TokenAtOffset::Single(t),
            _ => TokenAtOffset::None,
        }
    }
    /// Returns the deepest element containing an absolute byte range, or None
    /// outside this subtree. Empty ranges, including EOF, choose the first
    /// containing child in source order, including zero-width children. A node
    /// with no containing child covers the range itself. Descent is iterative.
    pub fn covering_element(&self, range: TextRange) -> Option<SyntaxElement<K>> {
        if range.start() < self.text_range().start() || range.end() > self.text_range().end() {
            return None;
        }
        let mut element = NodeOrToken::Node(self.clone());
        loop {
            let next = match &element {
                NodeOrToken::Token(_) => return Some(element),
                NodeOrToken::Node(n) => n.children_with_tokens().find(|child| {
                    let child = child.text_range();
                    child.start() <= range.start() && range.end() <= child.end()
                }),
            };
            match next {
                Some(child) => element = child,
                None => return Some(element),
            }
        }
    }
}
