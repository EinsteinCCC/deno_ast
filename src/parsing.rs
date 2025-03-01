// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::sync::Arc;

use crate::comments::MultiThreadedComments;
use crate::swc::ast::EsVersion;
use crate::swc::ast::Module;
use crate::swc::ast::Program;
use crate::swc::ast::Script;
use crate::swc::common::comments::SingleThreadedComments;
use crate::swc::common::input::StringInput;
use crate::swc::parser::error::Error as SwcError;
use crate::swc::parser::lexer::Lexer;
use crate::swc::parser::token::TokenAndSpan;
use crate::swc::parser::EsConfig;
use crate::swc::parser::Syntax;
use crate::swc::parser::TsConfig;
use crate::Diagnostic;
use crate::MediaType;
use crate::ParsedSource;
use crate::SourceTextInfo;

/// Ecmascript version used for lexing and parsing.
pub const ES_VERSION: EsVersion = EsVersion::Es2021;

/// Parameters for parsing.
pub struct ParseParams {
  /// Specifier of the source text.
  pub specifier: String,
  /// Source text stored in a `SourceTextInfo`.
  pub text_info: SourceTextInfo,
  /// Media type of the source text.
  pub media_type: MediaType,
  /// Whether to capture tokens or not.
  pub capture_tokens: bool,
  /// Whether to apply swc's scope analysis.
  pub scope_analysis: bool,
  /// Syntax to use when parsing.
  ///
  /// `deno_ast` will get a default `Syntax` to use based on the
  /// media type, but you may use this to provide a custom `Syntax`.
  pub maybe_syntax: Option<Syntax>,
}

/// Parses the provided information attempting to figure out if the provided
/// text is for a script or a module.
pub fn parse_program(params: ParseParams) -> Result<ParsedSource, Diagnostic> {
  parse(params, ParseMode::Program, |p| p)
}

/// Parses the provided information as a program with the option of providing some
/// post-processing to the result.
///
/// # Example
///
/// ```
/// deno_ast::parse_program_with_post_process(
///  deno_ast::ParseParams {
///    specifier: "file:///my_file.ts".to_string(),
///    media_type: deno_ast::MediaType::TypeScript,
///    text_info: deno_ast::SourceTextInfo::from_string("".to_string()),
///    capture_tokens: true,
///    maybe_syntax: None,
///    scope_analysis: false,
///  },
///  |program| {
///    // do something with the program here before it gets stored
///    program
///  },
/// );
/// ```
pub fn parse_program_with_post_process(
  params: ParseParams,
  post_process: impl FnOnce(Program) -> Program,
) -> Result<ParsedSource, Diagnostic> {
  parse(params, ParseMode::Program, post_process)
}

/// Parses the provided information to a module.
pub fn parse_module(params: ParseParams) -> Result<ParsedSource, Diagnostic> {
  parse(params, ParseMode::Module, |p| p)
}

/// Parses a module with post processing (see docs on `parse_program_with_post_process`).
pub fn parse_module_with_post_process(
  params: ParseParams,
  post_process: impl FnOnce(Module) -> Module,
) -> Result<ParsedSource, Diagnostic> {
  parse(params, ParseMode::Module, |program| match program {
    Program::Module(module) => Program::Module(post_process(module)),
    Program::Script(_) => unreachable!(),
  })
}

/// Parses the provided information to a script.
pub fn parse_script(params: ParseParams) -> Result<ParsedSource, Diagnostic> {
  parse(params, ParseMode::Script, |p| p)
}

/// Parses a script with post processing (see docs on `parse_program_with_post_process`).
pub fn parse_script_with_post_process(
  params: ParseParams,
  post_process: impl FnOnce(Script) -> Script,
) -> Result<ParsedSource, Diagnostic> {
  parse(params, ParseMode::Script, |program| match program {
    Program::Module(_) => unreachable!(),
    Program::Script(script) => Program::Script(post_process(script)),
  })
}

enum ParseMode {
  Program,
  Module,
  Script,
}

