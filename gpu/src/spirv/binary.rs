use std::any::Any;
use std::collections::{BTreeMap, HashMap};

use tir::attributes::AttributeValue;
use tir::builtin::{
    FloatType, FuncOp, IntegerType, ModuleOp as BuiltinModuleOp, UnitType, ops as bops,
};
use tir::vector::VectorType;
use tir::{BlockId, Context, IRBuilder, Operation, TypeId, ValueId};

use super::*;

const MAGIC: u32 = 0x0723_0203;

type Result<T> = std::result::Result<T, String>;

pub fn write_binary(context: &Context, root: &BuiltinModuleOp) -> Result<Vec<u8>> {
    let module = root
        .body()
        .iter(context.clone())
        .find_map(|op| op.as_op::<ModuleOp>())
        .ok_or("input has no spirv.module")?;
    let words = Writer::new(context, module).write()?;
    Ok(words.into_iter().flat_map(u32::to_le_bytes).collect())
}

pub fn read_binary(context: &Context, bytes: &[u8]) -> Result<BuiltinModuleOp> {
    if !bytes.len().is_multiple_of(4) {
        return Err("SPIR-V binary length is not a multiple of four".into());
    }
    let words = bytes
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect::<Vec<_>>();
    Reader::new(context, &words)?.read()
}

struct Writer<'a> {
    context: &'a Context,
    module: ModuleOp,
    next_id: u32,
    type_ids: HashMap<TypeId, u32>,
    value_ids: HashMap<ValueId, u32>,
    symbol_ids: HashMap<String, u32>,
    type_words: Vec<u32>,
    annotation_words: Vec<u32>,
}

