# The TIR project guidelines

TIR is a post-modern compiler framework. Where LLVM or GCC manually traverse
graphs and do rewrites, TIR prefers to use math and formal methods to infer
desired transformations. This applies to optimizations and instruction selection.

## Coding guidelines

1. Think before coding. Do not assume anything. Verify, don't hide confusion.
   Consider tradeoffs. Consult with the user when the task is unclear. If
   multiple interpretations exist, present them clearly - don't pick silently.
2. Strive for simplicity. Produce the minimum amount of code required to solve
   the problem. No features beyond what's asked. No abstractions for single-use
   code. No flexibility or configurability that was not requested. No error
   handling for impossible situations. If the solution of 200 lines of code can
   be done in 50 - rewrite it. This does not give you permission to cheat.
   Resolve core issue end-to-end. Examples of unacceptable behavior: removing
   assertion from a test instead of fixing the bug; adding an escape hatch to
   ISel rules instead of properly defining formal semantics. 
3. Touch only those pieces of existing code that are relevant to the fix. Don't
   improve adjacent code unless explicitly asked. If you need to refactor existing
   interfaces, ask user first. Match existing style always. If you see existing
   unrelated dead code - highlight that, but don't delete silently.
4. Cover your changes with reasonable testing. Test only public interfaces. Do
   not test mock instead of real behavior. Do not add test-only methods in production
   code. Write tests before writing any real code. If you wrote code before tests,
   throw that code away - no exceptions. Do not write all tests at once - move
   step-by-step. Write single test, verify it's red, then write minimal code that fixes it.
   Any test should focus only on single behavior. For IR changes, assembly, etc,
   prefer snapshot testing via LIT checks. If a test can be expressed as a LIT
   check, do not add a unit test that does the same thing. After writing all of your
   code, do a refactor pass: remove all duplication, improve names, extract helpers.
   Follow testing guidelines in docs/dev_guide.md.
5. Keep code tidy. Remove imports/variables/functions that YOUR changes made
   unused. Don't remove pre-existing dead code unless asked. Every changed line
   should trace directly to the user's request. After all changes are done and
   all tests are passing, run formatting routines and linters. Fix all warnings.
   Do not put everything in a giant file. Split large functions to be no more
   than 400 lines. Respect responsibility ownership between modules.
6. Make your answers short and simple and on task. Do not apologize, do not try
   to be polite, do not explain yourself unless explicitly asked. Avoid comments
   in the code. Only add documentation for public interfaces or non-obvious
   behavior justification. Such comments should answer "Why?" not "What?".
   Code must explain itself via good function or variable names. In prose
   or PR description avoid filler words (actually, well, etc).
7. Use conventional commits v1.0.0 spec for commit titles and descriptions.
   If future PR has multiple commits, PR title is also conventional commits.
   All PRs are squashed anyways.
8. Do not scan node_modules, target, build and other automatically-generated
   files and directories, unless explicitly asked to.

## Working with code

- `cargo build`: build Rust code
- `cargo test`: run Rust tests
- `cargo fmt`: automatically format Rust code
- `cargo clippy`: Rust linter

Make sure all code is formatted and linters are green before you hand over work
back to the human.
