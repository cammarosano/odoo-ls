use ruff_text_size::TextRange;

use crate::{constants::{OYarn, SymType}, core::evaluation::{ContextValue, Evaluation}, oyarn, threads::SessionInfo, S};
use std::{cell::RefCell, collections::HashMap, rc::{Rc, Weak}};

use super::symbol::Symbol;

/// Represents a variable, import, or function parameter in the symbol tree.
///
/// # Usage Contexts
///
/// Variables are created in multiple contexts:
/// - **Assignments**: `x = 5`, `self.field = fields.Char()`
/// - **Imports**: `from odoo import models` (creates import variable for `models`)
/// - **Parameters**: Function arguments
///
/// # Type Tracking
///
/// The `evaluations` field stores inferred types. Can be multiple for ambiguous assignments:
/// ```python
/// x = "str" if condition else 5  # evaluations: [str, int]
/// ```
///
/// # Import Variables
///
/// When `is_import_variable = true`, the evaluation points to the imported symbol.
/// Following the import chain during type inference resolves to the actual definition.
///
/// # Odoo Fields
///
/// For Odoo model fields, `get_relational_model()` extracts the comodel for Many2one/One2many/Many2many.
///
/// See [Python Core Onboarding Guide](../../docs/python-core-onboarding.md#variablesymbol) for details.
#[derive(Debug)]
pub struct VariableSymbol {
    pub name: OYarn,
    pub is_external: bool,
    pub doc_string: Option<String>,
    pub weak_self: Option<Weak<RefCell<Symbol>>>,
    pub parent: Option<Weak<RefCell<Symbol>>>,
    
    /// True if created from import statement (follows import chain during type inference)
    pub is_import_variable: bool,
    /// True if this is a function parameter
    pub is_parameter: bool,
    /// Inferred types (can be multiple for ambiguous assignments like `x = a if c else b`)
    pub evaluations: Vec<Evaluation>,
    pub range: TextRange,
}

impl VariableSymbol {

    pub fn new(name: OYarn, range: TextRange, is_external: bool) -> Self {
        Self {
            name,
            is_external,
            doc_string: None,
            weak_self: None,
            parent: None,
            range,
            is_import_variable: false,
            is_parameter: false,
            evaluations: vec![],
        }
    }

    /// Checks if this variable is a type alias.
    ///
    /// # Returns
    ///
    /// `true` if all evaluations point to type symbols (not instances) and this is not an import.
    ///
    /// # Example
    ///
    /// ```python
    /// MyType = List[str]  # is_type_alias() -> true
    /// my_var = MyType()   # is_type_alias() -> false (instance)
    /// ```
    pub fn is_type_alias(&self) -> bool {
        //TODO it does not use get_symbol call, and only evaluate "sym" from EvaluationSymbol
        return self.evaluations.len() >= 1 && self.evaluations.iter().all(|x| !x.symbol.is_instance().unwrap_or(true)) && !self.is_import_variable;
    }

    // pub fn full_size_of(self) -> serde_json::Value {
    //     let name_to_add = if self.name.len() > 15 {
    //         self.name.len()
    //     } else {
    //         0
    //     };
    //     let mut evals = 0;
    //     for eval in self.evaluations.iter() {
    //         evals += eval.full_size_of();
    //     }
    //     size_of::<Self>() +
    //     name_to_add +
    //     self.doc_string.map(|x| x.capacity()).unwrap_or(0) +
    //     self.ast_indexes.capacity() +
    //     evals
    // }

    /// Extracts the comodel class symbol for Odoo relational fields.
    ///
    /// If this variable is a Many2one, One2many, or Many2many field, this method
    /// returns the main class symbols of the target model.
    ///
    /// # Arguments
    ///
    /// * `session` - Current session info
    /// * `from_module` - Context module (for dependency filtering)
    ///
    /// # Returns
    ///
    /// Main class symbols of the comodel, or empty vec if not a relational field.
    ///
    /// # Example
    ///
    /// ```python
    /// partner_id = fields.Many2one('res.partner')
    /// # get_relational_model() -> [Partner class symbol]
    /// ```
    pub fn get_relational_model(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>) -> Vec<Rc<RefCell<Symbol>>> {
        for eval in self.evaluations.iter() {
            let symbol = eval.symbol.get_symbol(session, &mut None, &mut vec![], None);
            let mut context = None;
            if let Some(parent) = self.parent.as_ref() {
                // To be able to follow related fields, we need to have the base_attr set in order to find the __get__ hook in next_refs
                // we update the context here for the case where we are coming from a decorator for example.
                context = Some(HashMap::new());
                context.as_mut().unwrap().insert(S!("base_attr"), ContextValue::SYMBOL(parent.clone()));
            }
            let eval_weaks = Symbol::follow_ref(&symbol, session, &mut context, false, false, None);
            for eval_weak in eval_weaks.iter() {
                if let Some(symbol) = eval_weak.upgrade_weak() {
                    if ["Many2one", "One2many", "Many2many"].contains(&symbol.borrow().name().as_str()) {
                        let Some(comodel) = eval_weak.as_weak().context.get("comodel_name") else {
                            continue;
                        };
                        let Some(model) = session.sync_odoo.models.get(&oyarn!("{}", &comodel.as_string())).cloned() else {
                            continue;
                        };
                        return model.borrow().get_main_symbols(session, from_module);
                    } else if symbol.borrow().typ() == SymType::CLASS { // Already evaluated from descriptor in follow_ref
                        return vec![symbol];
                    }
                }
            }
        }
        vec![]
    }

    pub fn is_value(&self) -> bool {
        return !self.evaluations.iter().any(|x| x.value.is_none());
    }

}