impl<'a> Writer<'a> {
    fn new(context: &'a Context, module: ModuleOp) -> Self {
        Self {
            context,
            module,
            next_id: 1,
            type_ids: HashMap::new(),
            value_ids: HashMap::new(),
            symbol_ids: HashMap::new(),
            type_words: Vec::new(),
            annotation_words: Vec::new(),
        }
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn write(mut self) -> Result<Vec<u32>> {
        let ops = self
            .module
            .body()
            .iter(self.context.clone())
            .collect::<Vec<_>>();
        for op in &ops {
            if let Some(global) = op.clone().as_op::<GlobalVariableOp>() {
                let id = self.id();
                self.value_ids.insert(global.result(), id);
                self.symbol_ids.insert(attr_str(&global, "sym_name")?, id);
            } else if let Some(func) = op.clone().as_op::<FuncOp>() {
                let id = self.id();
                self.symbol_ids.insert(attr_str(&func, "sym_name")?, id);
            }
        }

        let mut capabilities = Vec::new();
        let mut entries = Vec::new();
        let mut modes = Vec::new();
        let mut debug = Vec::new();
        let mut globals = Vec::new();
        let mut functions = Vec::new();
        for op in &ops {
            if let Some(capability) = op.clone().as_op::<CapabilityOp>() {
                instruction(
                    &mut capabilities,
                    17,
                    &[capability_number(&attr_str(&capability, "name")?)?],
                );
            } else if let Some(entry) = op.clone().as_op::<EntryPointOp>() {
                self.write_entry(&entry, &mut entries)?;
            } else if let Some(mode) = op.clone().as_op::<ExecutionModeOp>() {
                self.write_mode(&mode, &mut modes)?;
            } else if let Some(global) = op.clone().as_op::<GlobalVariableOp>() {
                self.write_global(&global, &mut debug, &mut globals)?;
            } else if let Some(func) = op.clone().as_op::<FuncOp>() {
                self.write_function(&func, &mut debug, &mut functions)?;
            }
        }
        let addressing = addressing_number(&attr_str(&self.module, "addressing_model")?)?;
        let memory = memory_model_number(&attr_str(&self.module, "memory_model")?)?;
        let mut body = Vec::new();
        body.extend(capabilities);
        instruction(&mut body, 14, &[addressing, memory]);
        body.extend(entries);
        body.extend(modes);
        body.extend(debug);
        body.extend(self.annotation_words);
        body.extend(self.type_words);
        body.extend(globals);
        body.extend(functions);
        let version = version_word(&attr_str(&self.module, "version")?)?;
        let mut result = vec![MAGIC, version, 0, self.next_id, 0];
        result.extend(body);
        Ok(result)
    }

    fn write_entry(&self, op: &EntryPointOp, out: &mut Vec<u32>) -> Result<()> {
        let function = attr_str(op, "function")?;
        let mut operands = vec![
            execution_model_number(&attr_str(op, "execution_model")?)?,
            *self
                .symbol_ids
                .get(&function)
                .ok_or_else(|| format!("unknown function @{function}"))?,
        ];
        operands.extend(encode_string(&function));
        for name in attr_array(op, "interfaces")? {
            let AttributeValue::Str(name) = name else {
                return Err("entry point interfaces must be strings".into());
            };
            operands.push(
                *self
                    .symbol_ids
                    .get(&name)
                    .ok_or_else(|| format!("unknown interface @{name}"))?,
            );
        }
        instruction(out, 15, &operands);
        Ok(())
    }

    fn write_mode(&self, op: &ExecutionModeOp, out: &mut Vec<u32>) -> Result<()> {
        let function = attr_str(op, "function")?;
        let mut operands = vec![
            *self
                .symbol_ids
                .get(&function)
                .ok_or_else(|| format!("unknown function @{function}"))?,
            execution_mode_number(&attr_str(op, "mode")?)?,
        ];
        operands.extend(
            attr_array(op, "values")?
                .iter()
                .map(attr_u32)
                .collect::<Result<Vec<_>>>()?,
        );
        instruction(out, 16, &operands);
        Ok(())
    }

    fn write_global(
        &mut self,
        op: &GlobalVariableOp,
        debug: &mut Vec<u32>,
        out: &mut Vec<u32>,
    ) -> Result<()> {
        let result = op.result();
        let id = self.value_ids[&result];
        let name = attr_str(op, "sym_name")?;
        write_name(debug, id, &name);
        let ty = self.context.get_value(result).ty();
        let type_id = self.type_id(ty)?;
        let storage = storage_class_number(&attr_str(op, "storage_class")?)?;
        let decorations = attr_dict(op, "decorations")?;
        for (name, value) in decorations {
            let (decoration, literal) = decoration_number(&name, &value)?;
            instruction(&mut self.annotation_words, 71, &[id, decoration, literal]);
        }
        instruction(out, 59, &[type_id, id, storage]);
        Ok(())
    }

    fn write_function(
        &mut self,
        func: &FuncOp,
        debug: &mut Vec<u32>,
        out: &mut Vec<u32>,
    ) -> Result<()> {
        let name = attr_str(func, "sym_name")?;
        let function_id = self.symbol_ids[&name];
        write_name(debug, function_id, &name);
        let return_type = attr_type(func, "ret_type")?;
        let return_id = self.type_id(return_type)?;
        let entry = func.body();
        let params = entry.arguments();
        let param_types = params.iter().map(|value| value.ty()).collect::<Vec<_>>();
        let function_type = self.function_type(return_type, &param_types)?;
        instruction(out, 54, &[return_id, function_id, 0, function_type]);
        for param in params {
            let id = self.id();
            self.value_ids.insert(param.id(), id);
            instruction(out, 55, &[self.type_id(param.ty())?, id]);
        }
        let blocks = func
            .regions()
            .next()
            .unwrap()
            .iter(self.context.clone())
            .collect::<Vec<_>>();
        let block_ids = blocks
            .iter()
            .map(|block| (block.id(), self.id()))
            .collect::<HashMap<_, _>>();
        for (block_index, block) in blocks.iter().enumerate() {
            if block_index != 0 {
                for argument in block.arguments() {
                    let id = self.id();
                    self.value_ids.insert(argument.id(), id);
                }
            }
            for operation in block.iter(self.context.clone()) {
                for result in &operation.results {
                    let id = self.id();
                    self.value_ids.insert(*result, id);
                }
            }
        }
        for (block_index, block) in blocks.iter().enumerate() {
            instruction(out, 248, &[block_ids[&block.id()]]);
            if block_index != 0 {
                for (index, argument) in block.arguments().iter().enumerate() {
                    let mut operands =
                        vec![self.type_id(argument.ty())?, self.value_ids[&argument.id()]];
                    for pred in &blocks {
                        let terminator = pred
                            .iter(self.context.clone())
                            .next_back()
                            .ok_or("empty predecessor block")?;
                        if let Some(incoming) = branch_argument(terminator, block.id(), index) {
                            operands.extend([self.value(incoming)?, block_ids[&pred.id()]]);
                        }
                    }
                    if operands.len() == 2 {
                        return Err("block argument has no incoming SPIR-V edge".into());
                    }
                    instruction(out, 245, &operands);
                }
            }
            for operation in block.iter(self.context.clone()) {
                self.write_operation(operation, &block_ids, out)?;
            }
        }
        instruction(out, 56, &[]);
        Ok(())
    }

    fn write_operation(
        &mut self,
        op: std::sync::Arc<tir::OpInstance>,
        block_ids: &HashMap<BlockId, u32>,
        out: &mut Vec<u32>,
    ) -> Result<()> {
        if let Some(load) = op.clone().as_op::<LoadOp>() {
            self.write_result_op(61, load.result(), load.operands(), out)
        } else if let Some(store) = op.clone().as_op::<StoreOp>() {
            instruction(
                out,
                62,
                &[
                    self.value(store.operands()[0])?,
                    self.value(store.operands()[1])?,
                ],
            );
            Ok(())
        } else if let Some(access) = op.clone().as_op::<AccessChainOp>() {
            self.write_result_op(65, access.result(), access.operands(), out)
        } else if let Some(barrier) = op.clone().as_op::<ControlBarrierOp>() {
            instruction(
                out,
                224,
                &barrier
                    .operands()
                    .iter()
                    .map(|value| self.value(*value))
                    .collect::<Result<Vec<_>>>()?,
            );
            Ok(())
        } else if let Some(barrier) = op.clone().as_op::<MemoryBarrierOp>() {
            instruction(
                out,
                225,
                &barrier
                    .operands()
                    .iter()
                    .map(|value| self.value(*value))
                    .collect::<Result<Vec<_>>>()?,
            );
            Ok(())
        } else if let Some(extract) = op.clone().as_op::<CompositeExtractOp>() {
            let mut operands = self.result_prefix(extract.result())?;
            operands.push(self.value(extract.operands()[0])?);
            operands.extend(
                attr_array(&extract, "indices")?
                    .iter()
                    .map(attr_u32)
                    .collect::<Result<Vec<_>>>()?,
            );
            instruction(out, 81, &operands);
            Ok(())
        } else if let Some(constant) = op.clone().as_op::<ConstantOp>() {
            let mut operands = self.result_prefix(constant.result())?;
            operands.extend(constant_words(
                self.context,
                constant.result(),
                attr_value(&constant, "value")?,
            )?);
            instruction(out, 43, &operands);
            Ok(())
        } else if let Some(branch) = op.clone().as_op::<tir::builtin::BranchOp>() {
            instruction(out, 249, &[block_ids[&branch.dest()]]);
            Ok(())
        } else if let Some(branch) = op.clone().as_op::<tir::builtin::CondBranchOp>() {
            instruction(
                out,
                250,
                &[
                    self.value(branch.condition())?,
                    block_ids[&branch.true_dest()],
                    block_ids[&branch.false_dest()],
                ],
            );
            Ok(())
        } else if let Some(ret) = op.clone().as_op::<ReturnOp>() {
            if ret.operands().is_empty() {
                instruction(out, 253, &[]);
            } else {
                instruction(out, 254, &[self.value(ret.operands()[0])?]);
            }
            Ok(())
        } else if op.dialect().as_str() == "spirv" {
            let opcode = opcode_for_name(op.name().as_str())
                .ok_or_else(|| format!("unsupported SPIR-V operation {}", op.name()))?;
            if op.results.len() != 1 {
                return Err(format!("unsupported result count for {}", op.name()));
            }
            self.write_result_op(opcode, op.results[0], &op.operands, out)
        } else {
            Err(format!(
                "unsupported operation {}.{} in SPIR-V function",
                op.dialect(),
                op.name()
            ))
        }
    }

    fn write_result_op(
        &mut self,
        opcode: u16,
        result: ValueId,
        values: &[ValueId],
        out: &mut Vec<u32>,
    ) -> Result<()> {
        let mut operands = self.result_prefix(result)?;
        operands.extend(
            values
                .iter()
                .map(|value| self.value(*value))
                .collect::<Result<Vec<_>>>()?,
        );
        instruction(out, opcode, &operands);
        Ok(())
    }

    fn result_prefix(&mut self, result: ValueId) -> Result<Vec<u32>> {
        let ty = self.context.get_value(result).ty();
        let type_id = self.type_id(ty)?;
        let id = if let Some(id) = self.value_ids.get(&result) {
            *id
        } else {
            let id = self.id();
            self.value_ids.insert(result, id);
            id
        };
        Ok(vec![type_id, id])
    }

    fn value(&self, value: ValueId) -> Result<u32> {
        self.value_ids
            .get(&value)
            .copied()
            .ok_or_else(|| format!("value %{value:?} is used before definition"))
    }

    fn function_type(&mut self, return_type: TypeId, params: &[TypeId]) -> Result<u32> {
        let id = self.id();
        let mut operands = vec![id, self.type_id(return_type)?];
        for param in params {
            operands.push(self.type_id(*param)?);
        }
        instruction(&mut self.type_words, 33, &operands);
        Ok(id)
    }

    fn type_id(&mut self, ty: TypeId) -> Result<u32> {
        if let Some(id) = self.type_ids.get(&ty) {
            return Ok(*id);
        }
        let data = self.context.get_type_data(ty);
        let id = self.id();
        if let Some(integer) = (data.as_ref() as &dyn Any).downcast_ref::<IntegerType>() {
            if integer.width() == 1 {
                instruction(&mut self.type_words, 20, &[id]);
            } else {
                instruction(&mut self.type_words, 21, &[id, integer.width(), 0]);
            }
        } else if let Some(float) = (data.as_ref() as &dyn Any).downcast_ref::<FloatType>() {
            let width = float.exp_width() + float.mant_width() + 1;
            instruction(&mut self.type_words, 22, &[id, width]);
        } else if (data.as_ref() as &dyn Any).is::<UnitType>() {
            instruction(&mut self.type_words, 19, &[id]);
        } else if let Some(vector) = (data.as_ref() as &dyn Any).downcast_ref::<VectorType>() {
            let element = self.type_id(vector.element(self.context))?;
            let count = vector
                .length()
                .ok_or("SPIR-V vectors must be statically sized")?;
            instruction(&mut self.type_words, 23, &[id, element, count]);
        } else if let Some(array) = (data.as_ref() as &dyn Any).downcast_ref::<RuntimeArrayType>() {
            let element = self.type_id(array.element(self.context))?;
            instruction(&mut self.type_words, 29, &[id, element]);
            instruction(&mut self.annotation_words, 71, &[id, 6, array.stride()]);
        } else if let Some(pointer) = (data.as_ref() as &dyn Any).downcast_ref::<PointerType>() {
            let pointee = self.type_id(pointer.pointee(self.context))?;
            instruction(
                &mut self.type_words,
                32,
                &[
                    id,
                    storage_class_number(pointer.storage_class().name())?,
                    pointee,
                ],
            );
        } else {
            return Err("unsupported SPIR-V type".into());
        }
        self.type_ids.insert(ty, id);
        Ok(id)
    }
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

    fn read(mut self) -> Result<BuiltinModuleOp> {
        self.collect_metadata()?;
        self.collect_types()?;
        let root = bops::module(self.context, None).build();
        let region = self.context.create_region();
        let block = self.context.create_block(vec![]);
        region.add_block(block.id());
        let module = ModuleOpBuilder::new(self.context)
            .attr("version", AttributeValue::Str(version_string(self.version)))
            .attr(
                "addressing_model",
                AttributeValue::Str(self.addressing_model()?),
            )
            .attr("memory_model", AttributeValue::Str(self.memory_model()?))
            .body(region.id())
            .build();
        let mut body = IRBuilder::new(block);
        for inst in self.instructions.clone() {
            if inst.opcode == 17 {
                body.insert(
                    CapabilityOpBuilder::new(self.context)
                        .attr(
                            "name",
                            AttributeValue::Str(capability_name(inst.operands[0])?.into()),
                        )
                        .build(),
                );
            }
        }
        self.read_globals(&mut body)?;
        self.read_entries(&mut body)?;
        self.read_functions(&mut body)?;
        body.insert(ModuleEndOpBuilder::new(self.context).build());
        let mut root_body = IRBuilder::new(root.body());
        root_body.insert(module);
        root_body.insert(bops::module_end(self.context).build());
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
                21 => Some(IntegerType::new(self.context, o[1])),
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

    fn read_globals(&mut self, body: &mut IRBuilder) -> Result<()> {
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
            let op = GlobalVariableOpBuilder::new(self.context)
                .attr("sym_name", AttributeValue::Str(name))
                .attr("storage_class", AttributeValue::Str(storage.name().into()))
                .attr("decorations", AttributeValue::Dict(decorations))
                .result_type(ty)
                .build();
            self.values.insert(id, op.result());
            body.insert(op);
        }
        Ok(())
    }

    fn read_entries(&self, body: &mut IRBuilder) -> Result<()> {
        for inst in &self.instructions {
            if inst.opcode == 15 {
                let (name, used) = decode_string_with_len(&inst.operands[2..])?;
                let interfaces = inst.operands[2 + used..]
                    .iter()
                    .map(|id| {
                        AttributeValue::Str(
                            self.names
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| format!("global_{id}")),
                        )
                    })
                    .collect();
                body.insert(
                    EntryPointOpBuilder::new(self.context)
                        .attr(
                            "execution_model",
                            AttributeValue::Str(execution_model_name(inst.operands[0])?.into()),
                        )
                        .attr("function", AttributeValue::Str(name))
                        .attr("interfaces", AttributeValue::Array(interfaces))
                        .build(),
                );
            } else if inst.opcode == 16 {
                let function = self
                    .names
                    .get(&inst.operands[0])
                    .cloned()
                    .unwrap_or_else(|| format!("function_{}", inst.operands[0]));
                body.insert(
                    ExecutionModeOpBuilder::new(self.context)
                        .attr("function", AttributeValue::Str(function))
                        .attr(
                            "mode",
                            AttributeValue::Str(execution_mode_name(inst.operands[1])?.into()),
                        )
                        .attr(
                            "values",
                            AttributeValue::Array(
                                inst.operands[2..]
                                    .iter()
                                    .map(|v| AttributeValue::UInt((*v).into()))
                                    .collect(),
                            ),
                        )
                        .build(),
                );
            }
        }
        Ok(())
    }

