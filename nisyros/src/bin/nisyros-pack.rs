use nisyros::package::{
    ARG_DEVICE_ADDRESS, ARG_VALUE, ArgumentField, BinaryKey, Package, PipelineBinary,
    ROCM_CODE_OBJECT_FORMAT, VULKAN_PIPELINE_BINARY_FORMAT, XDNA_XCLBIN_FORMAT,
};
use nisyros::{
    CooperativeMatrixTuple, FEATURE_BFLOAT16_DOT, FEATURE_BFLOAT16_MATRIX, FEATURE_BFLOAT16_TYPE,
    FEATURE_COOPERATIVE_MATRIX, FEATURE_SHADER_FLOAT16, FEATURE_VULKAN_MEMORY_MODEL,
    capture_vulkan_package,
};
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

fn matrix(text: &str) -> Result<CooperativeMatrixTuple, String> {
    let parts = text.split(':').collect::<Vec<_>>();
    let ["matrix", m, n, k, a, b, c, result, saturating, scope] = *parts.as_slice() else {
        return Err(format!("invalid matrix tuple: {text}"));
    };
    Ok(CooperativeMatrixTuple {
        m: number(m)?,
        n: number(n)?,
        k: number(k)?,
        a_type: number(a)?,
        b_type: number(b)?,
        c_type: number(c)?,
        result_type: number(result)?,
        saturating_accumulation: number(saturating)?,
        scope: number(scope)?,
    })
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = env::args().collect();
    let rocm = args.get(1).is_some_and(|argument| argument == "--rocm");
    let xdna = args.get(1).is_some_and(|argument| argument == "--xdna");
    if rocm || xdna {
        args.remove(1);
    }
    if args.len() < 10 {
        return Err("usage: nisyros-pack [--rocm|--xdna] <spirv|hsaco|xclbin> <package> <entry> <arg-size> <arg-align> <group-x> <group-y> <group-z> <addr:offset|value:offset:width|matrix:m:n:k:a:b:c:result:saturation:scope|float16|memory-model|bfloat:type|bfloat:dot|bfloat:matrix>...".into());
    }
    let bytes = fs::read(&args[1]).map_err(|error| error.to_string())?;
    let mut fields = Vec::new();
    let mut required_features = 0;
    let mut required_matrices = Vec::new();
    for text in &args[9..] {
        if text.starts_with("matrix:") {
            required_matrices.push(matrix(text)?);
            required_features |= FEATURE_COOPERATIVE_MATRIX;
        } else if text == "bfloat:type" {
            required_features |= FEATURE_BFLOAT16_TYPE;
        } else if text == "float16" {
            required_features |= FEATURE_SHADER_FLOAT16;
        } else if text == "memory-model" {
            required_features |= FEATURE_VULKAN_MEMORY_MODEL;
        } else if text == "bfloat:dot" {
            required_features |= FEATURE_BFLOAT16_TYPE | FEATURE_BFLOAT16_DOT;
        } else if text == "bfloat:matrix" {
            required_features |= FEATURE_BFLOAT16_TYPE | FEATURE_BFLOAT16_MATRIX;
        } else {
            fields.push(field(text)?);
        }
    }
    if xdna {
        let layout = [
            (ARG_DEVICE_ADDRESS, 0, 8),
            (ARG_VALUE, 8, 4),
            (ARG_VALUE, 12, 4),
        ];
        if fields.len() < 3
            || fields
                .iter()
                .take(3)
                .zip(layout)
                .any(|(field, expected)| (field.kind, field.offset, field.width) != expected)
            || number(&args[4])? < 16
            || number(&args[4])? > 4080
            || [number(&args[6])?, number(&args[7])?, number(&args[8])?] != [1, 1, 1]
        {
            return Err("XDNA needs a 16 to 4080-byte argument block starting with addr:0 value:8:4 value:12:4 and a 1x1x1 grid".into());
        }
    }
    let template = Package {
        format: if xdna {
            XDNA_XCLBIN_FORMAT
        } else if rocm {
            ROCM_CODE_OBJECT_FORMAT
        } else {
            VULKAN_PIPELINE_BINARY_FORMAT
        },
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
        required_features,
        required_matrices,
        scratch_bytes: 0,
        binaries: Vec::new(),
    };
    let package = if rocm || xdna {
        if rocm && !bytes.starts_with(b"\x7fELF") && !bytes.starts_with(b"__CLANG_OFFLOAD_BUNDLE__")
        {
            return Err("ROCm input must be a HIP code object or offload bundle".into());
        }
        if xdna && !bytes.starts_with(b"xclbin2\0") {
            return Err("XDNA input must be an XCLBIN image".into());
        }
        if template.required_features != 0 || !template.required_matrices.is_empty() {
            return Err("native package feature manifests are not supported".into());
        }
        Package {
            vendor_id: if xdna { 0x1022 } else { 0x1002 },
            binaries: vec![PipelineBinary {
                key: BinaryKey {
                    size: 1,
                    bytes: [0; 32],
                },
                data: bytes,
            }],
            ..template
        }
    } else {
        if !bytes.len().is_multiple_of(4) {
            return Err("SPIR-V length must be a multiple of four bytes".into());
        }
        let words: Vec<u32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| u32::from_le_bytes(*chunk))
            .collect();
        let device_index = env::var("NISYROS_DEVICE_INDEX")
            .ok()
            .map(|value| {
                value
                    .parse::<usize>()
                    .map_err(|_| "invalid NISYROS_DEVICE_INDEX".to_string())
            })
            .transpose()?
            .unwrap_or(0);
        capture_vulkan_package(device_index, &words, template)?
    };
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
