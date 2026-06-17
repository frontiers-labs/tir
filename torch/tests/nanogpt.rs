//! Minimal viable demo: build the forward pass of a nanoGPT-style model as
//! `torch`-dialect IR, verify it, and check that it round-trips through the
//! textual IR parser. No flash attention, no dropout, no autograd — plain
//! tensor ops in their simplest form.
//!
//! Reference: https://github.com/karpathy/nanoGPT/blob/master/model.py

use tir::builtin::{ModuleOp, ops as builtin_ops};
use tir::parse::ir::parse_ir;
use tir::{Context, IRBuilder, IRFormatter, Operation, TypeId, ValueId};

use tir_torch::*;

/// A tiny GPT configuration. Real dimensions are irrelevant to the IR (shapes
/// are descriptive), so we keep them small and legible.
struct Config {
    n_layer: i64,
    n_head: i64,
    n_embd: i64,
    block_size: i64,
    vocab_size: i64,
}

impl Config {
    fn head_size(&self) -> i64 {
        self.n_embd / self.n_head
    }
}

fn tensor(ctx: &Context, dt: DType, shape: &[i64]) -> TypeId {
    TensorType::new(ctx, dt, shape.to_vec())
}

fn f32t(ctx: &Context, shape: &[i64]) -> TypeId {
    tensor(ctx, DType::F32, shape)
}

// One helper per op, each inserting into the block and returning its result.

fn get_attr(ctx: &Context, b: &mut IRBuilder, name: &str, ty: TypeId) -> ValueId {
    b.insert(
        GetAttrOpBuilder::new(ctx)
            .attr("name", name.into())
            .result_type(ty)
            .build(),
    )
    .result()
}

fn arange(ctx: &Context, b: &mut IRBuilder, n: i64, ty: TypeId) -> ValueId {
    b.insert(
        ArangeOpBuilder::new(ctx)
            .attr("n", n.into())
            .result_type(ty)
            .build(),
    )
    .result()
}

fn embedding(
    ctx: &Context,
    b: &mut IRBuilder,
    weight: ValueId,
    idx: ValueId,
    ty: TypeId,
) -> ValueId {
    b.insert(
        EmbeddingOpBuilder::new(ctx)
            .weight(weight)
            .indices(idx)
            .result_type(ty)
            .build(),
    )
    .result()
}

fn add(ctx: &Context, b: &mut IRBuilder, lhs: ValueId, rhs: ValueId, ty: TypeId) -> ValueId {
    b.insert(
        AddOpBuilder::new(ctx)
            .lhs(lhs)
            .rhs(rhs)
            .result_type(ty)
            .build(),
    )
    .result()
}

fn mul(ctx: &Context, b: &mut IRBuilder, lhs: ValueId, rhs: ValueId, ty: TypeId) -> ValueId {
    b.insert(
        MulOpBuilder::new(ctx)
            .lhs(lhs)
            .rhs(rhs)
            .result_type(ty)
            .build(),
    )
    .result()
}

fn matmul(ctx: &Context, b: &mut IRBuilder, lhs: ValueId, rhs: ValueId, ty: TypeId) -> ValueId {
    b.insert(
        MatmulOpBuilder::new(ctx)
            .lhs(lhs)
            .rhs(rhs)
            .result_type(ty)
            .build(),
    )
    .result()
}

fn linear(
    ctx: &Context,
    b: &mut IRBuilder,
    input: ValueId,
    weight: ValueId,
    bias: Option<ValueId>,
    ty: TypeId,
) -> ValueId {
    let mut builder = LinearOpBuilder::new(ctx)
        .input(input)
        .weight(weight)
        .result_type(ty);
    if let Some(bias) = bias {
        builder = builder.bias(bias);
    }
    b.insert(builder.build()).result()
}

fn layer_norm(
    ctx: &Context,
    b: &mut IRBuilder,
    input: ValueId,
    weight: ValueId,
    bias: ValueId,
    ty: TypeId,
) -> ValueId {
    b.insert(
        LayerNormOpBuilder::new(ctx)
            .input(input)
            .weight(weight)
            .bias(bias)
            .result_type(ty)
            .build(),
    )
    .result()
}

fn gelu(ctx: &Context, b: &mut IRBuilder, input: ValueId, ty: TypeId) -> ValueId {
    b.insert(GeluOpBuilder::new(ctx).input(input).result_type(ty).build())
        .result()
}

fn softmax(ctx: &Context, b: &mut IRBuilder, input: ValueId, dim: i64, ty: TypeId) -> ValueId {
    b.insert(
        SoftmaxOpBuilder::new(ctx)
            .input(input)
            .attr("dim", dim.into())
            .result_type(ty)
            .build(),
    )
    .result()
}

fn transpose(
    ctx: &Context,
    b: &mut IRBuilder,
    input: ValueId,
    dim0: i64,
    dim1: i64,
    ty: TypeId,
) -> ValueId {
    b.insert(
        TransposeOpBuilder::new(ctx)
            .input(input)
            .attr("dim0", dim0.into())
            .attr("dim1", dim1.into())
            .result_type(ty)
            .build(),
    )
    .result()
}

