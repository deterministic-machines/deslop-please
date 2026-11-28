; Python extraction queries.
;
; extract.rs compiles this file once and drives extraction from the captures
; below. ScopeIndex, fed by the declaration captures, supplies enclosing
; holders and parents instead of recursive AST walking.

; --- Imports ----------------------------------------------------------------
(import_statement) @import
(import_from_statement) @import

; --- Declarations -----------------------------------------------------------
; A `def`, `class`, or assignment anywhere; extract.rs keeps only those that a
; module or class body holds directly, so nested closures stay out.
(class_definition) @class
(function_definition) @function
(assignment) @assignment

; --- References -------------------------------------------------------------
(attribute) @member
(call) @call

; --- Function-body facts ----------------------------------------------------
(augmented_assignment) @computes
(binary_operator) @computes
(boolean_operator) @computes
(comparison_operator) @computes
(conditional_expression) @computes
(named_expression) @computes
(if_statement) @computes
(for_statement) @computes
(while_statement) @computes
(with_statement) @computes
(try_statement) @computes
(match_statement) @computes
(raise_statement) @computes
(list_comprehension) @computes
(set_comprehension) @computes
(dictionary_comprehension) @computes
(generator_expression) @computes
(yield) @computes
(unary_operator) @unary

; A lambda is a value of its own: what its body computes is not the work of
; the declaration that holds it, so the enclosing body never counts it.
(lambda) @skip
