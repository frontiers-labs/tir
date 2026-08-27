//! The view's textual form, which is what the checks read.

use std::fmt::Write;

use super::*;

impl AffineView {
    /// The view as `tir opt --print-affine` prints it.
    pub fn render(&self, context: &Context) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "affine.view %{} {{", number(context, self.root));
        if self.opaque {
            let _ = writeln!(out, "  opaque");
        }
        if !self.symbols.is_empty() {
            let names = self
                .symbols
                .iter()
                .enumerate()
                .map(|(index, &value)| format!("s{index} = %{}", value.number()))
                .collect::<Vec<_>>();
            let _ = writeln!(out, "  symbols {}", names.join(", "));
        }
        for (depth, bounds) in self.loops.iter().enumerate() {
            let _ = writeln!(out, "  loop {depth} {}", self.render_loop(context, bounds));
        }
        for (index, access) in self.accesses.iter().enumerate() {
            let _ = writeln!(
                out,
                "  access {index} {}",
                self.render_access(context, access)
            );
        }
        for pair in &self.pairs {
            let _ = writeln!(
                out,
                "  dep {} {} {}",
                pair.left,
                pair.right,
                self.render_dependence(&pair.dependence)
            );
        }
        for (index, port) in self.ports.iter().enumerate() {
            let _ = writeln!(out, "  port {index} {}", render_recurrence(port));
        }
        out.push_str("}\n");
        out
    }

    fn render_loop(&self, context: &Context, bounds: &Loop) -> String {
        let counter = match bounds.counter {
            Some(value) => format!("%{}", value.number()),
            None => "none".to_string(),
        };
        let trip = match bounds.trip {
            Some(trip) => trip.to_string(),
            None => "unknown".to_string(),
        };
        format!(
            "%{} i{} [{}, {}) step {} counter {counter} trip {trip}",
            number(context, bounds.op),
            bounds.width,
            self.render_form(&bounds.lower),
            self.render_form(&bounds.upper),
            self.render_form(&bounds.step),
        )
    }

    fn render_access(&self, context: &Context, access: &Access) -> String {
        let mut text = format!(
            "{} %{} chain %{} base %{} offset {} extent {}",
            if access.write { "write" } else { "read" },
            number(context, access.op),
            access.chain.number(),
            access.base.number(),
            match &access.offset {
                Offset::Affine(form) => self.render_form(form),
                Offset::NonAffine => "non-affine".to_string(),
            },
            access.extent,
        );
        if access.guarded {
            text.push_str(" guarded");
        }
        if access.wrapping {
            text.push_str(" wrapping");
        }
        text
    }

    fn render_dependence(&self, dependence: &Dependence) -> String {
        match dependence {
            Dependence::Independent => "independent".to_string(),
            Dependence::Unknown => "unknown".to_string(),
            Dependence::Distances(components) => format!(
                "({})",
                components
                    .iter()
                    .map(render_component)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Dependence::Conditional(sides) => format!(
                "conditional disjoint {} {}",
                self.render_extremes(&sides.0),
                self.render_extremes(&sides.1)
            ),
        }
    }

    fn render_extremes(&self, extremes: &Extremes) -> String {
        format!(
            "%{}[{}, {}]",
            extremes.base.number(),
            self.render_form(&extremes.low),
            self.render_form(&extremes.high)
        )
    }

    /// A form as `2*d0 + 4*s1 + 3`, or `0` where every term is zero.
    fn render_form(&self, form: &AffineForm) -> String {
        let mut terms = Vec::new();
        for depth in 0..self.loops.len() {
            push_term(
                &mut terms,
                form.counter_coefficient(depth),
                &format!("d{depth}"),
            );
        }
        for index in 0..self.symbols.len() {
            push_term(
                &mut terms,
                form.symbol_coefficient(index),
                &format!("s{index}"),
            );
        }
        if form.constant_term() != 0 || terms.is_empty() {
            terms.push(form.constant_term().to_string());
        }
        terms.join(" + ")
    }
}

fn push_term(terms: &mut Vec<String>, coefficient: i128, name: &str) {
    match coefficient {
        0 => {}
        1 => terms.push(name.to_string()),
        _ => terms.push(format!("{coefficient}*{name}")),
    }
}

fn render_component(component: &Component) -> String {
    match component {
        Component::Distance(distance) => distance.to_string(),
        Component::Direction(Sign::Positive) => "+".to_string(),
        Component::Direction(Sign::Negative) => "-".to_string(),
        Component::Any => "*".to_string(),
    }
}

fn render_recurrence(port: &Port) -> String {
    match &port.recurrence {
        Recurrence::Induction { init, step } => format!(
            "%{} induction init %{} step {step}",
            port.arg.number(),
            init.number()
        ),
        Recurrence::Reduction(reduction) => {
            format!(
                "%{} reduction {}",
                port.arg.number(),
                match reduction {
                    Reduction::Add => "add",
                    Reduction::Mul => "mul",
                    Reduction::And => "and",
                    Reduction::Or => "or",
                    Reduction::Xor => "xor",
                }
            )
        }
        Recurrence::Other => format!("%{} other", port.arg.number()),
    }
}

/// A loop is named by the value it produces where it produces one, so a check
/// can point at the same `%n` the IR prints.
fn number(context: &Context, op: OpId) -> String {
    match context.get_op(op).results().first() {
        Some(value) => value.number().to_string(),
        None => format!("op{}", op.index()),
    }
}