fn parse(
  params: ParseParams,
  parse_mode: ParseMode,
  post_process: impl FnOnce(Program) -> Program,
) -> Result<ParsedSource, Diagnostic> {
  let source = params.text_info;
  let specifier = params.specifier;
  let input = source.as_string_input();
  let media_type = params.media_type;
  let syntax = params
    .maybe_syntax
    .unwrap_or_else(|| get_syntax(media_type));
  let (comments, program, tokens, errors) =
    parse_string_input(input, syntax, params.capture_tokens, parse_mode)
      .map_err(|err| {
        Diagnostic::from_swc_error(err, &specifier, source.clone())
      })?;
  let diagnostics = errors
    .into_iter()
    .map(|err| Diagnostic::from_swc_error(err, &specifier, source.clone()))
    .collect();
  let program = post_process(program);

  let (program, syntax_contexts) = if params.scope_analysis {
    scope_analysis_transform(program)
  } else {
    (program, None)
  };

  Ok(ParsedSource::new(
    specifier,
    params.media_type.to_owned(),
    source,
    MultiThreadedComments::from_single_threaded(comments),
    Arc::new(program),
    tokens.map(Arc::new),
    syntax_contexts,
    diagnostics,
  ))
}

pub(crate) fn scope_analysis_transform(
  _program: Program,
) -> (Program, Option<crate::SyntaxContexts>) {
  #[cfg(feature = "transforms")]
  {
    scope_analysis_transform_inner(_program)
  }
  #[cfg(not(feature = "transforms"))]
  panic!(
    "Cannot parse with scope analysis. Please enable the 'transforms' feature."
  )
}

#[cfg(feature = "transforms")]
fn scope_analysis_transform_inner(
  program: Program,
) -> (Program, Option<crate::SyntaxContexts>) {
  use crate::swc::common::Globals;
  use crate::swc::common::Mark;
  use crate::swc::common::SyntaxContext;
  use crate::swc::transforms::resolver;
  use crate::swc::visit::FoldWith;

  let globals = Globals::new();
  crate::swc::common::GLOBALS.set(&globals, || {
    let unresolved_mark = Mark::new();
    let top_level_mark = Mark::new();
    let program =
      program.fold_with(&mut resolver(unresolved_mark, top_level_mark, true));

    (
      program,
      Some(crate::SyntaxContexts {
        unresolved: SyntaxContext::empty().apply_mark(unresolved_mark),
        top_level: SyntaxContext::empty().apply_mark(top_level_mark),
      }),
    )
  })
}

#[allow(clippy::type_complexity)]
fn parse_string_input(
  input: StringInput,
  syntax: Syntax,
  capture_tokens: bool,
  parse_mode: ParseMode,
) -> Result<
  (
    SingleThreadedComments,
    Program,
    Option<Vec<TokenAndSpan>>,
    Vec<SwcError>,
  ),
  SwcError,
> {
  let comments = SingleThreadedComments::default();
  let lexer = Lexer::new(syntax, ES_VERSION, input, Some(&comments));

  if capture_tokens {
    let lexer = crate::swc::parser::Capturing::new(lexer);
    let mut parser = crate::swc::parser::Parser::new_from(lexer);
    let program = match parse_mode {
      ParseMode::Program => parser.parse_program()?,
      ParseMode::Module => Program::Module(parser.parse_module()?),
      ParseMode::Script => Program::Script(parser.parse_script()?),
    };
    let tokens = parser.input().take();
    let errors = parser.take_errors();

    Ok((comments, program, Some(tokens), errors))
  } else {
    let mut parser = crate::swc::parser::Parser::new_from(lexer);
    let program = match parse_mode {
      ParseMode::Program => parser.parse_program()?,
      ParseMode::Module => Program::Module(parser.parse_module()?),
      ParseMode::Script => Program::Script(parser.parse_script()?),
    };
    let errors = parser.take_errors();

    Ok((comments, program, None, errors))
  }
}

