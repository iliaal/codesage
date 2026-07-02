; Pattern 0: Function declaration → Function
(function_declaration name: (identifier) @name) @def

; Pattern 1: Class declaration → Class
(class_declaration name: (type_identifier) @name) @def

; Pattern 2: Method definition → Method
(method_definition name: (property_identifier) @name) @def

; Pattern 3: Interface → Interface
(interface_declaration name: (type_identifier) @name) @def

; Pattern 4: Type alias → Constant (proxy)
(type_alias_declaration name: (type_identifier) @name) @def

; Pattern 5: Enum → Enum
(enum_declaration name: (identifier) @name) @def

; Pattern 6: Exported const/let → Constant
(export_statement declaration: (lexical_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 7: Top-level const/let → Constant
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 8: export default class X → Class
(export_statement value: (class name: (type_identifier) @name) @def)

; Pattern 9: abstract class → Class
(abstract_class_declaration name: (type_identifier) @name) @def

; Pattern 10: exported / default-exported abstract class → Class
(export_statement declaration: (abstract_class_declaration name: (type_identifier) @name) @def)

; Pattern 11: generator function (function* g()) → Function
(generator_function_declaration name: (identifier) @name) @def

; Pattern 12: top-level var → Constant
(program (variable_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 13: exported var → Constant
(export_statement declaration: (variable_declaration (variable_declarator name: (identifier) @name) @def))

; Pattern 14: abstract method signature (abstract m(): void;) → Method
(abstract_method_signature name: (property_identifier) @name) @def
