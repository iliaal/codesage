; Pattern 0: Function declaration → Function
(function_declaration name: (identifier) @name) @def

; Pattern 1: Class declaration → Class
(class_declaration name: (identifier) @name) @def

; Pattern 2: Method definition → Method
(method_definition name: (property_identifier) @name) @def

; Pattern 3: Exported const/let → Constant
(export_statement declaration: (lexical_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 4: Top-level const/let → Constant
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 5: export default class X → Class
(export_statement value: (class name: (identifier) @name) @def)

; Pattern 6: exports.X = ... (CommonJS named export) → Constant
(expression_statement
  (assignment_expression
    left: (member_expression
      object: (identifier) @_obj
      property: (property_identifier) @name)
    (#eq? @_obj "exports")) @def)

; Pattern 7: generator function (function* g()) → Function
(generator_function_declaration name: (identifier) @name) @def

; Pattern 8: top-level var → Constant
(program (variable_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 9: exported var → Constant
(export_statement declaration: (variable_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 10: module.exports = ... (whole-exports assignment) → Constant.
; Named "exports": the module's entry point. Appended (not inserted after
; pattern 6) so every pattern index above keeps its kind-map meaning.
(expression_statement
  (assignment_expression
    left: (member_expression
      object: (identifier) @_mod
      property: (property_identifier) @name)
    (#eq? @_mod "module")
    (#eq? @name "exports")) @def)

; Pattern 11: module.exports.X = ... (named CJS export) → Constant.
; The `exports.X = ...` shorthand is pattern 6; this is its qualified form.
(expression_statement
  (assignment_expression
    left: (member_expression
      object: (member_expression
        object: (identifier) @_mod
        property: (property_identifier) @_exp)
      property: (property_identifier) @name)
    (#eq? @_mod "module")
    (#eq? @_exp "exports")) @def)
