"""Compile a nanoGPT forward pass to TIR through the Torch Inductor backend.

Run:  python3 demo_nanogpt.py

This builds a small GPT (the simplest form of karpathy/nanoGPT's model.py — no
flash attention, no dropout), compiles it with ``torch.compile`` using our TIR
backend, and prints the `torch`-dialect TIR the backend produced. The IR is
parsed back through the TIR bindings to confirm it is valid.
"""

import math
import os
import sys

import torch
import torch.nn as nn
import torch.nn.functional as F

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from tir_inductor import tir_backend  # noqa: E402


class CausalSelfAttention(nn.Module):
    def __init__(self, n_embd, n_head, block_size):
        super().__init__()
        self.c_attn = nn.Linear(n_embd, 3 * n_embd)
        self.c_proj = nn.Linear(n_embd, n_embd)
        self.n_head, self.n_embd = n_head, n_embd
        self.register_buffer(
            "bias",
            torch.tril(torch.ones(block_size, block_size)).view(1, 1, block_size, block_size),
        )

    def forward(self, x):
        B, T, C = x.size()
        q, k, v = self.c_attn(x).split(self.n_embd, dim=2)
        k = k.view(B, T, self.n_head, C // self.n_head).transpose(1, 2)
        q = q.view(B, T, self.n_head, C // self.n_head).transpose(1, 2)
        v = v.view(B, T, self.n_head, C // self.n_head).transpose(1, 2)
        att = (q @ k.transpose(-2, -1)) * (1.0 / math.sqrt(k.size(-1)))
        att = att.masked_fill(self.bias[:, :, :T, :T] == 0, float("-inf"))
        att = F.softmax(att, dim=-1)
        y = att @ v
        y = y.transpose(1, 2).contiguous().view(B, T, C)
        return self.c_proj(y)


class MLP(nn.Module):
    def __init__(self, n_embd):
        super().__init__()
        self.c_fc = nn.Linear(n_embd, 4 * n_embd)
        self.gelu = nn.GELU()
        self.c_proj = nn.Linear(4 * n_embd, n_embd)

    def forward(self, x):
        return self.c_proj(self.gelu(self.c_fc(x)))


class Block(nn.Module):
    def __init__(self, n_embd, n_head, block_size):
        super().__init__()
        self.ln_1 = nn.LayerNorm(n_embd)
        self.attn = CausalSelfAttention(n_embd, n_head, block_size)
        self.ln_2 = nn.LayerNorm(n_embd)
        self.mlp = MLP(n_embd)

    def forward(self, x):
        x = x + self.attn(self.ln_1(x))
        x = x + self.mlp(self.ln_2(x))
        return x


class GPT(nn.Module):
    def __init__(self, vocab, block_size, n_layer, n_head, n_embd):
        super().__init__()
        self.wte = nn.Embedding(vocab, n_embd)
        self.wpe = nn.Embedding(block_size, n_embd)
        self.h = nn.ModuleList([Block(n_embd, n_head, block_size) for _ in range(n_layer)])
        self.ln_f = nn.LayerNorm(n_embd)
        self.lm_head = nn.Linear(n_embd, vocab, bias=False)

    def forward(self, idx):
        _, T = idx.size()
        pos = torch.arange(0, T, dtype=torch.long, device=idx.device)
        x = self.wte(idx) + self.wpe(pos)
        for block in self.h:
            x = block(x)
        x = self.ln_f(x)
        return self.lm_head(x)


def main():
    model = GPT(vocab=16, block_size=4, n_layer=2, n_head=2, n_embd=8).eval()
    idx = torch.randint(0, 16, (1, 4))

    compiled = torch.compile(model, backend=tir_backend, fullgraph=True)
    with torch.no_grad():
        compiled(idx)

    block = tir_backend.block
    args = ", ".join("%s: %s" % (a, a.type) for a in block.args)
    print("// nanoGPT.forward lowered to torch-dialect TIR")
    print("// args: %s   ->   %s" % (args, tir_backend.result.type))
    print(tir_backend.text())

    # Everything above was built through the bindings; inspect it the same way.
    names = [op.name for op in block.ops]
    print("\ntorch ops lowered:", sorted(set(n for n in names if n != "return")))
    for needle in ("embedding", "linear", "softmax", "masked_fill", "gelu", "layer_norm"):
        assert needle in names, f"missing {needle}"
    print("OK: %d ops, nanoGPT forward lowered to valid TIR" % len(names))


if __name__ == "__main__":
    main()
