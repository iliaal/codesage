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

; Pattern 3: Out-of-line method definition → Method; captures the full `Foo::bar`.
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

; Pattern 6: Class → Class; require a body to exclude forward declarations.
(class_specifier
  name: (type_identifier) @name
  body: (field_declaration_list)) @def

; Pattern 7: Struct -> Struct (forward declarations excluded, as above)
(struct_specifier
  name: (type_identifier) @name
  body: (field_declaration_list)) @def

; Pattern 8: Union -> Struct (closest available kind; fwd decls excluded)
(union_specifier
  name: (type_identifier) @name
  body: (field_declaration_list)) @def

; Pattern 9: Enum / enum class → Enum; require a body to exclude opaque declarations.
(enum_specifier
  name: (type_identifier) @name
  body: (enumerator_list)) @def

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

; Pattern 19: in-class method definition → Function, refined to Method.
; Member names use field_identifier rather than pattern 0's identifier.
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

; Pattern 23: file-scope const/constexpr object with an initializer → Constant.
; Anchored to translation_unit so function-local consts stay out of the index,
; and gated on the qualifier text because `volatile`, `mutable` and the other
; cv-qualifiers are type_qualifier nodes too. Pointer declarators are
; deliberately excluded: in `const char *p` the pointer itself is mutable.
(translation_unit
  (declaration
    (type_qualifier) @_qual
    declarator: (init_declarator
      declarator: (identifier) @name)
    (#any-of? @_qual "const" "constexpr" "constinit")) @def)

; Pattern 24: namespace-scope (and `extern "C" {}`-scope) const → Constant.
(declaration_list
  (declaration
    (type_qualifier) @_qual
    declarator: (init_declarator
      declarator: (identifier) @name)
    (#any-of? @_qual "const" "constexpr" "constinit")) @def)

; Pattern 25: in-class const/constexpr member with an in-class initializer →
; Constant. `static` is not required, so a non-static const field with a default
; initializer is a Constant too; a member initialized only in a constructor body
; has no default_value and stays out. A field_declaration occurs only in a class,
; struct or union body, so no anchor is needed, and requiring a bare
; field_identifier declarator keeps member function declarations out.
(field_declaration
  (type_qualifier) @_qual
  declarator: (field_identifier) @name
  default_value: (_)
  (#any-of? @_qual "const" "constexpr" "constinit")) @def
