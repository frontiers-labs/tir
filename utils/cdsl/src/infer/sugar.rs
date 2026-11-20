use super::*;

pub(crate) fn operation_record(
    fields: &[Node<OpField>],
    range: std::ops::Range<usize>,
) -> Result<Expr, Error> {
    let mut entries = Vec::new();
    for field in fields {
        let (name, value) = match &field.value {
            OpField::Field { name, value } => (name.clone(), value.clone()),
            OpField::Signature {
                parameters,
                result,
                results,
            } => {
                let mut names = BTreeSet::new();
                if parameters
                    .iter()
                    .any(|parameter| !names.insert(&parameter.name))
                {
                    return Err(Error("duplicate operand name".into()));
                }
                let unique = results.iter().collect::<BTreeSet<_>>();
                if unique.len() != results.len() {
                    return Err(Error("duplicate result name".into()));
                }
                if !results.is_empty()
                    && !matches!(&result.value, Expression::Tuple(items) if items.len() == results.len())
                {
                    return Err(Error(
                        "named result count disagrees with result product".into(),
                    ));
                }
                let left = Node {
                    value: Expression::Tuple(
                        parameters
                            .iter()
                            .map(|parameter| parameter.ty.clone())
                            .collect(),
                    ),
                    range: field.range.clone(),
                };
                (
                    "signature".into(),
                    Node {
                        value: Expression::Binary {
                            op: BinaryOp::Arrow,
                            left: Box::new(left),
                            right: Box::new(result.clone()),
                        },
                        range: field.range.clone(),
                    },
                )
            }
        };
        if entries.iter().any(|(previous, _)| previous == &name) {
            return Err(Error(format!("duplicate operation field {name}")));
        }
        entries.push((name, value));
    }
    Ok(Node {
        value: Expression::Record(entries),
        range,
    })
}
