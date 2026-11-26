//! Formatting without import loading, name resolution, or type inference.
use std::ops::Range;

use tir_cst::{GreenElement, GreenNode, GreenToken, NodeOrToken};

use crate::{
    layout::Doc,
    syntax::{SyntaxCache, SyntaxError, SyntaxKind, SyntaxTree, parse_source},
};

/// Formatting preferences. Token spelling and declaration order are preserved.
#[derive(Clone, Copy, Debug)]
pub struct FormatOptions {
    /// Preferred maximum line width. An indivisible token can exceed it.
    pub width: usize,
    /// Spaces per indentation level.
    pub indent: usize,
}
impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            width: 100,
            indent: 4,
        }
    }
}

/// A replacement in UTF-8 byte coordinates in the original source.
#[derive(Clone, Debug)]
pub struct TextEdit {
    /// Complete layout group selected for replacement.
    pub range: Range<usize>,
    /// Formatted replacement text.
    pub replacement: String,
}

/// Parses and formats one complete source file without semantic analysis.
pub fn format_source(source: &str, options: FormatOptions) -> Result<String, SyntaxError> {
    let tree = parse_source(source, &mut SyntaxCache::new())?;
    Ok(format_tree(&tree, options))
}

/// Formats a valid parsed root through direct green traversal.
/// After arbitrary editor edits, validate with `parse_source` before formatting.
pub fn format_tree(tree: &SyntaxTree, options: FormatOptions) -> String {
    let root = tree.root();
    let mut text = document(root.green(), options).render(options.width, 0, 0);
    while text.ends_with('\n') {
        text.pop();
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// Expands a selection to a complete enclosing syntax group. The returned edit
/// leaves all bytes outside that group unchanged. Coordinates must be UTF-8 bytes.
pub fn format_range(
    tree: &SyntaxTree,
    selection: Range<usize>,
    options: FormatOptions,
) -> Result<TextEdit, SyntaxError> {
    let root = tree.root();
    let source = root.green().text();
    if selection.start > selection.end
        || selection.end > source.len()
        || !source.is_char_boundary(selection.start)
        || !source.is_char_boundary(selection.end)
    {
        return Err(SyntaxError::Storage(tir_cst::BuildError::InvalidRange));
    }
    if selection == (0..source.len()) {
        return Ok(TextEdit {
            range: selection,
            replacement: format_tree(tree, options),
        });
    }
    let mut group = root.green();
    let mut start = 0;
    loop {
        let mut offset = start;
        let mut enclosed = None;
        for child in group.children() {
            let end = offset + child.text_len().get() as usize;
            if let NodeOrToken::Node(node) = child
                && offset <= selection.start
                && end >= selection.end
            {
                enclosed = Some((node, offset));
                break;
            }
            offset = end;
        }
        match enclosed {
            Some((node, offset)) => {
                group = node;
                start = offset;
            }
            None => break,
        }
    }
    if group.kind() == SyntaxKind::Module {
        return Ok(TextEdit {
            range: 0..source.len(),
            replacement: format_tree(tree, options),
        });
    }
    let end = start + group.text_len().get() as usize;
    let line = source[..start]
        .rfind(['\n', '\r'])
        .map_or(0, |index| index + 1);
    let indent = source[line..start]
        .chars()
        .take_while(|ch| *ch == ' ')
        .count();
    let column = source[line..start].chars().count();
    let replacement = document(group, options).render(options.width, indent, column);
    Ok(TextEdit {
        range: start..end,
        replacement,
    })
}

struct LayoutPiece<'a> {
    doc: Doc<'a>,
    first: &'a str,
    last: &'a str,
}

enum LayoutChild<'a> {
    Token(&'a GreenToken<SyntaxKind>),
    Node(LayoutPiece<'a>),
}

impl<'a> LayoutChild<'a> {
    fn bounds(&self) -> Option<(&'a str, &'a str)> {
        match self {
            Self::Node(piece) => Some((piece.first, piece.last)),
            Self::Token(token) if token.kind() == SyntaxKind::Token => {
                Some((token.text(), token.text()))
            }
            Self::Token(_) => None,
        }
    }
}

fn document(root: &GreenNode<SyntaxKind>, options: FormatOptions) -> Doc<'_> {
    struct Pending<'a> {
        kind: SyntaxKind,
        children: std::slice::Iter<'a, GreenElement<SyntaxKind>>,
        layout: Vec<LayoutChild<'a>>,
    }
    let mut pending = vec![Pending {
        kind: root.kind(),
        children: root.children().iter(),
        layout: Vec::new(),
    }];
    loop {
        let frame = pending.last_mut().unwrap_or_else(|| unreachable!());
        match frame.children.next() {
            Some(NodeOrToken::Node(node)) => pending.push(Pending {
                kind: node.kind(),
                children: node.children().iter(),
                layout: Vec::new(),
            }),
            Some(NodeOrToken::Token(token)) => frame.layout.push(LayoutChild::Token(token)),
            None => {
                let frame = pending.pop().unwrap_or_else(|| unreachable!());
                let first = frame
                    .layout
                    .iter()
                    .find_map(LayoutChild::bounds)
                    .map_or("", |bounds| bounds.0);
                let last = frame
                    .layout
                    .iter()
                    .rev()
                    .find_map(LayoutChild::bounds)
                    .map_or("", |bounds| bounds.1);
                let doc = layout_node(frame.kind, frame.layout, options);
                match pending.last_mut() {
                    Some(parent) => {
                        parent
                            .layout
                            .push(LayoutChild::Node(LayoutPiece { doc, first, last }))
                    }
                    None => return doc,
                }
            }
        }
    }
}

