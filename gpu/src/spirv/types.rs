use tir::{Context, Ctor, Error, Terms, TypeArg, TypeId, TypeSpelling};
use tir_symbolic::term::{no_carrier, valid};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageClass {
    UniformConstant,
    Input,
    Uniform,
    Output,
    Workgroup,
    CrossWorkgroup,
    Private,
    Function,
    Generic,
    PushConstant,
    AtomicCounter,
    Image,
    StorageBuffer,
    PhysicalStorageBuffer,
}

/// Every storage class and the name SPIR-V spells it with.
const STORAGE_CLASSES: &[(StorageClass, &str)] = &[
    (StorageClass::UniformConstant, "UniformConstant"),
    (StorageClass::Input, "Input"),
    (StorageClass::Uniform, "Uniform"),
    (StorageClass::Output, "Output"),
    (StorageClass::Workgroup, "Workgroup"),
    (StorageClass::CrossWorkgroup, "CrossWorkgroup"),
    (StorageClass::Private, "Private"),
    (StorageClass::Function, "Function"),
    (StorageClass::Generic, "Generic"),
    (StorageClass::PushConstant, "PushConstant"),
    (StorageClass::AtomicCounter, "AtomicCounter"),
    (StorageClass::Image, "Image"),
    (StorageClass::StorageBuffer, "StorageBuffer"),
    (StorageClass::PhysicalStorageBuffer, "PhysicalStorageBuffer"),
];

impl StorageClass {
    pub fn parse(name: &str) -> Option<Self> {
        STORAGE_CLASSES
            .iter()
            .find(|(_, candidate)| *candidate == name)
            .map(|&(class, _)| class)
    }

    pub fn name(self) -> &'static str {
        STORAGE_CLASSES
            .iter()
            .find(|&&(class, _)| class == self)
            .map(|&(_, name)| name)
            .expect("every storage class is in the table")
    }
}

static POINTER: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "../core/defs/types/gpu_src_spirv_types.cdsl", binding: constructors::POINTER, hosts: { POINTER_validate => valid, POINTER_carrier => no_carrier } },
);

/// A pointer into one storage class, written `!spirv.ptr<!pointee, Class>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerType {
    pointee: TypeId,
    storage_class: StorageClass,
}

impl PointerType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, pointee: TypeId, storage_class: StorageClass) -> TypeId {
        context
            .terms()
            .intern(
                POINTER.id(context),
                vec![
                    TypeArg::Type(pointee),
                    TypeArg::Sym(context.terms().name(storage_class.name())),
                ],
            )
            .expect("a pointer to a data type")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        if !POINTER.is(terms, ty) {
            return None;
        }
        let Some(TypeArg::Sym(class)) = terms.arg(ty, 1) else {
            return None;
        };
        Some(Self {
            pointee: terms.child(ty, 0)?,
            storage_class: StorageClass::parse(&terms.name_text(class))?,
        })
    }

    pub fn pointee(&self) -> TypeId {
        self.pointee
    }

    pub fn storage_class(&self) -> StorageClass {
        self.storage_class
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        {
            const __TIR_TYPE_PARSE: ::tir::TypeParser = |_, parser, context| {
                use tir::parse::common::Cursor;
                parser.expect_token("<")?;
                let pointee = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                parser.expect_token(",")?;
                let storage_name = parser
                    .parse_ident()
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("storage class")))?;
                let storage_class = StorageClass::parse(storage_name)
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("storage class")))?;
                parser.expect_token(">")?;
                Ok(PointerType::new(context, pointee, storage_class))
            };
            const __TIR_TYPE_PRINT: ::tir::TypePrinter = |spellings, ty, fmt| {
                let pointer = PointerType::view(spellings, ty).expect("a SPIR-V pointer");
                fmt.write("spirv.ptr<")?;
                spellings.print(pointer.pointee, fmt)?;
                fmt.write(format!(", {}>", pointer.storage_class.name()))
            };
            const __TIR_TYPE_CTOR_ID: fn(&::tir::Context) -> ::tir::CtorId =
                |context| POINTER.id(context);
            ::tir::helpers::type_spelling! { file: "../core/defs/types/gpu_src_spirv_types.cdsl", binding: spellings::PointerType, context: context, hosts: { PointerType_parse => __TIR_TYPE_PARSE, PointerType_print => __TIR_TYPE_PRINT, PointerType_ctor_id => __TIR_TYPE_CTOR_ID } }
        }
    }
}

static RUNTIME_ARRAY: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "../core/defs/types/gpu_src_spirv_types.cdsl", binding: constructors::RUNTIME_ARRAY, hosts: { RUNTIME_ARRAY_validate => valid, RUNTIME_ARRAY_carrier => no_carrier } },
);

/// An array of unknown length, written `!spirv.rtarray<!element, stride=N>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuntimeArrayType {
    element: TypeId,
    stride: u32,
}

impl RuntimeArrayType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, element: TypeId, stride: u32) -> TypeId {
        context
            .terms()
            .intern(
                RUNTIME_ARRAY.id(context),
                vec![TypeArg::Type(element), TypeArg::Nat(u64::from(stride))],
            )
            .expect("an array of a data type")
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        if !RUNTIME_ARRAY.is(terms, ty) {
            return None;
        }
        Some(Self {
            element: terms.child(ty, 0)?,
            stride: u32::try_from(terms.nat(ty, 1)?).ok()?,
        })
    }

    pub fn element(&self) -> TypeId {
        self.element
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn spelling(context: &Context) -> TypeSpelling {
        {
            const __TIR_TYPE_PARSE: ::tir::TypeParser = |_, parser, context| {
                use tir::parse::common::Cursor;
                parser.expect_token("<")?;
                let element = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                parser.expect_token(",")?;
                if parser.parse_ident() != Some("stride") {
                    return Err((parser.span(), Error::ExpectedToken("stride")));
                }
                parser.expect_token("=")?;
                let stride = parser
                    .parse_number()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("array stride")))?;
                parser.expect_token(">")?;
                Ok(RuntimeArrayType::new(context, element, stride))
            };
            const __TIR_TYPE_PRINT: ::tir::TypePrinter = |spellings, ty, fmt| {
                let array = RuntimeArrayType::view(spellings, ty).expect("a SPIR-V runtime array");
                fmt.write("spirv.rtarray<")?;
                spellings.print(array.element, fmt)?;
                fmt.write(format!(", stride={}>", array.stride))
            };
            const __TIR_TYPE_CTOR_ID: fn(&::tir::Context) -> ::tir::CtorId =
                |context| RUNTIME_ARRAY.id(context);
            ::tir::helpers::type_spelling! { file: "../core/defs/types/gpu_src_spirv_types.cdsl", binding: spellings::RuntimeArrayType, context: context, hosts: { RuntimeArrayType_parse => __TIR_TYPE_PARSE, RuntimeArrayType_print => __TIR_TYPE_PRINT, RuntimeArrayType_ctor_id => __TIR_TYPE_CTOR_ID } }
        }
    }
}
