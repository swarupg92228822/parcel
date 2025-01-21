mod collect;
mod constant_module;
mod dependency_collector;
mod env_replacer;
mod fs;
mod global_replacer;
mod hoist;
mod mdx;
mod modules;
mod node_replacer;
mod react_lazy;
#[cfg(test)]
mod test_utils;
mod typeof_replacer;
mod utils;

use std::{
  collections::{HashMap, HashSet},
  path::{Path, PathBuf},
  str::FromStr,
};

pub use collect::CollectImportedSymbol;
use collect::{Collect, CollectResult};
use constant_module::ConstantModule;
use dependency_collector::Helpers;
pub use dependency_collector::{dependency_collector, DependencyDescriptor, DependencyKind};
use env_replacer::*;
use fs::inline_fs;
use global_replacer::GlobalReplacer;
use hoist::{hoist, HoistResult};
pub use hoist::{ExportedSymbol, ImportedSymbol};
use indexmap::IndexMap;
use mdx::{mdx, MdxAsset, TocNode};
use modules::esm2cjs;
use node_replacer::NodeReplacer;
use parcel_macros::{JsValue, MacroCallback, MacroError, Macros};
use path_slash::PathExt;
use react_lazy::ReactLazy;
use serde::{Deserialize, Serialize};
use swc_core::{
  common::{
    comments::SingleThreadedComments, errors::Handler, pass::Optional,
    source_map::SourceMapGenConfig, sync::Lrc, FileName, Globals, Mark, SourceMap,
  },
  ecma::{
    ast::{Expr, ExprStmt, Lit, Module, ModuleItem, Program, Stmt, Str},
    atoms::JsWord,
    codegen::text_writer::JsWriter,
    parser::{error::Error, lexer::Lexer, EsSyntax, Parser, StringInput, Syntax, TsSyntax},
    preset_env::{preset_env, Mode::Entry, Targets, Version, Versions},
    transforms::{
      base::{
        assumptions::Assumptions,
        fixer::{fixer, paren_remover},
        helpers,
        hygiene::hygiene,
        resolver,
      },
      compat::reserved_words::reserved_words,
      optimization::simplify::{dead_branch_remover, expr_simplifier},
      proposal::decorators,
      react, typescript,
    },
    visit::{FoldWith, VisitMutWith, VisitWith},
  },
};
use typeof_replacer::*;
use utils::{
  error_buffer_to_diagnostics, CodeHighlight, Diagnostic, DiagnosticSeverity, ErrorBuffer,
};
pub use utils::{SourceLocation, SourceType};

type SourceMapBuffer = Vec<(swc_core::common::BytePos, swc_core::common::LineCol)>;

#[derive(Default, Serialize, Debug, Deserialize)]
pub struct Config {
  pub filename: String,
  #[serde(with = "serde_bytes")]
  pub code: Vec<u8>,
  pub module_id: String,
  pub project_root: String,
  pub env: HashMap<JsWord, JsWord>,
  pub inline_fs: bool,
  pub context: EnvContext,
  #[serde(rename = "type")]
  pub asset_type: Type,
  pub jsx_pragma: Option<String>,
  pub jsx_pragma_frag: Option<String>,
  pub automatic_jsx_runtime: bool,
  pub jsx_import_source: Option<String>,
  pub decorators: bool,
  pub use_define_for_class_fields: bool,
  pub is_development: bool,
  pub react_refresh: bool,
  pub targets: Option<HashMap<String, String>>,
  pub source_maps: bool,
  pub scope_hoist: bool,
  pub source_type: SourceType,
  pub supports_module_workers: bool,
  pub is_library: bool,
  pub is_esm_output: bool,
  pub trace_bailouts: bool,
  pub is_swc_helpers: bool,
  pub standalone: bool,
  pub inline_constants: bool,
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Type {
  #[default]
  Js,
  Jsx,
  Ts,
  Tsx,
  Mdx,
}

#[derive(Default, Serialize, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvContext {
  #[default]
  Browser,
  WebWorker,
  ServiceWorker,
  Worklet,
  Node,
  ElectronRenderer,
  ElectronMain,
  EdgeRoutine,
  ReactClient,
  ReactServer,
}