struct DelimiterFrame<'a> {
    open: &'a str,
    docs: Vec<Doc<'a>>,
    previous: &'a str,
}
impl<'a> DelimiterFrame<'a> {
    fn comment(&mut self, token: &'a GreenToken<SyntaxKind>, whitespace: &str) {
        let text = token.text();
        let standalone = whitespace.contains(['\n', '\r']) || self.previous.is_empty();
        if !self.docs.is_empty() {
            if standalone {
                if !matches!(self.docs.last(), Some(Doc::Hardline)) {
                    self.docs.push(Doc::Hardline);
                }
                if blank_separator(whitespace) {
                    self.docs.push(Doc::Hardline);
                }
            } else {
                self.docs.push(Doc::Text(" "));
            }
        }
        self.docs.push(Doc::Text(text));
        if token.kind() == SyntaxKind::LineComment || standalone || text.contains(['\n', '\r']) {
            self.docs.push(Doc::Hardline);
            self.previous = "";
        } else {
            self.previous = "comment";
        }
    }
}

fn layout_node<'a>(
    kind: SyntaxKind,
    children: Vec<LayoutChild<'a>>,
    options: FormatOptions,
) -> Doc<'a> {
    if kind == SyntaxKind::Module {
        return module_document(children);
    }
    let mut stack = vec![DelimiterFrame {
        open: "",
        docs: Vec::new(),
        previous: "",
    }];
    let mut whitespace = "";
    for child in children {
        let token = match child {
            LayoutChild::Node(piece) => {
                let frame = stack.last_mut().unwrap_or_else(|| unreachable!());
                if let Some(separator) = separator(frame.previous, piece.first) {
                    frame.docs.push(separator);
                }
                let doc = if kind == SyntaxKind::BinaryExpression
                    && matches!(frame.docs.last(), Some(Doc::Hardline))
                {
                    Doc::Nest(options.indent, Box::new(piece.doc))
                } else {
                    piece.doc
                };
                frame.docs.push(doc);
                frame.previous = piece.last;
                whitespace = "";
                continue;
            }
            LayoutChild::Token(token) => token,
        };
        let text = token.text();
        if token.kind() == SyntaxKind::Whitespace {
            whitespace = text;
            continue;
        }
        let frame = &mut stack.last_mut().unwrap_or_else(|| unreachable!());
        if matches!(
            token.kind(),
            SyntaxKind::LineComment | SyntaxKind::BlockComment
        ) {
            frame.comment(token, whitespace);
            whitespace = "";
            continue;
        }
        if matches!(text, ")" | "]" | "}" | ">") && stack.len() > 1 {
            let frame = stack.pop().unwrap_or_else(|| unreachable!());
            let empty = frame.docs.is_empty();
            let ends_line = matches!(frame.docs.last(), Some(Doc::Hardline));
            let brace = frame.open == "{";
            let mut docs = vec![Doc::Text(frame.open)];
            if !empty {
                let mut body = vec![Doc::Softline(if brace { " " } else { "" })];
                body.extend(frame.docs);
                docs.push(Doc::Nest(options.indent, Box::new(Doc::Concat(body))));
                if !ends_line {
                    docs.push(Doc::Softline(if brace { " " } else { "" }));
                }
            }
            docs.push(Doc::Text(text));
            let parent = stack.last_mut().unwrap_or_else(|| unreachable!());
            parent.docs.push(Doc::Concat(docs).group(options.width));
            parent.previous = text;
        } else {
            let frame = stack.last_mut().unwrap_or_else(|| unreachable!());
            let previous = frame.previous;
            if kind == SyntaxKind::BinaryExpression && binary_operator(text) {
                let line = if matches!(frame.docs.last(), Some(Doc::Hardline)) {
                    Doc::Text("")
                } else {
                    Doc::Softline(" ")
                };
                frame.docs.push(Doc::Nest(
                    options.indent,
                    Box::new(Doc::Concat(vec![line, Doc::Text(text)])),
                ));
                frame.previous = text;
                whitespace = "";
                continue;
            }
            if let Some(separator) = separator(previous, text) {
                frame.docs.push(separator);
            }
            if matches!(text, "(" | "[" | "{" | "<") {
                stack.push(DelimiterFrame {
                    open: text,
                    docs: Vec::new(),
                    previous: "",
                });
            } else {
                frame.docs.push(Doc::Text(text));
                frame.previous = text;
            }
        }
        whitespace = "";
    }
    let frame = stack.pop().unwrap_or_else(|| unreachable!());
    Doc::Concat(frame.docs).group(options.width)
}

