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
