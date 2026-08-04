; TypeScript reference patterns. Patterns 0-5 mirror javascript_refs.scm;
; pattern 6 handles the TS-only `extends_clause` heritage node, which does not
; exist in the JavaScript grammar (so it cannot live in the shared JS file).
; The JS `class_heritage (identifier)` inheritance form is an impossible pattern
; in the TSX grammar (heritage is always wrapped in extends/implements clauses),
; so it is omitted here.

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

; Pattern 5: instantiation (new Foo())
(new_expression constructor: (identifier) @ref)

; Pattern 6: class inheritance (class Foo extends Bar) -- TS extends_clause form
(extends_clause value: (identifier) @ref)

; Patterns 7-9: the imported BINDING names. See javascript_refs.scm for why —
; the module specifier alone leaves `Foo.staticMethod()` / `instanceof Foo`
; users with no row naming the symbol.
(import_statement (import_clause (identifier) @ref))
(import_statement (import_clause (named_imports (import_specifier name: (identifier) @ref))))
(import_statement (import_clause (namespace_import (identifier) @ref)))

; Patterns 10-11: re-export and CommonJS destructuring bindings.
; See javascript_refs.scm — the module string alone leaves barrel files and
; `const { a } = require(...)` consumers naming no symbol.
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

; Patterns 13-14: destructuring a module's exports off a VALUE rather than a
; `require(...)` call. A barrel does `const { Foo } = axios;` after importing
; the default export, so the symbols it unwraps are named nowhere else in the
; file. Restricted to a bare identifier on the right: broadening to arbitrary
; expressions would bind every `const { data } = resp.body` to any symbol
; named `data`.
(variable_declarator
  name: (object_pattern (shorthand_property_identifier_pattern) @ref)
  value: (identifier))
(variable_declarator
  name: (object_pattern (pair_pattern key: (property_identifier) @ref))
  value: (identifier))
