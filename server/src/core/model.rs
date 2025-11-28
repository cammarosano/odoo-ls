use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::rc::Rc;
use std::rc::Weak;
use lsp_types::MessageType;
use weak_table::PtrWeakHashSet;
use std::collections::HashSet;

use crate::constants::BuildStatus;
use crate::constants::BuildSteps;
use crate::constants::OYarn;
use crate::constants::SymType;
use crate::threads::SessionInfo;

use super::symbols::module_symbol::ModuleSymbol;
use super::symbols::symbol::Symbol;

/// Metadata for an Odoo model attached to a `ClassSymbol`.
///
/// # Purpose
///
/// Stores Odoo-specific attributes extracted from the class definition:
/// - `_name`: Model identifier
/// - `_inherit`: Models to extend
/// - `_inherits`: Delegation inheritance
/// - Field definitions and computed field tracking
///
/// # Inheritance Patterns
///
/// ## `_name`: Define New Model
/// ```python
/// class Partner(models.Model):
///     _name = 'res.partner'
///     name = fields.Char()
/// ```
///
/// ## `_inherit`: Extend Existing Model(s)
/// ```python
/// class Partner(models.Model):
///     _inherit = 'res.partner'  # or ['res.partner', 'mail.thread']
///     new_field = fields.Char()
/// ```
///
/// ## `_inherits`: Delegation Inheritance
/// ```python
/// class User(models.Model):
///     _name = 'res.users'
///     _inherits = {'res.partner': 'partner_id'}
///     partner_id = fields.Many2one('res.partner', required=True)
/// ```
///
/// See [Python Core Onboarding Guide](../../docs/python-core-onboarding.md#model-inheritance-patterns) for details.
#[derive(Debug)]
pub struct ModelData {
    /// Model's `_name` attribute (e.g., "res.partner")
    pub name: OYarn,
    /// List of models to inherit from (`_inherit`)
    pub inherit: Vec<OYarn>,
    /// Delegation inheritance: (model_name, field_name)
    pub inherits: Vec<(OYarn, OYarn)>,

    pub description: String,
    pub auto: bool,
    pub log_access: bool,
    pub table: String,
    pub sequence: String,
    pub sql_constraints: Vec<String>,
    pub is_abstract: bool,
    pub transient: bool,
    pub rec_name: Option<String>,
    pub order: String,
    pub check_company_auto: bool,
    pub parent_name: String,
    pub active_name: Option<String>,
    pub parent_store: bool,
    pub data_name: String,
    pub fold_name: String,
    /// Key: compute function name, Value: field names that are computed by this function
    pub computes: HashMap<OYarn, HashSet<OYarn>>,
}

impl ModelData {
    pub fn new() -> Self {
        Self {
            name: OYarn::from(""),
            inherit: Vec::new(),
            inherits: Vec::new(),
            description: String::new(),
            auto: false,
            log_access: false,
            table: String::new(),
            sequence: String::new(),
            sql_constraints: Vec::new(),
            is_abstract: false,
            transient: false,
            rec_name: None,
            order: String::from("id"),
            check_company_auto: false,
            parent_name: String::from("parent_id"),
            active_name: None,
            parent_store: false,
            data_name: String::from("date"),
            fold_name: String::from("fold"),
            computes: HashMap::new(),
        }
    }
}

/// Aggregates all class symbols that define or extend the same Odoo model.
///
/// # Purpose
///
/// A single Odoo model (identified by `_name`) can be defined and extended across
/// multiple Python files and modules. This struct collects all those class symbols.
///
/// # Example
///
/// ```python
/// # In base module:
/// class Partner(models.Model):
///     _name = 'res.partner'
///     name = fields.Char()
///
/// # In sale module:
/// class Partner(models.Model):
///     _inherit = 'res.partner'
///     sale_count = fields.Integer()
///
/// # In account module:
/// class Partner(models.Model):
///     _inherit = 'res.partner'
///     credit_limit = fields.Float()
/// ```
///
/// All three class symbols belong to the same `Model` with `name = "res.partner"`.
///
/// # Storage
///
/// Stored in `SyncOdoo.models: HashMap<OYarn, Rc<RefCell<Model>>>` keyed by model name.
///
/// See [Python Core Onboarding Guide](../../docs/python-core-onboarding.md#model-aggregation) for details.
#[derive(Debug)]
pub struct Model {
    /// Model name (e.g., "res.partner")
    name: OYarn,
    /// All class symbols that define/extend this model
    symbols: PtrWeakHashSet<Weak<RefCell<Symbol>>>,
    /// Files that depend on this model (for invalidation)
    pub dependents: PtrWeakHashSet<Weak<RefCell<Symbol>>>,
}