fn reshape(ctx: &Context, b: &mut IRBuilder, input: ValueId, ty: TypeId) -> ValueId {
    b.insert(
        ReshapeOpBuilder::new(ctx)
            .input(input)
            .result_type(ty)
            .build(),
    )
    .result()
}

fn causal_mask(ctx: &Context, b: &mut IRBuilder, input: ValueId, ty: TypeId) -> ValueId {
    b.insert(
        CausalMaskOpBuilder::new(ctx)
            .input(input)
            .result_type(ty)
            .build(),
    )
    .result()
}

fn ln_weights(ctx: &Context, b: &mut IRBuilder, prefix: &str, cfg: &Config) -> (ValueId, ValueId) {
    let vec_ty = f32t(ctx, &[cfg.n_embd]);
    (
        get_attr(ctx, b, &format!("{prefix}.weight"), vec_ty),
        get_attr(ctx, b, &format!("{prefix}.bias"), vec_ty),
    )
}

/// `CausalSelfAttention.forward`, written with separate q/k/v projections
/// instead of the fused `c_attn` + split — same math, single-result ops.
fn attention(ctx: &Context, b: &mut IRBuilder, x: ValueId, l: i64, cfg: &Config) -> ValueId {
    let (t, c, h, hs) = (cfg.block_size, cfg.n_embd, cfg.n_head, cfg.head_size());
    let p = format!("transformer.h.{l}.attn");

    let proj = |b: &mut IRBuilder, kind: &str| {
        let w = get_attr(ctx, b, &format!("{p}.{kind}.weight"), f32t(ctx, &[c, c]));
        linear(ctx, b, x, w, None, f32t(ctx, &[1, t, c]))
    };
    let q = proj(b, "q");
    let k = proj(b, "k");
    let v = proj(b, "v");

    // (1, T, C) -> (1, T, H, hs) -> (1, H, T, hs)
    let to_heads = |b: &mut IRBuilder, v: ValueId| {
        let r = reshape(ctx, b, v, f32t(ctx, &[1, t, h, hs]));
        transpose(ctx, b, r, 1, 2, f32t(ctx, &[1, h, t, hs]))
    };
    let q = to_heads(b, q);
    let k = to_heads(b, k);
    let v = to_heads(b, v);

    // att = softmax(causal_mask((q @ k^T) * scale)) @ v
    let k_t = transpose(ctx, b, k, 2, 3, f32t(ctx, &[1, h, hs, t]));
    let att = matmul(ctx, b, q, k_t, f32t(ctx, &[1, h, t, t]));
    let scale = get_attr(ctx, b, &format!("{p}.scale"), f32t(ctx, &[]));
    let att = mul(ctx, b, att, scale, f32t(ctx, &[1, h, t, t]));
    let att = causal_mask(ctx, b, att, f32t(ctx, &[1, h, t, t]));
    let att = softmax(ctx, b, att, 3, f32t(ctx, &[1, h, t, t]));
    let y = matmul(ctx, b, att, v, f32t(ctx, &[1, h, t, hs]));

    // (1, H, T, hs) -> (1, T, H, hs) -> (1, T, C)
    let y = transpose(ctx, b, y, 1, 2, f32t(ctx, &[1, t, h, hs]));
    let y = reshape(ctx, b, y, f32t(ctx, &[1, t, c]));

    let w = get_attr(ctx, b, &format!("{p}.c_proj.weight"), f32t(ctx, &[c, c]));
    let bias = get_attr(ctx, b, &format!("{p}.c_proj.bias"), f32t(ctx, &[c]));
    linear(ctx, b, y, w, Some(bias), f32t(ctx, &[1, t, c]))
}

/// The `MLP.forward`: c_fc -> gelu -> c_proj.
fn mlp(ctx: &Context, b: &mut IRBuilder, x: ValueId, l: i64, cfg: &Config) -> ValueId {
    let (t, c) = (cfg.block_size, cfg.n_embd);
    let hidden = 4 * c;
    let p = format!("transformer.h.{l}.mlp");

    let w_fc = get_attr(ctx, b, &format!("{p}.c_fc.weight"), f32t(ctx, &[c, hidden]));
    let b_fc = get_attr(ctx, b, &format!("{p}.c_fc.bias"), f32t(ctx, &[hidden]));
    let h = linear(ctx, b, x, w_fc, Some(b_fc), f32t(ctx, &[1, t, hidden]));
    let h = gelu(ctx, b, h, f32t(ctx, &[1, t, hidden]));

    let w_proj = get_attr(
        ctx,
        b,
        &format!("{p}.c_proj.weight"),
        f32t(ctx, &[hidden, c]),
    );
    let b_proj = get_attr(ctx, b, &format!("{p}.c_proj.bias"), f32t(ctx, &[c]));
    linear(ctx, b, h, w_proj, Some(b_proj), f32t(ctx, &[1, t, c]))
}

