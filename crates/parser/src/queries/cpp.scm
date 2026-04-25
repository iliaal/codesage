; Pattern 0: Function (simple declarator) -> Function
; Refined to Method later if the def is inside a class/struct body.
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @def

; Pattern 1: Function with pointer return -> Function
(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (identifier) @name))) @def

; Pattern 2: Function with reference return (T& foo()) -> Function
(function_definition
  declarator: (reference_declarator
    (function_declarator
      declarator: (identifier) @name))) @def

; Pattern 3: Out-of-line method definition (void Foo::bar() {}) -> Method
; @name captures the whole `Foo::bar`; build_qualified_name splits on `::`.
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier) @name)) @def

; Pattern 4: Destructor (~Foo()) -> Method
(function_definition
  declarator: (function_declarator
    declarator: (destructor_name) @name)) @def

; Pattern 5: Operator overload -> Method (refined from Function via parent walk)
(function_definition
  declarator: (function_declarator
    declarator: (operator_name) @name)) @def

; Pattern 6: Class -> Class (matches both top-level and template-wrapped)
(class_specifier
  name: (type_identifier) @name) @def

; Pattern 7: Struct -> Struct
(struct_specifier
  name: (type_identifier) @name) @def

; Pattern 8: Union -> Struct (closest available kind)
(union_specifier
  name: (type_identifier) @name) @def

; Pattern 9: Enum / enum class -> Enum
(enum_specifier
  name: (type_identifier) @name) @def

; Pattern 10: typedef -> Constant (parity with C)
(type_definition
  declarator: (type_identifier) @name) @def

; Pattern 11: using X = Y; -> Constant (type alias)
(alias_declaration
  name: (type_identifier) @name) @def

; Pattern 12: C++20 concept -> Constant (no Concept kind in protocol)
(concept_definition
  name: (identifier) @name) @def

; Pattern 13: #define MACRO -> Macro
(preproc_def
  name: (identifier) @name) @def

; Pattern 14: in-class method declaration (no body) -> Method
;   class Foo { void bar(); };
; In-class member names parse as field_identifier (not identifier).
(field_declaration
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @def

; Pattern 15: in-class operator declaration (no body, no ref return) -> Method
(field_declaration
  declarator: (function_declarator
    declarator: (operator_name) @name)) @def

; Pattern 16: in-class method declaration with reference return -> Method
;   class Foo { Foo& bar(); };
(field_declaration
  declarator: (reference_declarator
    (function_declarator
      declarator: (field_identifier) @name))) @def

; Pattern 17: in-class operator declaration with reference return -> Method
;   class Foo { Foo& operator=(...); };
(field_declaration
  declarator: (reference_declarator
    (function_declarator
      declarator: (operator_name) @name))) @def

; Pattern 18: in-class method declaration with pointer return -> Method
(field_declaration
  declarator: (pointer_declarator
    (function_declarator
      declarator: (field_identifier) @name))) @def

; Pattern 19: in-class method definition (with body) -> Function (refined to Method)
;   class Foo { void bar() { ... } };
; In-class member names parse as field_identifier (not identifier), so this is
; the method-definition counterpart of pattern 0.
(function_definition
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @def

; Pattern 20: in-class method definition with pointer return -> Function
(function_definition
  declarator: (pointer_declarator
    (function_declarator
      declarator: (field_identifier) @name))) @def

; Pattern 21: in-class method definition with reference return -> Function
(function_definition
  declarator: (reference_declarator
    (function_declarator
      declarator: (field_identifier) @name))) @def

; Pattern 22: in-class operator definition with reference return -> Function
(function_definition
  declarator: (reference_declarator
    (function_declarator
      declarator: (operator_name) @name))) @def
