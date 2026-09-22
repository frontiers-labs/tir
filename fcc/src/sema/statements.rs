//! Statement checks and control-flow context.

use super::references::{
    break_reference, condition_reference, continue_reference, goto_reference, label_reference,
    return_conversion_reference, return_reference, switch_case_reference, switch_label_reference,
};
use super::{Analyzer, SwitchContext, TypeKind};
use crate::ast::{AstKind, AstLeaf};
use crate::diagnostics::{
    DuplicateLabel, DuplicateSwitchLabel, IncompatibleConversion, IntegerConstantRequired,
    InvalidControllingExpression, InvalidReturn, MisplacedBreak, MisplacedContinue,
    MisplacedSwitchLabel, UnknownLabel,
};
use std::collections::HashMap;
use tir::graph::{Dag, NodeId};

impl Analyzer<'_> {
    pub(super) fn function(&mut self, function: NodeId) {
        let previous_return = self.current_return;
        if let Some(AstLeaf::Function { ret, .. }) = self.ast.get_leaf_data(function).cloned() {
            self.current_return = Some(self.canonical_type(&ret));
        }
        self.scopes.push(HashMap::new());
        let children = self.ast.children(function).collect::<Vec<_>>();
        self.labels.clear();
        for &child in &children {
            self.collect_labels(child);
        }
        for child in children {
            match self.ast.get_node(child).kind {
                AstKind::Param | AstKind::Decl | AstKind::Typedef => self.declaration(child),
                AstKind::EnumDecl => self.enum_declaration(child),
                _ => self.node(child),
            }
        }
        self.scopes.pop();
        self.current_return = previous_return;
    }

    pub(super) fn collect_labels(&mut self, node: NodeId) {
        if self.ast.get_node(node).kind == AstKind::Label
            && let Some(AstLeaf::Label(name)) = self.ast.get_leaf_data(node).cloned()
        {
            let span = self.ast.get_node(node).span;
            if let Some(&previous) = self.labels.get(&name) {
                self.diagnostics.push(
                    DuplicateLabel::new(span, previous, name, label_reference(self.options)).into(),
                );
            } else {
                self.labels.insert(name, span);
            }
        }
        let children = self.ast.children(node).collect::<Vec<_>>();
        for child in children {
            self.collect_labels(child);
        }
    }

    pub(super) fn node(&mut self, node: NodeId) {
        let kind = self.ast.get_node(node).kind;
        let scoped = matches!(kind, AstKind::Block | AstKind::For);
        if scoped {
            self.scopes.push(HashMap::new());
        }
        let is_loop = matches!(kind, AstKind::While | AstKind::DoWhile | AstKind::For);
        let is_switch = kind == AstKind::Switch;
        if is_loop {
            self.loop_depth += 1;
        }
        if is_switch {
            self.switch_depth += 1;
            self.switches.push(SwitchContext::default());
        }
        let children = self.ast.children(node).collect::<Vec<_>>();
        for child in children {
            match self.ast.get_node(child).kind {
                AstKind::Decl | AstKind::Typedef => self.declaration(child),
                AstKind::EnumDecl => self.enum_declaration(child),
                _ => self.node(child),
            }
        }
        self.infer_expression(node);
        self.validate_statement(node);
        if is_loop {
            self.loop_depth -= 1;
        }
        if is_switch {
            self.switch_depth -= 1;
            self.switches.pop();
        }
        if scoped {
            self.scopes.pop();
        }
    }

    pub(super) fn validate_statement(&mut self, node: NodeId) {
        match self.ast.get_node(node).kind {
            AstKind::Goto => {
                if let Some(AstLeaf::Label(name)) = self.ast.get_leaf_data(node).cloned()
                    && !self.labels.contains_key(&name)
                {
                    self.diagnostics.push(
                        UnknownLabel::new(
                            self.ast.get_node(node).span,
                            name,
                            goto_reference(self.options),
                        )
                        .into(),
                    );
                }
                return;
            }
            AstKind::Case | AstKind::Default => {
                self.validate_switch_label(node);
                return;
            }
            AstKind::If | AstKind::While | AstKind::DoWhile | AstKind::For | AstKind::Switch => {
                self.validate_condition(node);
                return;
            }
            AstKind::Break if self.loop_depth == 0 && self.switch_depth == 0 => {
                self.diagnostics.push(
                    MisplacedBreak::new(
                        self.ast.get_node(node).span,
                        break_reference(self.options),
                    )
                    .into(),
                );
                return;
            }
            AstKind::Continue if self.loop_depth == 0 => {
                self.diagnostics.push(
                    MisplacedContinue::new(
                        self.ast.get_node(node).span,
                        continue_reference(self.options),
                    )
                    .into(),
                );
                return;
            }
            AstKind::Return => {}
            _ => return,
        }
        let Some(return_ty) = self.current_return else {
            return;
        };
        let has_value = self.ast.children(node).next().is_some();
        let is_void = matches!(self.types.kind(return_ty), TypeKind::Void);
        let message = match (is_void, has_value) {
            (true, true) => Some("void function must not return a value"),
            (false, false) => Some("non-void function must return a value"),
            _ => None,
        };
        if let Some(message) = message {
            self.diagnostics.push(
                InvalidReturn::new(
                    self.ast.get_node(node).span,
                    message,
                    return_reference(self.options),
                )
                .into(),
            );
            return;
        }
        if !is_void && has_value {
            let expression = self.ast.children(node).next().unwrap();
            let source = self
                .ast
                .get_annotation(expression)
                .and_then(|info| info.ty)
                .unwrap_or(return_ty);
            let source = self.assignment_source(return_ty, source, expression);
            if !self.assignment_compatible(return_ty, source, expression) {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(expression).span,
                        None,
                        format!(
                            "cannot return value of {} type as {}",
                            self.type_category(source),
                            self.type_category(return_ty)
                        ),
                        return_conversion_reference(self.options),
                    )
                    .into(),
                );
            } else {
                self.record_conversion(expression, return_ty);
            }
        }
    }

    pub(super) fn validate_switch_label(&mut self, node: NodeId) {
        let Some(context) = self.switches.last_mut() else {
            self.diagnostics.push(
                MisplacedSwitchLabel::new(
                    self.ast.get_node(node).span,
                    switch_label_reference(self.options),
                )
                .into(),
            );
            return;
        };
        let span = self.ast.get_node(node).span;
        if self.ast.get_node(node).kind == AstKind::Default {
            if let Some(previous) = context.default.replace(span) {
                self.diagnostics.push(
                    DuplicateSwitchLabel::new(
                        span,
                        previous,
                        "duplicate default label",
                        switch_case_reference(self.options),
                    )
                    .into(),
                );
            }
            return;
        }
        let expression = self.ast.children(node).next().unwrap();
        let Some(value) = self
            .ast
            .get_annotation(expression)
            .and_then(|info| info.constant)
        else {
            self.diagnostics.push(
                IntegerConstantRequired::new(
                    self.ast.get_node(expression).span,
                    "case label is not an integer constant expression",
                    switch_case_reference(self.options),
                )
                .into(),
            );
            return;
        };
        if let Some(previous) = context.cases.insert(value, span) {
            self.diagnostics.push(
                DuplicateSwitchLabel::new(
                    span,
                    previous,
                    format!("duplicate case value {value}"),
                    switch_case_reference(self.options),
                )
                .into(),
            );
        }
    }

    pub(super) fn validate_condition(&mut self, node: NodeId) {
        let kind = self.ast.get_node(node).kind;
        let children = self.ast.children(node).collect::<Vec<_>>();
        let index = match kind {
            AstKind::DoWhile | AstKind::For => 1,
            _ => 0,
        };
        let Some(condition) = children.get(index).copied() else {
            return;
        };
        if self.ast.get_node(condition).kind == AstKind::Empty {
            return;
        }
        let Some(ty) = self.ast.get_annotation(condition).and_then(|info| info.ty) else {
            return;
        };
        if self.types.kind(ty) == &TypeKind::Error {
            return;
        }
        let valid = if kind == AstKind::Switch {
            matches!(
                self.types.kind(ty),
                TypeKind::Integer(_) | TypeKind::Enum(_)
            )
        } else {
            self.is_arithmetic(ty) || matches!(self.types.kind(ty), TypeKind::Pointer(_))
        };
        if valid {
            if kind == AstKind::Switch {
                let promoted = self.integer_promotion(ty);
                self.record_conversion(condition, promoted);
            }
            return;
        }
        let statement = match kind {
            AstKind::If => "if",
            AstKind::While => "while",
            AstKind::DoWhile => "do-while",
            AstKind::For => "for",
            AstKind::Switch => "switch",
            _ => unreachable!(),
        };
        let expected = if kind == AstKind::Switch {
            "integer"
        } else {
            "scalar"
        };
        self.diagnostics.push(
            InvalidControllingExpression::new(
                self.ast.get_node(condition).span,
                format!("{statement} condition must have {expected} type"),
                condition_reference(self.options, kind),
            )
            .into(),
        );
    }
}
