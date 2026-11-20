use crate::{BuildError, NodeOrToken, TextSize};
use std::{
    collections::HashMap,
    fmt,
    hash::{BuildHasher, Hash, Hasher},
    sync::{Arc, Weak},
};
use tir_adt::FxBuildHasher;

struct NodeData<K> {
    kind: K,
    len: TextSize,
    children: Vec<GreenElement<K>>,
    offsets: Vec<TextSize>,
}
struct TokenData<K> {
    kind: K,
    len: TextSize,
    text: Box<str>,
}

/// An immutable, position-independent node. Cloning shares its allocation.
/// Equality and hashing compare allocations; use `text` for source text.
pub struct GreenNode<K>(Option<Arc<NodeData<K>>>);
/// An immutable token owning its exact UTF-8 text. Equality compares allocations.
pub struct GreenToken<K>(Arc<TokenData<K>>);
/// A self-contained green child, safe to combine with children from any cache.
pub type GreenElement<K> = NodeOrToken<GreenNode<K>, GreenToken<K>>;

impl<K> Clone for GreenNode<K> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<K> Clone for GreenToken<K> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<K> GreenNode<K> {
    fn data(&self) -> &NodeData<K> {
        self.arc()
    }
    /// Whether both handles refer to the same allocation.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(self.arc(), other.arc())
    }
    fn arc(&self) -> &Arc<NodeData<K>> {
        match &self.0 {
            Some(data) => data,
            None => unreachable!("live green node"),
        }
    }
    /// Direct children in source order. This does not traverse descendants.
    pub fn children(&self) -> &[GreenElement<K>] {
        &self.data().children
    }
    /// Total UTF-8 byte length.
    pub fn text_len(&self) -> TextSize {
        self.data().len
    }
    pub(crate) fn offset(&self, index: usize) -> TextSize {
        self.data().offsets[index]
    }
    /// Collects exact source text. Display streams it without this allocation.
    pub fn text(&self) -> String {
        self.to_string()
    }
}
impl<K: Copy> GreenNode<K> {
    /// Constructs a node from existing children without copying their subtrees.
    pub fn new(
        kind: K,
        children: impl IntoIterator<Item = GreenElement<K>>,
    ) -> Result<Self, BuildError>
    where
        K: Eq + Hash,
    {
        NodeCache::new().node(kind, children)
    }
    /// Language-supplied syntax kind.
    pub fn kind(&self) -> K {
        self.data().kind
    }
}
impl<K> GreenToken<K> {
    /// Whether both handles refer to the same allocation.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// Exact UTF-8 token text, including trivia.
    pub fn text(&self) -> &str {
        &self.0.text
    }
    /// UTF-8 byte length.
    pub fn text_len(&self) -> TextSize {
        self.0.len
    }
}
impl<K: Copy + Eq + Hash> GreenToken<K> {
    /// Constructs an independently owned token. Use a cache to share allocations.
    pub fn new(kind: K, text: &str) -> Result<Self, BuildError> {
        NodeCache::new().token(kind, text)
    }
}
impl<K: Copy> GreenToken<K> {
    /// Language-supplied syntax kind.
    pub fn kind(&self) -> K {
        self.0.kind
    }
}
impl<K> Drop for GreenNode<K> {
    fn drop(&mut self) {
        let Some(arc) = self.0.take() else {
            return;
        };
        let Some(data) = Arc::into_inner(arc) else {
            return;
        };
        let mut pending = vec![data];
        while let Some(mut data) = pending.pop() {
            // into_inner assigns destruction to one owner even when clones are
            // dropped concurrently; descendants never drop through the Rust stack.
            for child in data.children.drain(..) {
                if let NodeOrToken::Node(mut node) = child
                    && let Some(arc) = node.0.take()
                    && let Some(data) = Arc::into_inner(arc)
                {
                    pending.push(data);
                }
            }
        }
    }
}
macro_rules! green_identity {
    ($ty:ident, $arc:expr) => {
        impl<K> PartialEq for $ty<K> {
            fn eq(&self, other: &Self) -> bool {
                self.ptr_eq(other)
            }
        }
        impl<K> Eq for $ty<K> {}
        impl<K> Hash for $ty<K> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                std::ptr::hash(($arc)(self), state);
            }
        }
    };
}
green_identity!(GreenNode, |n: &GreenNode<K>| Arc::as_ptr(n.arc()));
green_identity!(GreenToken, |t: &GreenToken<K>| Arc::as_ptr(&t.0));
impl<K: Copy + fmt::Debug> fmt::Debug for GreenNode<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GreenNode")
            .field("kind", &self.kind())
            .field("len", &self.text_len())
            .field("children", &self.children().len())
            .finish()
    }
}
impl<K: Copy + fmt::Debug> fmt::Debug for GreenToken<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GreenToken")
            .field("kind", &self.kind())
            .field("text", &self.text())
            .finish()
    }
}
impl<K> GreenElement<K> {
    /// Direct children in source order; tokens have no children.
    pub fn children(&self) -> &[Self] {
        match self {
            Self::Node(n) => n.children(),
            Self::Token(_) => &[],
        }
    }
    /// UTF-8 byte length.
    pub fn text_len(&self) -> TextSize {
        match self {
            Self::Node(n) => n.text_len(),
            Self::Token(t) => t.text_len(),
        }
    }
    /// Allocation identity, distinct from structural equality.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        self == other
    }
    /// Collects exact source text.
    pub fn text(&self) -> String {
        self.to_string()
    }
}
impl<K: Copy> GreenElement<K> {
    /// Language-supplied syntax kind.
    pub fn kind(&self) -> K {
        match self {
            Self::Node(n) => n.kind(),
            Self::Token(t) => t.kind(),
        }
    }
}
/// Reusable weak hash-consing cache. Roots own their storage independently.
/// Nodes are keyed by kind and ordered child allocation identities, tokens by
/// kind and bytes. Collisions use exact equality without hashing descendants.
/// Cross-cache children stay self-contained and retain their allocations.
/// Dead entries are pruned when their bucket is used, or by `prune`.
pub struct NodeCache<K> {
    nodes: HashMap<u64, Vec<Weak<NodeData<K>>>, FxBuildHasher>,
    tokens: HashMap<u64, Vec<Weak<TokenData<K>>>, FxBuildHasher>,
}
impl<K> Default for NodeCache<K> {
    fn default() -> Self {
        Self {
            nodes: HashMap::default(),
            tokens: HashMap::default(),
        }
    }
}
impl<K> NodeCache<K> {
    /// Creates an empty cache. No global state or tree-local numeric IDs are used.
    pub fn new() -> Self {
        Self::default()
    }
    /// Releases weak entries for allocations no longer owned by any tree.
    pub fn prune(&mut self) {
        self.nodes.retain(|_, bucket| {
            bucket.retain(|w| w.strong_count() != 0);
            !bucket.is_empty()
        });
        self.tokens.retain(|_, bucket| {
            bucket.retain(|w| w.strong_count() != 0);
            !bucket.is_empty()
        });
    }
}
impl<K: Copy + Eq + Hash> NodeCache<K> {
    /// Interns a token by kind and exact bytes.
    pub fn token(&mut self, kind: K, text: &str) -> Result<GreenToken<K>, BuildError> {
        let len = TextSize::from_usize(text.len())?;
        let digest = FxBuildHasher::default().hash_one((0u8, kind, text));
        let bucket = self.tokens.entry(digest).or_default();
        bucket.retain(|w| w.strong_count() != 0);
        for weak in bucket.iter() {
            if let Some(data) = weak.upgrade() {
                let token = GreenToken(data);
                if token.kind() == kind && token.text() == text {
                    return Ok(token);
                }
            }
        }
        let token = GreenToken(Arc::new(TokenData {
            kind,
            len,
            text: text.into(),
        }));
        bucket.push(Arc::downgrade(&token.0));
        Ok(token)
    }
    /// Interns a node, retaining its children by shared ownership. Children may
    /// come from any cache. Only the direct child array is allocated/copied.
    pub fn node(
        &mut self,
        kind: K,
        children: impl IntoIterator<Item = GreenElement<K>>,
    ) -> Result<GreenNode<K>, BuildError> {
        let children: Vec<_> = children.into_iter().collect();
        TextSize::from_usize(children.len())?;
        let mut len = TextSize::ZERO;
        let mut offsets = Vec::with_capacity(children.len());
        let mut hasher = FxBuildHasher::default().build_hasher();
        (1u8, kind, children.len()).hash(&mut hasher);
        for child in &children {
            offsets.push(len);
            len = len.checked_add(child.text_len())?;
            child.hash(&mut hasher);
        }
        let digest = hasher.finish();
        let bucket = self.nodes.entry(digest).or_default();
        bucket.retain(|w| w.strong_count() != 0);
        for weak in bucket.iter() {
            if let Some(data) = weak.upgrade() {
                let node = GreenNode(Some(data));
                if node.kind() == kind && node.children() == children {
                    return Ok(node);
                }
            }
        }
        let node = GreenNode(Some(Arc::new(NodeData {
            kind,
            len,
            children,
            offsets,
        })));
        bucket.push(Arc::downgrade(node.arc()));
        Ok(node)
    }
}
impl<K> fmt::Display for GreenToken<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.text())
    }
}
impl<K> fmt::Display for GreenNode<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut pending: Vec<_> = self.children().iter().rev().collect();
        while let Some(e) = pending.pop() {
            match e {
                NodeOrToken::Node(n) => pending.extend(n.children().iter().rev()),
                NodeOrToken::Token(t) => f.write_str(t.text())?,
            }
        }
        Ok(())
    }
}
impl<K> fmt::Display for GreenElement<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Node(n) => n.fmt(f),
            Self::Token(t) => t.fmt(f),
        }
    }
}
