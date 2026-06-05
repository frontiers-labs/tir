# The TIR project guidelines

## Coding guidelines

1. Think before coding. Do not assume anything. Verify, don't hide confusion.
   Consider tradeoffs. Consult with the user when the task is unclear. If
   multiple interpretations exist, present them clearly - don't pick silently.
2. Strive for simplicity. Produce the minimum amount of code required to solve
   the problem. No features beyond what's asked. No abstractions for single-use
   code. No flexibility or configurability that was not requested. No error
   handling for impossible situations. If the solution of 200 lines of code can
   be done in 50 - rewrite it. 
3. Touch only those pieces of existing code that you absolutely must. Don't
   improve adjacent code unless explicitly asked. Don't refactor things that are
   not brokent. Match existing style always. If you see existing unrelated dead
   code - highlight that, but don't delete silently.
4. Pair your changes with reasonable testing. Tests must be driven by the goals
   of initial prompt. Define success criteria and express it as a test. Loop
   until the goal is reached and verified by testing. If the task is to refactor
   something, make sure existing tests work both before and after your changes.
   For multistep tasks follow these instructions for each step.
5. Keep code tidy. Remove imports/variables/functions that YOUR changes made
   unused. Don't remove pre-existing dead code unless asked. Every changed line
   should trace directly to the user's request. After all changes are done and
   all tests are passing, run formatting routines and linters. Fix all warnings.
   Do not put everything in a giant file. Split large functions to be no more
   than 400 lines. Respect responsibility ownership between modules.
6. Make your answers short and simple and on task. Do not apologize, do not try
   to be polite, do not explain yourself unless explicitly asked. When writing
   code, avoid obvious commentary - do not explain what the code does. If needed,
   you can add a comment that explains non-obvious design decisions. Such comments
   should answer "Why?" not "What?". Conserve your token budget and avoid any
   kind of duplication. Code must explain itself without additions. While thinking
   or reasoning, speak like a caveman, skip articles and other unnecessary noise.
7. Use conventional commits v1.0.0 spec for commit titles and descriptions.
8. Before giving an answer, figure out definition of done. If possible, formulate
   it as a test and verify your assumptions first.

## Working with code

- `cargo build`: build Rust code
- `cargo test`: run Rust tests
- `cargo fmt`: automatically format Rust code
- `cargo clippy`: Rust linter