impl Config {
  fn is_browser(&self) -> bool {
    use EnvContext::*;
    matches!(
      self.context,
      Browser | WebWorker | ServiceWorker | Worklet | ElectronRenderer | ReactClient
    )
  }

  fn is_node(&self) -> bool {
    use EnvContext::*;
    matches!(
      self.context,
      Node | ElectronMain | ElectronRenderer | ReactServer
    )
  }

  fn is_server(&self) -> bool {
    use EnvContext::*;
    matches!(self.context, Node | ReactServer)
  }

  fn is_worker(&self) -> bool {
    use EnvContext::*;
    matches!(self.context, WebWorker | ServiceWorker)
  }

  fn is_worklet(&self) -> bool {
    use EnvContext::*;
    matches!(self.context, Worklet)
  }

  fn react_refresh(&self) -> bool {
    self.is_browser()
      && !self.is_library
      && !self.is_worker()
      && !self.is_worklet()
      && self.react_refresh
  }

  fn inline_fs(&self) -> bool {
    self.inline_fs && !self.is_node() && self.source_type != SourceType::Script
  }

  fn node_replacer(&self) -> bool {
    self.is_node()
  }

  fn insert_node_globals(&self) -> bool {
    !self.is_node() && self.source_type != SourceType::Script
  }

  fn replace_env(&self) -> bool {
    !self.is_node() || matches!(self.context, EnvContext::ReactServer)
  }

  fn is_jsx(&self) -> bool {
    matches!(self.asset_type, Type::Jsx | Type::Tsx | Type::Mdx)
  }

  fn is_type_script(&self) -> bool {
    matches!(self.asset_type, Type::Ts | Type::Tsx)
  }
}

#[derive(Serialize, Debug, Default)]
#[non_exhaustive]
pub struct TransformResult {
  #[serde(with = "serde_bytes")]
  pub code: Vec<u8>,
  pub map: Option<String>,
  pub shebang: Option<String>,
  pub dependencies: Vec<DependencyDescriptor>,
  pub hoist_result: Option<HoistResult>,
  pub symbol_result: Option<CollectResult>,
  pub diagnostics: Option<Vec<Diagnostic>>,
  pub needs_esm_helpers: bool,
  pub used_env: HashSet<JsWord>,
  pub has_node_replacements: bool,
  pub is_constant_module: bool,
  pub directives: Vec<JsWord>,
  pub helpers: Helpers,
  pub mdx_toc: Vec<TocNode>,
  pub mdx_exports: HashMap<JsWord, JsValue>,
  pub mdx_assets: Vec<MdxAsset>,
}

fn targets_to_versions(targets: &Option<HashMap<String, String>>) -> Option<Versions> {
  if let Some(targets) = targets {
    macro_rules! set_target {
      ($versions: ident, $name: ident) => {
        let version = targets.get(stringify!($name));
        if let Some(version) = version {
          if let Ok(version) = Version::from_str(version.as_str()) {
            $versions.$name = Some(version);
          }
        }
      };
    }

    let mut versions = Versions::default();
    set_target!(versions, chrome);
    set_target!(versions, opera);
    set_target!(versions, edge);
    set_target!(versions, firefox);
    set_target!(versions, safari);
    set_target!(versions, ie);
    set_target!(versions, ios);
    set_target!(versions, android);
    set_target!(versions, node);
    set_target!(versions, electron);
    return Some(versions);
  }

  None
}

