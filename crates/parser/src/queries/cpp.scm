; Shallow captures avoid expanding C++ declarator grammar during compilation.
; definition_capture shares the exact shape filters with fingerprint extraction.

; Pattern 0: Function definitions; out-of-line and in-class methods are refined.
(function_definition declarator: (_) @name) @def

; Pattern 1: Class -> Class; definition_capture requires a body.
(class_specifier name: (type_identifier) @name) @def

; Pattern 2: Struct -> Struct; definition_capture requires a body.
(struct_specifier name: (type_identifier) @name) @def

; Pattern 3: Union -> Struct (closest available kind); body required.
(union_specifier name: (type_identifier) @name) @def

; Pattern 4: Enum / enum class -> Enum; body required to exclude opaque declarations.
(enum_specifier name: (type_identifier) @name) @def

; Pattern 5: typedef -> Constant (parity with C).
(type_definition declarator: (type_identifier) @name) @def

; Pattern 6: using X = Y -> Constant.
(alias_declaration name: (type_identifier) @name) @def

; Pattern 7: C++20 concept -> Constant (no Concept kind in protocol).
(concept_definition name: (identifier) @name) @def

; Pattern 8: #define MACRO -> Macro.
(preproc_def name: (identifier) @name) @def

; Pattern 9: In-class callable declarations -> Method, initialized const fields
; -> Constant. Static is not required; constructor-only initialization stays out.
; Pointer declarators do not make const fields: const char *p is mutable.
(field_declaration declarator: (_) @name) @def

; Pattern 10: Initialized const/constexpr/constinit objects -> Constant, only at
; translation_unit or declaration_list scope. Function-local consts stay out.
(declaration
  declarator: (init_declarator
    declarator: (identifier) @name)) @def