    fn read_functions(&mut self, body: &mut IRBuilder) -> Result<()> {
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
            let chunk = self.instructions[start..=index].to_vec();
            body.insert(self.read_function(&chunk)?);
            index += 1;
        }
        Ok(())
    }

    fn read_function(&mut self, instructions: &[RawInst]) -> Result<FuncOp> {
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
            let (op_id, result) = match inst.opcode {
                43 => {
                    let ty = self.type_ref(o[0])?;
                    let op = ConstantOpBuilder::new(self.context)
                        .attr("value", decode_constant(self.context, ty, &o[2..])?)
                        .result_type(ty)
                        .build();
                    (op.id(), Some((o[1], op.result())))
                }
                61 => {
                    let op = LoadOpBuilder::new(self.context)
                        .pointer(self.value_ref(o[2])?)
                        .result_type(self.type_ref(o[0])?)
                        .build();
                    (op.id(), Some((o[1], op.result())))
                }
                62 => {
                    let op = StoreOpBuilder::new(self.context)
                        .pointer(self.value_ref(o[0])?)
                        .value(self.value_ref(o[1])?)
                        .build();
                    (op.id(), None)
                }
                65 => {
                    let op = AccessChainOpBuilder::new(self.context)
                        .base(self.value_ref(o[2])?)
                        .indices(
                            o[3..]
                                .iter()
                                .map(|id| self.value_ref(*id))
                                .collect::<Result<Vec<_>>>()?,
                        )
                        .result_type(self.type_ref(o[0])?)
                        .build();
                    (op.id(), Some((o[1], op.result())))
                }
                81 => {
                    let op = CompositeExtractOpBuilder::new(self.context)
                        .attr(
                            "indices",
                            AttributeValue::Array(
                                o[3..]
                                    .iter()
                                    .map(|v| AttributeValue::UInt((*v).into()))
                                    .collect(),
                            ),
                        )
                        .composite(self.value_ref(o[2])?)
                        .result_type(self.type_ref(o[0])?)
                        .build();
                    (op.id(), Some((o[1], op.result())))
                }
                224 => {
                    let op = ControlBarrierOpBuilder::new(self.context)
                        .execution_scope(self.value_ref(o[0])?)
                        .memory_scope(self.value_ref(o[1])?)
                        .memory_semantics(self.value_ref(o[2])?)
                        .build();
                    (op.id(), None)
                }
                225 => {
                    let op = MemoryBarrierOpBuilder::new(self.context)
                        .memory_scope(self.value_ref(o[0])?)
                        .memory_semantics(self.value_ref(o[1])?)
                        .build();
                    (op.id(), None)
                }
                249 => {
                    let args = phi_edge_values(&phis, o[0], current_label)
                        .into_iter()
                        .map(|id| self.value_ref(id))
                        .collect::<Result<Vec<_>>>()?;
                    (
                        bops::br(self.context, args, blocks[&o[0]].id())
                            .build()
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
                        bops::cond_br(
                            self.context,
                            self.value_ref(o[0])?,
                            true_args,
                            false_args,
                            blocks[&o[1]].id(),
                            blocks[&o[2]].id(),
                        )
                        .build()
                        .id(),
                        None,
                    )
                }
                253 => (ReturnOpBuilder::new(self.context).build().id(), None),
                254 => (
                    ReturnOpBuilder::new(self.context)
                        .value(self.value_ref(o[0])?)
                        .build()
                        .id(),
                    None,
                ),
                54 | 55 | 56 | 245 => continue,
                opcode => {
                    if o.len() < 2 {
                        return Err(format!("unsupported SPIR-V opcode {opcode}"));
                    }
                    let values = o[2..]
                        .iter()
                        .map(|id| self.value_ref(*id))
                        .collect::<Result<Vec<_>>>()?;
                    let (id, result) =
                        build_generated(self.context, opcode, &values, self.type_ref(o[0])?)
                            .ok_or_else(|| format!("unsupported SPIR-V opcode {opcode}"))?;
                    (id, Some((o[1], result)))
                }
            };
            block.insert(block.len(), op_id);
            if let Some((spirv_id, value)) = result {
                self.values.insert(spirv_id, value);
            }
        }
        Ok(bops::func(self.context, name.as_str(), return_type, Some(region.id())).build())
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

