//! XML Architecture Builder - ARCH Phase Implementation
//!
//! This module implements the first phase (ARCH) of the XML two-phase build pipeline.
//! It parses XML data files, validates their structure using RNG-style rules, and extracts
//! XML IDs for registration in the module's symbol table.
//!
//! # Build Pipeline
//!
//! XML files go through two phases (compared to Python's three phases):
//! 1. **ARCH** (this module): Parse XML, validate structure, extract XML IDs
//! 2. **VALIDATION** (`xml_validation.rs`): Check model/field existence
//!
//! # Entry Point
//!
//! The builder is created and invoked from `ModuleSymbol::load_data()` when processing
//! files listed in `__manifest__.py`'s `data` key:
//!
//! ```ignore
//! let xml_sym = symbol.add_new_xml_file(session, &file_name, &path);
//! let mut xml_builder = XmlArchBuilder::new(xml_sym);
//! xml_builder.load_arch(session, &mut file_info, &root);
//! ```
//!
//! # Related Modules
//!
//! - `xml_arch_builder_rng_validation.rs`: Element-specific validation rules
//! - `xml_validation.rs`: VALIDATION phase (model/field checks)
//! - `xml_data.rs`: Data structures for XML records

use std::{cell::RefCell, rc::Rc};

use lsp_types::Diagnostic;
use roxmltree::{Attribute, Node};
use tracing::warn;
use weak_table::PtrWeakHashSet;

use crate::core::{diagnostics::{create_diagnostic, DiagnosticCode}, odoo::SyncOdoo};
use crate::{constants::{BuildStatus, BuildSteps, OYarn}, core::{entry_point::EntryPointType, xml_data::OdooData}, threads::SessionInfo, Sy};

use super::{file_mgr::FileInfo, symbols::{symbol::Symbol}};

/// Builder for processing XML data files during the ARCH phase.
///
/// The `XmlArchBuilder` parses Odoo XML data files (views, data, demo) and:
/// - Validates XML structure against Odoo's expected format
/// - Extracts XML IDs from `<record>`, `<menuitem>`, `<template>`, etc.
/// - Registers XML IDs in the module's symbol table for cross-file lookups
/// - Generates diagnostics for structural errors
///
/// # Fields
///
/// * `is_in_main_ep` - Whether this file is in a MAIN or ADDON entry point.
///   XML IDs are only registered for files in these entry points (not BUILTIN/PUBLIC).
/// * `xml_symbol` - The `XmlFileSymbol` being built, which stores extracted XML IDs.
///
/// # Example
///
/// ```ignore
/// let xml_sym = module.add_new_xml_file(session, "views.xml", &path);
/// let mut builder = XmlArchBuilder::new(xml_sym);
/// builder.load_arch(session, &mut file_info, &root_node);
/// // After this, xml_sym.xml_ids contains all extracted records
/// ```
pub struct XmlArchBuilder {
    pub is_in_main_ep: bool,
    pub xml_symbol: Rc<RefCell<Symbol>>,
}

impl XmlArchBuilder {

    /// Creates a new XML architecture builder for the given XML file symbol.
    ///
    /// # Arguments
    ///
    /// * `xml_symbol` - The `XmlFileSymbol` that will store extracted XML IDs
    pub fn new(xml_symbol: Rc<RefCell<Symbol>>) -> Self {
        Self {
            is_in_main_ep: false,
            xml_symbol
        }
    }

    /// Main entry point for the ARCH phase of XML processing.
    ///
    /// This method orchestrates the XML parsing and validation:
    /// 1. Sets build status to IN_PROGRESS
    /// 2. Determines if file is in MAIN/ADDON entry point (for XML ID registration)
    /// 3. Calls `load_odoo_openerp_data()` to validate and extract data
    /// 4. Sets build status to DONE
    /// 5. Stores diagnostics in the file info
    /// 6. Queues the symbol for VALIDATION phase
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `file_info` - File information including the parsed XML document
    /// * `node` - Root node of the XML document (should be `<odoo>`, `<openerp>`, or `<data>`)
    ///
    /// # Side Effects
    ///
    /// - Updates `xml_symbol.build_status` to DONE
    /// - Populates `xml_symbol.xml_ids` with extracted records
    /// - Adds diagnostics to `file_info`
    /// - Adds symbol to `session.sync_odoo.validations` queue
    pub fn load_arch(&mut self, session: &mut SessionInfo, file_info: &mut FileInfo, node: &Node) {
        let mut diagnostics = vec![];
        self.xml_symbol.borrow_mut().set_build_status(BuildSteps::ARCH, BuildStatus::IN_PROGRESS);
        let ep = self.xml_symbol.borrow().get_entry();
        if let Some(ep) = ep {
            self.is_in_main_ep = ep.borrow().typ == EntryPointType::MAIN || ep.borrow().typ == EntryPointType::ADDON;
        }
        self.load_odoo_openerp_data(session, node, &mut diagnostics);
        self.xml_symbol.borrow_mut().set_build_status(BuildSteps::ARCH, BuildStatus::DONE);
        file_info.replace_diagnostics(BuildSteps::ARCH, diagnostics);
        session.sync_odoo.add_to_validations(self.xml_symbol.clone());
    }