/// One transformer `Block`: x = x + attn(ln_1(x)); x = x + mlp(ln_2(x)).
fn block(ctx: &Context, b: &mut IRBuilder, x: ValueId, l: i64, cfg: &Config) -> ValueId {
    let (t, c) = (cfg.block_size, cfg.n_embd);

    let (w1, b1) = ln_weights(ctx, b, &format!("transformer.h.{l}.ln_1"), cfg);
    let n1 = layer_norm(ctx, b, x, w1, b1, f32t(ctx, &[1, t, c]));
    let a = attention(ctx, b, n1, l, cfg);
    let x = add(ctx, b, x, a, f32t(ctx, &[1, t, c]));

    let (w2, b2) = ln_weights(ctx, b, &format!("transformer.h.{l}.ln_2"), cfg);
    let n2 = layer_norm(ctx, b, x, w2, b2, f32t(ctx, &[1, t, c]));
    let m = mlp(ctx, b, n2, l, cfg);
    add(ctx, b, x, m, f32t(ctx, &[1, t, c]))
}

/// `GPT.forward`: embed tokens + positions, run the blocks, final norm, lm_head.
fn build_forward(ctx: &Context, cfg: &Config) -> ModuleOp {
    let (t, c, v) = (cfg.block_size, cfg.n_embd, cfg.vocab_size);

    let logits_ty = f32t(ctx, &[1, t, v]);
    let idx_ty = tensor(ctx, DType::I64, &[1, t]);

    // func @forward(%idx: !torch.tensor<i64, 1, T>) -> logits
    let idx = ctx.create_value(idx_ty, None);
    let idx_id = idx.id();
    let region = ctx.create_region();
    let entry = ctx.create_block(vec![idx]);
    region.add_block(entry.id());
    let func = builtin_ops::func(ctx, "forward", logits_ty, Some(region.id())).build();

    let mut b = IRBuilder::new(func.body());

    let wte = get_attr(ctx, &mut b, "transformer.wte.weight", f32t(ctx, &[v, c]));
    let wpe = get_attr(ctx, &mut b, "transformer.wpe.weight", f32t(ctx, &[t, c]));
    let tok_emb = embedding(ctx, &mut b, wte, idx_id, f32t(ctx, &[1, t, c]));
    let pos = arange(ctx, &mut b, t, tensor(ctx, DType::I64, &[t]));
    let pos_emb = embedding(ctx, &mut b, wpe, pos, f32t(ctx, &[t, c]));
    let mut x = add(ctx, &mut b, tok_emb, pos_emb, f32t(ctx, &[1, t, c]));

    for l in 0..cfg.n_layer {
        x = block(ctx, &mut b, x, l, cfg);
    }

    let (wf, bf) = ln_weights(ctx, &mut b, "transformer.ln_f", cfg);
    let x = layer_norm(ctx, &mut b, x, wf, bf, f32t(ctx, &[1, t, c]));
    let head_w = get_attr(ctx, &mut b, "lm_head.weight", f32t(ctx, &[c, v]));
    let logits = linear(ctx, &mut b, x, head_w, None, logits_ty);

    b.insert(builtin_ops::r#return(ctx, logits).build());

    // Wrap the function in a module.
    let m_region = ctx.create_region();
    let m_block = ctx.create_block(vec![]);
    m_region.add_block(m_block.id());
    let module = builtin_ops::module(ctx, Some(m_region.id())).build();
    let mut mb = IRBuilder::new(module.body());
    mb.insert(func);
    mb.insert(builtin_ops::module_end(ctx).build());
    module
}

fn print_op(op: &impl Operation) -> String {
    let mut s = String::new();
    let mut f = IRFormatter::new(&mut s);
    op.print(&mut f).expect("print");
    s
}

#[test]
fn nanogpt_forward_builds_verifies_and_roundtrips() {
    let ctx = Context::with_default_dialects();
    ctx.register_dialect::<TorchDialect>();

    let cfg = Config {
        n_layer: 2,
        n_head: 2,
        n_embd: 8,
        block_size: 4,
        vocab_size: 16,
    };

    let module = build_forward(&ctx, &cfg);
    assert!(module.verify(&ctx).is_ok(), "module must verify");

    let text = print_op(&module);

    // Sanity: the graph contains the signature ops of a GPT forward pass.
    for needle in [
        "torch.get_attr",
        "torch.embedding",
        "torch.layer_norm",
        "torch.matmul",
        "torch.softmax",
        "torch.causal_mask",
        "torch.gelu",
        "torch.linear",
        "!torch.tensor<f32, 1, 4, 16>",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }

    // Round-trip: parse the printed IR into a fresh context (so SSA numbering
    // restarts from the same base) and print again; the two must match.
    let ctx2 = Context::with_default_dialects();
    ctx2.register_dialect::<TorchDialect>();
    let parsed = parse_ir::<ModuleOp>(&ctx2, &text).expect("re-parse module");
    assert!(parsed.verify(&ctx2).is_ok());
    assert_eq!(text, print_op(&parsed), "IR is not stable under round-trip");
}