/// Gets the default `Syntax` used by `deno_ast` for the provided media type.
pub fn get_syntax(media_type: MediaType) -> Syntax {
  match media_type {
    MediaType::TypeScript
    | MediaType::Mts
    | MediaType::Cts
    | MediaType::Dts
    | MediaType::Dmts
    | MediaType::Dcts
    | MediaType::Tsx => {
      Syntax::Typescript(TsConfig {
        decorators: true,
        // should be true for mts and cts:
        // https://babeljs.io/docs/babel-preset-typescript#disallowambiguousjsxlike
        disallow_ambiguous_jsx_like: matches!(
          media_type,
          MediaType::Mts | MediaType::Cts
        ),
        dts: matches!(
          media_type,
          MediaType::Dts | MediaType::Dmts | MediaType::Dcts
        ),
        tsx: media_type == MediaType::Tsx,
        no_early_errors: false,
      })
    }
    MediaType::JavaScript
    | MediaType::Mjs
    | MediaType::Cjs
    | MediaType::Jsx
    | MediaType::Json
    | MediaType::Wasm
    | MediaType::TsBuildInfo
    | MediaType::SourceMap
    | MediaType::Unknown => Syntax::Es(EsConfig {
      allow_return_outside_function: true,
      allow_super_outside_method: true,
      auto_accessors: true,
      decorators: false,
      decorators_before_export: false,
      export_default_from: true,
      fn_bind: false,
      import_attributes: true,
      jsx: media_type == MediaType::Jsx,
      explicit_resource_management: true,
    }),
  }
}

#[cfg(test)]
mod test {
  use crate::LineAndColumnDisplay;

  use super::*;

