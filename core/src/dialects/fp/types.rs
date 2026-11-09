use tir_symbolic::term::valid;

use crate::ty::{Ctor, Terms, TypeArg, TypeSpelling};
use crate::{Context, TypeId};

static ENV: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_fp_types.cdsl", binding: constructors::ENV, hosts: { ENV_validate => valid, ENV_carrier => |_: &Terms, _: &[TypeArg]| Some(TypeArg::Nat(13)) } },
);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvironmentType;
impl EnvironmentType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context
            .terms()
            .intern(ENV.id(context), vec![])
            .expect("env")
    }
    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        ENV.is(terms, ty).then_some(Self)
    }
    pub fn spelling(context: &Context) -> TypeSpelling {
        ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_fp_types.cdsl", binding: spellings::EnvironmentType, context: context, hosts: { EnvironmentType_parse => |_, _, context| Ok(EnvironmentType::new(context)), EnvironmentType_print => |_, _, fmt| fmt.write(concat!("fp.", "env")), EnvironmentType_ctor_id => |context| { ENV.id(context) } } }
    }
}
static ROUNDING: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_fp_types.cdsl", binding: constructors::ROUNDING, hosts: { ROUNDING_validate => valid, ROUNDING_carrier => |_: &Terms, _: &[TypeArg]| Some(TypeArg::Nat(3)) } },
);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundingType;
impl RoundingType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context
            .terms()
            .intern(ROUNDING.id(context), vec![])
            .expect("rounding")
    }
    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        ROUNDING.is(terms, ty).then_some(Self)
    }
    pub fn spelling(context: &Context) -> TypeSpelling {
        ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_fp_types.cdsl", binding: spellings::RoundingType, context: context, hosts: { RoundingType_parse => |_, _, context| Ok(RoundingType::new(context)), RoundingType_print => |_, _, fmt| fmt.write(concat!("fp.", "rounding")), RoundingType_ctor_id => |context| { ROUNDING.id(context) } } }
    }
}