    /// Registers an XML ID in the module's symbol table.
    ///
    /// Called after parsing each XML element that declares an ID (`<record>`, `<menuitem>`,
    /// `<template>`, `<delete>`). This method:
    /// 1. Validates the XML ID format (at most one dot for module prefix)
    /// 2. Determines the target module (current or referenced)
    /// 3. Registers the ID in `ModuleSymbol.xml_id_locations`
    /// 4. Stores the `OdooData` in `XmlFileSymbol.xml_ids`
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `id` - The XML ID value (e.g., `"view_partner_form"` or `"base.view_partner_form"`)
    /// * `node` - The XML node (for error range reporting)
    /// * `xml_data` - The parsed data structure (`OdooDataRecord`, `XmlDataMenuItem`, etc.)
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # XML ID Format
    ///
    /// - `view_form` → registered in current module
    /// - `sale.view_form` → registered in `sale` module (if it exists)
    /// - `a.b.c` → error OLS05051 (too many dots)
    ///
    /// # Skipped Cases
    ///
    /// - Files not in MAIN/ADDON entry points (no registration)
    /// - Elements without an `id` attribute
    /// - Invalid XML ID format (diagnostic emitted but no registration)
    pub fn on_operation_creation(
        &self,
        session: &mut SessionInfo,
        id: Option<String>,
        node: &Node,
        mut xml_data: OdooData,
        diagnostics: &mut Vec<Diagnostic>
    ) {
        if !self.is_in_main_ep {
            return;
        }
        if let Some(id) = id {
            let module = self.xml_symbol.borrow().find_module();
            if module.is_none() {
                warn!("Module not found for id: {}", id);
                return;
            }
            let module = module.unwrap();
            let id_split = id.split(".").collect::<Vec<&str>>();
            if id_split.len() > 2 {
                if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05051, &[&id]) {
                    diagnostics.push(lsp_types::Diagnostic {
                        range: lsp_types::Range {
                            start: lsp_types::Position::new(node.range().start as u32, 0),
                            end: lsp_types::Position::new(node.range().end as u32, 0),
                        },
                        ..diagnostic.clone()
                    });
                }
                return;
            }
            let id = id_split.last().unwrap().to_string();
            let mut xml_module = module.clone();
            if id_split.len() == 2 {
                let module_name = Sy!(id_split.first().unwrap().to_string());
                if let Some(m) = session.sync_odoo.modules.get(&module_name) {
                    xml_module = m.upgrade().unwrap();
                }
            }
            xml_data.set_file_symbol(&self.xml_symbol);
            xml_module.borrow_mut().as_module_package_mut().xml_id_locations.entry(Sy!(id.clone())).or_insert(PtrWeakHashSet::new()).insert(self.xml_symbol.clone());
            self.xml_symbol.borrow_mut().as_xml_file_sym_mut().xml_ids.entry(Sy!(id.clone())).or_insert(vec![]).push(xml_data);
        }
    }

    /// Resolves group XML IDs and filters to only `res.groups` records.
    ///
    /// Used when validating `groups` attributes on `<menuitem>` and `<template>` elements.
    /// Looks up the XML ID and verifies it points to a `res.groups` record.
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `xml_id` - The group XML ID to resolve (may have `-` prefix for negation)
    /// * `attr` - The attribute node (for error range reporting)
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// Vector of `OdooData::RECORD` entries where the model is `res.groups`.
    /// Empty if the XML ID doesn't exist or doesn't point to a group.
    pub fn get_group_ids(&self, session: &mut SessionInfo, xml_id: &str, attr: &Attribute, diagnostics: &mut Vec<Diagnostic>) -> Vec<OdooData> {
        let xml_ids = SyncOdoo::get_xml_ids(session, &self.xml_symbol, xml_id, &attr.range(), diagnostics);
        let mut res = vec![];
        for data in xml_ids.iter() {
            match data {
                OdooData::RECORD(r) => {
                    if r.model.0 == "res.groups" {
                        res.push(data.clone());
                    }
                },
                _ => {}
            }
        }
        res
    }
}