impl Model {
    pub fn new(name: OYarn, symbol: Rc<RefCell<Symbol>>) -> Self {
        let mut res = Self {
            name,
            symbols: PtrWeakHashSet::new(),
            dependents: PtrWeakHashSet::new(),
        };
        res.symbols.insert(symbol);
        res
    }

    /// Adds a class symbol to this model's collection.
    ///
    /// When a new class extends or defines this model, it's added to the `symbols` set.
    /// All dependents are then invalidated (marked for revalidation) since the model changed.
    ///
    /// # Arguments
    ///
    /// * `session` - Current session info
    /// * `symbol` - Class symbol to add
    ///
    /// # Side Effects
    ///
    /// Invalidates all files that depend on this model (triggers VALIDATION rebuild).
    pub fn add_symbol(&mut self, session: &mut SessionInfo, symbol: Rc<RefCell<Symbol>>) {
        if self.symbols.contains(&symbol) {
            return;
        }
        self.symbols.insert(symbol.clone());
        let from_module = symbol.borrow().find_module();
        self.add_dependents_to_validation(session, from_module);
    }

    pub fn remove_symbol(&mut self, session: &mut SessionInfo, symbol: &Rc<RefCell<Symbol>>, from_module: Option<Rc<RefCell<Symbol>>>) {
        self.symbols.remove(symbol);
        self.add_dependents_to_validation(session, from_module);
    }

    /// Retrieves all class symbols for this model visible from a given module.
    ///
    /// # Arguments
    ///
    /// * `session` - Current session info
    /// * `from_module` - Context module (only returns symbols in dependencies)
    ///
    /// # Returns
    ///
    /// All class symbols (main definitions and extensions) visible from `from_module`.
    /// If `from_module` is `None`, returns all symbols regardless of dependencies.
    pub fn get_symbols(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>) -> Vec<Rc<RefCell<Symbol>>> {
        let mut symbol = Vec::new();
        for s in self.symbols.iter() {
            let module = s.borrow().find_module().expect("Model should be declared in a module");
            if from_module.is_none() || ModuleSymbol::is_in_deps(session, from_module.as_ref().unwrap(), &module.borrow().as_module_package().dir_name) {
                symbol.push(s);
            }
        }
        symbol
    }

    /// Retrieves only the main class symbols for this model (excludes extensions).
    ///
    /// Main symbols are classes where `_name` != `_inherit` (i.e., they define the model
    /// rather than just extending it).
    ///
    /// # Arguments
    ///
    /// * `session` - Current session info
    /// * `from_module` - Context module (only returns symbols in dependencies)
    ///
    /// # Returns
    ///
    /// Main class symbols visible from `from_module`.
    ///
    /// # Example
    ///
    /// ```python
    /// class Partner(models.Model):
    ///     _name = 'res.partner'        # Main symbol
    ///
    /// class Partner(models.Model):
    ///     _inherit = 'res.partner'     # Extension symbol (excluded)
    /// ```
    pub fn get_main_symbols(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>) -> Vec<Rc<RefCell<Symbol>>> {
        let mut res: Vec<Rc<RefCell<Symbol>>> = vec![];
        for sym in self.symbols.iter() {
            if !sym.borrow().as_class_sym()._model.as_ref().unwrap().inherit.contains(&sym.borrow().as_class_sym()._model.as_ref().unwrap().name) {
                if from_module.is_none() || sym.as_ref().borrow().find_module().is_none() {
                    res.push(sym);
                } else {
                    let dir_name = sym.borrow().find_module().unwrap().borrow().as_module_package().dir_name.clone();
                    if ModuleSymbol::is_in_deps(session, from_module.as_ref().unwrap(), &dir_name) {
                        res.push(sym);
                    }
                }
            }
        }
        res
    }

