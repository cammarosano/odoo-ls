use weak_table::{PtrWeakHashSet, PtrWeakKeyHashMap};

use crate::{constants::{BuildStatus, BuildSteps, OYarn}, core::{file_mgr::NoqaInfo, model::Model, xml_data::OdooData}, oyarn};
use std::{cell::RefCell, collections::HashMap, rc::{Rc, Weak}};

use super::{symbol::Symbol, symbol_mgr::{SectionRange, SymbolMgr}};

/// Represents a Python source file in the symbol tree.
///
/// # Build Status Tracking
///
/// Each file tracks its build status independently for each phase:
/// - `arch_status`: ARCH phase (symbol tree building)
/// - `arch_eval_status`: ARCH_EVAL phase (type evaluation)
/// - `validation_status`: VALIDATION phase (diagnostics)
///
/// # Dependency Tracking
///
/// Dependencies are stored in a 2D structure: `dependencies[step][level]`
/// - `step`: The build step that needs the dependency (ARCH=0, ARCH_EVAL=1, VALIDATION=2)
/// - `level`: The build level required from the dependency (ARCH=0, ARCH_EVAL=1)
///
/// Example: If this file's ARCH_EVAL needs another file's ARCH to be done:
/// `dependencies[1][0]` contains a weak reference to that file.
///
/// # Symbol Storage
///
/// Implements the `SymbolMgr` trait for managing symbols (classes, functions, variables)
/// defined within this file using section-based visibility for control flow.
///
/// See [Python Core Onboarding Guide](../../docs/python-core-onboarding.md#filesymbol) for details.
#[derive(Debug)]
pub struct FileSymbol {
    pub name: OYarn,
    pub path: String,
    pub is_external: bool,
    pub weak_self: Option<Weak<RefCell<Symbol>>>,
    pub parent: Option<Weak<RefCell<Symbol>>>,
    
    /// Build status for ARCH phase (symbol tree building)
    pub arch_status: BuildStatus,
    /// Build status for ARCH_EVAL phase (type evaluation)
    pub arch_eval_status: BuildStatus,
    /// Build status for VALIDATION phase (diagnostics generation)
    pub validation_status: BuildStatus,
    
    /// Import paths that couldn't be resolved (will retry after dependencies built)
    pub not_found_paths: Vec<(BuildSteps, Vec<OYarn>)>,
    /// Odoo models referenced but not found yet
    pub not_found_models: HashMap<OYarn, BuildSteps>,
    /// Dynamic XML IDs created by this file (e.g., ir.model records)
    pub xml_ids: HashMap<OYarn, Vec<OdooData>>,
    in_workspace: bool,
    pub self_import: bool,
    /// Odoo models this file depends on (for invalidation)
    pub model_dependencies: PtrWeakHashSet<Weak<RefCell<Model>>>,
    
    /// Dependencies: `[step][level] -> Set<Symbol>`. This file needs those symbols.
    pub dependencies: Vec<Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>>,
    /// Dependents: `[level][step] -> Set<Symbol>`. Those symbols need this file.
    pub dependents: Vec<Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>>,
    
    /// Hash of processed file content (for change detection)
    pub processed_text_hash: u64,
    pub noqas: NoqaInfo,

    //Trait SymbolMgr
    pub sections: Vec<SectionRange>,
    pub symbols: HashMap<OYarn, HashMap<u32, Vec<Rc<RefCell<Symbol>>>>>, // doc-todo: why inner hashmap with u32? Is it section-based storage?
    //--- dynamics variables
    pub ext_symbols: HashMap<OYarn, PtrWeakHashSet<Weak<RefCell<Symbol>>>>,
    pub decl_ext_symbols: PtrWeakKeyHashMap<Weak<RefCell<Symbol>>, HashMap<OYarn, HashMap<u32, Vec<Rc<RefCell<Symbol>>>>>>
}

impl FileSymbol {

