use std::any::Any;
use std::sync::Arc;

use tir::parse::Span;
use tir::{Context, Error, IRFormatter, Type, TypeConstraint, TypeId};

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

impl StorageClass {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "UniformConstant" => Self::UniformConstant,
            "Input" => Self::Input,
            "Uniform" => Self::Uniform,
            "Output" => Self::Output,
            "Workgroup" => Self::Workgroup,
            "CrossWorkgroup" => Self::CrossWorkgroup,
            "Private" => Self::Private,
            "Function" => Self::Function,
            "Generic" => Self::Generic,
            "PushConstant" => Self::PushConstant,
            "AtomicCounter" => Self::AtomicCounter,
            "Image" => Self::Image,
            "StorageBuffer" => Self::StorageBuffer,
            "PhysicalStorageBuffer" => Self::PhysicalStorageBuffer,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::UniformConstant => "UniformConstant",
            Self::Input => "Input",
            Self::Uniform => "Uniform",
            Self::Output => "Output",
            Self::Workgroup => "Workgroup",
            Self::CrossWorkgroup => "CrossWorkgroup",
            Self::Private => "Private",
            Self::Function => "Function",
            Self::Generic => "Generic",
            Self::PushConstant => "PushConstant",
            Self::AtomicCounter => "AtomicCounter",
            Self::Image => "Image",
            Self::StorageBuffer => "StorageBuffer",
            Self::PhysicalStorageBuffer => "PhysicalStorageBuffer",
        }
    }
}

pub struct PointerType {
    pointee: Arc<dyn Type>,
    storage_class: StorageClass,
}

impl PointerType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, pointee: TypeId, storage_class: StorageClass) -> TypeId {
        context.get_type_id(Arc::new(Self {
            pointee: context.get_type_data(pointee),
            storage_class,
        }))
    }

    pub fn pointee(&self, context: &Context) -> TypeId {
        context.get_type_id(self.pointee.clone())
    }

    pub fn storage_class(&self) -> StorageClass {
        self.storage_class
    }
}

impl TypeConstraint for PointerType {}

impl Type for PointerType {
    fn dialect(&self) -> &'static str {
        "spirv"
    }

    fn parse_key() -> &'static str {
        "ptr"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        use tir::parse::common::Cursor;
        expect(parser, "<")?;
        let pointee = parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        expect(parser, ",")?;
        let storage_name = parser
            .parse_ident()
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("storage class")))?;
        let storage_class = StorageClass::parse(storage_name)
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("storage class")))?;
        expect(parser, ">")?;
        Ok(Self::new(context, pointee, storage_class))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("ptr<")?;
        print_nested_type(fmt, self.pointee.as_ref())?;
        fmt.write(format!(", {}>", self.storage_class.name()))
    }

    fn eq(&self, other: &dyn Type) -> bool {
        let Some(other) = (other as &dyn Any).downcast_ref::<Self>() else {
            return false;
        };
        self.storage_class == other.storage_class && self.pointee.eq(other.pointee.as_ref())
    }
}

pub struct RuntimeArrayType {
    element: Arc<dyn Type>,
    stride: u32,
}

impl RuntimeArrayType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, element: TypeId, stride: u32) -> TypeId {
        context.get_type_id(Arc::new(Self {
            element: context.get_type_data(element),
            stride,
        }))
    }

    pub fn element(&self, context: &Context) -> TypeId {
        context.get_type_id(self.element.clone())
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }
}

impl TypeConstraint for RuntimeArrayType {}

impl Type for RuntimeArrayType {
    fn dialect(&self) -> &'static str {
        "spirv"
    }

    fn parse_key() -> &'static str {
        "rtarray"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        use tir::parse::common::Cursor;
        expect(parser, "<")?;
        let element = parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        expect(parser, ",")?;
        if parser.parse_ident() != Some("stride") {
            return Err((parser.span(), Error::ExpectedToken("stride")));
        }
        expect(parser, "=")?;
        let stride = parser
            .parse_number()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("array stride")))?;
        expect(parser, ">")?;
        Ok(Self::new(context, element, stride))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("rtarray<")?;
        print_nested_type(fmt, self.element.as_ref())?;
        fmt.write(format!(", stride={}>", self.stride))
    }

    fn eq(&self, other: &dyn Type) -> bool {
        let Some(other) = (other as &dyn Any).downcast_ref::<Self>() else {
            return false;
        };
        self.stride == other.stride && self.element.eq(other.element.as_ref())
    }
}

fn print_nested_type(fmt: &mut IRFormatter<'_>, ty: &dyn Type) -> Result<(), std::fmt::Error> {
    fmt.write("!")?;
    if ty.dialect() != "builtin" {
        fmt.write(format!("{}.", ty.dialect()))?;
    }
    ty.print(fmt)
}

fn expect(
    parser: &mut tir::parse::text::Parser<'_>,
    token: &'static str,
) -> Result<(), (Span, Error)> {
    use tir::parse::common::Cursor;
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}
