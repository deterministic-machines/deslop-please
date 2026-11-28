; TypeScript/JavaScript extraction queries.
;
; extract.rs compiles this file once per grammar (LANGUAGE_TYPESCRIPT and
; LANGUAGE_TSX) and drives extraction from the captures below. ScopeIndex, fed
; by the declaration captures, supplies enclosing holders and parents instead
; of recursive AST walking.

; --- Imports and re-exports -------------------------------------------------
(import_statement) @import
(export_statement) @export

; CommonJS bindings: `const x = require('./x')`, `const { a, b: c } = require(...)`.
(variable_declarator
  name: [(identifier) (object_pattern)]
  value: (call_expression
    function: (identifier) @cjs.function
    arguments: (arguments (string) @cjs.source))) @cjs.declarator

; --- Declarations -----------------------------------------------------------
; A declaration is a program child, the `declaration` of an `export`, a child of
; an `ambient_declaration`, or a direct statement of a namespace body.
(program
  [
    (class_declaration)
    (abstract_class_declaration)
    (interface_declaration)
    (type_alias_declaration)
    (enum_declaration)
    (internal_module)
    (module)
    (function_declaration)
    (generator_function_declaration)
    (lexical_declaration)
    (variable_declaration)
  ] @decl)

(export_statement
  declaration:
  [
    (class_declaration)
    (abstract_class_declaration)
    (interface_declaration)
    (type_alias_declaration)
    (enum_declaration)
    (internal_module)
    (module)
    (function_declaration)
    (generator_function_declaration)
    (lexical_declaration)
    (variable_declaration)
    (ambient_declaration)
  ] @decl)

(ambient_declaration
  [
    (class_declaration)
    (abstract_class_declaration)
    (interface_declaration)
    (type_alias_declaration)
    (enum_declaration)
    (internal_module)
    (module)
    (function_declaration)
    (generator_function_declaration)
    (lexical_declaration)
    (variable_declaration)
  ] @decl)

(internal_module
  body:
  (statement_block
    [
      (class_declaration)
      (abstract_class_declaration)
      (interface_declaration)
      (type_alias_declaration)
      (enum_declaration)
      (internal_module)
      (module)
      (function_declaration)
      (generator_function_declaration)
      (lexical_declaration)
      (variable_declaration)
    ] @decl))

(module
  body:
  (statement_block
    [
      (class_declaration)
      (abstract_class_declaration)
      (interface_declaration)
      (type_alias_declaration)
      (enum_declaration)
      (internal_module)
      (module)
      (function_declaration)
      (generator_function_declaration)
      (lexical_declaration)
      (variable_declaration)
    ] @decl))

; `export default <expression>` values that declare nothing themselves.
(export_statement value: [(arrow_function) (function_expression) (generator_function)] @default_function)
(export_statement value: (class) @default_class)

; --- Class members ----------------------------------------------------------
(class_declaration body: (class_body (public_field_definition) @field))
(abstract_class_declaration body: (class_body (public_field_definition) @field))
(class_declaration body: (class_body [(method_definition) (method_signature) (abstract_method_signature)] @method))
(abstract_class_declaration body: (class_body [(method_definition) (method_signature) (abstract_method_signature)] @method))
; Constructor parameter properties: `constructor(private items: Item[])`.
(required_parameter [(accessibility_modifier) "readonly"]) @param_property
(optional_parameter [(accessibility_modifier) "readonly"]) @param_property

; --- Enum members -----------------------------------------------------------
(enum_declaration body: (enum_body [(enum_assignment) (property_identifier)] @enum_member))

; --- Function bodies --------------------------------------------------------
(arrow_function body: (_) @function_body)
(function_expression body: (statement_block) @function_body)
(function_declaration body: (statement_block) @function_body)
(generator_function body: (statement_block) @function_body)
(generator_function_declaration body: (statement_block) @function_body)
(method_definition body: (statement_block) @function_body)

; --- References -------------------------------------------------------------
(type_identifier) @type_ref
(nested_type_identifier) @type_path_ref
(new_expression) @new
(call_expression) @call
(member_expression) @member
; Function values passed along or returned: `items.map(handler)`, `return helper`.
(arguments (identifier) @value_ref)
(return_statement (identifier) @value_ref)

; --- Function-body facts ----------------------------------------------------
(variable_declarator) @local
(binary_expression) @computes
(unary_expression) @unary
(update_expression) @computes
(augmented_assignment_expression) @computes
(ternary_expression) @computes
(if_statement) @computes
(switch_statement) @computes
(for_statement) @computes
(for_in_statement) @computes
(while_statement) @computes
(do_statement) @computes
(try_statement) @computes
(throw_statement) @computes
