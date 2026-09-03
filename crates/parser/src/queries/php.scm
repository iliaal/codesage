; Pattern 0: Function → Function
(function_definition name: (name) @name) @def

; Pattern 1: Class → Class
(class_declaration name: (name) @name) @def

; Pattern 2: Method → Method
(method_declaration name: (name) @name) @def

; Pattern 3: Trait → Trait
(trait_declaration name: (name) @name) @def

; Pattern 4: Interface → Interface
(interface_declaration name: (name) @name) @def

; Pattern 5: Enum → Enum
(enum_declaration name: (name) @name) @def

; Pattern 6: Constant → Constant
(const_declaration (const_element (name) @name)) @def

; Pattern 7: Enum case → Constant (PHP 8.1 `case Hearts;`)
; (Namespace declarations intentionally have no pattern: `extract_symbols`
; used to match them only to discard them via the Namespace kind. Leaving
; them out keeps pattern indices dense and the kind map total.)
(enum_case name: (name) @name) @def
