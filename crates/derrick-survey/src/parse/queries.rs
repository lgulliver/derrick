//! Tree-sitter S-expression queries per language.
//!
//! Symbol queries name the definition node after its [`SymbolKind`]
//! (`@function`, `@type`, `@interface`, `@enum`, `@constant`, `@module`) and
//! capture the identifier as `@name`. Reference queries capture the textual
//! target as `@call` or `@reference`.

pub(super) const RUST_SYMBOLS: &str = r#"
(function_item name: (identifier) @name) @function
(function_signature_item name: (identifier) @name) @function
(macro_definition name: (identifier) @name) @function
(struct_item name: (type_identifier) @name) @type
(union_item name: (type_identifier) @name) @type
(type_item name: (type_identifier) @name) @type
(enum_item name: (type_identifier) @name) @enum
(trait_item name: (type_identifier) @name) @interface
(const_item name: (identifier) @name) @constant
(static_item name: (identifier) @name) @constant
(mod_item name: (identifier) @name) @module
"#;

pub(super) const RUST_REFS: &str = r#"
(call_expression function: (identifier) @call)
(call_expression function: (scoped_identifier name: (identifier) @call))
(call_expression function: (field_expression field: (field_identifier) @call))
(macro_invocation macro: (identifier) @call)
"#;

pub(super) const PYTHON_SYMBOLS: &str = r#"
(function_definition name: (identifier) @name) @function
(class_definition name: (identifier) @name) @type
"#;

pub(super) const PYTHON_REFS: &str = r#"
(call function: (identifier) @call)
(call function: (attribute attribute: (identifier) @call))
"#;

pub(super) const GO_SYMBOLS: &str = r#"
(function_declaration name: (identifier) @name) @function
(method_declaration name: (field_identifier) @name) @function
(type_declaration (type_spec name: (type_identifier) @name)) @type
(const_declaration (const_spec name: (identifier) @name)) @constant
"#;

pub(super) const GO_REFS: &str = r#"
(call_expression function: (identifier) @call)
(call_expression function: (selector_expression field: (field_identifier) @call))
"#;

pub(super) const JAVASCRIPT_SYMBOLS: &str = r#"
(function_declaration name: (identifier) @name) @function
(generator_function_declaration name: (identifier) @name) @function
(class_declaration name: (identifier) @name) @type
(method_definition name: (property_identifier) @name) @function
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression)]) @function
"#;

pub(super) const TYPESCRIPT_SYMBOLS: &str = r#"
(function_declaration name: (identifier) @name) @function
(generator_function_declaration name: (identifier) @name) @function
(class_declaration name: (type_identifier) @name) @type
(abstract_class_declaration name: (type_identifier) @name) @type
(method_definition name: (property_identifier) @name) @function
(interface_declaration name: (type_identifier) @name) @interface
(type_alias_declaration name: (type_identifier) @name) @type
(enum_declaration name: (identifier) @name) @enum
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression)]) @function
"#;

pub(super) const JS_TS_REFS: &str = r#"
(call_expression function: (identifier) @call)
(call_expression function: (member_expression property: (property_identifier) @call))
"#;

pub(super) const CSHARP_SYMBOLS: &str = r#"
(class_declaration name: (identifier) @name) @type
(struct_declaration name: (identifier) @name) @type
(record_declaration name: (identifier) @name) @type
(interface_declaration name: (identifier) @name) @interface
(enum_declaration name: (identifier) @name) @enum
(delegate_declaration name: (identifier) @name) @type
(method_declaration name: (identifier) @name) @function
(constructor_declaration name: (identifier) @name) @function
(local_function_statement name: (identifier) @name) @function
(property_declaration name: (identifier) @name) @function
(namespace_declaration name: (_) @name) @module
(file_scoped_namespace_declaration name: (_) @name) @module
"#;

pub(super) const CSHARP_REFS: &str = r#"
(invocation_expression function: (identifier) @call)
(invocation_expression function: (member_access_expression name: (identifier) @call))
(object_creation_expression type: (identifier) @call)
"#;
