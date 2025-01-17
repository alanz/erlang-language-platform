/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

// Diagnostic: unused-macro
//
// Return a warning if a macro defined in an .erl file has no references to it

use elp_ide_assists::Assist;
use elp_ide_db::elp_base_db::FileId;
use elp_ide_db::source_change::SourceChange;
use elp_ide_db::SymbolDefinition;
use elp_syntax::AstNode;
use elp_syntax::SyntaxKind;
use elp_syntax::TextRange;
use elp_syntax::TextSize;
use hir::Semantic;
use text_edit::TextEdit;

use crate::diagnostics::DiagnosticCode;
use crate::fix;
use crate::Diagnostic;

pub(crate) fn unused_macro(
    acc: &mut Vec<Diagnostic>,
    sema: &Semantic,
    file_id: FileId,
    ext: Option<&str>,
) -> Option<()> {
    if Some("erl") == ext {
        let def_map = sema.def_map(file_id);
        for (name, def) in def_map.get_macros() {
            // Only run the check for macros defined in the local module,
            // not in the included files.
            if def.file.file_id == file_id {
                if !SymbolDefinition::Define(def.clone())
                    .usages(&sema)
                    .at_least_one()
                {
                    let source = def.source(sema.db.upcast());
                    let macro_syntax = source.syntax();
                    // If after the macro there's a new line, drop it
                    let next_token = macro_syntax.last_token()?.next_token()?;
                    let macro_range = if next_token.kind() == SyntaxKind::WHITESPACE
                        && next_token.text().starts_with("\n")
                    {
                        let start = macro_syntax.text_range().start();
                        let end = macro_syntax.text_range().end() + TextSize::from(1);
                        // Temporary for T148094436
                        let _pctx =
                            stdx::panic_context::enter(format!("\ndiagnostics::unused_macro"));
                        TextRange::new(start, end)
                    } else {
                        macro_syntax.text_range()
                    };
                    let name_range = source.name()?.syntax().text_range();
                    let d = make_diagnostic(file_id, macro_range, name_range, &name.to_string());
                    acc.push(d);
                }
            }
        }
    }
    Some(())
}

fn make_diagnostic(
    file_id: FileId,
    macro_range: TextRange,
    name_range: TextRange,
    name: &str,
) -> Diagnostic {
    Diagnostic::warning(
        DiagnosticCode::UnusedMacro,
        name_range,
        format!("Unused macro ({name})"),
    )
    .with_fixes(Some(vec![delete_unused_macro(file_id, macro_range, name)]))
}

fn delete_unused_macro(file_id: FileId, range: TextRange, name: &str) -> Assist {
    let mut builder = TextEdit::builder();
    builder.delete(range);
    let edit = builder.finish();
    fix(
        "delete_unused_macro",
        &format!("Delete unused macro ({name})"),
        SourceChange::from_text_edit(file_id, edit),
        range,
    )
}

#[cfg(test)]
mod tests {

    use crate::tests::check_diagnostics;
    use crate::tests::check_fix;

    #[test]
    fn test_unused_macro() {
        check_diagnostics(
            r#"
-module(main).
-define(MEANING_OF_LIFE, 42).
    %%  ^^^^^^^^^^^^^^^ 💡 warning: Unused macro (MEANING_OF_LIFE)
            "#,
        );
        check_fix(
            r#"
-module(main).
-define(MEA~NING_OF_LIFE, 42).
            "#,
            r#"
-module(main).
            "#,
        )
    }

    #[test]
    fn test_unused_macro_not_applicable() {
        check_diagnostics(
            r#"
-module(main).
-define(MEANING_OF_LIFE, 42).
main() ->
  ?MEANING_OF_LIFE.
            "#,
        );
    }

    #[test]
    fn test_unused_macro_not_applicable_for_hrl_file() {
        check_diagnostics(
            r#"
//- /include/foo.hrl
-define(MEANING_OF_LIFE, 42).
            "#,
        );
    }

    #[test]
    fn test_unused_macro_with_arg() {
        check_diagnostics(
            r#"
-module(main).
-define(USED_MACRO, used_macro).
-define(UNUSED_MACRO, unused_macro).
     %% ^^^^^^^^^^^^ 💡 warning: Unused macro (UNUSED_MACRO)
-define(UNUSED_MACRO_WITH_ARG(C), C).
     %% ^^^^^^^^^^^^^^^^^^^^^ 💡 warning: Unused macro (UNUSED_MACRO_WITH_ARG/1)

main() ->
  ?MOD:foo(),
  ?USED_MACRO.
            "#,
        );
    }

    #[test]
    fn test_unused_macro_dynamic_call() {
        // Ported from issue #1021 in Erlang LS
        check_diagnostics(
            r#"
-module(main).
-define(MOD, module). %% MOD
main() ->
  ?MOD:foo().
            "#,
        );
    }

    #[test]
    fn test_unused_macro_include() {
        check_diagnostics(
            r#"
//- /src/foo.hrl
-define(A, a).
-define(B, b).
//- /src/foo.erl
-module(foo).
-include("foo.hrl").
-define(BAR, 42).
     %% ^^^ 💡 warning: Unused macro (BAR)
main() ->
  ?A.
        "#,
        );
    }
}
