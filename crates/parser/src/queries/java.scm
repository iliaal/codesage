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

; Pattern 6: class / interface field -> Constant (no Field kind in protocol).
; Multi-declarator fields (`String x, y, z;`) match this pattern once per
; variable_declarator child — extract.rs's dedup key includes the @name node
; so all three declarators emit as separate symbols rather than collapsing
; to the first.
(field_declaration
  declarator: (variable_declarator
    name: (identifier) @name)) @def

; Pattern 7: @interface MyAnnotation -> Interface (annotation type definition).
;   The body's `String value() default "x"` elements are method-shape and
;   match pattern 4 (method_declaration) under the annotation_type_body, so
;   they surface as Method symbols.
(annotation_type_declaration
  name: (identifier) @name) @def
