; Pattern 0: import statement (captures the module source string)
(import_statement source: (string) @ref)

; Pattern 1: require("module") -- captures the module string
(call_expression
  function: (identifier) @_fn
  arguments: (arguments (string) @ref)
  (#eq? @_fn "require"))

; Pattern 2: function call (simple identifier)
(call_expression function: (identifier) @ref)

; Pattern 3: function call (member expression like obj.method())
(call_expression function: (member_expression property: (property_identifier) @ref))

; Pattern 4: re-export (export { x } from "./mod" / export * from "./mod")
(export_statement source: (string) @ref)

; Pattern 5: class inheritance (class Foo extends Bar) -- JS heritage form
(class_heritage (identifier) @ref)

; Pattern 6: instantiation (new Foo())
(new_expression constructor: (identifier) @ref)

; Patterns 7-9: local, named, and namespace import bindings.
; Module strings alone miss uses such as `instanceof Foo`.
(import_statement (import_clause (identifier) @ref))
(import_statement (import_clause (named_imports (import_specifier name: (identifier) @ref))))
(import_statement (import_clause (namespace_import (identifier) @ref)))

; Patterns 10-11: re-exported names (with or without `from`) and require destructuring.
(export_statement (export_clause (export_specifier name: (identifier) @ref)))
(variable_declarator
  name: (object_pattern (shorthand_property_identifier_pattern) @ref)
  value: (call_expression
    function: (identifier) @_req
    arguments: (arguments (string)))
  (#eq? @_req "require"))

; Pattern 12: aliased CommonJS destructuring, `const { a: localA } = require(...)`.
; The shorthand form above is a `shorthand_property_identifier_pattern`; an
; alias is a `pair_pattern`, whose KEY names the exported symbol.
(variable_declarator
  name: (object_pattern (pair_pattern key: (property_identifier) @ref))
  value: (call_expression
    function: (identifier) @_req
    arguments: (arguments (string)))
  (#eq? @_req "require"))

; Patterns 13-14: value destructuring; references.rs requires `@rhs` to be imported
; so arbitrary object properties do not become module references.
(variable_declarator
  name: (object_pattern (shorthand_property_identifier_pattern) @ref)
  value: (identifier) @rhs)
(variable_declarator
  name: (object_pattern (pair_pattern key: (property_identifier) @ref))
  value: (identifier) @rhs)

; Pattern 15: require-bound local, added to the receiver allowlist in references.rs.
(variable_declarator
  name: (identifier) @ref
  value: (call_expression
    function: (identifier) @_req
    arguments: (arguments (string)))
  (#eq? @_req "require"))

; Pattern 16: module-member access, gated by imported `@rhs` in references.rs.
; Skip callees there to avoid duplicating pattern 3; retain `CancelToken` in
; `axios.CancelToken.source()`.
(member_expression
  object: (identifier) @rhs
  property: (property_identifier) @ref)

; Pattern 17: local initialized from a require member, e.g. `require('axios').default`.
(variable_declarator
  name: (identifier) @ref
  value: (member_expression
    object: (call_expression
      function: (identifier) @_req
      arguments: (arguments (string)))
    property: (property_identifier))
  (#eq? @_req "require"))
