#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT_LEN: usize = 32 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_LEN {
        return;
    }

    if let Ok(input) = std::str::from_utf8(data) {
        let (tokens, errors) = tmdl::lex(input);
        if !errors.is_empty() {
            return;
        }

        // Macro-expand between lex and parse. Arena is stack-local (no leaks).
        let arena = tmdl::StringArena::new();
        let mut table = tmdl::MacroTable::new();
        let mut diags = Vec::new();
        let tokens = tmdl::collect_macros("<fuzz>", tokens, &mut table, &mut diags);
        if !diags.is_empty() {
            return;
        }
        let (tokens, diags) = tmdl::expand("<fuzz>", tokens, &table, &arena);
        if !diags.is_empty() {
            return;
        }

        let (file, errors) = tmdl::parse(input, &tokens, "<fuzz>");
        if !errors.is_empty() {
            return;
        }

        let Some(file) = file else {
            return;
        };

        let files = [file];
        let _ = tmdl::sema_analyze(&files, false);
        let _ = tmdl::type_check(&files);
    }
});