fn branch_argument(
    op: std::sync::Arc<tir::OpInstance>,
    destination: BlockId,
    index: usize,
) -> Option<ValueId> {
    if let Some(branch) = op.clone().as_op::<tir::builtin::BranchOp>()
        && branch.dest() == destination
    {
        return branch.dest_args().get(index).copied();
    }
    if let Some(branch) = op.as_op::<tir::builtin::CondBranchOp>() {
        let args = if branch.true_dest() == destination {
            branch.true_args()
        } else if branch.false_dest() == destination {
            branch.false_args()
        } else {
            return None;
        };
        return args.get(index).copied();
    }
    None
}

fn phi_edge_values(phis: &HashMap<u32, Vec<RawInst>>, target: u32, predecessor: u32) -> Vec<u32> {
    phis.get(&target)
        .into_iter()
        .flatten()
        .filter_map(|phi| {
            phi.operands[2..]
                .chunks_exact(2)
                .find(|pair| pair[1] == predecessor)
                .map(|pair| pair[0])
        })
        .collect()
}

fn instruction(out: &mut Vec<u32>, opcode: u16, operands: &[u32]) {
    out.push((((operands.len() + 1) as u32) << 16) | u32::from(opcode));
    out.extend_from_slice(operands);
}
fn encode_string(value: &str) -> Vec<u32> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(0);
    bytes.resize(bytes.len().div_ceil(4) * 4, 0);
    bytes
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
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
fn write_name(out: &mut Vec<u32>, id: u32, name: &str) {
    let mut operands = vec![id];
    operands.extend(encode_string(name));
    instruction(out, 5, &operands);
}
fn attr_value<'a>(op: &'a dyn Operation, name: &str) -> Result<&'a AttributeValue> {
    op.attributes()
        .iter()
        .find(|a| a.name == name)
        .map(|a| &a.value)
        .ok_or_else(|| format!("missing attribute {name}"))
}
fn attr_str(op: &dyn Operation, name: &str) -> Result<String> {
    match attr_value(op, name)? {
        AttributeValue::Str(v) => Ok(v.clone()),
        _ => Err(format!("attribute {name} must be a string")),
    }
}
fn attr_type(op: &dyn Operation, name: &str) -> Result<TypeId> {
    match attr_value(op, name)? {
        AttributeValue::Type(v) => Ok(*v),
        _ => Err(format!("attribute {name} must be a type")),
    }
}
fn attr_array(op: &dyn Operation, name: &str) -> Result<Vec<AttributeValue>> {
    match attr_value(op, name)? {
        AttributeValue::Array(v) => Ok(v.clone()),
        _ => Err(format!("attribute {name} must be an array")),
    }
}
fn attr_dict(op: &dyn Operation, name: &str) -> Result<BTreeMap<String, AttributeValue>> {
    match attr_value(op, name)? {
        AttributeValue::Dict(v) => Ok(v.clone()),
        _ => Err(format!("attribute {name} must be a dictionary")),
    }
}
fn attr_u32(value: &AttributeValue) -> Result<u32> {
    match value {
        AttributeValue::Int(v) => (*v)
            .try_into()
            .map_err(|_| "integer attribute is out of range".into()),
        AttributeValue::UInt(v) => (*v)
            .try_into()
            .map_err(|_| "integer attribute is out of range".into()),
        _ => Err("expected integer attribute".into()),
    }
}