pub fn transform(
  mut config: Config,
  call_macro: Option<MacroCallback>,
) -> Result<TransformResult, std::io::Error> {
  let mut result = TransformResult::default();
  let mut map_buf = vec![];

  let code = unsafe { std::str::from_utf8_unchecked(&config.code) };
  let source_map = Lrc::new(SourceMap::default());
  // Attempt to convert the path to be relative to the project root.
  // If outside the project root, use an absolute path so that if the project root moves the path still works.
  let filename: PathBuf =
    if let Ok(relative) = Path::new(&config.filename).strip_prefix(&config.project_root) {
      relative.to_slash_lossy().into()
    } else {
      PathBuf::from(&config.filename)
    };

  let (module, comments) = if matches!(config.asset_type, Type::Mdx) {
    source_map.new_source_file(Lrc::new(FileName::Real(filename)), code.into());

    let res = mdx(&config);
    match res {
      Err(diagnostic) => {
        result.diagnostics = Some(vec![diagnostic]);
        return Ok(result);
      }
      Ok(res) => {
        result.mdx_toc = res.toc;
        result.mdx_exports = res.exports;
        result.mdx_assets = res.assets;
        (Program::Module(res.module), res.comments)
      }
    }
  } else {
    let module = parse(code, filename, &source_map, &config);

    match module {
      Err(errs) => {
        let error_buffer = ErrorBuffer::default();
        let handler = Handler::with_emitter(true, false, Box::new(error_buffer.clone()));
        for err in errs {
          err.into_diagnostic(&handler).emit();
        }

        result.diagnostics = Some(error_buffer_to_diagnostics(&error_buffer, &source_map));
        return Ok(result);
      }
      Ok((module, comments)) => (module, comments),
    }
  };

  let mut module = module;
  result.shebang = match &mut module {
    Program::Module(module) => module.shebang.take().map(|s| s.to_string()),
    Program::Script(script) => script.shebang.take().map(|s| s.to_string()),
  };

  match &module {
    Program::Module(module) => {
      for item in &module.body {
        if let ModuleItem::Stmt(Stmt::Expr(ExprStmt { expr, .. })) = item {
          if let Expr::Lit(Lit::Str(Str { value, .. })) = &**expr {
            result.directives.push(value.clone());
            continue;
          }
        }
        break;
      }
    }
    Program::Script(script) => {
      for item in &script.body {
        if let Stmt::Expr(ExprStmt { expr, .. }) = item {
          if let Expr::Lit(Lit::Str(Str { value, .. })) = &**expr {
            result.directives.push(value.clone());
            continue;
          }
        }
        break;
      }
    }
  }

  if config.is_server() && !config.is_library && result.directives.contains(&"use client".into()) {
    config.context = EnvContext::ReactClient;
    config.is_esm_output = true;
  } else if !config.is_server()
    && !config.is_library
    && result.directives.contains(&"use server".into())
  {
    config.context = EnvContext::ReactServer;
    config.is_esm_output = false;
  }

  let mut global_deps = vec![];
  let mut fs_deps = vec![];
  let should_inline_fs =
    config.inline_fs() && config.source_type != SourceType::Script && code.contains("readFileSync");
  let should_import_swc_helpers = match config.source_type {
    SourceType::Module => true,
    SourceType::Script => false,
  };

  swc_core::common::GLOBALS.set(&Globals::new(), || {
    let error_buffer = ErrorBuffer::default();
    let handler = Handler::with_emitter(true, false, Box::new(error_buffer.clone()));
    swc_core::common::errors::HANDLER.set(&handler, || {
      helpers::HELPERS.set(
        &helpers::Helpers::new(
          /* external helpers from @swc/helpers */ should_import_swc_helpers,
        ),
        || {
          let mut react_options = react::Options::default();
          if config.is_jsx() {
            if let Some(jsx_pragma) = &config.jsx_pragma {
              react_options.pragma = Some(jsx_pragma.clone());
            }
            if let Some(jsx_pragma_frag) = &config.jsx_pragma_frag {
              react_options.pragma_frag = Some(jsx_pragma_frag.clone());
            }
            react_options.development = Some(config.is_development);
            react_options.refresh = if config.react_refresh() {
              Some(react::RefreshOptions::default())
            } else {
              None
            };

            react_options.runtime = if config.automatic_jsx_runtime {
              if let Some(import_source) = &config.jsx_import_source {
                react_options.import_source = Some(import_source.clone());
              }
              Some(react::Runtime::Automatic)
            } else {
              Some(react::Runtime::Classic)
            };
          }

          let global_mark = Mark::fresh(Mark::root());
          let unresolved_mark = Mark::fresh(Mark::root());
          module.mutate(&mut (
            resolver(unresolved_mark, global_mark, config.is_type_script()),
            // Decorators can use type information, so must run before the TypeScript pass.
            Optional::new(
              decorators::decorators(decorators::Config {
                legacy: true,
                // Always disabled for now, SWC's implementation doesn't match TSC.
                emit_metadata: false,
                // use_define_for_class_fields is ignored here, uses preset-env assumptions instead
                ..Default::default()
              }),
              config.decorators,
            ),
            Optional::new(
              typescript::tsx(
                source_map.clone(),
                Default::default(),
                typescript::TsxConfig {
                  pragma: react_options.pragma.clone(),
                  pragma_frag: react_options.pragma_frag.clone(),
                },
                Some(&comments),
                unresolved_mark,
                global_mark,
              ),
              config.is_type_script() && config.is_jsx(),
            ),
            Optional::new(
              typescript::strip(unresolved_mark, global_mark),
              config.is_type_script() && !config.is_jsx(),
            ),
          ));

          let is_module = module.is_module();
          // If it's a script, convert into module. This needs to happen after
          // the resolver (which behaves differently for non-/strict mode).
          let module = match module {
            Program::Module(module) => module,
            Program::Script(script) => Module {
              span: script.span,
              shebang: None,
              body: script.body.into_iter().map(ModuleItem::Stmt).collect(),
            },
          };

          let mut program = Program::Module(module);
          program.mutate(&mut Optional::new(
            react::react(
              source_map.clone(),
              Some(&comments),
              react_options,
              global_mark,
              unresolved_mark,
            ),
            config.is_jsx(),
          ));
          let mut module = program.expect_module();

          let mut preset_env_config = swc_core::ecma::preset_env::Config {
            dynamic_import: true,
            ..Default::default()
          };
          let versions = targets_to_versions(&config.targets);
          let mut should_run_preset_env = false;
          if !config.is_swc_helpers {
            // Avoid transpiling @swc/helpers so that we don't cause infinite recursion.
            // Filter the versions for preset_env only so that syntax support checks
            // (e.g. in esm2cjs) still work correctly.
            if let Some(versions) = versions {
              should_run_preset_env = true;
              preset_env_config.targets = Some(Targets::Versions(versions));
              preset_env_config.shipped_proposals = true;
              preset_env_config.mode = Some(Entry);
              preset_env_config.bugfixes = true;
            }
          }

          let mut assumptions = Assumptions::default();
          if config.is_type_script() && !config.use_define_for_class_fields {
            assumptions.set_public_class_fields |= true;
          }

          let mut diagnostics = vec![];
          if let Some(call_macro) = call_macro {
            let mut errors = Vec::new();
            module = module.fold_with(&mut Macros::new(call_macro, &source_map, &mut errors));
            for error in errors {
              diagnostics.push(macro_error_to_diagnostic(error, &source_map));
            }
          }

          if config.scope_hoist && config.inline_constants {
            let mut constant_module = ConstantModule::new();
            module.visit_with(&mut constant_module);
            result.is_constant_module = constant_module.is_constant_module;
          }

          module.visit_mut_with(&mut (
            Optional::new(
              TypeofReplacer::new(unresolved_mark, config.is_node()),
              config.source_type != SourceType::Script,
            ),
            // Inline process.env and process.browser,
            Optional::new(
              EnvReplacer {
                replace_env: config.replace_env(),
                env: &config.env,
                is_browser: config.is_browser(),
                used_env: &mut result.used_env,
                source_map: source_map.clone(),
                diagnostics: &mut diagnostics,
                unresolved_mark,
              },
              config.source_type != SourceType::Script,
            ),
            paren_remover(Some(&comments)),
            // Simplify expressions and remove dead branches so that we
            // don't include dependencies inside conditionals that are always false.
            expr_simplifier(unresolved_mark, Default::default()),
            dead_branch_remover(unresolved_mark),
          ));

          let mut module = module.fold_with(&mut Optional::new(
            inline_fs(
              config.filename.as_str(),
              source_map.clone(),
              unresolved_mark,
              global_mark,
              &config.project_root,
              &mut fs_deps,
              is_module,
            ),
            should_inline_fs,
          ));

          module.visit_mut_with(
            // Replace __dirname and __filename with placeholders in Node env
            &mut Optional::new(
              NodeReplacer {
                source_map: source_map.clone(),
                items: &mut global_deps,
                global_mark,
                globals: IndexMap::new(),
                filename: Path::new(&config.filename),
                unresolved_mark,
                has_node_replacements: &mut result.has_node_replacements,
                is_esm: config.is_esm_output,
              },
              config.node_replacer(),
            ),
          );

          module.visit_mut_with(
            // Insert dependencies for node globals
            &mut Optional::new(
              GlobalReplacer {
                source_map: source_map.clone(),
                items: &mut global_deps,
                global_mark,
                globals: IndexMap::new(),
                project_root: Path::new(&config.project_root),
                filename: Path::new(&config.filename),
                unresolved_mark,
                scope_hoist: config.scope_hoist,
              },
              config.insert_node_globals(),
            ),
          );

          let mut program = Program::Module(module);
          program.mutate(&mut (
            // Transpile new syntax to older syntax if needed
            Optional::new(
              preset_env(
                unresolved_mark,
                Some(&comments),
                preset_env_config,
                assumptions,
                &mut Default::default(),
              ),
              should_run_preset_env,
            ),
            // Inject SWC helpers if needed.
            helpers::inject_helpers(global_mark),
          ));
          let mut module = program.expect_module();

          // Flush Id=(JsWord, SyntaxContexts) into unique names and reresolve to
          // set global_mark for all nodes, even generated ones.
          // - This will also remove any other other marks (like ignore_mark)
          // This only needs to be done if preset_env ran because all other transforms
          // insert declarations with global_mark (even though they are generated).
          if config.scope_hoist && should_run_preset_env {
            module.visit_mut_with(&mut (hygiene(), resolver(unresolved_mark, global_mark, false)))
          }

          // Collect dependencies
          let ignore_mark = Mark::fresh(Mark::root());
          let (module, helpers) = dependency_collector(
            module,
            source_map.clone(),
            &mut result.dependencies,
            ignore_mark,
            unresolved_mark,
            &config,
            &mut diagnostics,
          );

          result.helpers = helpers;
          diagnostics.extend(error_buffer_to_diagnostics(&error_buffer, &source_map));

          if diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error)
          {
            result.diagnostics = Some(diagnostics);
            return Ok(result);
          }

          let mut collect = Collect::new(
            source_map.clone(),
            unresolved_mark,
            ignore_mark,
            global_mark,
            config.trace_bailouts,
            is_module,
          );
          module.visit_with(&mut collect);
          if let Some(bailouts) = &collect.bailouts {
            diagnostics.extend(bailouts.iter().map(|bailout| bailout.to_diagnostic()));
          }

          if matches!(
            config.context,
            EnvContext::ReactClient | EnvContext::ReactServer
          ) {
            module.visit_with(&mut ReactLazy::new(&collect, &mut result.dependencies));
          }

          let mut module = if config.scope_hoist {
            let res = hoist(module, config.module_id.as_str(), unresolved_mark, &collect);
            match res {
              Ok((module, hoist_result, hoist_diagnostics)) => {
                result.hoist_result = Some(hoist_result);
                diagnostics.extend(hoist_diagnostics);
                module
              }
              Err(diagnostics) => {
                result.diagnostics = Some(diagnostics);
                return Ok(result);
              }
            }
          } else {
            // Bail if we could not statically analyze.
            if collect.static_cjs_exports && !collect.should_wrap {
              result.symbol_result = Some(collect.into());
            }

            let (module, needs_helpers) = esm2cjs(module, unresolved_mark, versions);
            result.needs_esm_helpers = needs_helpers;
            module
          };

          module.visit_mut_with(&mut (reserved_words(), hygiene(), fixer(Some(&comments))));

          result.dependencies.extend(global_deps);
          result.dependencies.extend(fs_deps);

          if !diagnostics.is_empty() {
            result.diagnostics = Some(diagnostics);
          }

          let (buf, src_map_buf) = emit(source_map.clone(), comments, &module, config.source_maps)?;
          if config.source_maps
            && source_map
              .build_source_map_with_config(&src_map_buf, None, SourceMapConfig)
              .to_writer(&mut map_buf)
              .is_ok()
          {
            result.map = Some(String::from_utf8(map_buf).unwrap());
          }
          result.code = buf;
          Ok(result)
        },
      )
    })
  })
}

