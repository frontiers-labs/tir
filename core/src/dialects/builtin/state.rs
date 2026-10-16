use tir_symbolic::term::ctor;

use crate::parse::common::Cursor;
use crate::ty::TypeSpelling;
use crate::{Context, Error, TypeId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateResource {
    Memory,
    FpEnv,
}

impl StateResource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::FpEnv => "fp.env",
        }
    }

    pub fn semantic_code(self) -> u64 {
        match self {
            Self::Memory => tir_symbolic::lang::StateResourceKind::Memory as u64,
            Self::FpEnv => tir_symbolic::lang::StateResourceKind::FpEnvironment as u64,
        }
    }
}

/// An execution dependency for one resource, written `!state<resource>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateType {
    resource: StateResource,
}

impl StateType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(_context: &Context, resource: StateResource) -> TypeId {
        match resource {
            StateResource::Memory => TypeId::STATE,
            StateResource::FpEnv => TypeId::FP_ENV,
        }
    }

    pub fn memory(context: &Context) -> TypeId {
        Self::new(context, StateResource::Memory)
    }

    pub fn fp_env(context: &Context) -> TypeId {
        Self::new(context, StateResource::FpEnv)
    }

    /// The state `ty` is. Every store interns the two resources above first,
    /// so telling a state from a value compares ids and reads no term.
    pub fn view(ty: TypeId) -> Option<Self> {
        let resource = if ty == TypeId::STATE {
            StateResource::Memory
        } else if ty == TypeId::FP_ENV {
            StateResource::FpEnv
        } else {
            return None;
        };
        Some(Self { resource })
    }

    pub fn resource(&self) -> StateResource {
        self.resource
    }

    pub fn spelling(_context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: ctor::STATE,
            keys: &["state"],
            parse: |_, parser, context| {
                if !parser.parse_token("<") {
                    return Err((parser.span(), Error::ExpectedToken("<")));
                }
                let resource = [StateResource::Memory, StateResource::FpEnv]
                    .into_iter()
                    .find(|resource| parser.parse_token(resource.name()))
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("state resource")))?;
                if !parser.parse_token(">") {
                    return Err((parser.span(), Error::ExpectedToken(">")));
                }
                Ok(Self::new(context, resource))
            },
            print: |spellings, ty, fmt| match Self::view(ty) {
                Some(state) => fmt.write(format!("state<{}>", state.resource.name())),
                None => spellings.print_generic(ty, fmt),
            },
            variadic_tail: false,
        }
    }
}