fn constant_words(context: &Context, result: ValueId, value: &AttributeValue) -> Result<Vec<u32>> {
    let ty = context.get_type_data(context.get_value(result).ty());
    if let Some(i) = (ty.as_ref() as &dyn Any).downcast_ref::<IntegerType>() {
        let bits = match value {
            AttributeValue::Int(v) => *v as u64,
            AttributeValue::UInt(v) => *v,
            AttributeValue::Bool(v) => u64::from(*v),
            _ => return Err("integer constant requires integer value".into()),
        };
        return Ok(if i.width() > 32 {
            vec![bits as u32, (bits >> 32) as u32]
        } else {
            vec![bits as u32]
        });
    }
    if let Some(f) = (ty.as_ref() as &dyn Any).downcast_ref::<FloatType>() {
        let value = match value {
            AttributeValue::F32(v) => f64::from(*v),
            AttributeValue::F64(v) => *v,
            _ => return Err("float constant requires float value".into()),
        };
        return Ok(if f.exp_width() == 8 {
            vec![(value as f32).to_bits()]
        } else {
            let bits = value.to_bits();
            vec![bits as u32, (bits >> 32) as u32]
        });
    }
    Err("unsupported constant type".into())
}
fn decode_constant(context: &Context, ty: TypeId, words: &[u32]) -> Result<AttributeValue> {
    let data = context.get_type_data(ty);
    if let Some(i) = (data.as_ref() as &dyn Any).downcast_ref::<IntegerType>() {
        let bits = u64::from(words[0]) | words.get(1).map(|v| u64::from(*v) << 32).unwrap_or(0);
        return Ok(if i.width() == 1 {
            AttributeValue::Bool(bits != 0)
        } else {
            AttributeValue::UInt(bits)
        });
    }
    if let Some(f) = (data.as_ref() as &dyn Any).downcast_ref::<FloatType>() {
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

fn version_word(value: &str) -> Result<u32> {
    let (major, minor) = value.split_once('.').ok_or("version must be major.minor")?;
    Ok(
        (major.parse::<u32>().map_err(|_| "invalid SPIR-V version")? << 16)
            | (minor.parse::<u32>().map_err(|_| "invalid SPIR-V version")? << 8),
    )
}
fn version_string(word: u32) -> String {
    format!("{}.{}", word >> 16, (word >> 8) & 0xff)
}

macro_rules! names { ($number:ident, $name:ident, {$($text:literal => $value:literal),+ $(,)?}) => { fn $number(name: &str) -> Result<u32> { Ok(match name { $($text => $value,)+ _ => return Err(format!("unsupported {} {name}", stringify!($name))) }) } fn $name(value: u32) -> Result<&'static str> { Ok(match value { $($value => $text,)+ _ => return Err(format!("unsupported {} {value}", stringify!($name))) }) } }; }
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
fn decoration_number(name: &str, value: &AttributeValue) -> Result<(u32, u32)> {
    Ok(match name {
        "builtin" => {
            let AttributeValue::Str(value) = value else {
                return Err("builtin decoration must be a string".into());
            };
            (11, builtin_number(value)?)
        }
        "binding" => (33, attr_u32(value)?),
        "descriptor_set" => (34, attr_u32(value)?),
        _ => return Err(format!("unsupported decoration {name}")),
    })
}
names!(builtin_number, builtin_name, {"Position" => 0, "WorkgroupSize" => 25, "WorkgroupId" => 26, "LocalInvocationId" => 27, "GlobalInvocationId" => 28, "LocalInvocationIndex" => 29, "NumWorkgroups" => 24});