pub type ParseResult<T> = Result<T, Vec<Error>>;

fn parse(
  code: &str,
  filename: PathBuf,
  source_map: &Lrc<SourceMap>,
  config: &Config,
) -> ParseResult<(Program, SingleThreadedComments)> {
  let source_file = source_map.new_source_file(Lrc::new(FileName::Real(filename)), code.into());
  let comments = SingleThreadedComments::default();
  let syntax = if config.is_type_script() {
    Syntax::Typescript(TsSyntax {
      tsx: config.is_jsx(),
      decorators: config.decorators,
      ..Default::default()
    })
  } else {
    Syntax::Es(EsSyntax {
      jsx: config.is_jsx(),
      export_default_from: true,
      decorators: config.decorators,
      import_attributes: true,
      allow_return_outside_function: true,
      ..Default::default()
    })
  };

  let lexer = Lexer::new(
    syntax,
    Default::default(),
    StringInput::from(&*source_file),
    Some(&comments),
  );

  let mut parser = Parser::new_from(lexer);
  let result = parser.parse_program();

  let module = match result {
    Err(err) => {
      // A fatal error
      return Err(vec![err]);
    }
    Ok(module) => module,
  };
  // Recoverable errors
  let errors = parser.take_errors();
  if !errors.is_empty() {
    return Err(errors);
  }

  Ok((module, comments))
}

