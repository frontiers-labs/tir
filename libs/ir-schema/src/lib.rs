/// The normalized structural IR and type schema.
pub const IR_SCHEMA: &str = include_str!("ir-schema.egg");

/// Build-time compiled rules and declarations of the structural IR.
pub const IR_PROGRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ir-schema.bin"));