    pub fn model_in_deps(&self, session: &mut SessionInfo, from_module: &Rc<RefCell<Symbol>>) -> bool {
        for sym in self.symbols.iter() {
            if !sym.borrow().as_class_sym()._model.as_ref().unwrap().inherit.contains(&sym.borrow().as_class_sym()._model.as_ref().unwrap().name) {
                let dir_name = sym.borrow().find_module().unwrap().borrow().as_module_package().dir_name.clone();
                if ModuleSymbol::is_in_deps(session, from_module, &dir_name) {
                    return true;
                }
            }
        }
        false
    }

    pub fn get_full_model_symbols(model_rc: Rc<RefCell<Model>>, session: &mut SessionInfo, from_module: Rc<RefCell<Symbol>>) -> PtrWeakHashSet<Weak<RefCell<Symbol>>> {
        let mut symbol_set  = PtrWeakHashSet::new();
        let mut already_in = HashSet::new();
        let mut queue = VecDeque::from([model_rc]);
        while let Some(current_model_rc) = queue.pop_front(){
            let current_model = current_model_rc.borrow();
            let symbols = current_model.get_symbols(session, Some(from_module.clone()));
            for symbol in symbols.iter() {
                let sym_ref = symbol.borrow();
                let Some(model_data) = &sym_ref.as_class_sym()._model else {continue};
                for inherit in model_data.inherit.iter() {
                    if let Some(model) = session.sync_odoo.models.get(inherit).cloned() {
                        if !already_in.contains(&model.borrow().name) {
                            already_in.insert(model.borrow().name.clone());
                            queue.push_back(model.clone());
                        }
                    }
                }
            }
            symbol_set.extend(symbols.into_iter());
        }
        symbol_set
    }

    pub fn get_inherits_models(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>) -> Vec<Rc<RefCell<Model>>> {
        let mut res = vec![];
        let mut already_in = HashSet::new();
        if let Some(from_module) = from_module {
            let symbols = self.get_symbols(session, Some(from_module));
            for symbol in symbols {
                if let Some(model_data) = &symbol.borrow().as_class_sym()._model {
                    for (model_name, _field) in model_data.inherits.iter() {
                        if let Some(model) = session.sync_odoo.models.get(model_name).cloned() {
                            if !already_in.contains(&model.borrow().name) {
                                res.push(model.clone());
                                already_in.insert(model.borrow().name.clone());
                            }
                        }
                    }
                }
            }
        }
        res
    }

    pub fn has_symbols(&mut self) -> bool {
        self.symbols.remove_expired();
        !self.symbols.is_empty()
    }

    /* Return all symbols that build this model.
        It returns the symbol and an optional string that represents the module name that should be added to dependencies to be used.
        if with_inheritance is true, it will also return symbols from inherited models (NOT Base classes).
    */
    pub fn all_symbols(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>, with_inheritance: bool) -> Vec<(Rc<RefCell<Symbol>>, Option<OYarn>)> {
        self.all_symbols_helper(session, from_module, with_inheritance, &mut HashSet::new())
    }

    fn all_symbols_helper(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>, with_inheritance: bool, seen_inherited_models: &mut HashSet<OYarn>) -> Vec<(Rc<RefCell<Symbol>>, Option<OYarn>)> {
        let mut symbols = Vec::new();
        for s in self.symbols.iter() {
            if let Some(from_module) = from_module.as_ref() {
                let module = s.borrow().find_module();
                if let Some(module) = module {
                    if ModuleSymbol::is_in_deps(session, &from_module, &module.borrow().as_module_package().dir_name) {
                        symbols.push((s.clone(), None));
                    } else {
                        symbols.push((s.clone(), Some(module.borrow().as_module_package().dir_name.clone())));
                    }
                } else {
                    session.log_message(MessageType::WARNING, "A model should be declared in a module.".to_string());
                }
            } else {
                symbols.push((s.clone(), None));
            }
            if !with_inheritance {
                continue;
            }
            let inherited_models = s.borrow().as_class_sym()._model.as_ref().unwrap().inherit.clone();
            for inherited_model in inherited_models.iter() {
                if !seen_inherited_models.contains(inherited_model) {
                    seen_inherited_models.insert(inherited_model.clone());
                    if let Some(model) = session.sync_odoo.models.get(inherited_model).cloned() {
                        symbols.extend(model.borrow().all_symbols_helper(session, from_module.clone(), true, seen_inherited_models));
                    }
                }
            }
        }
        symbols
    }

