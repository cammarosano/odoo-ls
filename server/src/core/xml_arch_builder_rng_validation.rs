//! XML Element Validation (RNG-Style)
//!
//! This module implements structural validation for Odoo XML data files. The validation
//! rules are inspired by RelaxNG schemas but implemented directly in Rust code for
//! better performance and integration with the build pipeline.
//!
//! # Validation Scope
//!
//! This module validates:
//! - Element names and their allowed contexts
//! - Attribute names and their valid values
//! - Required vs optional attributes
//! - Mutually exclusive attributes
//! - Valid child elements
//! - Text content restrictions
//!
//! # Supported Elements
//!
//! | Element | Function | Key Validations |
//! |---------|----------|------------------|
//! | `<odoo>`, `<openerp>`, `<data>` | `load_odoo_openerp_data()` | Root element, valid children |
//! | `<record>` | `load_record()` | `model` required, only `<field>` children |
//! | `<field>` | `load_field()` | `name` required, type/ref/eval/search exclusive |
//! | `<menuitem>` | `load_menuitem()` | `id` required, action/parent/groups checks |
//! | `<function>` | `load_function()` | `model` + `name` required, eval vs children |
//! | `<delete>` | `load_delete()` | `model` required, id XOR search |
//! | `<template>` | `load_template()` | Extracts id for XML ID registration |
//! | `<value>` | `load_value()` | Attribute exclusivity rules |
//! | `<report>` | `load_report()` | model/name/string required |
//!
//! # Error Codes
//!
//! All errors from this module are in the OLS05XXX range. See `diagnostic_codes_list.rs`
//! for the complete list.

use std::rc::Rc;

use lsp_types::{Diagnostic, Position, Range};
use roxmltree::Node;

use crate::{constants::OYarn, core::{diagnostics::{create_diagnostic, DiagnosticCode}, odoo::SyncOdoo, xml_data::{OdooData, XmlDataDelete, OdooDataField, XmlDataMenuItem, OdooDataRecord, XmlDataTemplate}}, oyarn, threads::SessionInfo, Sy};

use super::xml_arch_builder::XmlArchBuilder;

/// Element-specific validation methods for `XmlArchBuilder`.
///
/// These methods implement the "RNG-style" validation that checks XML structure
/// during the ARCH phase. Each `load_*` method:
/// 1. Checks if the node matches the expected element name
/// 2. Validates attributes (required, allowed, mutually exclusive)
/// 3. Validates children (allowed elements, text content)
/// 4. Extracts data into `OdooData*` structures
/// 5. Registers XML IDs via `on_operation_creation()`
impl XmlArchBuilder {

