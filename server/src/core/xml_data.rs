//! XML Data Structures
//!
//! This module defines the data structures used to represent parsed XML elements
//! from Odoo data files. These structures are created during the ARCH phase and
//! stored in `XmlFileSymbol.xml_ids` for later validation and LSP feature support.
//!
//! # Hierarchy
//!
//! ```text
//! XmlFileSymbol
//! └── xml_ids: HashMap<OYarn, Vec<OdooData>>
//!     ├── OdooData::RECORD(OdooDataRecord)
//!     │   └── fields: Vec<OdooDataField>
//!     ├── OdooData::MENUITEM(XmlDataMenuItem)
//!     ├── OdooData::TEMPLATE(XmlDataTemplate)
//!     └── OdooData::DELETE(XmlDataDelete)
//! ```
//!
//! # Usage
//!
//! These structures are used by:
//! - `xml_validation.rs`: To validate model/field existence
//! - `xml_ast_utils.rs`: To resolve symbols for LSP features
//! - `workspace_symbols.rs`: To search XML IDs across the workspace

use std::{cell::RefCell, ops::Range, rc::{Rc, Weak}};

use crate::{constants::{OYarn, SymType}, core::symbols::symbol::Symbol};

/// Represents a parsed XML data element from an Odoo data file.
///
/// This enum captures the different types of data-defining elements in Odoo XML:
/// - `<record>` - Create or update database records
/// - `<menuitem>` - Define menu entries
/// - `<template>` - QWeb templates (views)
/// - `<delete>` - Remove existing records
///
/// Each variant stores the element's XML ID, byte range, and type-specific data.
/// The `file_symbol` weak reference allows navigation back to the containing file.
///
/// # Example
///
/// ```xml
/// <record model="res.partner" id="partner_demo">
///     <field name="name">Demo Partner</field>
/// </record>
/// ```
///
/// Becomes:
/// ```ignore
/// OdooData::RECORD(OdooDataRecord {
///     model: ("res.partner", 8..19),
///     xml_id: Some("partner_demo"),
///     fields: vec![OdooDataField { name: "name", ... }],
///     ...
/// })
/// ```
#[derive(Debug, Clone)]
pub enum OdooData {
    /// A `<record>` element defining database record data.
    RECORD(OdooDataRecord),
    /// A `<menuitem>` element defining a menu entry.
    MENUITEM(XmlDataMenuItem),
    /// A `<template>` element defining a QWeb template.
    TEMPLATE(XmlDataTemplate),
    /// A `<delete>` element removing existing records.
    DELETE(XmlDataDelete),
}

/// Data from a `<record>` XML element.
///
/// Records are the primary way to define data in Odoo XML files. They create
/// or update records in the database for a specific model.
///
/// # Fields
///
/// * `file_symbol` - Weak reference to the containing `XmlFileSymbol`
/// * `model` - The target model name and its byte range in the source
/// * `xml_id` - Optional XML ID for referencing this record
/// * `fields` - Field values defined within the record
/// * `range` - Byte range of the entire `<record>` element
///
/// # Example XML
///
/// ```xml
/// <record model="ir.ui.view" id="view_partner_form">
///     <field name="name">partner.form</field>
///     <field name="model">res.partner</field>
///     <field name="arch" type="xml">
///         <form>...</form>
///     </field>
/// </record>
/// ```
#[derive(Debug, Clone)]
pub struct OdooDataRecord {
    /// Weak reference to the `XmlFileSymbol` containing this record.
    pub file_symbol: Weak<RefCell<Symbol>>,
    /// The model name (e.g., `"res.partner"`) and its byte range in source.
    pub model: (OYarn, Range<usize>),
    /// Optional XML ID for this record (e.g., `"partner_demo"`).
    pub xml_id: Option<OYarn>,
    /// Field values defined within this record.
    pub fields: Vec<OdooDataField>,
    /// Byte range of the entire `<record>` element in the source file.
    pub range: Range<usize>,
}

/// Data from a `<field>` XML element within a record.
///
/// Fields define values for model fields. They can contain:
/// - Text content (for simple values)
/// - `ref` attribute (for Many2one references via XML ID)
/// - `eval` attribute (for Python expressions)
/// - `search` attribute (for domain-based lookups)
/// - `type` attribute (for typed values like `int`, `float`, `list`)
///
/// # Fields
///
/// * `name` - The field name on the model
/// * `range` - Byte range of the `<field>` element
/// * `text` - Text content if present
/// * `ref_key` - Value and range of `ref` attribute if present
/// * `text_range` - Byte range of text content if present
///
/// # Example XML
///
/// ```xml
/// <field name="partner_id" ref="base.partner_demo"/>
/// <field name="amount">100.50</field>
/// <field name="date" eval="time.strftime('%Y-%m-%d')"/>
/// ```
#[derive(Debug, Clone)]
pub struct OdooDataField {
    /// The field name on the target model (e.g., `"partner_id"`).
    pub name: OYarn,
    /// Byte range of the entire `<field>` element.
    pub range: Range<usize>,
    /// Text content of the field (for simple values).
    pub text: Option<String>,
    /// Value and range of `ref="..."` attribute (for XML ID references).
    pub ref_key: Option<(String, Range<usize>)>,
    /// Byte range of text content within the field element.
    pub text_range: Option<Range<usize>>,
}

