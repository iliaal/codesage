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

; Pattern 7: Namespace → Namespace
(namespace_definition name: (namespace_name) @name) @def
