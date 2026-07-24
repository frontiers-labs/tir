# TIR Machine Description Language

TMDL is a built-in Domain Specific Language targeted at accurate representation
of various aspects of computer architecture and microarchitecture. It features
declarative Rust-like syntax and robust tooling.

## Motivation

There are quite a few Architecture Description Languages out there. In fact,
many compilers introduce a language of their own to facilitate backend 
development. So, why does TIR need a new one? Why not just re-use something
that already exists? To answer this questions we must explore available options
in greater detail.

First, there's LLVM's TableGen. While it's relatively widely adopted and has
great documentation, many new LLVM contributors struggle to fully understand
how it's structured. TableGen grew from a domain specific language into a
framework that can be tailored to ones needs. As a result, someone who's fluent
with MLIR operation description TableGen syntax may not find it easy to add a
new instruction to RISC-V backend and vice versa. Another big pain point is its
focus on capturing records rather than behavior. This works well when
describing encoding of an instruction, but fails to provide a helpful toolbox
when one needs to capture the semantics of the instructions or some
microarchitectural details of the target platform.

On the other end of the spectrum there's Sail. Sail is an academic architecture
description language derived from OCaml. It can accurately capture the
semantics of the instructions, provides integration with various formal
verification tools and can even generate a functional simulator for your ISA.
The official RISC-V specification uses Sail to describe the semantics of ISA
instructions. So, why not go this way? There're a few problems with Sail.
First, its OCaml origins make it harder for users not familiar with functional
languages to read the code and provide meaningful contributions. Second, Sail
tooling and documentation leave much to be desired. Finally, Sail focuses on
capturing the semantics of the instructions, and not their structural
properties and definitions. While we could definitely make this work, Sail is
just not the best level of abstraction for a classic compiler.

ASL definitely deserves an honorable mention here. It provides features similar
to what Sail can offer, but has a more declarative syntax, and for that reason
feels more comfortable to an average programmer with proficiency in C++.
Unfortunately, ASL is targeted at ARM architectures exclusively.

This brings us to TIR Machine Description Language. TMDL tries to fill the
space in between Sail and TableGen. It features Rust-like declarative syntax,
that should be easy to pick up for anyone with at least basic Rust skills.
The primary goals for our DSL are:

1. Providing an accurate representation of Instruction Set Architecture in
   terms of assembly syntax and binary encoding. Providing definitions for ASM
   pseudo-instructions, like TableGen does, is out of scope.
2. Provide means to capture documentation for ISAs and render it in a
   human-readable format, like Markdown or HTML.
3. Provide means to capture semantics of the instructions in a way that is 
   suitable for usage with formal verification tools, as well as being able to
   generate definitions for various stages of compilation process.
4. Be extensible enough to capture microarchitecture features so that TMDL
   description can be used to generate cycle approximate simulators and static
   performance analysis tools, like `llvm-mca`.
5. Support tooling as first-class citizen. Ideally, TMDL has support for
   language server protocol and a machine-readable AST format for users to
   build tools on top of TMDL definitions.
6. Provide interoperability with TableGen definitions and Sail. TMDL should be
   easy to translate to LLVM's TableGen definitions (albeit, not always easy to
   make the generated code concise and readable). And verification against
   formal Sail models should prevent many of the bugs in such a complex part of
   the compiler as backend.