/// Data from a `<menuitem>` XML element.
///
/// Menu items define entries in Odoo's menu hierarchy. They can specify:
/// - Parent menu (for nesting)
/// - Action to execute when clicked
/// - Security groups for access control
///
/// # Example XML
///
/// ```xml
/// <menuitem id="menu_sale_root" name="Sales"
///           sequence="10" web_icon="sale,static/description/icon.png"/>
/// <menuitem id="menu_sale_orders" parent="menu_sale_root"
///           action="action_sale_orders" groups="sales_team.group_sale_salesman"/>
/// ```
#[derive(Debug, Clone)]
pub struct XmlDataMenuItem {
    /// Weak reference to the `XmlFileSymbol` containing this menu item.
    pub file_symbol: Weak<RefCell<Symbol>>,
    /// Optional XML ID for this menu item.
    pub xml_id: Option<OYarn>,
    /// Byte range of the `<menuitem>` element in the source file.
    pub range: Range<usize>,
}

/// Data from a `<template>` XML element.
///
/// Templates define QWeb views for rendering HTML. They can:
/// - Inherit from other templates via `inherit_id`
/// - Be restricted to security groups
/// - Define primary views or view extensions
///
/// # Example XML
///
/// ```xml
/// <template id="portal_my_home" name="Portal Home">
///     <t t-call="portal.portal_layout">...</t>
/// </template>
/// <template id="portal_my_home_inherit" inherit_id="portal.portal_my_home">
///     <xpath expr="//div[@id='content']" position="inside">...</xpath>
/// </template>
/// ```
#[derive(Debug, Clone)]
pub struct XmlDataTemplate {
    /// Weak reference to the `XmlFileSymbol` containing this template.
    pub file_symbol: Weak<RefCell<Symbol>>,
    /// Optional XML ID for this template.
    pub xml_id: Option<OYarn>,
    /// Byte range of the `<template>` element in the source file.
    pub range: Range<usize>,
}

/// Data from a `<delete>` XML element.
///
/// Delete elements remove existing records from the database. They must specify:
/// - A model to delete from
/// - Either an `id` (XML ID) or `search` domain to identify records
///
/// # Example XML
///
/// ```xml
/// <delete model="ir.ui.view" id="old_deprecated_view"/>
/// <delete model="res.partner" search="[('active', '=', False)]"/>
/// ```
#[derive(Debug, Clone)]
pub struct XmlDataDelete {
    /// Weak reference to the `XmlFileSymbol` containing this delete.
    pub file_symbol: Weak<RefCell<Symbol>>,
    /// Optional XML ID of the record to delete.
    pub xml_id: Option<OYarn>,
    /// Byte range of the `<delete>` element in the source file.
    pub range: Range<usize>,
    /// The model to delete records from.
    pub model: OYarn,
}

impl OdooData {

    pub fn set_file_symbol(&mut self, xml_symbol: &Rc<RefCell<Symbol>>) {
        match self {
            OdooData::RECORD(record) => {
                record.file_symbol = Rc::downgrade(xml_symbol);
            },
            OdooData::MENUITEM(menu_item) => {
                menu_item.file_symbol = Rc::downgrade(xml_symbol);
            },
            OdooData::TEMPLATE(template) => {
                template.file_symbol = Rc::downgrade(xml_symbol);
            },
            OdooData::DELETE(delete) => {
                delete.file_symbol = Rc::downgrade(xml_symbol);
            },
        }
    }

    pub fn get_range(&self) -> Range<usize> {
        match self {
            OdooData::RECORD(record) => record.range.clone(),
            OdooData::MENUITEM(menu_item) => menu_item.range.clone(),
            OdooData::TEMPLATE(template) => template.range.clone(),
            OdooData::DELETE(delete) => delete.range.clone(),
        }
    }

    pub fn get_xml_file_symbol(&self) -> Option<Rc<RefCell<Symbol>>> {
        let file_symbol = self.get_file_symbol()?;
        if let Some(symbol) = file_symbol.upgrade() {
            if symbol.borrow().typ() == SymType::XML_FILE {
                return Some(symbol);
            }
        }
        None
    }

    /* Warning: the returned symbol can of a different type than an XML_SYMBOL */
    pub fn get_file_symbol(&self) -> Option<Weak<RefCell<Symbol>>> {
        match self {
            OdooData::RECORD(record) => {
                Some(record.file_symbol.clone())
            },
            OdooData::MENUITEM(menu_item) => {
                Some(menu_item.file_symbol.clone())
            },
            OdooData::TEMPLATE(template) => {
                Some(template.file_symbol.clone())
            },
            OdooData::DELETE(delete) => {
                Some(delete.file_symbol.clone())
            }
        }
    }
}