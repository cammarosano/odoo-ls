use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;
use crate::constants::{BuildStatus, BuildSteps, SymType};
use crate::core::evaluation::{AnalyzeAstResult, Context, ContextValue, Evaluation, ExprOrIdent};
use crate::core::odoo::SyncOdoo;
use crate::core::import_resolver::{resolve_from_stmt, resolve_import_stmt};
use crate::core::symbols::symbol::Symbol;
use crate::core::file_mgr::FileInfoAst;
use crate::threads::SessionInfo;
use crate::S;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt, walk_alias, walk_except_handler, walk_parameter, walk_keyword, walk_pattern_keyword, walk_type_param, walk_pattern};
use ruff_python_ast::{Alias, AtomicNodeIndex, ExceptHandler, Expr, ExprCall, Identifier, Keyword, Parameter, Pattern, PatternKeyword, Stmt, TypeParam};
use ruff_text_size::{Ranged, TextRange, TextSize};
use tracing::warn;

/// Utilities for finding and analyzing AST nodes at cursor positions.
///
/// This struct provides the core symbol-finding functionality used by LSP features
/// like hover, definition, and completion. It bridges the gap between cursor positions
/// (byte offsets) and the semantic symbol information stored in the symbol tree.
///
/// # Key Responsibilities
/// - Finding the AST expression at a given cursor offset
/// - Evaluating expressions to determine their types/symbols
/// - Special handling for import statements (which don't have symbols in the file tree)
/// - Ensuring scopes are built before analysis (lazy building)
pub struct AstUtils {}