fn module_document(children: Vec<LayoutChild<'_>>) -> Doc<'_> {
    let mut docs = Vec::new();
    let mut whitespace = "";
    let mut previous_line_comment = false;
    for child in children {
        if let LayoutChild::Token(token) = &child
            && token.kind() == SyntaxKind::Whitespace
        {
            whitespace = token.text();
            continue;
        }
        let comment = matches!(&child, LayoutChild::Token(token) if matches!(token.kind(), SyntaxKind::LineComment | SyntaxKind::BlockComment));
        if !docs.is_empty() {
            if comment && !whitespace.contains(['\n', '\r']) && !previous_line_comment {
                docs.push(Doc::Text(" "));
            } else {
                docs.push(Doc::Hardline);
                if blank_separator(whitespace) {
                    docs.push(Doc::Hardline);
                }
            }
        }
        previous_line_comment =
            matches!(&child, LayoutChild::Token(token) if token.kind() == SyntaxKind::LineComment);
        match child {
            LayoutChild::Node(piece) => docs.push(piece.doc),
            LayoutChild::Token(token) => docs.push(Doc::Text(token.text())),
        }
        whitespace = "";
    }
    Doc::Concat(docs)
}

fn separator(previous: &str, text: &str) -> Option<Doc<'static>> {
    if previous.is_empty()
        || matches!(text, "," | ";" | "." | "::" | ":" | ">" | "?")
        || matches!(previous, "." | "::" | "<" | "!" | "..")
        || (matches!(text, "(" | "<")
            && !matches!(
                previous,
                "=" | ":"
                    | ","
                    | ";"
                    | "+"
                    | "++"
                    | "-"
                    | "*"
                    | "/"
                    | "%"
                    | "&&"
                    | "||"
                    | "=="
                    | "!="
                    | "<="
                    | ">="
                    | "->"
                    | "~>"
                    | "=>"
                    | "if"
                    | "match"
                    | "rec"
                    | "memo"
            ))
    {
        None
    } else {
        Some(match previous {
            ";" => Doc::Hardline,
            "," => Doc::Softline(" "),
            _ => Doc::Text(" "),
        })
    }
}

fn binary_operator(text: &str) -> bool {
    matches!(
        text,
        "+" | "++"
            | "-"
            | "*"
            | "/"
            | "%"
            | "&&"
            | "||"
            | "=="
            | "!="
            | "<"
            | "<="
            | ">"
            | ">="
            | "->"
            | "~>"
    )
}

fn blank_separator(text: &str) -> bool {
    let mut bytes = text.bytes().peekable();
    let mut lines = 0;
    while let Some(byte) = bytes.next() {
        if byte == b'\r' {
            if bytes.peek() == Some(&b'\n') {
                bytes.next();
            }
            lines += 1;
        } else if byte == b'\n' {
            lines += 1;
        }
        if lines == 2 {
            return true;
        }
    }
    false
}
