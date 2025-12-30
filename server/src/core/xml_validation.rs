//! XML Validation Phase Implementation
//!
//! This module implements the second phase (VALIDATION) of the XML two-phase build pipeline.
//! After the ARCH phase has parsed XML structure and extracted XML IDs, this phase validates
//! that referenced models and fields actually exist in the Python symbol table.
//!
//! # Build Pipeline Position
//!
//! ```text
//! XML ARCH (xml_arch_builder.rs)
//!     ↓
//! Python ARCH_EVAL (must complete first)
//!     ↓
//! XML VALIDATION (this module)  ← Validates against Python symbols
//! ```
//!
//! # Validation Checks
//!
//! | Element | Validation | Diagnostic |
//! |---------|------------|------------|
//! | `<record model="...">` | Model exists | OLS05056 |
//! | `<record model="...">` | Model in dependencies | OLS05055 |
//! | `<field name="...">` | Field exists on model | OLS05057 |
//! | `<field ref="...">` | XML ID format valid | OLS05039, OLS05003, OLS05051 |
//! | `<field>model</field>` | Model in text exists | OLS05055, OLS05056 |
//!
//! # Dependency Tracking
//!
//! This phase also records dependencies for incremental rebuilds:
//! - Symbol dependencies: XML file → Python file defining the model
//! - Model dependencies: XML file → Model struct
//!
//! When a Python model is modified, dependent XML files are re-validated.

use std::{cell::RefCell, cmp::Ordering, collections::{HashMap, HashSet}, rc::Rc};

use lsp_types::{Diagnostic, Position, Range};
use tracing::{info, trace};

use crate::{Sy, constants::{BuildSteps, DEBUG_STEPS, OYarn}, core::{diagnostics::{DiagnosticCode, create_diagnostic}, entry_point::{EntryPoint, EntryPointType}, evaluation::ContextValue, file_mgr::{FileInfo, FileMgr}, model::Model, odoo::SyncOdoo, symbols::{module_symbol::ModuleSymbol, symbol::Symbol}, xml_data::{OdooData, OdooDataRecord, XmlDataDelete, XmlDataMenuItem, XmlDataTemplate}}, oyarn, threads::SessionInfo, utils::compare_semver};



/// Validator for the VALIDATION phase of XML processing.
///
/// The `XmlValidator` checks that models and fields referenced in XML data files
/// actually exist in the Python symbol table. This validation runs after Python
/// symbols have been built (ARCH_EVAL phase).
///
/// # Fields
///
/// * `xml_symbol` - The `XmlFileSymbol` being validated
/// * `is_in_main_ep` - Whether the file is in MAIN/ADDON entry point
///
/// # Usage
///
/// ```ignore
/// let mut validator = XmlValidator::new(&entry, symbol.clone());
/// validator.validate(session);
/// // Diagnostics are published to the client
/// ```
///
/// # Dependency Tracking
///
/// During validation, the validator records:
/// - File dependencies (for rebuilding when models change)
/// - Model dependencies (for tracking model changes)
/// - Missing model names (for deferred validation when models are added)
pub struct XmlValidator {
    pub xml_symbol: Rc<RefCell<Symbol>>,
    pub is_in_main_ep: bool,
}

impl XmlValidator {

    /// Creates a new XML validator for the given symbol.
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry point containing this file
    /// * `symbol` - The `XmlFileSymbol` to validate
    pub fn new(entry: &Rc<RefCell<EntryPoint>>, symbol: Rc<RefCell<Symbol>>) -> Self {
        let is_in_main_ep = entry.borrow().typ == EntryPointType::MAIN || entry.borrow().typ == EntryPointType::ADDON;
        Self {
            xml_symbol: symbol,
            is_in_main_ep,
        }
    }