fn emit(
  source_map: Lrc<SourceMap>,
  comments: SingleThreadedComments,
  module: &Module,
  source_maps: bool,
) -> Result<(Vec<u8>, SourceMapBuffer), std::io::Error> {
  let mut src_map_buf = vec![];
  let mut buf = vec![];
  {
    let writer = Box::new(JsWriter::new(
      source_map.clone(),
      "\n",
      &mut buf,
      if source_maps {
        Some(&mut src_map_buf)
      } else {
        None
      },
    ));
    let config = swc_core::ecma::codegen::Config::default()
      .with_target(swc_core::ecma::ast::EsVersion::Es5)
      // Make sure the output works regardless of whether it's loaded with the correct (utf8) encoding
      .with_ascii_only(true);
    let mut emitter = swc_core::ecma::codegen::Emitter {
      cfg: config,
      comments: Some(&comments),
      cm: source_map,
      wr: writer,
    };

    emitter.emit_module(module)?;
  }

  Ok((buf, src_map_buf))
}

// Exclude macro expansions from source maps.
struct SourceMapConfig;
impl SourceMapGenConfig for SourceMapConfig {
  fn file_name_to_source(&self, f: &FileName) -> String {
    f.to_string()
  }

  fn skip(&self, f: &FileName) -> bool {
    matches!(f, FileName::MacroExpansion | FileName::Internal(..))
  }
}

