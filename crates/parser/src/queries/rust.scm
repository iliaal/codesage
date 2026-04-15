; Pattern 0: Function → Function (also catches methods inside impl blocks)
(function_item name: (identifier) @name) @def

; Pattern 1: Struct → Struct
(struct_item name: (type_identifier) @name) @def

; Pattern 2: Enum → Enum
(enum_item name: (type_identifier) @name) @def

; Pattern 3: Trait → Trait
(trait_item name: (type_identifier) @name) @def

; Pattern 4: Type alias → Constant (proxy for typedef)
(type_item name: (type_identifier) @name) @def

; Pattern 5: Const → Constant
(const_item name: (identifier) @name) @def

; Pattern 6: Static → Constant
(static_item name: (identifier) @name) @def

; Pattern 7: Module → Module
(mod_item name: (identifier) @name) @def

; Pattern 8: Macro definition → Macro
(macro_definition name: (identifier) @name) @def
