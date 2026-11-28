; Go extraction queries.
;
; extract.rs compiles this file once and drives extraction from the captures
; below. ScopeIndex, fed by the declaration captures, supplies enclosing
; holders and parents instead of recursive AST walking.

; --- Top-level declarations -------------------------------------------------
; Only file-level declarations are module items; a type or var declared in a
; function body is local to that function.
(source_file
  [
    (import_declaration)
    (type_declaration)
    (function_declaration)
    (method_declaration)
    (const_declaration)
    (var_declaration)
  ] @decl)

; --- Type members -----------------------------------------------------------
(struct_type (field_declaration_list (field_declaration) @field))
(interface_type (method_elem) @interface_method)

; --- Function bodies --------------------------------------------------------
(function_declaration body: (block) @function_body)
(method_declaration body: (block) @function_body)
(func_literal body: (block) @function_body)

; --- Locals -----------------------------------------------------------------
(short_var_declaration) @local
(var_spec) @local_spec

; --- Computation ------------------------------------------------------------
(binary_expression) @computes
(inc_statement) @computes
(dec_statement) @computes
(if_statement) @computes
(for_statement) @computes
(expression_switch_statement) @computes
(type_switch_statement) @computes
(select_statement) @computes
(go_statement) @computes
(defer_statement) @computes
(assignment_statement) @assignment
(unary_expression) @unary

; --- References -------------------------------------------------------------
(call_expression) @call
(composite_literal) @composite
(selector_expression) @selector
(type_identifier) @type_ref
(qualified_type) @type_path_ref
