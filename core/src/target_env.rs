//! The target environment in scope at an operation: the hardware half of the
//! target description — which chip the code runs on, rather than how its ABI
//! lays data out (that is [`DataLayout`](crate::DataLayout)).
//!
//! An environment is a [`target_env`](TARGET_ENV) dict attribute, resolved
//! through the enclosing scopes by [`scoped_dict`].
//!
//! ```text
//! #target_env = {arch = "riscv64", cpu = "sifive-u74", features = ["m", "a", "c"]}
//! ```
//!
//! `arch` names the target as `--march` spells it, and `features` its enabled
//! ISA extensions. Entries outside that predefined set are carried through
//! untouched, which is where a dialect puts its own chip facts (a GPU's shared
//! memory size, a cache hierarchy).

use crate::attributes::AttributeValue;
use crate::scoped_attr::invalid_spec;
use crate::{AttributeDict, Context, OpId, scoped_dict};

/// The attribute a target environment spec is carried in.
pub const TARGET_ENV: &str = "target_env";

pub struct TargetEnv {
    entries: AttributeDict,
}

impl TargetEnv {
    /// The environment in scope at `op`, or `None` when no enclosing scope
    /// declares one.
    pub fn for_op(context: &Context, op: OpId) -> Option<Self> {
        scoped_dict(context, op, TARGET_ENV).map(|entries| Self { entries })
    }

    /// An environment read from a spec that is not attached to the IR — a
    /// target's own description of itself.
    pub fn from_value(value: &AttributeValue) -> Option<Self> {
        match value {
            AttributeValue::Dict(entries) => Some(Self {
                entries: entries.clone(),
            }),
            _ => None,
        }
    }

    /// The target architecture, spelled as `--march` accepts it.
    pub fn arch(&self) -> Option<&str> {
        string(self.entries.get("arch"))
    }

    pub fn cpu(&self) -> Option<&str> {
        string(self.entries.get("cpu"))
    }

    /// Whether `name` is among the enabled ISA extensions.
    pub fn has_feature(&self, name: &str) -> bool {
        let Some(AttributeValue::Array(features)) = self.entries.get("features") else {
            return false;
        };
        features
            .iter()
            .any(|feature| string(Some(feature)) == Some(name))
    }

    /// A spec entry outside the predefined set, for chip facts a dialect
    /// defines itself.
    pub fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.entries.get(key)
    }
}

/// Builds a [`TARGET_ENV`] spec, the form a target describes itself in.
/// `features` names the enabled extensions as `--mattr` spells them.
pub fn target_env_spec(arch: &str, features: &[String]) -> AttributeValue {
    let features = features
        .iter()
        .map(|feature| AttributeValue::Str(feature.clone()))
        .collect();
    AttributeValue::Dict(
        [
            ("arch".to_string(), AttributeValue::from(arch)),
            ("features".to_string(), AttributeValue::Array(features)),
        ]
        .into(),
    )
}

/// Checks the shape of a [`TARGET_ENV`] spec. Entries outside the predefined
/// set are an extension point and pass unexamined.
pub(crate) fn verify_spec(value: &AttributeValue) -> Result<(), crate::Error> {
    let AttributeValue::Dict(entries) = value else {
        return Err(invalid_spec("target_env must be a dictionary"));
    };

    for (key, value) in entries {
        match key.as_str() {
            "arch" | "cpu" if string(Some(value)).is_none() => {
                return Err(invalid_spec(format!("target_env '{key}' must be a string")));
            }
            "features" => {
                let AttributeValue::Array(features) = value else {
                    return Err(invalid_spec("target_env 'features' must be an array"));
                };
                if features
                    .iter()
                    .any(|feature| string(Some(feature)).is_none())
                {
                    return Err(invalid_spec(
                        "target_env 'features' must hold extension name strings",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn string(value: Option<&AttributeValue>) -> Option<&str> {
    match value? {
        AttributeValue::Str(value) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::ModuleOp;
    use crate::parse::ir::parse_ir;
    use crate::{Context, Operation};

    /// The environment of a module declaring `spec`.
    fn target_env(context: &Context, spec: &str) -> TargetEnv {
        let src = format!("module {{target_env = {spec}}} {{\n  module_end\n}}");
        let module = parse_ir::<ModuleOp>(context, &src).expect("parse module");
        TargetEnv::for_op(context, module.id()).expect("environment in scope")
    }

    #[test]
    fn arch_and_cpu_are_read_from_the_spec() {
        let context = Context::with_default_dialects();

        let env = target_env(&context, r#"{arch = "riscv64", cpu = "sifive-u74"}"#);

        assert_eq!(env.arch(), Some("riscv64"));
        assert_eq!(env.cpu(), Some("sifive-u74"));
    }

    #[test]
    fn an_absent_cpu_is_unknown() {
        let context = Context::with_default_dialects();

        let env = target_env(&context, r#"{arch = "arm64"}"#);

        assert_eq!(env.cpu(), None);
    }

    #[test]
    fn enabled_features_are_queryable_by_name() {
        let context = Context::with_default_dialects();

        let env = target_env(&context, r#"{arch = "riscv64", features = ["m", "c"]}"#);

        assert!(env.has_feature("m"));
        assert!(env.has_feature("c"));
        assert!(!env.has_feature("v"));
    }

    #[test]
    fn a_spec_without_features_enables_none() {
        let context = Context::with_default_dialects();

        let env = target_env(&context, r#"{arch = "riscv64"}"#);

        assert!(!env.has_feature("m"));
    }

    #[test]
    fn entries_outside_the_predefined_set_stay_readable() {
        let context = Context::with_default_dialects();

        let env = target_env(&context, "{shared_memory = 65536}");

        assert_eq!(env.get("shared_memory"), Some(&AttributeValue::Int(65536)));
    }

    #[test]
    fn a_target_description_needs_no_enclosing_scope() {
        let spec = AttributeValue::Dict(
            [("arch".to_string(), AttributeValue::Str("arm64".to_string()))]
                .into_iter()
                .collect(),
        );

        let env = TargetEnv::from_value(&spec).expect("spec is a dict");

        assert_eq!(env.arch(), Some("arm64"));
        assert!(TargetEnv::from_value(&AttributeValue::UInt(0)).is_none());
    }
}
