//! Width-bounded layout documents and an iterative renderer.

pub(crate) enum Doc<'a> {
    Text(&'a str),
    Concat(Vec<Doc<'a>>),
    Nest(usize, Box<Doc<'a>>),
    Hardline,
    Softline(&'static str),
    Group {
        width: Option<usize>,
        body: Box<Doc<'a>>,
    },
}

impl<'a> Doc<'a> {
    pub fn group(self, budget: usize) -> Self {
        let mut pending = vec![&self];
        let mut width = Some(0usize);
        while let Some(doc) = pending.pop() {
            let extra = match doc {
                Self::Text(text) => {
                    let available = budget.saturating_sub(width.unwrap_or_default());
                    let length = text.chars().take(available.saturating_add(1)).try_fold(
                        0usize,
                        |length, character| {
                            if matches!(character, '\n' | '\r') {
                                None
                            } else {
                                Some(length + 1)
                            }
                        },
                    );
                    match length {
                        Some(length) => length,
                        None => {
                            width = None;
                            break;
                        }
                    }
                }
                Self::Softline(text) => text.len(),
                Self::Concat(children) => {
                    pending.extend(children.iter().rev());
                    continue;
                }
                Self::Nest(_, body) => {
                    pending.push(body);
                    continue;
                }
                Self::Group {
                    width: Some(value), ..
                } => *value,
                _ => {
                    width = None;
                    break;
                }
            };
            width = width
                .and_then(|value| value.checked_add(extra))
                .filter(|value| *value <= budget);
            if width.is_none() {
                break;
            }
        }
        Self::Group {
            width,
            body: Box::new(self),
        }
    }

    pub fn render(&self, width: usize, initial_indent: usize, initial_column: usize) -> String {
        let mut result = String::new();
        let mut column = initial_column;
        let mut pending = vec![(self, initial_indent, false)];
        let mut line_start = false;
        while let Some((doc, indent, flat)) = pending.pop() {
            match doc {
                Self::Text(text) => {
                    if line_start && !text.is_empty() {
                        result.extend(std::iter::repeat_n(' ', indent));
                        column = indent;
                        line_start = false;
                    }
                    result.push_str(text);
                    column = match text.rsplit_once(['\n', '\r']) {
                        Some((_, last)) => last.chars().count(),
                        None => column + text.chars().count(),
                    };
                }
                Self::Concat(children) => {
                    pending.extend(children.iter().rev().map(|doc| (doc, indent, flat)))
                }
                Self::Nest(amount, body) => pending.push((body, indent + amount, flat)),
                Self::Group {
                    width: length,
                    body,
                } => {
                    pending.push((
                        body,
                        indent,
                        flat || length.is_some_and(|length| {
                            length <= width.saturating_sub(if line_start { indent } else { column })
                        }),
                    ));
                }
                Self::Softline(text) if flat => {
                    result.push_str(text);
                    column += text.len();
                }
                Self::Hardline | Self::Softline(_) => {
                    result.push('\n');
                    column = 0;
                    line_start = true;
                }
            }
        }
        result
    }
}