    /// Retrieves the file info for the XML file being validated.
    fn get_file_info(&mut self, odoo: &mut SyncOdoo) -> Rc<RefCell<FileInfo>> {
        let file_symbol = self.xml_symbol.borrow();
        let path = file_symbol.paths()[0].clone();
        let file_info_rc = odoo.get_file_mgr().borrow().get_file_info(&path).expect("File not found in cache").clone();
        file_info_rc
    }

    /// Main entry point for the VALIDATION phase.
    ///
    /// Iterates through all XML IDs in the file and validates each one:
    /// 1. For each `OdooData` in `xml_symbol.xml_ids`
    /// 2. Dispatch to type-specific validator (`validate_record`, etc.)
    /// 3. Track symbol and model dependencies
    /// 4. Track missing models for deferred validation
    /// 5. Publish diagnostics to the client
    ///
    /// # Side Effects
    ///
    /// - Adds dependencies to `xml_symbol` for incremental rebuilds
    /// - Updates `not_found_symbols_for_models` for deferred validation
    /// - Publishes diagnostics via `file_info.publish_diagnostics()`
    pub fn validate(&mut self, session: &mut SessionInfo) {
        if DEBUG_STEPS {
            trace!("Validating XML File {}", self.xml_symbol.borrow().name());
        }
        let module = self.xml_symbol.borrow().find_module().unwrap();
        let mut dependencies = vec![];
        let mut model_dependencies = vec![];
        let mut missing_model_dependencies = HashSet::new();
        let mut diagnostics = vec![];
        for xml_ids in self.xml_symbol.borrow().as_xml_file_sym().xml_ids.values() {
            for xml_id in xml_ids.iter() {
                self.validate_xml_id(session, &module, xml_id, &mut diagnostics, &mut dependencies, &mut model_dependencies, &mut missing_model_dependencies);
            }
        }
        for dep in dependencies.iter_mut() {
            self.xml_symbol.borrow_mut().add_dependency(&mut dep.borrow_mut(), BuildSteps::VALIDATION, BuildSteps::ARCH_EVAL);
        }
        for model in model_dependencies.iter() {
            self.xml_symbol.borrow_mut().add_model_dependencies(&model);
        }
        if !missing_model_dependencies.is_empty() {
            session.sync_odoo.get_main_entry().borrow_mut().not_found_symbols_for_models.insert(self.xml_symbol.clone());
        }
        self.xml_symbol.borrow_mut().as_xml_file_sym_mut().not_found_models.extend(missing_model_dependencies.into_iter().map(|m| (m, BuildSteps::VALIDATION)));
        let file_info = self.get_file_info(&mut session.sync_odoo);
        file_info.borrow_mut().replace_diagnostics(BuildSteps::VALIDATION, diagnostics);
        file_info.borrow_mut().publish_diagnostics(session);
    }

    /// Dispatches validation to the appropriate handler based on `OdooData` type.
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `module` - The module containing this XML file
    /// * `data` - The XML data to validate
    /// * `diagnostics` - Collector for validation errors
    /// * `dependencies` - Collector for file symbol dependencies
    /// * `model_dependencies` - Collector for model dependencies
    /// * `missing_model_dependencies` - Collector for models not found (for deferred validation)
    pub fn validate_xml_id(&self, session: &mut SessionInfo, module: &Rc<RefCell<Symbol>>, data: &OdooData, diagnostics: &mut Vec<Diagnostic>, dependencies: &mut Vec<Rc<RefCell<Symbol>>>, model_dependencies: &mut Vec<Rc<RefCell<Model>>>, missing_model_dependencies: &mut HashSet<OYarn>) {
        let Some(_) = data.get_xml_file_symbol() else {
            return;
        };
        match data {
            OdooData::RECORD(xml_data_record) => self.validate_record(session, module, xml_data_record, diagnostics, dependencies, model_dependencies, missing_model_dependencies),
            OdooData::MENUITEM(xml_data_menu_item) => self.validate_menu_item(session, module, xml_data_menu_item, diagnostics, dependencies, model_dependencies, missing_model_dependencies),
            OdooData::TEMPLATE(xml_data_template) => self.validate_template(session, module, xml_data_template, diagnostics, dependencies, model_dependencies, missing_model_dependencies),
            OdooData::DELETE(xml_data_delete) => self.validate_delete(session, module, xml_data_delete, diagnostics, dependencies, model_dependencies, missing_model_dependencies),
        }
    }

