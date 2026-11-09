use std::collections::{BTreeMap, HashMap};

use tir::attributes::AttributeValue;
use tir::builtin::{FloatType, IntegerType, UnitType, ops as bops};
use tir::cfg::ops as cbops;
use tir::func::ops as func_ops;
use tir::vector::VectorType;
use tir::{BlockHandle, Context, OpHandle, Operation, TypeId, ValueId};

use super::*;

const MAGIC: u32 = 0x0723_0203;

type Result<T> = std::result::Result<T, String>;

pub fn write_binary(_context: &Context, _root: &OpHandle) -> Result<Vec<u8>> {
    Err("SPIR-V binary emission is unavailable during the IR migration".into())
}

pub fn read_binary(context: &Context, bytes: &[u8]) -> Result<OpHandle> {
    if !bytes.len().is_multiple_of(4) {
        return Err("SPIR-V binary length is not a multiple of four".into());
    }
    let words = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word))
        .collect::<Vec<_>>();
    Reader::new(context, &words)?.read()
}

#[derive(Clone)]
struct RawInst {
    opcode: u16,
    operands: Vec<u32>,
}

struct Reader<'a> {
    context: &'a Context,
    version: u32,
    instructions: Vec<RawInst>,
    names: HashMap<u32, String>,
    decorations: HashMap<u32, Vec<(u32, Vec<u32>)>>,
    types: HashMap<u32, TypeId>,
    function_types: HashMap<u32, (u32, Vec<u32>)>,
    values: HashMap<u32, ValueId>,
}

impl<'a> Reader<'a> {
    fn new(context: &'a Context, words: &[u32]) -> Result<Self> {
        if words.len() < 5 || words[0] != MAGIC {
            return Err("invalid SPIR-V header".into());
        }
        let mut instructions = Vec::new();
        let mut offset = 5;
        while offset < words.len() {
            let word_count = (words[offset] >> 16) as usize;
            let opcode = words[offset] as u16;
            if word_count == 0 || offset + word_count > words.len() {
                return Err("invalid SPIR-V instruction length".into());
            }
            instructions.push(RawInst {
                opcode,
                operands: words[offset + 1..offset + word_count].to_vec(),
            });
            offset += word_count;
        }
        Ok(Self {
            context,
            version: words[1],
            instructions,
            names: HashMap::new(),
            decorations: HashMap::new(),
            types: HashMap::new(),
            function_types: HashMap::new(),
            values: HashMap::new(),
        })
    }

