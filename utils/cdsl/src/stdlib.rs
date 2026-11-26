//! Embedded CDSL v3 standard library (`std::*`).
//!
//! Pure CDSL source files loaded automatically into `mod std` whenever a user
//! module references `std` without declaring its own local `mod std;`.

use std::collections::HashMap;

use crate::elaborate::ModuleNode;

fn leaf(source: &str, label: &str) -> ModuleNode {
    ModuleNode::File {
        source: source.to_owned(),
        label: label.to_owned(),
        children: HashMap::new(),
    }
}

// Builds the embedded `std` module tree from `utils/cdsl/stdlib/`.
pub(crate) fn embedded_std() -> ModuleNode {
    let mut ir_children = HashMap::new();
    ir_children.insert(
        "interfaces".to_owned(),
        leaf(
            include_str!("../stdlib/std/ir/interfaces.cdsl"),
            "<std>/ir/interfaces.cdsl",
        ),
    );
    ir_children.insert(
        "rewrites".to_owned(),
        leaf(
            include_str!("../stdlib/std/ir/rewrites.cdsl"),
            "<std>/ir/rewrites.cdsl",
        ),
    );
    ir_children.insert(
        "integers".to_owned(),
        leaf(
            include_str!("../stdlib/std/ir/integers.cdsl"),
            "<std>/ir/integers.cdsl",
        ),
    );

    let mut std_children = HashMap::new();
    std_children.insert(
        "core".to_owned(),
        leaf(include_str!("../stdlib/std/core.cdsl"), "<std>/core.cdsl"),
    );
    std_children.insert(
        "bits".to_owned(),
        leaf(include_str!("../stdlib/std/bits.cdsl"), "<std>/bits.cdsl"),
    );
    std_children.insert(
        "bigint".to_owned(),
        leaf(
            include_str!("../stdlib/std/bigint.cdsl"),
            "<std>/bigint.cdsl",
        ),
    );
    std_children.insert(
        "int".to_owned(),
        leaf(include_str!("../stdlib/std/int.cdsl"), "<std>/int.cdsl"),
    );
    std_children.insert(
        "option".to_owned(),
        leaf(
            include_str!("../stdlib/std/option.cdsl"),
            "<std>/option.cdsl",
        ),
    );
    std_children.insert(
        "ir".to_owned(),
        ModuleNode::File {
            source: include_str!("../stdlib/std/ir/mod.cdsl").to_owned(),
            label: "<std>/ir/mod.cdsl".to_owned(),
            children: ir_children,
        },
    );

    ModuleNode::File {
        source: include_str!("../stdlib/std/mod.cdsl").to_owned(),
        label: "<std>/mod.cdsl".to_owned(),
        children: std_children,
    }
}