    /// Validates and processes the root `<odoo>`, `<openerp>`, or `<data>` element.
    ///
    /// This is the main entry point for XML structure validation. It:
    /// 1. Validates root element attributes (`noupdate`, `auto_sequence`, `uid`, `context`)
    /// 2. Recursively processes all child elements
    /// 3. Delegates to element-specific loaders (`load_record`, `load_menuitem`, etc.)
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The root XML node to validate
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `true` if the node is a valid root element (`odoo`, `openerp`, or `data`),
    /// `false` otherwise.
    ///
    /// # Valid Root Attributes
    ///
    /// - `noupdate` - Skip updates on module upgrade
    /// - `auto_sequence` - Auto-generate sequence numbers
    /// - `uid` - User context for record creation
    /// - `context` - Evaluation context
    ///
    /// # Diagnostics
    ///
    /// - **OLS05004**: Invalid attribute on root element
    /// - **OLS05005**: Invalid child element
    pub fn load_odoo_openerp_data(&mut self, session: &mut SessionInfo, node: &Node, diagnostics: &mut Vec<Diagnostic>) -> bool {
        match node.tag_name().name() {
            "odoo" | "openerp" | "data" => {
                for attr in node.attributes() {
                    match attr.name() {
                        "noupdate" | "auto_sequence" | "uid" | "context" => {},
                        _ => {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05004, &[attr.name(), node.tag_name().name()]) {
                                diagnostics.push(
                                    Diagnostic {
                                        range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                        ..diagnostic.clone()
                                    }
                                );
                            }
                        }
                    }
                }

                for child in node.children().filter(|n| n.is_element()) {
                    if !(self.load_odoo_openerp_data(session, &child, diagnostics)
                        || self.load_menuitem(session, &child, false, diagnostics)
                        || self.load_record(session, &child, diagnostics)
                        || self.load_template(session, &child, diagnostics)
                        || self.load_delete(session, &child, diagnostics)
                        || self.load_function(session, &child, diagnostics)
                        || child.is_text() || child.is_comment()) {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05005, &[child.tag_name().name(), node.tag_name().name()]) {
                            diagnostics.push(
                                Diagnostic {
                                    range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                                    ..diagnostic.clone()
                                }
                            );
                        }
                    }
                }
                return true;
            }
            _ => { return false;},
        }
    }

    /// Validates and processes a `<menuitem>` element.
    ///
    /// Menu items define entries in Odoo's navigation hierarchy. This method validates:
    /// - Required `id` attribute
    /// - Valid attributes (`name`, `sequence`, `parent`, `action`, `groups`, `web_icon`, `active`)
    /// - Integer format for `sequence`
    /// - Existence of referenced `parent` and `action` XML IDs
    /// - Existence of referenced `groups`
    /// - Nested menuitem constraints
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The `<menuitem>` node to validate
    /// * `is_submenu` - Whether this is a nested menuitem (different rules apply)
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `true` if the node is a `<menuitem>`, `false` otherwise.
    ///
    /// # Diagnostics
    ///
    /// - **OLS05006**: Missing `id` attribute
    /// - **OLS05007**: Invalid attribute
    /// - **OLS05008**: Non-integer `sequence` value
    /// - **OLS05009**: Submenu not allowed with `action` and `parent`
    /// - **OLS05010**: `web_icon` not allowed with `parent`
    /// - **OLS05011**: Invalid child element (only `<menuitem>` allowed)
    /// - **OLS05012**: `parent` attribute not allowed in submenus
    /// - **OLS05052**: Parent menuitem not found
    /// - **OLS05053**: Action not found
    /// - **OLS05054**: Group(s) not found
    fn load_menuitem(&mut self, session: &mut SessionInfo, node: &Node, is_submenu: bool, diagnostics: &mut Vec<Diagnostic>) -> bool {
        if node.tag_name().name() != "menuitem" { return false; }
        let mut found_id = None;
        let has_parent = node.attribute("parent").is_some();
        for attr in node.attributes() {
            match attr.name() {
                "id" => {
                    found_id = Some(attr.value().to_string());
                },
                "sequence" => {
                    if attr.value().parse::<i32>().is_err() {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05008, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                },
                "groups" => {
                    let missing_groups = attr.value().split(",")
                        .filter(|group| self.get_group_ids(session, group.trim_start_matches("-"), &attr, diagnostics).is_empty())
                        .collect::<Vec<&str>>()
                        .join(",");
                    if missing_groups.len() > 0 {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05054, &[&missing_groups]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                },
                "name" | "active" => {},
                "action" => {
                    if (has_parent || is_submenu) && node.has_children() {
                        for sub_menu in node.children().filter(|c| c.is_element() && c.tag_name().name() == "menuitem") {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05009, &[]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(sub_menu.range().start as u32, 0), end: Position::new(sub_menu.range().end as u32, 0) },
                                    ..diagnostic.clone()
                                });
                            }
                        }
                    }
                    //check that action exists
                    if SyncOdoo::get_xml_ids(session, &self.xml_symbol, attr.value(), &attr.range(), diagnostics).is_empty() {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05053, &[attr.value()]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                }
                "parent" => {
                    if is_submenu {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05012, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    } else {
                        //check that parent exists
                        if SyncOdoo::get_xml_ids(session, &self.xml_symbol, attr.value(), &attr.range(), diagnostics).is_empty() {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05052, &[attr.value()]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                    ..diagnostic.clone()
                                });
                            }
                        }
                    }
                },
                "web_icon" => {
                    if has_parent || is_submenu {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05010, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                }
                _ => {
                    if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05007, &[attr.name()]) {
                        diagnostics.push(Diagnostic {
                            range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                            ..diagnostic.clone()
                        });
                    }
                }
            }
        }
        if found_id.is_none() {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05006, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
        }
        for child in node.children().filter(|n| n.is_element()) {
            if child.tag_name().name() != "menuitem" {
                if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05011, &[child.tag_name().name()]) {
                    diagnostics.push(Diagnostic {
                        range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                        ..diagnostic.clone()
                    });
                }
            }
            else {
                self.load_menuitem(session, &child, true, diagnostics);
            }
        }
        let data = OdooData::MENUITEM(XmlDataMenuItem {
            file_symbol: Rc::downgrade(&self.xml_symbol),
            xml_id: found_id.clone().map(|id| oyarn!("{}", id)),
            range: node.range().clone()
        });
        self.on_operation_creation(session, found_id, node, data, diagnostics);
        true
    }

    /// Validates and processes a `<record>` element.
    ///
    /// Records are the primary data definition mechanism in Odoo XML. This method:
    /// - Validates the required `model` attribute
    /// - Validates optional attributes (`id`, `forcecreate`, `uid`, `context`)
    /// - Processes child `<field>` elements
    /// - Creates an `OdooDataRecord` with extracted data
    /// - Registers the XML ID if present
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The `<record>` node to validate
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `true` if the node is a `<record>`, `false` otherwise.
    ///
    /// # Valid Attributes
    ///
    /// - `model` (required) - Target model name
    /// - `id` - XML ID for referencing
    /// - `forcecreate` - Force creation even if record exists
    /// - `uid` - User context
    /// - `context` - Evaluation context
    ///
    /// # Diagnostics
    ///
    /// - **OLS05013**: Invalid attribute
    /// - **OLS05014**: Missing `model` attribute
    /// - **OLS05015**: Invalid child (only `<field>` allowed)
    ///
    /// # Note
    ///
    /// Model/field existence is NOT validated here - that happens in the VALIDATION
    /// phase (`xml_validation.rs`) after Python symbols are built.
    /// Load a <record> node, returning true if node is a record node
    fn load_record(&mut self, session: &mut SessionInfo, node: &Node, diagnostics: &mut Vec<Diagnostic>) -> bool {
        if node.tag_name().name() != "record" { return false; }
        let mut found_model = false;
        let mut found_id = None;
        for attr in node.attributes() {
            match attr.name() {
                "id" => {found_id = Some(attr.value().to_string());},
                "forcecreate" => {},
                "model" => {found_model = true;},
                "uid" => {},
                "context" => {},
                _ => {
                    if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05013, &[attr.name()]) {
                        diagnostics.push(Diagnostic {
                            range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                            ..diagnostic.clone()
                        });
                    }
                }
            }
        }

        if !found_model {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05014, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
            return true;
        }
        let mut data = OdooDataRecord {
            file_symbol: Rc::downgrade(&self.xml_symbol),
            model: (oyarn!("{}", node.attribute("model").unwrap()), node.attribute_node("model").unwrap().range()),
            xml_id: found_id.clone().map(|id| oyarn!("{}", id)),
            fields: vec![],
            range: node.range().clone()
        };
        for child in node.children().filter(|n| n.is_element()) {
            if let Some(field) = self.load_field(session, &child, diagnostics) {
                data.fields.push(field);
            } else if child.tag_name().name() != "field" {
                // Diagnostic only for non-field tags, not for invalid ones
                if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05015, &[child.tag_name().name()]) {
                    diagnostics.push(Diagnostic {
                        range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                        ..diagnostic.clone()
                    });
                }
            }
        }
        let data = OdooData::RECORD(data);
        self.on_operation_creation(session, found_id, node, data, diagnostics);
        true
    }

    /// Validates and processes a `<field>` element within a record.
    ///
    /// Fields define values for model fields. This method validates:
    /// - Required `name` attribute
    /// - Mutually exclusive attributes (`type`, `ref`, `eval`, `search`)
    /// - Type-specific content validation (`int`, `float`, `list`, `tuple`)
    /// - Attribute-content compatibility (e.g., no text with `ref`)
    /// - Valid child elements (only `<record>` for non-xml/html types)
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The `<field>` node to validate
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `Some(OdooDataField)` with extracted field data, or `None` if invalid.
    ///
    /// # Valid Attributes
    ///
    /// - `name` (required) - Field name on the model
    /// - `type` - Value type: `int`, `float`, `list`, `tuple`, `xml`, `html`, `base64`, `char`, `file`
    /// - `ref` - XML ID reference (for Many2one fields)
    /// - `eval` - Python expression to evaluate
    /// - `search` - Domain to search for records
    /// - `model` - Model context (only with `eval`/`search`)
    /// - `use` - Field to use from search result (only with `search`)
    /// - `file` - External file path
    ///
    /// # Diagnostics
    ///
    /// - **OLS05016**: Missing `name` attribute
    /// - **OLS05017**: Multiple exclusive attributes
    /// - **OLS05018**: Invalid int content
    /// - **OLS05019**: Invalid float content
    /// - **OLS05020**: Invalid child in list/tuple
    /// - **OLS05021**: Text with `file` attribute
    /// - **OLS05022**: Text with `ref`/`eval`/`search`
    /// - **OLS05023**: `model` without `eval`/`search`
    /// - **OLS05024**: `use` without `search`
    /// - **OLS05025**: Invalid attribute
    /// - **OLS05026**: Invalid child element
    fn load_field(&mut self, session: &mut SessionInfo, node: &Node, diagnostics: &mut Vec<Diagnostic>) -> Option<OdooDataField> {
        if node.tag_name().name() != "field" { return None; }
        let Some(node_name_node) = node.attribute_node("name") else {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05016, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
            return None;
        };

        let has_type = node.attribute("type").is_some();
        let ref_key = node.attribute_node("ref").map(|rk| (rk.value().to_string(), rk.range()));
        let has_ref = ref_key.is_some();
        let has_eval = node.attribute("eval").is_some();
        let has_search = node.attribute("search").is_some();
        if [has_type, has_ref, has_eval, has_search].iter().filter(|b| **b).count() > 1 {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05017, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
            return None;
        }
        let mut is_xml_or_html = false;
        let mut iterable_child_node = false;
        if let Some(field_type) = node.attribute("type") {
            match field_type {
                "int" => {
                    let content = node.text().unwrap_or("");
                    if !(content.parse::<i32>().is_ok() || content == "None") {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05018, &[content]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                }
                "float" => {
                    let content = node.text().unwrap_or("");
                    if content.parse::<f64>().is_err() {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05019, &[content]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                }
                "list" | "tuple" => {
                    iterable_child_node = true;
                    for child in node.children() {
                        if !self.load_value(session, &child, diagnostics) {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05020, &[child.tag_name().name()]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                                    ..diagnostic.clone()
                                });
                            }
                        }
                    }
                }
                "html" | "xml" => {
                    is_xml_or_html = true;
                }
                "base64" | "char" | "file" => {
                    if node.has_attribute("file") {
                        if node.text().is_some() {
                            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05021, &[]) {
                                diagnostics.push(Diagnostic {
                                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                                    ..diagnostic.clone()
                                });
                            }
                        }
                    }
                }
                _ => {},
            }
        } 
        for attr in node.attributes() {
            match attr.name() {
                "name" | "type" | "file" => {},
                "ref" | "eval" | "search" => {
                    if node.text().is_some() {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05022, &[attr.name()]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                },
                "model" => {
                    if !has_eval && !has_search {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05023, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                },
                "use" => {
                    if !has_search {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05024, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                }
                _ => {
                    if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05025, &[attr.name()]) {
                        diagnostics.push(Diagnostic {
                            range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                            ..diagnostic.clone()
                        });
                    }
                }
            }
        }
        for child in node.children() {
            if !self.load_record(session, &child, diagnostics) && !child.is_text() && !child.is_comment() && !is_xml_or_html && !iterable_child_node{
                if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05026, &[]) {
                    diagnostics.push(Diagnostic {
                        range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                        ..diagnostic.clone()
                    });
                }
            }
        }
        let mut text = None;
        let mut text_range = None;
        for child in node.children() {
            if child.is_text() {
                text = child.text().map(|s| s.to_string());
                text_range = Some(child.range());
            }
        }
        Some(OdooDataField {
            name: oyarn!("{}", node_name_node.value()),
            range: node_name_node.range(),
            text: text,
            text_range: text_range,
            ref_key,
        })
    }

    /// Validates and processes a `<value>` element (used in `<function>` calls).
    ///
    /// Value elements define arguments for function calls. This method validates:
    /// - Mutually exclusive attributes (`search`, `eval`, `type`, `file`)
    /// - Content requirements based on attributes
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The `<value>` node to validate
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `true` if the node is a `<value>`, `false` otherwise.
    ///
    /// # Attribute Exclusivity
    ///
    /// Only one of these can be present:
    /// - `search` - Domain to search for records
    /// - `eval` - Python expression
    /// - `type` + text content - Typed value
    /// - `file` - External file
    /// - Plain text content
    ///
    /// # Diagnostics
    ///
    /// - **OLS05027**: `search` with conflicting attributes
    /// - **OLS05028**: `eval` with conflicting attributes
    /// - **OLS05029**: `type` with `search`/`eval`
    /// - **OLS05030**: Text with `file`
    /// - **OLS05031**: `file` with `search`/`eval`
    /// - **OLS05032**: Invalid attribute
    /// - **OLS05036**: Empty value with `type`
    /// - **OLS05037**: Empty value without required content
    fn load_value(&mut self, session: &mut SessionInfo, node: &Node, diagnostics: &mut Vec<Diagnostic>) -> bool {
        if node.tag_name().name() != "value" { return false; }
        let mut has_search = false;
        let mut has_eval = false;
        let mut has_type_or_file_or_text =  node.text().is_some();
        for attr in node.attributes() {
            match attr.name() {
                "name" | "model" | "use" => {},
                "search" => {
                    has_search = true;
                    if has_eval || has_type_or_file_or_text {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05027, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                },
                "eval" => {
                    has_eval = true;
                    if has_search || has_type_or_file_or_text {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05028, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                },
                "type" => {
                    has_type_or_file_or_text = true;
                    if has_search || has_eval {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05029, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                            continue;
                        }
                    }
                    if !node.has_attribute("file") && !node.text().is_some() {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05036, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                },
                "file" => {
                    has_type_or_file_or_text = true;
                    if node.text().is_some() {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05030, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                            continue;
                        }
                    }
                    if has_search || has_eval {
                        if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05031, &[]) {
                            diagnostics.push(Diagnostic {
                                range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                                ..diagnostic.clone()
                            });
                        }
                    }
                }
                _ => {
                    if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05032, &[attr.name()]) {
                        diagnostics.push(Diagnostic {
                            range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                            ..diagnostic.clone()
                        });
                    }
                }
            }
        }
        if !has_search && !has_eval && !has_type_or_file_or_text {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05037, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
        }
        true
    }

    /// Validates and processes a `<template>` element.
    ///
    /// Templates define QWeb views for rendering. This method:
    /// - Extracts the optional `id` attribute
    /// - Creates an `XmlDataTemplate` record
    /// - Registers the XML ID
    ///
    /// Note: Template content validation is not performed here; QWeb syntax
    /// is complex and context-dependent.
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The `<template>` node to validate
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `true` if the node is a `<template>`, `false` otherwise.
    fn load_template(&mut self, session: &mut SessionInfo, node: &Node, diagnostics: &mut Vec<Diagnostic>) -> bool {
        if node.tag_name().name() != "template" { return false; }
        //no interesting rule to check, as 'any' is valid
        let found_id = node.attribute("id").map(|s| s.to_string());
        let data = OdooData::TEMPLATE(XmlDataTemplate {
            file_symbol: Rc::downgrade(&self.xml_symbol),
            xml_id: found_id.clone().map(|id| oyarn!("{}", id)),
            range: node.range().clone(),
        });
        self.on_operation_creation(session, found_id, node, data, diagnostics);
        true
    }

    /// Validates and processes a `<delete>` element.
    ///
    /// Delete elements remove records from the database. This method validates:
    /// - Required `model` attribute
    /// - Exactly one of `id` or `search` (not both, not neither)
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The `<delete>` node to validate
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `true` if the node is a `<delete>`, `false` otherwise.
    ///
    /// # Diagnostics
    ///
    /// - **OLS05033**: Missing `model` attribute
    /// - **OLS05034**: Both `id` and `search` present
    /// - **OLS05035**: Neither `id` nor `search` present
    fn load_delete(&mut self, session: &mut SessionInfo, node: &Node, diagnostics: &mut Vec<Diagnostic>) -> bool {
        if node.tag_name().name() != "delete" { return false; }
        if node.attribute("model").is_none() {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05033, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
            return true;
        }
        let found_id = node.attribute("id").map(|s| s.to_string());
        let has_search = node.attribute("search").is_some();
        if found_id.is_some() && has_search {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05034, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
        }
        if found_id.is_none() && !has_search {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05035, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
        }
        let data = OdooData::DELETE(XmlDataDelete {
            file_symbol: Rc::downgrade(&self.xml_symbol),
            xml_id: found_id.clone().map(|id| oyarn!("{}", id)),
            range: node.range().clone(),
            model: Sy!(node.attribute("model").unwrap().to_string()),
        });
        self.on_operation_creation(session, found_id, node, data, diagnostics);
        true
    }

    /// Validates and processes a `<function>` element.
    ///
    /// Function elements call Python methods during data loading. This method validates:
    /// - Required `model` and `name` attributes
    /// - Valid attributes (`uid`, `context`, `eval`)
    /// - Content requirements: either `eval` OR `<value>`/`<function>` children
    /// - No `<value>` or `<function>` children when `eval` is present
    ///
    /// # Arguments
    ///
    /// * `session` - Current session with server state
    /// * `node` - The `<function>` node to validate
    /// * `diagnostics` - Collector for validation errors
    ///
    /// # Returns
    ///
    /// `true` if the node is a `<function>`, `false` otherwise.
    ///
    /// # Valid Attributes
    ///
    /// - `model` (required) - Model to call method on
    /// - `name` (required) - Method name to call
    /// - `eval` - Arguments as Python expression (mutually exclusive with children)
    /// - `uid` - User context
    /// - `context` - Evaluation context
    ///
    /// # Diagnostics
    ///
    /// - **OLS05044**: Missing `model` or `name`
    /// - **OLS05045**: `<value>` child with `eval` attribute
    /// - **OLS05046**: Invalid attribute
    /// - **OLS05047**: `<function>` child with `eval` attribute
    /// - **OLS05048**: Invalid child element
    /// - **OLS05038**: Empty function (no `eval` and no children)
    fn load_function(&mut self, session: &mut SessionInfo, node: &Node, diagnostics: &mut Vec<Diagnostic>) -> bool {
        if node.tag_name().name() != "function" { return false; }
        for attr in ["model", "name"] {
            if node.attribute(attr).is_none() {
                if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05044, &[attr]) {
                    diagnostics.push(Diagnostic {
                        range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                        ..diagnostic.clone()
                    });
                }
            }
        }
        let mut has_eval = false;
        for attr in node.attributes() {
            match attr.name() {
                "model" | "name" => {},
                "uid" => {},
                "context" => {},
                "eval" => {
                    has_eval = true;
                }
                _ => {
                    if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05046, &[attr.name()]) {
                        diagnostics.push(Diagnostic {
                            range: Range { start: Position::new(attr.range().start as u32, 0), end: Position::new(attr.range().end as u32, 0) },
                            ..diagnostic.clone()
                        });
                    }
                }
            }
        }
        let mut has_value_or_function_child = false;
        for child in node.children().filter(|n| n.is_element()) {
            if self.load_value(session, &child, diagnostics) {
                has_value_or_function_child = true;
                if has_eval {
                    if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05045, &[]) {
                        diagnostics.push(Diagnostic {
                            range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                            ..diagnostic.clone()
                        });
                    }
                }
            } else if self.load_function(session, &child, diagnostics) {
                has_value_or_function_child = true;
                if has_eval {
                    if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05047, &[]) {
                        diagnostics.push(Diagnostic {
                            range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                            ..diagnostic.clone()
                        });
                    }
                }
            } else {
                if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05048, &[child.tag_name().name()]) {
                    diagnostics.push(Diagnostic {
                        range: Range { start: Position::new(child.range().start as u32, 0), end: Position::new(child.range().end as u32, 0) },
                        ..diagnostic.clone()
                    });
                }
            }
        }
        if !has_eval && !has_value_or_function_child {
            if let Some(diagnostic) = create_diagnostic(session, DiagnosticCode::OLS05038, &[]) {
                diagnostics.push(Diagnostic {
                    range: Range { start: Position::new(node.range().start as u32, 0), end: Position::new(node.range().end as u32, 0) },
                    ..diagnostic.clone()
                });
            }
        }
        true
    }
}