  #[test]
  fn should_parse_program() {
    let program = parse_program(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string("// 1\n1 + 1\n// 2".to_string()),
      media_type: MediaType::JavaScript,
      capture_tokens: true,
      maybe_syntax: None,
      scope_analysis: false,
    })
    .unwrap();
    assert_eq!(program.specifier(), "my_file.js");
    assert_eq!(program.text_info().text_str(), "// 1\n1 + 1\n// 2");
    assert_eq!(program.media_type(), MediaType::JavaScript);
    assert!(matches!(
      program.script().body[0],
      crate::swc::ast::Stmt::Expr(..)
    ));
    assert_eq!(program.get_leading_comments().unwrap().len(), 1);
    assert_eq!(program.get_leading_comments().unwrap()[0].text, " 1");
    assert_eq!(program.tokens().len(), 3);
    assert_eq!(program.comments().get_vec().len(), 2);
  }

  #[test]
  fn should_parse_module() {
    let program = parse_module(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string("// 1\n1 + 1\n// 2".to_string()),
      media_type: MediaType::JavaScript,
      capture_tokens: true,
      maybe_syntax: None,
      scope_analysis: false,
    })
    .unwrap();
    assert!(matches!(
      program.module().body[0],
      crate::swc::ast::ModuleItem::Stmt(..)
    ));
  }

  #[cfg(feature = "view")]
  #[test]
  fn should_parse_brand_checks_in_js() {
    use crate::view::ClassDecl;
    use crate::view::ClassMethod;
    use crate::view::NodeTrait;

    let program = parse_module(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string(
        "class T { method() { #test in this; } }".to_string(),
      ),
      media_type: MediaType::JavaScript,
      capture_tokens: true,
      maybe_syntax: None,
      scope_analysis: false,
    })
    .unwrap();

    program.with_view(|program| {
      let class_decl = program.children()[0].expect::<ClassDecl>();
      let class_method = class_decl.class.body[0].expect::<ClassMethod>();
      let method_stmt = class_method.function.body.unwrap().stmts[0];
      assert_eq!(method_stmt.text(), "#test in this;");
    });
  }

  #[test]
  #[should_panic(
    expected = "Tokens not found because they were not captured during parsing."
  )]
  fn should_panic_when_getting_tokens_and_tokens_not_captured() {
    let program = parse_module(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string("// 1\n1 + 1\n// 2".to_string()),
      media_type: MediaType::JavaScript,
      capture_tokens: false,
      maybe_syntax: None,
      scope_analysis: false,
    })
    .unwrap();
    program.tokens();
  }

  #[test]
  fn should_handle_parse_error() {
    let diagnostic = parse_module(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string("t u".to_string()),
      media_type: MediaType::JavaScript,
      capture_tokens: true,
      maybe_syntax: None,
      scope_analysis: false,
    })
    .err()
    .unwrap();
    assert_eq!(diagnostic.specifier, "my_file.js".to_string());
    assert_eq!(
      diagnostic.display_position(),
      LineAndColumnDisplay {
        line_number: 1,
        column_number: 3,
      }
    );
    assert_eq!(diagnostic.message(), "Expected ';', '}' or <eof>");
  }

  #[test]
  #[should_panic(
    expected = "Could not get syntax context because the source was not parsed with scope analysis."
  )]
  fn should_panic_when_getting_top_level_context_and_scope_analysis_false() {
    get_scope_analysis_false_parsed_source().top_level_context();
  }

  #[test]
  #[should_panic(
    expected = "Could not get syntax context because the source was not parsed with scope analysis."
  )]
  fn should_panic_when_getting_unresolved_context_and_scope_analysis_false() {
    get_scope_analysis_false_parsed_source().unresolved_context();
  }

  fn get_scope_analysis_false_parsed_source() -> ParsedSource {
    parse_module(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string("// 1\n1 + 1\n// 2".to_string()),
      media_type: MediaType::JavaScript,
      capture_tokens: false,
      maybe_syntax: None,
      scope_analysis: false,
    })
    .unwrap()
  }

  #[cfg(all(feature = "view", feature = "transforms"))]
  #[test]
  fn should_do_scope_analysis() {
    let parsed_source = parse_module(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string(
        "export function test() { const test = 2; test; } test()".to_string(),
      ),
      media_type: MediaType::JavaScript,
      capture_tokens: true,
      maybe_syntax: None,
      scope_analysis: true,
    })
    .unwrap();

    parsed_source.with_view(|view| {
      use crate::view::*;

      let func_decl = view.children()[0]
        .expect::<ExportDecl>()
        .decl
        .expect::<FnDecl>();
      let func_decl_inner_expr = func_decl.function.body.unwrap().stmts[1]
        .expect::<ExprStmt>()
        .expr
        .expect::<Ident>();
      let call_expr = view.children()[1]
        .expect::<ExprStmt>()
        .expr
        .expect::<CallExpr>();
      let call_expr_id = call_expr.callee.expect::<Ident>();

      // these should be the same identifier
      assert_eq!(func_decl.ident.to_id(), call_expr_id.to_id());
      // but these shouldn't be
      assert_ne!(func_decl.ident.to_id(), func_decl_inner_expr.to_id());
    });
  }

  #[cfg(all(feature = "view", feature = "transforms"))]
  #[test]
  fn should_allow_scope_analysis_after_the_fact() {
    let parsed_source = parse_module(ParseParams {
      specifier: "my_file.js".to_string(),
      text_info: SourceTextInfo::from_string(
        "export function test() { const test = 2; test; } test()".to_string(),
      ),
      media_type: MediaType::JavaScript,
      capture_tokens: true,
      maybe_syntax: None,
      scope_analysis: false,
    })
    .unwrap();

    parsed_source.with_view(|view| {
      use crate::view::*;
      let func_decl = view.children()[0]
        .expect::<ExportDecl>()
        .decl
        .expect::<FnDecl>();
      let func_decl_inner_expr = func_decl.function.body.unwrap().stmts[1]
        .expect::<ExprStmt>()
        .expr
        .expect::<Ident>();
      // these will be equal because scope analysis hasn't been done
      assert_eq!(func_decl.ident.to_id(), func_decl_inner_expr.to_id());
    });

    // now do scope analysis
    let parsed_source = parsed_source.into_with_scope_analysis();

    parsed_source.with_view(|view| {
      use crate::view::*;
      let func_decl = view.children()[0]
        .expect::<ExportDecl>()
        .decl
        .expect::<FnDecl>();
      let func_decl_inner_expr = func_decl.function.body.unwrap().stmts[1]
        .expect::<ExprStmt>()
        .expr
        .expect::<Ident>();
      // now they'll be not equal because scope analysis has occurred
      assert_ne!(func_decl.ident.to_id(), func_decl_inner_expr.to_id());
    });
  }

  #[cfg(all(feature = "view", feature = "transforms"))]
  #[test]
  fn should_scope_analyze_typescript() {
    let parsed_source = parse_module(ParseParams {
      specifier: "my_file.ts".to_string(),
      text_info: SourceTextInfo::from_string(
        r#"import type { Foo } from "./foo.ts";
function _bar(...Foo: Foo) {
  console.log(Foo);
}"#
          .to_string(),
      ),
      media_type: MediaType::TypeScript,
      capture_tokens: true,
      maybe_syntax: None,
      scope_analysis: true,
    })
    .unwrap();

    parsed_source.with_view(|view| {
      use crate::view::*;

      let named_import_ident =
        view.children()[0].expect::<ImportDecl>().specifiers[0]
          .expect::<ImportNamedSpecifier>()
          .local;
      let bar_func = view.children()[1].expect::<FnDecl>();
      let bar_param_rest_pat =
        bar_func.function.params[0].pat.expect::<RestPat>();
      let bar_param_ident = bar_param_rest_pat.arg.expect::<BindingIdent>().id;
      let bar_param_type_ident = bar_param_rest_pat
        .type_ann
        .unwrap()
        .type_ann
        .expect::<TsTypeRef>()
        .type_name
        .expect::<Ident>();
      let console_log_arg_ident = bar_func.function.body.unwrap().stmts[0]
        .expect::<ExprStmt>()
        .expr
        .expect::<CallExpr>()
        .args[0]
        .expr
        .expect::<Ident>();

      assert_eq!(console_log_arg_ident.to_id(), bar_param_ident.to_id());
      assert_ne!(console_log_arg_ident.to_id(), named_import_ident.to_id());
      assert_ne!(console_log_arg_ident.to_id(), bar_param_type_ident.to_id());

      assert_eq!(named_import_ident.to_id(), bar_param_type_ident.to_id());
      assert_ne!(named_import_ident.to_id(), bar_param_ident.to_id());
    });
  }

  #[test]
  fn should_error_on_syntax_diagnostic() {
    let diagnostic = parse_ts_module("test;\nas#;").err().unwrap();
    assert_eq!(diagnostic.message(), concat!("Expected ';', '}' or <eof>"));
  }

  #[test]
  fn should_error_without_issue_when_there_exists_multi_byte_char_on_line_with_syntax_error(
  ) {
    let diagnostic = parse_ts_module(concat!(
      "test;\n",
      r#"console.log("x", `duration ${d} not in range - ${min} ≥ ${d} && ${max} ≥ ${d}`),;"#,
    )).err().unwrap();
    assert_eq!(diagnostic.message(), "Expression expected",);
  }

  #[test]
  fn should_diagnostic_for_no_equals_sign_in_var_decl() {
    let diagnostic =
      parse_for_diagnostic("const Methods {\nf: (x, y) => x + y,\n};");
    assert_eq!(
      diagnostic.message(),
      "'const' declarations must be initialized"
    );
  }

  #[test]
  fn should_diganotic_when_var_stmts_sep_by_comma() {
    let diagnostic = parse_for_diagnostic("let a = 0, let b = 1;");
    assert_eq!(
      diagnostic.message(),
      "`let` cannot be used as an identifier in strict mode"
    );
  }

  #[test]
  fn should_diagnostic_for_exected_expr_type_alias() {
    let diagnostic =
      parse_for_diagnostic("type T =\n  | unknown\n  { } & unknown;");
    assert_eq!(diagnostic.message(), "Expression expected");
  }

  #[test]
  fn should_diganotic_missing_init_in_using() {
    let diagnostic = parse_for_diagnostic("using test");
    assert_eq!(
      diagnostic.message(),
      "Using declaration requires initializer"
    );
  }

  fn parse_for_diagnostic(text: &str) -> Diagnostic {
    let result = parse_ts_module(text).unwrap();
    result.diagnostics().first().unwrap().to_owned()
  }

  fn parse_ts_module(text: &str) -> Result<ParsedSource, Diagnostic> {
    parse_module(ParseParams {
      specifier: "my_file.ts".to_string(),
      text_info: SourceTextInfo::from_string(text.to_string()),
      media_type: MediaType::TypeScript,
      capture_tokens: false,
      maybe_syntax: None,
      scope_analysis: false,
    })
  }
}