impl AstUtils {
    /// Finds symbols at a given cursor position within a Python file.
    ///
    /// This is the main entry point for symbol resolution used by hover, definition,
    /// and other features. It traverses the AST to find the expression at the cursor
    /// position and evaluates it to determine its type information.
    ///
    /// # Arguments
    /// * `session` - The current session containing server state
    /// * `file_info_ast` - The parsed AST of the file
    /// * `file_symbol` - The symbol representing the file in the symbol tree
    /// * `offset` - The byte offset of the cursor position
    ///
    /// # Returns
    /// A tuple containing:
    /// * `AnalyzeAstResult` - Evaluations (symbol types) and diagnostics
    /// * `Option<TextRange>` - The range of the found expression
    /// * `Option<ExprOrIdent>` - The AST node at the position (expression or identifier)
    /// * `Option<ExprCall>` - The enclosing call expression (if any, used for context)
    ///
    /// # Special Cases
    /// - Import statements are handled separately via `get_symbol_in_import` because
    ///   imported symbols aren't directly visible in the file's symbol tree
    /// - Returns default/empty results if no expression is found at the offset
    pub fn get_symbols<'a>(session: &mut SessionInfo, file_info_ast: &'a FileInfoAst, file_symbol: &Rc<RefCell<Symbol>>, offset: u32) -> (AnalyzeAstResult, Option<TextRange>, Option<ExprOrIdent<'a>>, Option<ExprCall>) {
        let mut expr: Option<ExprOrIdent<'a>> = None;
        let mut call_expr: Option<ExprCall> = None;
        for stmt in file_info_ast.get_stmts().unwrap().iter() {
            //we have to handle imports differently as symbols are not visible in file.
            if let Some((result, range)) = Self::get_symbol_in_import(session, file_symbol, offset, stmt) {
                return (result, range, None, None);
            }
            (expr, call_expr) = ExprFinderVisitor::find_expr_at(stmt, offset);
            if expr.is_some() {
                break;
            }
        }
        let Some(expr) = expr else {
            warn!("expr not found");
            return (AnalyzeAstResult::default(), None, None, None);
        };
        let (result, range) = Self::get_symbol_from_expr(session, file_symbol, &expr, offset);
        (result, range, Some(expr), call_expr)
    }

    /// Evaluates an expression to determine its type and symbol information.
    ///
    /// This function takes an already-found AST expression and performs type evaluation
    /// on it. It first ensures the enclosing scope is built (triggering lazy building if
    /// needed), then invokes the evaluation system.
    ///
    /// # Arguments
    /// * `session` - The current session containing server state
    /// * `file_symbol` - The symbol representing the file in the symbol tree
    /// * `expr` - The AST expression or identifier to evaluate
    /// * `offset` - The byte offset (used to find the enclosing scope)
    ///
    /// # Returns
    /// * `AnalyzeAstResult` - Contains evaluations with type information
    /// * `Option<TextRange>` - The range of the expression
    ///
    /// # Context
    /// The evaluation is performed with a context containing:
    /// * `module` - The containing module (for dependency resolution)
    /// * `range` - The expression range (used by some evaluators)
    pub fn get_symbol_from_expr<'a>(session: &mut SessionInfo, file_symbol: &Rc<RefCell<Symbol>>, expr: &ExprOrIdent<'a>, offset: u32) -> (AnalyzeAstResult, Option<TextRange>) {
        let parent_symbol = Symbol::get_scope_symbol(file_symbol.clone(), offset, matches!(expr, ExprOrIdent::Parameter(_)));
        AstUtils::build_scope(session, &parent_symbol);
        let from_module;
        if let Some(module) = file_symbol.borrow().find_module() {
            from_module = ContextValue::MODULE(Rc::downgrade(&module));
        } else {
            from_module = ContextValue::BOOLEAN(false);
        }
        let mut context: Option<Context> = Some(HashMap::from([
            (S!("module"), from_module),
            (S!("range"), ContextValue::RANGE(expr.range()))
        ]));
        let analyse_ast_result: AnalyzeAstResult = Evaluation::analyze_ast(session, &expr, parent_symbol.clone(), &expr.range().end(), &mut context,false, &mut vec![]);
        (analyse_ast_result, Some(expr.range()))
    }

    /// Converts an expression AST node to its dotted string representation.
    ///
    /// This is useful for displaying attribute access chains like `self.partner_id.name`
    /// as a single string.
    ///
    /// # Examples
    /// * `Name("self")` → `"self"`
    /// * `Attribute(Name("self"), "partner_id")` → `"selfpartner_id"` (note: no dot separator)
    ///
    /// # Note
    /// The current implementation doesn't add dot separators between components.
    /// Only handles `Name` and `Attribute` expressions; others return `"//Unhandled//"`.
    pub fn flatten_expr(expr: &Expr) -> String {
        match expr {
            Expr::Name(n) => {
                n.id.to_string()
            },
            Expr::Attribute(a) => {
                AstUtils::flatten_expr(&a.value) + &a.attr
            },
            _ => {S!("//Unhandled//")}
        }
    }

    /// Ensures a scope symbol is fully built before analysis.
    ///
    /// This implements lazy building of function bodies. When a feature needs to
    /// analyze an expression within a function, the function's symbols may not yet
    /// be built (they're built lazily for performance). This function triggers the
    /// build if needed.
    ///
    /// # Build Phases Triggered
    /// * `ARCH` - Parses the function body and creates child symbols
    /// * `ARCH_EVAL` - Evaluates types for the function's symbols
    ///
    /// # Scope Resolution
    /// For nested functions, this finds the outermost parent function that needs
    /// building, since building an outer function also builds inner ones.
    ///
    /// # Arguments
    /// * `session` - The current session
    /// * `scope` - The scope symbol (typically a function) to build
    pub fn build_scope(session: &mut SessionInfo<'_>, scope: &Rc<RefCell<Symbol>>) {
        if scope.borrow().typ() == SymType::FUNCTION {
            let parent_func = scope.borrow().get_in_parents(&vec![SymType::FUNCTION], true);
            let scope_to_test = parent_func.and_then(|w| w.upgrade());
            let scope_to_test = scope_to_test.as_ref().unwrap_or(scope);
            if scope_to_test.borrow().as_func().arch_status == BuildStatus::PENDING {
                SyncOdoo::build_now(session, scope_to_test, BuildSteps::ARCH);
            }
            if scope_to_test.borrow().as_func().arch_eval_status == BuildStatus::PENDING {
                SyncOdoo::build_now(session, scope_to_test, BuildSteps::ARCH_EVAL);
            }
        }
    }

    /// Resolves symbols within import statements.
    ///
    /// Import statements require special handling because the imported symbols aren't
    /// directly visible in the file's symbol tree. This function handles both:
    /// * `import X.Y.Z` statements
    /// * `from X.Y import Z` statements
    ///
    /// # Import Resolution Strategy
    ///
    /// For `import a.b.c`:
    /// - If cursor is on an intermediate part (e.g., `b`), resolve the partial path `a.b`
    ///   as a module using `resolve_from_stmt`
    /// - If cursor is on the last part or the alias, use `resolve_import_stmt` to get
    ///   the fully resolved symbol
    ///
    /// For `from a.b import c`:
    /// - Only handles the module part (`a.b`); the imported name `c` is handled by
    ///   normal AST walking since it becomes visible in the file's namespace
    ///
    /// # Arguments
    /// * `session` - The current session
    /// * `file_symbol` - The file symbol for context
    /// * `offset` - Cursor position
    /// * `stmt` - The statement to check (only Import/ImportFrom are handled)
    ///
    /// # Returns
    /// * `Some((result, range))` - If cursor is on an import and symbol was resolved
    /// * `None` - If not an import statement or cursor not on resolvable part
    fn get_symbol_in_import(session: &mut SessionInfo, file_symbol: &Rc<RefCell<Symbol>>, offset: u32, stmt: &Stmt) -> Option<(AnalyzeAstResult, Option<TextRange>)> {
        match stmt {
            Stmt::Import(stmt) => {
                for alias in stmt.names.iter() {
                    if alias.range().contains(TextSize::new(offset)) {
                        let mut is_last = false;
                        let (to_analyze, range) = if alias.name.range().contains(TextSize::new(offset)) {
                            let next_dot_offset = alias.name.id.as_str()[offset as usize - alias.name.range().start().to_usize()..].find(".");
                            if let Some(next_dot_offset) = next_dot_offset {
                                let end = offset as usize + next_dot_offset;
                                let text = &alias.name.id.as_str()[..end - alias.name.range().start().to_usize()];
                                let start_range = text.rfind(".").map(|p| p+1).unwrap_or(0) + alias.name.range().start().to_usize();
                                (text, TextRange::new(TextSize::new(start_range as u32), TextSize::new(end as u32)))
                            } else {
                                is_last = true;
                                (alias.name.id.as_str(), alias.name.range())
                            }
                        } else if alias.asname.is_some() && alias.asname.as_ref().unwrap().range().contains(TextSize::new(offset)) {
                            is_last = true;
                            (alias.asname.as_ref().unwrap().id.as_str(), alias.asname.as_ref().unwrap().range())
                        } else {
                            return None;
                        };
                        if !is_last {
                            //we import as a from_stmt, to refuse import of variables, as the import stmt is not complete
                            let to_analyze = Identifier { id: Name::new(to_analyze), range: TextRange::new(TextSize::new(0), TextSize::new(0)), node_index: AtomicNodeIndex::default() };
                            let (from_symbol, _fallback_sym, _file_tree) = resolve_from_stmt(session, file_symbol, Some(&to_analyze), 0);
                            if let Some(symbol) = from_symbol {
                                let result = AnalyzeAstResult {
                                    evaluations: vec![Evaluation::eval_from_symbol(&Rc::downgrade(&symbol), None)],
                                    diagnostics: vec![],
                                };
                                return Some((result, Some(range)));
                            }
                        } else {
                            let res = resolve_import_stmt(session, file_symbol, None, &[
                                Alias { //create a dummy alias with a asname to force full import
                                    name: Identifier { id: Name::new(to_analyze), range: TextRange::new(TextSize::new(0), TextSize::new(0)), node_index: AtomicNodeIndex::default() },
                                    asname: Some(Identifier { id: Name::new("fake_name"), range: alias.name.range().clone(), node_index: AtomicNodeIndex::default() }),
                                    range: alias.range(),
                                    node_index: AtomicNodeIndex::default()
                                }], 0, &mut None);
                            let res = res.into_iter().filter(|s| s.found).collect::<Vec<_>>();
                            if !res.is_empty() {
                                let result = AnalyzeAstResult {
                                    evaluations: res.iter().map(
                                        |s| Evaluation::eval_from_symbol(&Rc::downgrade(&s.symbol), None)
                                    ).collect(),
                                    diagnostics: vec![],
                                };
                                return Some((result, Some(range)));
                            }
                        }
                        return None;
                    }
                }
            },
            Stmt::ImportFrom(stmt) => {
                //only check module as names are already supported by default ast walking and name resolution
                if stmt.module.is_some() && stmt.module.as_ref().unwrap().range().contains(TextSize::new(offset)) {
                    let module = stmt.module.as_ref().unwrap();
                    let (to_analyze, range) = if module.range().contains(TextSize::new(offset)) {
                        let next_dot_offset = module.id.as_str()[offset as usize - module.range().start().to_usize()..].find(".");
                        if let Some(next_dot_offset) = next_dot_offset {
                            let end = offset as usize + next_dot_offset;
                            let text = &module.id.as_str()[..end - module.range().start().to_usize()];
                            let start_range = text.rfind(".").map(|p| p+1).unwrap_or(0) + module.range().start().to_usize();
                            (text, TextRange::new(TextSize::new(start_range as u32), TextSize::new(end as u32)))
                        } else {
                            (module.id.as_str(), module.range())
                        }
                    } else {
                        return None;
                    };
                    let to_analyze = Identifier { id: Name::new(to_analyze), range: TextRange::new(TextSize::new(0), TextSize::new(0)), node_index: AtomicNodeIndex::default() };
                    let (from_symbol, _fallback_sym, _file_tree) = resolve_from_stmt(session, file_symbol, Some(&to_analyze), 0);
                    if let Some(symbol) = from_symbol {
                        let result = AnalyzeAstResult {
                            evaluations: vec![Evaluation::eval_from_symbol(&Rc::downgrade(&symbol), None)],
                            diagnostics: vec![],
                        };
                        return Some((result, Some(range)));
                    }
                }
            },
            _ => {
                return None;
            }
        }
        None
    }
}