fn macro_error_to_diagnostic(error: MacroError, source_map: &SourceMap) -> Diagnostic {
  match error {
    MacroError::EvaluationError(span) => Diagnostic {
      message: "Could not statically evaluate macro argument".into(),
      code_highlights: Some(vec![CodeHighlight {
        message: None,
        loc: SourceLocation::from(source_map, span),
      }]),
      hints: None,
      show_environment: false,
      severity: crate::utils::DiagnosticSeverity::Error,
      documentation_url: None,
    },
    MacroError::LoadError(err, span) => Diagnostic {
      message: format!("Error loading macro: {}", err),
      code_highlights: Some(vec![CodeHighlight {
        message: None,
        loc: SourceLocation::from(source_map, span),
      }]),
      hints: None,
      show_environment: false,
      severity: crate::utils::DiagnosticSeverity::Error,
      documentation_url: None,
    },
    MacroError::ExecutionError(err, span) => Diagnostic {
      message: format!("Error evaluating macro: {}", err),
      code_highlights: Some(vec![CodeHighlight {
        message: None,
        loc: SourceLocation::from(source_map, span),
      }]),
      hints: None,
      show_environment: false,
      severity: crate::utils::DiagnosticSeverity::Error,
      documentation_url: None,
    },
    MacroError::ParseError(err) => {
      let error_buffer = ErrorBuffer::default();
      let handler = Handler::with_emitter(true, false, Box::new(error_buffer.clone()));
      err.into_diagnostic(&handler).emit();
      let mut diagnostics = error_buffer_to_diagnostics(&error_buffer, source_map);
      return diagnostics.pop().unwrap();
    }
  }
}
