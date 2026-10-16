use tir_symbolic::term::valid;

use crate::ty::{Ctor, Kind, Terms, TypeArg, TypeCtor, TypeSpelling};
use crate::{Context, TypeId};

macro_rules! scalar_type {
    ($name:ident, $ctor:ident, $mnemonic:literal, $bits:literal) => {
        static $ctor: Ctor = Ctor::new(TypeCtor {
            dialect: "fp",
            name: $mnemonic,
            params: &[],
            kind: Kind::Data,
            validate: valid,
            carrier: |_: &Terms, _: &[TypeArg]| Some(TypeArg::Nat($bits)),
        });

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name;

        impl $name {
            #[allow(clippy::new_ret_no_self)]
            pub fn new(context: &Context) -> TypeId {
                context
                    .terms()
                    .intern($ctor.id(context), vec![])
                    .expect($mnemonic)
            }

            pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
                let terms = terms.as_ref();
                $ctor.is(terms, ty).then_some(Self)
            }

            pub fn spelling(context: &Context) -> TypeSpelling {
                TypeSpelling {
                    ctor: $ctor.id(context),
                    keys: &[$mnemonic],
                    parse: |_, _, context| Ok(Self::new(context)),
                    print: |_, _, fmt| fmt.write(concat!("fp.", $mnemonic)),
                    variadic_tail: false,
                }
            }
        }
    };
}

scalar_type!(EnvironmentType, ENV, "env", 13);
scalar_type!(RoundingType, ROUNDING, "rounding", 3);