/// A visitor that finds the AST expression at a specific cursor offset.
///
/// This visitor traverses the AST tree and identifies:
/// 1. The innermost expression containing the cursor position
/// 2. The last call expression before the cursor (useful for completion context)
///
/// # How It Works
///
/// The visitor walks the AST depth-first. For each node:
/// - If the node's range contains the offset, it descends into children
/// - After visiting children, if no child claimed the expression, this node becomes it
/// - For call expressions, it tracks whether the cursor is within the arguments
///
/// # Special Node Handling
///
/// Several node types need special handling because they contain identifiers that
/// aren't represented as `Expr::Name`:
/// - **Alias** (imports): `import foo as bar` - handles both `foo` and `bar`
/// - **ExceptHandler**: `except E as e:` - handles the bound name `e`
/// - **Parameter**: Function parameters
/// - **Keyword**: Keyword arguments in calls
/// - **PatternKeyword**: Pattern matching keywords
/// - **TypeParam**: Generic type parameters
/// - **Pattern**: Match patterns (MatchMapping, MatchStar, MatchAs)
/// - **FunctionDef/ClassDef**: The defined name
/// - **Global/Nonlocal**: The listed names
///
/// # Fields
/// * `offset` - The target cursor position (byte offset)
/// * `expr` - The found expression or identifier (set during traversal)
/// * `last_call_expr` - The last call expression containing the cursor in its arguments
pub struct ExprFinderVisitor<'a> {
    offset: TextSize,
    expr: Option<ExprOrIdent<'a>>,
    last_call_expr: Option<&'a ExprCall>,
}

