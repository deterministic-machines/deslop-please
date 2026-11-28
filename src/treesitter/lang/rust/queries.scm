;; Rust extraction query: every file is read by this one flat pass.
;;
;; A capture files its node into one bucket of the extractor: declarations
;; (`@decl.*`), imports, locals, references (`@ref.*`), or nodes that make a
;; body compute (`@computes`). Patterns never nest inside each other: the
;; extractor resolves scopes, parents and holders from byte ranges
;; (`Query`'s `ScopeIndex`), so the same captures serve modules, items inside
;; items, and expressions inside bodies alike.

;; ----------------------------------------------------------------- scopes
;; An inline module's body is its own module scope; `mod foo;` is discovered
;; from the filesystem instead.
(mod_item) @module

;; ----------------------------------------------------------- declarations
(struct_item) @decl.struct
(union_item) @decl.union
(enum_item) @decl.enum
(trait_item) @decl.trait
(type_item) @decl.type
(associated_type) @decl.type
(impl_item) @decl.impl
(function_item) @decl.function
(function_signature_item) @decl.function
(const_item) @decl.constant
(static_item) @decl.constant
(enum_variant) @decl.variant
(field_declaration) @decl.field

;; Tuple fields and tuple variants hold their types as direct children of an
;; ordered list, with visibility modifiers between them.
(ordered_field_declaration_list) @decl.tuple_fields

;; ---------------------------------------------------------------- imports
(use_declaration) @import

;; ----------------------------------------------------------------- locals
(let_declaration) @local

;; ------------------------------------------------------------- references
(type_identifier) @ref.type
(scoped_type_identifier) @ref.type_path
(struct_expression) @ref.struct
(call_expression) @ref.call
(field_expression) @ref.field
(scoped_identifier) @ref.path
(generic_function) @ref.path
(identifier) @ref.identifier

;; --------------------------------------------------- computation markers
(binary_expression) @computes
(unary_expression) @computes
(compound_assignment_expr) @computes
(if_expression) @computes
(match_expression) @computes
(while_expression) @computes
(for_expression) @computes
(loop_expression) @computes
(macro_invocation) @computes
