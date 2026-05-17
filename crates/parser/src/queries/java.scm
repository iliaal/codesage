; Pattern 0: class Foo -> Class
(class_declaration
  name: (identifier) @name) @def

; Pattern 1: interface Foo -> Interface
(interface_declaration
  name: (identifier) @name) @def

; Pattern 2: enum Foo -> Enum
(enum_declaration
  name: (identifier) @name) @def

; Pattern 3: record Foo(...) -> Class (closest available kind)
(record_declaration
  name: (identifier) @name) @def

; Pattern 4: method declaration / definition -> Method
(method_declaration
  name: (identifier) @name) @def

; Pattern 5: constructor -> Method
(constructor_declaration
  name: (identifier) @name) @def

; Pattern 6: class / interface field -> Constant (no Field kind in protocol)
(field_declaration
  declarator: (variable_declarator
    name: (identifier) @name)) @def