    pub fn all_symbols_inherits(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>) -> (Vec<(Rc<RefCell<Symbol>>, Option<OYarn>)>, Vec<(Rc<RefCell<Symbol>>, Option<OYarn>)>) {
        let mut visited_models = HashSet::new();
        self.all_inherits_helper(session, from_module, &mut visited_models)
    }

    fn all_inherits_helper(&self, session: &mut SessionInfo, from_module: Option<Rc<RefCell<Symbol>>>, visited_models: &mut HashSet<String>) -> (Vec<(Rc<RefCell<Symbol>>, Option<OYarn>)>, Vec<(Rc<RefCell<Symbol>>, Option<OYarn>)>) {
        if visited_models.contains(&self.name.to_string()) {
            return (Vec::new(), Vec::new());
        }
        visited_models.insert(self.name.to_string());
        let mut symbols = Vec::new();
        let mut inherits_symbols = Vec::new();
        for s in self.symbols.iter() {
            if let Some(from_module) = from_module.as_ref() {
                let module = s.borrow().find_module();
                if let Some(module) = module {
                    if ModuleSymbol::is_in_deps(session, &from_module, &module.borrow().as_module_package().dir_name) {
                        symbols.push((s.clone(), None));
                    } else {
                        symbols.push((s.clone(), Some(module.borrow().as_module_package().dir_name.clone())));
                    }
                } else {
                    session.log_message(MessageType::WARNING, "A model should be declared in a module.".to_string());
                }
            } else {
                symbols.push((s.clone(), None));
            }
            // First get results from normal inherit
            // To make sure we visit all of inherit before inherits, since it is DFS
            // Only inherits in the tree that are not already visited will be processed in the next iteration
            let inherited_models = s.borrow().as_class_sym()._model.as_ref().unwrap().inherit.clone();
            for inherited_model in inherited_models.iter() {
                if let Some(model) = session.sync_odoo.models.get(inherited_model).cloned() {
                    let (main_result, inherits_result) = model.borrow().all_inherits_helper(session, from_module.clone(), visited_models);
                    symbols.extend(main_result);
                    inherits_symbols.extend(inherits_result);
                }
            }
            for (inherits_model, _) in s.borrow().as_class_sym()._model.as_ref().unwrap().inherits.clone() {
                if let Some(model) = session.sync_odoo.models.get(&inherits_model).cloned() {
                    let (main_result, inherits_result) = model.borrow().all_inherits_helper(session, from_module.clone(), visited_models);
                    // Everything that is in inherits should be added to inherits_symbols, regardless of whether
                    // it was in inherit or inherits. Since we need that distinction to later only get fields
                    inherits_symbols.extend(main_result);
                    inherits_symbols.extend(inherits_result);
                }
            }
        }
        (symbols, inherits_symbols)
    }

    pub fn add_dependent(&mut self, symbol: &Rc<RefCell<Symbol>>) {
        self.dependents.insert(symbol.clone());
    }

    pub fn add_dependents_to_validation(&self, session: &mut SessionInfo, module_change: Option<Rc<RefCell<Symbol>>>) {
        for dep in self.dependents.iter() {
            dep.borrow_mut().invalidate_sub_functions(session);
            let module = dep.borrow().find_module();
            if module_change.is_none() || module.is_none() || ModuleSymbol::is_in_deps(session, &module.as_ref().unwrap(), &module_change.as_ref().unwrap().borrow().as_module_package().dir_name) {
                let typ = dep.borrow().typ().clone();
                match typ {
                    SymType::FUNCTION => {
                        dep.borrow_mut().set_build_status(BuildSteps::ARCH_EVAL, BuildStatus::PENDING);
                        session.sync_odoo.add_to_validations(dep.clone());
                    },
                    _ => {
                        session.sync_odoo.add_to_validations(dep.clone());
                    }
                }
            }
        }
    }
}