impl<'a> ExprFinderVisitor<'a> {
    /// Finds the expression at a given offset within a statement.
    ///
    /// This is the main entry point for expression finding. It creates a visitor,
    /// walks the statement tree, and returns the results.
    ///
    /// # Arguments
    /// * `stmt` - The statement to search within
    /// * `offset` - The byte offset of the cursor position
    ///
    /// # Returns
    /// A tuple containing:
    /// * `Option<ExprOrIdent>` - The expression/identifier at the offset, if found
    /// * `Option<ExprCall>` - The enclosing call expression (cloned), if the cursor
    ///   is within a function call's arguments. This is used by completion to provide
    ///   parameter-aware suggestions.
    pub fn find_expr_at(stmt: &'a Stmt, offset: u32) -> (Option<ExprOrIdent<'a>>, Option<ExprCall>) {
        let mut visitor = Self {
            offset: TextSize::new(offset),
            expr: None,
            last_call_expr: None
        };
        visitor.visit_stmt(stmt);
        (visitor.expr, visitor.last_call_expr.cloned())
    }

}

impl<'a> Visitor<'a> for ExprFinderVisitor<'a> {

    fn visit_expr(&mut self, expr: &'a Expr) {
        if expr.range().contains(self.offset) {
            if let Expr::Call(expr_call) = expr {
                if expr_call.arguments.range().contains(self.offset){
                    self.last_call_expr = Some(expr_call);
                }
            }
            walk_expr(self, expr);
            if self.expr.is_none() {
                self.expr = Some(ExprOrIdent::Expr(expr));
            }
        } else {
            walk_expr(self, expr);
        }
    }

    fn visit_alias(&mut self, alias: &'a Alias) {
        walk_alias(self, alias);
        if self.expr.is_none() {
            if alias.name.range().contains(self.offset) {
                self.expr = Some(ExprOrIdent::Ident(&alias.name));
            } else if let Some(ref asname) = alias.asname {
                if asname.range().contains(self.offset) {
                    self.expr = Some(ExprOrIdent::Ident(asname))
                }
            }
        }
    }

    fn visit_except_handler(&mut self, except_handler: &'a ExceptHandler) {
        walk_except_handler(self, except_handler);
        if self.expr.is_none() {
            let ExceptHandler::ExceptHandler(ref handler) = *except_handler;
            if let Some(ref ident) = handler.name {
                if ident.clone().range().contains(self.offset) {
                    self.expr = Some(ExprOrIdent::Ident(ident));
                }
            }
        } else {
            walk_except_handler(self, except_handler);
        }
    }

    fn visit_parameter(&mut self, parameter: &'a Parameter) {
        walk_parameter(self, parameter);
        if self.expr.is_none() && parameter.name.range().contains(self.offset) {
            self.expr = Some(ExprOrIdent::Parameter(parameter));
        }
    }

    fn visit_keyword(&mut self, keyword: &'a Keyword) {
        walk_keyword(self, keyword);

        if self.expr.is_none() {
            if let Some(ref ident) = keyword.arg {
                if ident.range().contains(self.offset) {
                    self.expr = Some(ExprOrIdent::Ident(ident));
                }
            }
        } else {
            walk_keyword(self, keyword)
        }
    }

    fn visit_pattern_keyword(&mut self, pattern_keyword: &'a PatternKeyword) {
        walk_pattern_keyword(self, pattern_keyword);

        if self.expr.is_none() && pattern_keyword.clone().attr.range().contains(self.offset) {
            self.expr = Some(ExprOrIdent::Ident(&pattern_keyword.attr));
        } else {
            walk_pattern_keyword(self, pattern_keyword);
        }
    }

    fn visit_type_param(&mut self, type_param: &'a TypeParam) {
        if type_param.range().contains(self.offset) {
            if self.expr.is_none() {
                walk_type_param(self, type_param);
                let ident = match type_param {
                    TypeParam::TypeVar(t) => Some(&t.name),
                    TypeParam::ParamSpec(t) => Some(&t.name),
                    TypeParam::TypeVarTuple(t) => Some(&t.name),
                };

                if ident.is_some() && ident.unwrap().range().contains(self.offset) {
                    self.expr = Some(ExprOrIdent::Ident(ident.unwrap()));
                }

            }
        } else {
            walk_type_param(self, type_param);
        }
    }

    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        if pattern.range().contains(self.offset) {
            if self.expr.is_none() {
                walk_pattern(self, pattern);
                let ident  = match pattern {
                    Pattern::MatchMapping(mapping) => &mapping.rest,
                    Pattern::MatchStar(mapping) => &mapping.name,
                    Pattern::MatchAs(mapping) => &mapping.name,
                    _ => &None
                };

                if let Some(ident) = ident {
                    if ident.range().contains(self.offset) {
                        self.expr = Some(ExprOrIdent::Ident(ident));
                    }
                }
            }
        }
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        walk_stmt(self, stmt);
        if self.expr.is_none() {
            let idents = match stmt {
                Stmt::FunctionDef(stmt) => vec![&stmt.name],
                Stmt::ClassDef(stmt) => vec![&stmt.name],
                Stmt::Global(stmt) => stmt.names.iter().collect(),
                Stmt::Nonlocal(stmt) => stmt.names.iter().collect(),
                _ => vec![],
            };

            for ident in idents {
                if ident.range().contains(self.offset) {
                    self.expr = Some(ExprOrIdent::Ident(ident));
                    break;
                }
            }
        }
    }
}