    fn read(mut self) -> Result<OpHandle> {
        self.collect_metadata()?;
        self.collect_types()?;
        let root = bops::module(
            self.context,
            None,
            Default::default(),
            tir::InsertionPoint::Root,
        );
        let region = self.context.create_region();
        let block = self.context.create_block(vec![]);
        region.add_block(block.id());
        super::r#module(
            self.context,
            AttributeValue::Str(version_string(self.version).into()),
            AttributeValue::Str(self.addressing_model()?.into()),
            AttributeValue::Str(self.memory_model()?.into()),
            Some(region.id()),
            Default::default(),
            bops::module_body(&root).end(),
        );
        let body = block;
        for inst in self.instructions.clone() {
            if inst.opcode == 17 {
                super::r#Capability(
                    self.context,
                    AttributeValue::Str(capability_name(inst.operands[0])?.into()),
                    Default::default(),
                    body.end(),
                );
            }
        }
        self.read_globals(&body)?;
        self.read_entries(&body)?;
        self.read_functions(&body)?;
        super::r#module_end(self.context, Default::default(), body.end());
        let root_body = bops::module_body(&root);

        bops::module_end(self.context, Default::default(), root_body.end());
        Ok(root)
    }

    fn collect_metadata(&mut self) -> Result<()> {
        for inst in &self.instructions {
            match inst.opcode {
                5 => {
                    self.names
                        .insert(inst.operands[0], decode_string(&inst.operands[1..])?);
                }
                71 => {
                    self.decorations
                        .entry(inst.operands[0])
                        .or_default()
                        .push((inst.operands[1], inst.operands[2..].to_vec()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn collect_types(&mut self) -> Result<()> {
        for inst in self.instructions.clone() {
            let o = &inst.operands;
            let ty = match inst.opcode {
                19 => Some(UnitType::new(self.context)),
                20 => Some(IntegerType::new(self.context, 1)),
                21 => Some(IntegerType::new(self.context, u64::from(o[1]))),
                22 => Some(match o[1] {
                    16 => FloatType::f16(self.context),
                    32 => FloatType::f32(self.context),
                    64 => FloatType::f64(self.context),
                    width => return Err(format!("unsupported float width {width}")),
                }),
                23 => Some(VectorType::fixed(self.context, self.type_ref(o[1])?, o[2])),
                29 => {
                    let stride = self
                        .decoration_literal(o[0], 6)
                        .ok_or("OpTypeRuntimeArray requires ArrayStride")?;
                    Some(RuntimeArrayType::new(
                        self.context,
                        self.type_ref(o[1])?,
                        stride,
                    ))
                }
                32 => Some(PointerType::new(
                    self.context,
                    self.type_ref(o[2])?,
                    storage_class(o[1])?,
                )),
                33 => {
                    self.function_types.insert(o[0], (o[1], o[2..].to_vec()));
                    None
                }
                _ => None,
            };
            if let Some(ty) = ty {
                self.types.insert(o[0], ty);
            }
        }
        Ok(())
    }

    fn read_globals(&mut self, body: &BlockHandle) -> Result<()> {
        for inst in self
            .instructions
            .clone()
            .into_iter()
            .filter(|inst| inst.opcode == 59)
        {
            let o = inst.operands;
            let ty = self.type_ref(o[0])?;
            let id = o[1];
            let name = self
                .names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("global_{id}"));
            let storage = storage_class(o[2])?;
            let decorations = self.ir_decorations(id)?;
            let op = super::r#GlobalVariable(
                self.context,
                tir::attributes::AttributeValue::Str((name).into()),
                tir::attributes::AttributeValue::Str((storage.name()).into()),
                AttributeValue::Dict(Box::new(decorations)),
                ty,
                Default::default(),
                body.end(),
            );
            self.values.insert(id, op.value_results()[0]);
        }
        Ok(())
    }

    fn read_entries(&self, body: &BlockHandle) -> Result<()> {
        for inst in &self.instructions {
            if inst.opcode == 15 {
                let (name, used) = decode_string_with_len(&inst.operands[2..])?;
                let interfaces: Vec<AttributeValue> = inst.operands[2 + used..]
                    .iter()
                    .map(|id| {
                        AttributeValue::Str(
                            self.names
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| format!("global_{id}"))
                                .into(),
                        )
                    })
                    .collect();
                super::r#EntryPoint(
                    self.context,
                    tir::attributes::AttributeValue::Str(
                        (execution_model_name(inst.operands[0])?).into(),
                    ),
                    tir::attributes::AttributeValue::Str((name).into()),
                    AttributeValue::Array(interfaces.into()),
                    Default::default(),
                    body.end(),
                );
            } else if inst.opcode == 16 {
                let function = self
                    .names
                    .get(&inst.operands[0])
                    .cloned()
                    .unwrap_or_else(|| format!("function_{}", inst.operands[0]));
                super::r#ExecutionMode(
                    self.context,
                    tir::attributes::AttributeValue::Str((function).into()),
                    tir::attributes::AttributeValue::Str(
                        (execution_mode_name(inst.operands[1])?).into(),
                    ),
                    AttributeValue::Array(
                        inst.operands[2..]
                            .iter()
                            .map(|v| AttributeValue::UInt((*v).into()))
                            .collect::<Vec<_>>()
                            .into(),
                    ),
                    Default::default(),
                    body.end(),
                );
            }
        }
        Ok(())
    }

    fn read_functions(&mut self, body: &BlockHandle) -> Result<()> {
        // A constant is declared once for the module; each function spells
        // the ones it reads at its entry, where a value of its own can stand.
        let first_function = self
            .instructions
            .iter()
            .position(|inst| inst.opcode == 54)
            .unwrap_or(self.instructions.len());
        let constants: Vec<RawInst> = self.instructions[..first_function]
            .iter()
            .filter(|inst| inst.opcode == 43)
            .cloned()
            .collect();
        let mut index = 0;
        while index < self.instructions.len() {
            if self.instructions[index].opcode != 54 {
                index += 1;
                continue;
            }
            let start = index;
            while self.instructions[index].opcode != 56 {
                index += 1;
            }
            let mut chunk = self.instructions[start..=index].to_vec();
            let read: Vec<RawInst> = constants
                .iter()
                .filter(|constant| {
                    chunk
                        .iter()
                        .any(|inst| inst.operands.iter().any(|&id| id == constant.operands[1]))
                })
                .cloned()
                .collect();
            if let Some(label) = chunk.iter().position(|inst| inst.opcode == 248) {
                chunk.splice(label + 1..label + 1, read);
            }
            self.read_function(&chunk, body.end())?;
            index += 1;
        }
        Ok(())
    }

    fn read_function(
        &mut self,
        instructions: &[RawInst],
        insertion: tir::InsertionPoint,
    ) -> Result<OpHandle> {
        let header = &instructions[0].operands;
        let return_type = self.type_ref(header[0])?;
        let function_id = header[1];
        let name = self
            .names
            .get(&function_id)
            .cloned()
            .unwrap_or_else(|| format!("function_{function_id}"));
        let mut args = Vec::new();
        for inst in instructions.iter().filter(|inst| inst.opcode == 55) {
            let value = self
                .context
                .create_value(self.type_ref(inst.operands[0])?, None);
            self.values.insert(inst.operands[1], value.id());
            args.push(value);
        }
        let parameter_types: Vec<_> = args.iter().map(tir::Value::ty).collect();
        let region = self.context.create_region();
        let labels = instructions
            .iter()
            .filter(|inst| inst.opcode == 248)
            .map(|inst| inst.operands[0])
            .collect::<Vec<_>>();
        if labels.is_empty() {
            return Err("SPIR-V function has no OpLabel".into());
        }
        let mut blocks = HashMap::new();
        let mut phis: HashMap<u32, Vec<RawInst>> = HashMap::new();
        let mut current_label = None;
        for inst in instructions {
            if inst.opcode == 248 {
                current_label = Some(inst.operands[0]);
            } else if inst.opcode == 245 {
                phis.entry(current_label.ok_or("OpPhi before OpLabel")?)
                    .or_default()
                    .push(inst.clone());
            }
        }
        for (position, label) in labels.iter().enumerate() {
            let mut block_args = if position == 0 {
                std::mem::take(&mut args)
            } else {
                Vec::new()
            };
            for phi in phis.get(label).into_iter().flatten() {
                let value = self
                    .context
                    .create_value(self.type_ref(phi.operands[0])?, None);
                self.values.insert(phi.operands[1], value.id());
                block_args.push(value);
            }
            let block = self.context.create_block(block_args);
            region.add_block(block.id());
            blocks.insert(*label, block);
        }
        let mut current_label = labels[0];
        for inst in instructions {
            if inst.opcode == 248 {
                current_label = inst.operands[0];
                continue;
            }
            let block = blocks[&current_label].clone();
            let o = &inst.operands;
            let (_op_id, result) = match inst.opcode {
                43 => {
                    let ty = self.type_ref(o[0])?;
                    let op = super::r#Constant(
                        self.context,
                        decode_constant(self.context, ty, &o[2..])?,
                        ty,
                        Default::default(),
                        block.end(),
                    );
                    (op.id(), Some((o[1], op.value_results()[0])))
                }
                61 => {
                    let op = super::r#Load(
                        self.context,
                        self.value_ref(o[2])?,
                        self.type_ref(o[0])?,
                        Default::default(),
                        block.end(),
                    );
                    (op.id(), Some((o[1], op.value_results()[0])))
                }
                62 => {
                    let op = super::r#Store(
                        self.context,
                        self.value_ref(o[0])?,
                        self.value_ref(o[1])?,
                        Default::default(),
                        block.end(),
                    );
                    (op.id(), None)
                }
                65 => {
                    let op = super::r#AccessChain(
                        self.context,
                        self.value_ref(o[2])?,
                        o[3..]
                            .iter()
                            .map(|id| self.value_ref(*id))
                            .collect::<Result<Vec<_>>>()?,
                        self.type_ref(o[0])?,
                        Default::default(),
                        block.end(),
                    );
                    (op.id(), Some((o[1], op.value_results()[0])))
                }
                81 => {
                    let op = super::r#CompositeExtract(
                        self.context,
                        self.value_ref(o[2])?,
                        AttributeValue::Array(
                            o[3..]
                                .iter()
                                .map(|v| AttributeValue::UInt((*v).into()))
                                .collect::<Vec<_>>()
                                .into(),
                        ),
                        self.type_ref(o[0])?,
                        Default::default(),
                        block.end(),
                    );
                    (op.id(), Some((o[1], op.value_results()[0])))
                }
                224 => {
                    let op = super::r#ControlBarrier(
                        self.context,
                        self.value_ref(o[0])?,
                        self.value_ref(o[1])?,
                        self.value_ref(o[2])?,
                        Default::default(),
                        block.end(),
                    );
                    (op.id(), None)
                }
                225 => {
                    let op = super::r#MemoryBarrier(
                        self.context,
                        self.value_ref(o[0])?,
                        self.value_ref(o[1])?,
                        Default::default(),
                        block.end(),
                    );
                    (op.id(), None)
                }
                249 => {
                    let args = phi_edge_values(&phis, o[0], current_label)
                        .into_iter()
                        .map(|id| self.value_ref(id))
                        .collect::<Result<Vec<_>>>()?;
                    (
                        cbops::br(
                            self.context,
                            args,
                            blocks[&o[0]].id(),
                            Default::default(),
                            block.end(),
                        )
                        .id(),
                        None,
                    )
                }
                250 => {
                    let true_args = phi_edge_values(&phis, o[1], current_label)
                        .into_iter()
                        .map(|id| self.value_ref(id))
                        .collect::<Result<Vec<_>>>()?;
                    let false_args = phi_edge_values(&phis, o[2], current_label)
                        .into_iter()
                        .map(|id| self.value_ref(id))
                        .collect::<Result<Vec<_>>>()?;
                    (
                        cbops::cond_br(
                            self.context,
                            self.value_ref(o[0])?,
                            true_args,
                            false_args,
                            blocks[&o[1]].id(),
                            blocks[&o[2]].id(),
                            Default::default(),
                            block.end(),
                        )
                        .id(),
                        None,
                    )
                }
                253 => (
                    super::r#Return(
                        self.context,
                        tir::Operand::none(),
                        Default::default(),
                        block.end(),
                    )
                    .id(),
                    None,
                ),
                254 => (
                    super::r#Return(
                        self.context,
                        self.value_ref(o[0])?,
                        Default::default(),
                        block.end(),
                    )
                    .id(),
                    None,
                ),
                // Merge instructions describe the structure the blocks
                // already have; a caller asking for it restructures.
                54 | 55 | 56 | 245 | 246 | 247 => continue,
                opcode => {
                    if o.len() < 2 {
                        return Err(format!("unsupported SPIR-V opcode {opcode}"));
                    }
                    let values = o[2..]
                        .iter()
                        .map(|id| self.value_ref(*id))
                        .collect::<Result<Vec<_>>>()?;
                    let (id, result) = build_generated(
                        self.context,
                        opcode,
                        &values,
                        self.type_ref(o[0])?,
                        block.end(),
                    )
                    .ok_or_else(|| format!("unsupported SPIR-V opcode {opcode}"))?;
                    (id, Some((o[1], result)))
                }
            };
            if let Some((spirv_id, value)) = result {
                self.values.insert(spirv_id, value);
            }
        }
        Ok(func_ops::func(
            self.context,
            name.as_str(),
            return_type,
            tir::builtin::FnType::new(self.context, &parameter_types, return_type),
            Some(region.id()),
            Default::default(),
            insertion,
        ))
    }

    fn type_ref(&self, id: u32) -> Result<TypeId> {
        self.types
            .get(&id)
            .copied()
            .ok_or_else(|| format!("unknown type %{id}"))
    }
    fn value_ref(&self, id: u32) -> Result<ValueId> {
        self.values
            .get(&id)
            .copied()
            .ok_or_else(|| format!("unknown value %{id}"))
    }
    fn decoration_literal(&self, id: u32, decoration: u32) -> Option<u32> {
        self.decorations
            .get(&id)?
            .iter()
            .find(|item| item.0 == decoration)?
            .1
            .first()
            .copied()
    }
    fn addressing_model(&self) -> Result<String> {
        let value = self
            .instructions
            .iter()
            .find(|i| i.opcode == 14)
            .ok_or("missing OpMemoryModel")?
            .operands[0];
        Ok(addressing_name(value)?.into())
    }
    fn memory_model(&self) -> Result<String> {
        let value = self
            .instructions
            .iter()
            .find(|i| i.opcode == 14)
            .ok_or("missing OpMemoryModel")?
            .operands[1];
        Ok(memory_model_name(value)?.into())
    }
    fn ir_decorations(&self, id: u32) -> Result<BTreeMap<String, AttributeValue>> {
        let mut result = BTreeMap::new();
        for (decoration, operands) in self.decorations.get(&id).into_iter().flatten() {
            match *decoration {
                11 => {
                    result.insert(
                        "builtin".into(),
                        AttributeValue::Str(builtin_name(operands[0])?.into()),
                    );
                }
                33 => {
                    result.insert("binding".into(), AttributeValue::UInt(operands[0].into()));
                }
                34 => {
                    result.insert(
                        "descriptor_set".into(),
                        AttributeValue::UInt(operands[0].into()),
                    );
                }
                _ => {}
            }
        }
        Ok(result)
    }
}

