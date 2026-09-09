; Pattern 0: Function (simple declarator) → Function
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @def

; Pattern 1: Function (pointer return) → Function
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (identifier) @name))) @def

; Pattern 2: Macro-wrapped function e.g. PHP_FUNCTION(name) { ... } → Function
(function_definition
  declarator: (parenthesized_declarator
    (identifier) @name)) @def

; Pattern 3: Struct → Struct
(struct_specifier name: (type_identifier) @name) @def

; Pattern 4: Enum → Enum
(enum_specifier name: (type_identifier) @name) @def

; Pattern 5: Typedef → Constant (used as a proxy for typedef)
(type_definition
  declarator: (type_identifier) @name) @def

; Pattern 6: Macro (#define) → Macro
(preproc_def name: (identifier) @name) @def

; Pattern 7: Function (double-pointer return, e.g. char **f()) → Function
(function_definition
  declarator: (pointer_declarator
    declarator: (pointer_declarator
      declarator: (function_declarator
        declarator: (identifier) @name)))) @def

; Pattern 8: file-scope const object with an initializer → Constant.
; Anchored to translation_unit so function-local consts stay out of the index,
; and gated on the qualifier text because `volatile` and `_Atomic` are
; type_qualifier nodes too. Pointer declarators are deliberately excluded: in
; `const char *p` the pointer itself is mutable.
(translation_unit
  (declaration
    (type_qualifier) @_qual
    declarator: (init_declarator
      declarator: (identifier) @name)
    (#any-of? @_qual "const" "constexpr")) @def)
