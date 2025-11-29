use lsp_types::{Hover, HoverContents, MarkupContent};
use crate::core::evaluation::Evaluation;
use crate::core::file_mgr::FileInfo;
use crate::features::xml_ast_utils::{XmlAstResult, XmlAstUtils};
use crate::threads::SessionInfo;
use std::rc::Rc;
use crate::core::symbols::symbol::Symbol;
use crate::features::ast_utils::AstUtils;
use crate::features::features_utils::FeaturesUtils;
use std::cell::RefCell;


/// Provides hover information for Python and XML files.
///
/// The hover feature displays type information and documentation when users
/// hover over symbols. It handles both Python code (using AST analysis and
/// type evaluation) and XML files (using XML parsing and symbol lookup).
///
/// # Response Format
/// Returns markdown-formatted content including:
/// * Type signature (e.g., `(method) def compute(self) -> float`)
/// * Source location links
/// * Module information
/// * Docstrings
pub struct HoverFeature {}

impl HoverFeature {
    /// Provides hover information for Python files.
    ///
    /// # Flow
    /// 1. Convert cursor position to byte offset
    /// 2. Use `AstUtils::get_symbols` to find and evaluate symbols at cursor
    /// 3. Build markdown description using `FeaturesUtils::build_markdown_description`
    /// 4. Return `Hover` with content and highlighted range
    ///
    /// # Special Handling
    /// * Odoo field strings (model names, compute methods, etc.) are resolved
    ///   to their actual definitions via the call expression context
    pub fn hover_python(session: &mut SessionInfo, file_symbol: &Rc<RefCell<Symbol>>, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32) -> Option<Hover> {
        let offset = file_info.borrow().position_to_offset(line, character, session.sync_odoo.encoding);
        let file_info_ast_clone = file_info.borrow().file_info_ast.clone();
        let file_info_ast_ref = file_info_ast_clone.borrow();
        let (analyse_ast_result, range, expr, call_expr) = AstUtils::get_symbols(session, &file_info_ast_ref, file_symbol, offset as u32);
        let evals = analyse_ast_result.evaluations;
        if evals.is_empty() {
            return None;
        };
        drop(expr);
        drop(file_info_ast_ref);
        let range = Some(file_info.borrow().text_range_to_range(&range.unwrap(), session.sync_odoo.encoding));
        Some(Hover { contents:
            HoverContents::Markup(MarkupContent {
                kind: lsp_types::MarkupKind::Markdown,
                value: FeaturesUtils::build_markdown_description(session, Some(file_symbol.clone()), Some(&file_info.borrow().uri), &evals, &call_expr, Some(offset))
            }),
            range: range
        })
    }

    pub fn hover_xml(session: &mut SessionInfo, file_symbol: &Rc<RefCell<Symbol>>, file_info: &Rc<RefCell<FileInfo>>, line: u32, character: u32) -> Option<Hover> {
        let offset = file_info.borrow().position_to_offset(line, character, session.sync_odoo.encoding);
        let data = file_info.borrow().file_info_ast.borrow().text_document.as_ref().unwrap().contents().to_string();
        let document = roxmltree::Document::parse(&data);
        if let Ok(document) = document {
            let root = document.root_element();
            let (symbols, range) = XmlAstUtils::get_symbols(session, file_symbol, root, offset, true);
            let range = range.map(|r| file_info.borrow().std_range_to_range(&r, session.sync_odoo.encoding));
            let evals = symbols.iter().filter(|s| matches!(s, XmlAstResult::SYMBOL(_)))
                .map(|s| Evaluation::eval_from_symbol(&Rc::downgrade(&s.as_symbol()), Some(false))).collect::<Vec<Evaluation>>();
            return Some(Hover { contents:
                HoverContents::Markup(MarkupContent {
                    kind: lsp_types::MarkupKind::Markdown,
                    value: FeaturesUtils::build_markdown_description(session, Some(file_symbol.clone()), Some(&file_info.borrow().uri), &evals, &None, Some(offset))
                }),
                range: range
            })
        }
        None
    }

    pub fn hover_csv(_session: &mut SessionInfo, _file_symbol: &Rc<RefCell<Symbol>>, _file_info: &Rc<RefCell<FileInfo>>, _line: u32, _character: u32) -> Option<Hover> {
        None
    }
}