fn phi_edge_values(phis: &HashMap<u32, Vec<RawInst>>, target: u32, predecessor: u32) -> Vec<u32> {
    phis.get(&target)
        .into_iter()
        .flatten()
        .filter_map(|phi| {
            phi.operands[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .find(|pair| pair[1] == predecessor)
                .map(|pair| pair[0])
        })
        .collect()
}

fn decode_string(words: &[u32]) -> Result<String> {
    Ok(decode_string_with_len(words)?.0)
}
fn decode_string_with_len(words: &[u32]) -> Result<(String, usize)> {
    let bytes = words
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect::<Vec<_>>();
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .ok_or("unterminated SPIR-V string")?;
    let value =
        String::from_utf8(bytes[..end].to_vec()).map_err(|_| "invalid UTF-8 in SPIR-V string")?;
    Ok((value, (end + 1).div_ceil(4)))
}
fn decode_constant(context: &Context, ty: TypeId, words: &[u32]) -> Result<AttributeValue> {
    if let Some(i) = IntegerType::view(context, ty) {
        let bits = u64::from(words[0]) | words.get(1).map(|v| u64::from(*v) << 32).unwrap_or(0);
        return Ok(if i.width() == 1 {
            AttributeValue::Bool(bits != 0)
        } else {
            AttributeValue::UInt(bits)
        });
    }
    if let Some(f) = FloatType::view(context, ty) {
        return Ok(if f.exp_width() == 8 {
            AttributeValue::F32(f32::from_bits(words[0]))
        } else {
            AttributeValue::F64(f64::from_bits(
                u64::from(words[0]) | (u64::from(words[1]) << 32),
            ))
        });
    }
    Err("unsupported constant type".into())
}

fn version_string(word: u32) -> String {
    format!("{}.{}", word >> 16, (word >> 8) & 0xff)
}

macro_rules! names { ($number:ident, $name:ident, {$($text:literal => $value:literal),+ $(,)?}) => { fn $name(value: u32) -> Result<&'static str> { Ok(match value { $($value => $text,)+ _ => return Err(format!("unsupported {} {value}", stringify!($name))) }) } }; }
names!(addressing_number, addressing_name, {"Logical" => 0, "Physical32" => 1, "Physical64" => 2});
names!(memory_model_number, memory_model_name, {"Simple" => 0, "GLSL450" => 1, "OpenCL" => 2, "Vulkan" => 3});
names!(capability_number, capability_name, {"Matrix" => 0, "Shader" => 1, "Addresses" => 4, "Linkage" => 5, "Kernel" => 6, "Int64" => 11, "Int64Atomics" => 12, "ImageBasic" => 13, "Float64" => 10, "Int16" => 22, "Int8" => 39});
names!(execution_model_number, execution_model_name, {"GLCompute" => 5, "Kernel" => 6});
names!(execution_mode_number, execution_mode_name, {"LocalSize" => 17, "LocalSizeHint" => 18, "ContractionOff" => 31});
names!(storage_class_number, storage_class_name, {"UniformConstant" => 0, "Input" => 1, "Uniform" => 2, "Output" => 3, "Workgroup" => 4, "CrossWorkgroup" => 5, "Private" => 6, "Function" => 7, "Generic" => 8, "PushConstant" => 9, "AtomicCounter" => 10, "Image" => 11, "StorageBuffer" => 12, "PhysicalStorageBuffer" => 5349});
fn storage_class(value: u32) -> Result<StorageClass> {
    StorageClass::parse(storage_class_name(value)?)
        .ok_or_else(|| format!("unsupported storage class {value}"))
}
names!(builtin_number, builtin_name, {"Position" => 0, "WorkgroupSize" => 25, "WorkgroupId" => 26, "LocalInvocationId" => 27, "GlobalInvocationId" => 28, "LocalInvocationIndex" => 29, "NumWorkgroups" => 24});
