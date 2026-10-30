use nisyros::capture_vulkan_package;
use nisyros::package::{ARG_DEVICE_ADDRESS, ARG_VALUE, ArgumentField, BinaryKey, Package};
use std::env;
use std::fs;

fn number(text: &str) -> Result<u32, String> {
    text.parse().map_err(|_| format!("invalid integer: {text}"))
}

fn field(text: &str) -> Result<ArgumentField, String> {
    let parts: Vec<_> = text.split(':').collect();
    match *parts.as_slice() {
        ["addr", offset] => Ok(ArgumentField {
            offset: number(offset)?,
            width: 8,
            kind: ARG_DEVICE_ADDRESS,
        }),
        ["value", offset, width] => Ok(ArgumentField {
            offset: number(offset)?,
            width: number(width)?,
            kind: ARG_VALUE,
        }),
        _ => Err(format!("invalid argument field: {text}")),
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 10 {
        return Err("usage: nisyros-pack <spirv> <package> <entry> <arg-size> <arg-align> <group-x> <group-y> <group-z> <addr:offset|value:offset:width>...".into());
    }
    let bytes = fs::read(&args[1]).map_err(|error| error.to_string())?;
    if !bytes.len().is_multiple_of(4) {
        return Err("SPIR-V length must be a multiple of four bytes".into());
    }
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk))
        .collect();
    let fields = args[9..]
        .iter()
        .map(|text| field(text))
        .collect::<Result<Vec<_>, _>>()?;
    let template = Package {
        global_key: BinaryKey {
            size: 1,
            bytes: [0; 32],
        },
        vendor_id: 0,
        device_id: 0,
        api_version: 0,
        entry: args[3].clone(),
        argument_size: number(&args[4])?,
        argument_alignment: number(&args[5])?,
        argument_fields: fields,
        workgroup: [number(&args[6])?, number(&args[7])?, number(&args[8])?],
        required_features: 0,
        scratch_bytes: 0,
        binaries: Vec::new(),
    };
    let device_index = env::var("NISYROS_DEVICE_INDEX")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| "invalid NISYROS_DEVICE_INDEX".to_string())
        })
        .transpose()?
        .unwrap_or(0);
    let package = capture_vulkan_package(device_index, &words, template)?;
    let encoded = package
        .encode()
        .map_err(|error| format!("invalid captured package: {error:?}"))?;
    fs::write(&args[2], encoded).map_err(|error| error.to_string())?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