    pub fn new(name: String, path: String, is_external: bool) -> Self {
        let mut res = Self {
            name: oyarn!("{}", name),
            path,
            is_external,
            weak_self: None,
            parent: None,
            arch_status: BuildStatus::PENDING,
            arch_eval_status: BuildStatus::PENDING,
            validation_status: BuildStatus::PENDING,
            not_found_paths: vec![],
            xml_ids: HashMap::new(),
            in_workspace: false,
            self_import: false,
            sections: vec![],
            symbols: HashMap::new(),
            ext_symbols: HashMap::new(),
            decl_ext_symbols: PtrWeakKeyHashMap::new(),
            model_dependencies: PtrWeakHashSet::new(),
            dependencies: vec![],
            dependents: vec![],
            processed_text_hash: 0,
            not_found_models: HashMap::new(),
            noqas: NoqaInfo::None,
        };
        res._init_symbol_mgr();
        res
    }

    pub fn add_symbol(&mut self, content: &Rc<RefCell<Symbol>>, section: u32) {
        let sections = self.symbols.entry(content.borrow().name().clone()).or_insert_with(|| HashMap::new());
        let section_vec = sections.entry(section).or_insert_with(|| vec![]);
        section_vec.push(content.clone());
    }

    pub fn get_dependencies(&self, step: usize, level: usize) -> Option<&PtrWeakHashSet<Weak<RefCell<Symbol>>>>
    {
        self.dependencies.get(step)?.get(level)?.as_ref()
    }

    pub fn get_all_dependencies(&self, step: usize) -> Option<&Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>>
    {
        self.dependencies.get(step)
    }

    pub fn get_dependents(&self, level: usize, step: usize) -> Option<&PtrWeakHashSet<Weak<RefCell<Symbol>>>>
    {
        self.dependents.get(level)?.get(step)?.as_ref()
    }

    pub fn get_all_dependents(&self, level: usize) -> Option<&Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>>
    {
        self.dependents.get(level)
    }

    pub fn dependencies(&self) -> &Vec<Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>> {
        &self.dependencies
    }

    pub fn dependencies_mut(&mut self) -> &mut Vec<Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>> {
        &mut self.dependencies
    }

    pub fn set_in_workspace(&mut self, in_workspace: bool) {
        self.in_workspace = in_workspace;
        if in_workspace {
            self.dependencies= vec![
                vec![ //ARCH
                    None //ARCH
                ],
                vec![ //ARCH_EVAL
                    None, //ARCH,
                    None, //ARCH_EVAL
                ],
                vec![
                    None, // ARCH
                    None, //ARCH_EVAL
                    None, //VALIDATIOn
                ]
            ];
            self.dependents = vec![
                vec![ //ARCH
                    None, //ARCH
                    None, //ARCH_EVAL
                    None, //VALIDATION
                ],
                vec![ //ARCH_EVAL
                    None, //ARCH_EVAL
                    None //VALIDATION
                ],
                vec![ //VALIDATION
                    None //VALIDATION
                ]
            ];
        }
    }

    pub fn dependents(&self) -> &Vec<Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>> {
        &self.dependents
    }

    pub fn dependents_mut(&mut self) -> &mut Vec<Vec<Option<PtrWeakHashSet<Weak<RefCell<Symbol>>>>>> {
        &mut self.dependents
    }

    pub fn is_in_workspace(&self) -> bool {
        self.in_workspace
    }

    pub fn get_ext_symbol(&self, name: &OYarn) -> Vec<Rc<RefCell<Symbol>>> {
        let mut result = vec![];
        if let Some(owners) = self.ext_symbols.get(name) {
            for owner in owners.iter() {
                let owner = owner.borrow();
                result.extend(owner.get_decl_ext_symbol(&self.weak_self.as_ref().unwrap().upgrade().unwrap(), name));
            }
        }
        result
    }

    pub fn get_decl_ext_symbol(&self, symbol: &Rc<RefCell<Symbol>>, name: &OYarn) -> Vec<Rc<RefCell<Symbol>>> {
        let mut result = vec![];
        if let Some(object_decl_symbols) = self.decl_ext_symbols.get(symbol) {
            if let Some(symbols) = object_decl_symbols.get(name) {
                for end_symbols in symbols.values() {
                    //TODO actually we don't take position into account, but can we really?
                    result.extend(end_symbols.iter().map(|s| s.clone()));
                }
            }
        }
        result
    }

}