    /// Validates a `<record>` element's model and fields.
    ///
    /// This is the main validation logic for records. It checks:
    /// 1. Model exists in `sync_odoo.models`
    /// 2. Model is accessible from the current module (via dependencies)
    /// 3. Each field exists on the model (including inherited fields)
    /// 4. Field `ref` attributes have valid XML ID format
    /// 5. Special fields (`model`, `res_model`) reference existing models
    ///
    /// # Diagnostics
    ///
    /// - **OLS05056**: Model doesn't exist anywhere
    /// - **OLS05055**: Model exists but not in module dependencies
    /// - **OLS05057**: Field doesn't exist on the model
    /// - **OLS05039**: Empty XML ID in `ref` attribute
    /// - **OLS05003**: Unknown module in XML ID prefix
    /// - **OLS05051**: Invalid XML ID format (too many dots)
    fn validate_record(&self, session: &mut SessionInfo, module: &Rc<RefCell<Symbol>>, xml_data_record: &OdooDataRecord, diagnostics: &mut Vec<Diagnostic>, dependencies: &mut Vec<Rc<RefCell<Symbol>>>, model_dependencies: &mut Vec<Rc<RefCell<Model>>>, missing_model_dependencies: &mut HashSet<OYarn>) {
        let maybe_model = session.sync_odoo.models.get(&xml_data_record.model.0).cloned();
        let model_exists = maybe_model.as_ref().map(|m| m.borrow_mut().has_symbols()).unwrap_or(false);
        if !model_exists {
            missing_model_dependencies.insert(xml_data_record.model.0.clone());
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05056, &[&xml_data_record.model.0]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(xml_data_record.model.1.start.try_into().unwrap(), 0), end: Position::new(xml_data_record.model.1.end.try_into().unwrap(), 0) },
                    ..diagnostic.clone()
                });
            }
            info!("Model '{}' does not exist", xml_data_record.model.0);
            return;
        }
        let Some(model) = maybe_model else {unreachable!();};
        let has_symbols_in_deps = !model.borrow().get_main_symbols(session, Some(module.clone())).is_empty();
        if !has_symbols_in_deps {
            missing_model_dependencies.insert(xml_data_record.model.0.clone());
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05055, &[&xml_data_record.model.0, module.borrow().name()]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(xml_data_record.model.1.start.try_into().unwrap(), 0), end: Position::new(xml_data_record.model.1.end.try_into().unwrap(), 0) },
                    ..diagnostic.clone()
                });
            }
            info!("Model '{}' has no symbols in module '{}'", xml_data_record.model.0, module.borrow().name());
            return;
        }
        model_dependencies.push(model.clone());
        let main_symbols = model.borrow().get_main_symbols(session, Some(module.clone()));
        for main_sym in main_symbols.iter() {
            dependencies.push(main_sym.borrow().get_file().unwrap().upgrade().unwrap());
        }
        let Some(main_symbol) = main_symbols.get(0) else { return; };
        let all_fields = Symbol::all_fields(main_symbol, session, Some(module.clone()));
        self.validate_fields(session, xml_data_record, &all_fields, diagnostics, missing_model_dependencies);
    }

    /// Validates field references within a record.
    ///
    /// For each field in the record, this method:
    /// 1. Checks the field exists on the model (including inherited fields)
    /// 2. Validates `ref` attribute XML ID format
    /// 3. For special fields (`model`, `res_model`), validates the referenced model exists
    /// 4. Handles Odoo 18.2+ translation syntax (`field_name@lang`)
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `xml_data_record` - The record containing fields to validate
    /// * `all_fields` - Map of field names to their symbols (from `Symbol::all_fields`)
    /// * `diagnostics` - Collector for validation errors
    /// * `missing_model_dependencies` - Collector for models not found
    ///
    /// # Special Field Handling
    ///
    /// When the record's model is `ir.ui.view` or `ir.actions.act_window`, the `model`
    /// or `res_model` field text content is validated as a model name.
    fn validate_fields(&self, session: &mut SessionInfo, xml_data_record: &OdooDataRecord, all_fields: &HashMap<OYarn, Vec<(Rc<RefCell<Symbol>>, Option<OYarn>)>>, diagnostics: &mut Vec<Diagnostic>, missing_model_dependencies: &mut HashSet<OYarn>) {
        //Compute mandatory fields
        let mut mandatory_fields: Vec<String> = vec![];
        for (field_name, field_sym) in all_fields.iter() {
            for (fs, deps) in field_sym.iter() {
                if deps.is_none() {
                    let has_required = fs.borrow().evaluations().unwrap_or(&vec![]).iter()
                    .any(|eval|
                        eval.symbol.get_symbol_as_weak(session, &mut None, diagnostics, None)
                        .context.get("required").unwrap_or(&ContextValue::BOOLEAN(false)).as_bool()
                    );
                    let has_default = fs.borrow().evaluations().unwrap_or(&vec![]).iter()
                    .any(|eval|
                        eval.symbol.get_symbol_as_weak(session, &mut None, diagnostics, None)
                        .context.contains_key("default")
                    );
                    if has_required && !has_default {
                        mandatory_fields.push(field_name.to_string());
                    }
                }
            }
        }
        //check each field in the record
        for field in &xml_data_record.fields {
            let mut field_name = Sy!(field.name.clone());
            let mut has_translation = false;
            if compare_semver(&session.sync_odoo.full_version, "18.2.0") >= Ordering::Equal {
                let translation = field.name.split("@").collect::<Vec<&str>>();
                if translation.len() > 1 {
                    field_name = oyarn!("{}", translation[0]);
                    has_translation = true;
                    //TODO check that the language exists
                }
            }
            // Validate field ref_key
            if let Some((ref_key_val, ref_key_range)) = field.ref_key.as_ref(){
                let xml_id_split: Vec<_> = ref_key_val.split('.').collect();
                match xml_id_split.len() {
                    0 => {}, // Should not happen
                    1 => { // Local reference, check that it is not empty
                        let ref_xml_id = xml_id_split[0];
                        if ref_xml_id.is_empty() {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05039, &[]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(ref_key_range.start.try_into().unwrap(), 0), end: Position::new(ref_key_range.end.try_into().unwrap(), 0) },
                                    ..diagnostic
                                });
                            }
                        }

                    },
                    2 => {
                        let module_name = xml_id_split[0];
                        if session.sync_odoo.modules.get(module_name).is_none() {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05003, &[]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(ref_key_range.start.try_into().unwrap(), 0), end: Position::new(ref_key_range.end.try_into().unwrap(), 0) },
                                    ..diagnostic
                                });
                            }
                        }},
                    _ => { // >= 2
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05051, &[ref_key_val]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(ref_key_range.start.try_into().unwrap(), 0), end: Position::new(ref_key_range.end.try_into().unwrap(), 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                }
            }
            //Check that the field belong to the model
            if all_fields.contains_key(&field_name) {
                mandatory_fields.retain(|f| f != &field_name.to_string());
                //Check specific attributes
                let (Some(field_text), Some(field_text_range)) = (field.text.as_ref(), field.text_range.as_ref()) else {
                    continue;
                };
                match (xml_data_record.model.0.as_str(), field_name.as_str()) {
                    ("ir.ui.view", "model") | ("ir.actions.act_window", "res_model") => {
                        let model = session.sync_odoo.models.get(&Sy!(field_text.clone())).cloned();
                        let model_exists = model.as_ref().map(|m| m.borrow_mut().has_symbols()).unwrap_or(false);
                        if !model_exists {
                            missing_model_dependencies.insert(Sy!(field_text.clone()));
                        }
                        let mut main_sym = vec![];
                        let from_module = self.xml_symbol.borrow().find_module();
                        if let Some(model) = model {
                            main_sym = model.borrow().get_main_symbols(session, from_module.clone());
                        }
                        if !model_exists {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05056, &[field_text, &xml_data_record.model.0]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(field_text_range.start.try_into().unwrap(), 0), end: Position::new(field_text_range.end.try_into().unwrap(), 0) },
                                    ..diagnostic.clone()
                                });
                            }
                        }
                        if  let Some(module) = from_module &&model_exists && main_sym.is_empty() {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05055, &[field_text, module.borrow().name()]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(field_text_range.start.try_into().unwrap(), 0), end: Position::new(field_text_range.end.try_into().unwrap(), 0) },
                                    ..diagnostic.clone()
                                });
                            }
                        }
                    },
                    _ => {}
                }
                //TODO check type
            } else {
                if has_translation {
                    continue;
                }
                if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05057, &[&field.name, &xml_data_record.model.0]) {
                    diagnostics.push(Diagnostic {
                        range: Range { start: Position::new(field.range.start.try_into().unwrap(), 0), end: Position::new(field.range.end.try_into().unwrap(), 0) },
                        ..diagnostic.clone()
                    });
                }
            }
        }
        //Diagnostic if some mandatory fields are not detected
        // if !mandatory_fields.is_empty() {
        // We have to check  that remaining fields are not declared in an inherited record or is automatically field (delegate=True)
        //     diagnostics.push(Diagnostic::new(
        //         Range::new(Position::new(xml_data_record.range.start.try_into().unwrap(), 0), Position::new(xml_data_record.range.end.try_into().unwrap(), 0)),
        //         Some(lsp_types::DiagnosticSeverity::ERROR),
        //         Some(lsp_types::NumberOrString::String(S!("OLS30452"))),
        //         Some(EXTENSION_NAME.to_string()),
        //         format!("Some mandatory fields are not declared in the record: {:?}", mandatory_fields),
        //         None,
        //         None
        //     ));
        // }
    }

    /// Validates a `<menuitem>` element.
    ///
    /// Currently a placeholder - menuitem validation is handled during ARCH phase
    /// in `xml_arch_builder_rng_validation.rs`.
    fn validate_menu_item(&self, _session: &mut SessionInfo, _module: &Rc<RefCell<Symbol>>, _xml_data_menu_item: &XmlDataMenuItem, _diagnostics: &mut Vec<Diagnostic>, _dependencies: &mut Vec<Rc<RefCell<Symbol>>>, _model_dependencies: &mut Vec<Rc<RefCell<Model>>>, _missing_model_dependencies: &mut HashSet<OYarn>) {

    }

    /// Validates a `<template>` element.
    ///
    /// Currently a placeholder - template validation could be extended to check
    /// QWeb syntax, inherit_id references, etc.
    fn validate_template(&self, _session: &mut SessionInfo, _module: &Rc<RefCell<Symbol>>, _xml_data_template: &XmlDataTemplate, _diagnostics: &mut Vec<Diagnostic>, _dependencies: &mut Vec<Rc<RefCell<Symbol>>>, _model_dependencies: &mut Vec<Rc<RefCell<Model>>>, _missing_model_dependencies: &mut HashSet<OYarn>) {

    }

    /// Validates a `<delete>` element.
    ///
    /// Currently a placeholder - could be extended to validate the target model
    /// and check the referenced XML ID exists.
    fn validate_delete(&self, _session: &mut SessionInfo, _module: &Rc<RefCell<Symbol>>, _xml_data_delete: &XmlDataDelete, _diagnostics: &mut Vec<Diagnostic>, _dependencies: &mut Vec<Rc<RefCell<Symbol>>>, _model_dependencies: &mut Vec<Rc<RefCell<Model>>>, _missing_model_dependencies: &mut HashSet<OYarn>) {

    }
}