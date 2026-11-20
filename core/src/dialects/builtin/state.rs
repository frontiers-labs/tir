use tir_symbolic::term::ctor;

use crate::parse::common::Cursor;
use crate::ty::TypeSpelling;
use crate::{Context, Error, TypeId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StateResource {
    Memory,
}

impl StateResource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Memory => "memory",
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
        }
    }

    pub fn memory(context: &Context) -> TypeId {
        Self::new(context, StateResource::Memory)
    }

    /// The state `ty` is. Every store interns memory state first, so this
    /// compares ids and reads no term.
    pub fn view(ty: TypeId) -> Option<Self> {
        (ty == TypeId::STATE).then_some(Self {
            resource: StateResource::Memory,
        })
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
                if !parser.parse_token(StateResource::Memory.name()) {
                    return Err((parser.span(), Error::ExpectedToken("state resource")));
                }
                if !parser.parse_token(">") {
                    return Err((parser.span(), Error::ExpectedToken(">")));
                }
                Ok(Self::memory(context))
            },
            print: |spellings, ty, fmt| match Self::view(ty) {
                Some(state) => fmt.write(format!("state<{}>", state.resource.name())),
                None => spellings.print_generic(ty, fmt),
            },
            variadic_tail: false,
        }
    }
}
