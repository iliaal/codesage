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

; Patterns 7-9: the imported BINDING names. Pattern 0 captures only the module
; specifier, so a file that imports a symbol and then uses it in a form no other
; pattern captures — `Foo.staticMethod()` (pattern 3 sees only `staticMethod`),
; `x instanceof Foo`, `obj.Foo = Foo` — produced no reference row naming that
; symbol at all, and the file dropped out of its dependents.
; Appended at the end: pattern indices are positional and map to kinds by
; number in references.rs, so inserting above would renumber every kind.
(import_statement (import_clause (identifier) @ref))
(import_statement (import_clause (named_imports (import_specifier name: (identifier) @ref))))
(import_statement (import_clause (namespace_import (identifier) @ref)))

; Patterns 10-11: bindings that arrive by a route other than `import`.
; `export { x }` names the symbols it forwards, with or without a `from` clause
; — a barrel that re-exports a locally destructured binding has no source, and
; that is the only place those names appear. CommonJS destructuring names
; what it pulls off the module object; both previously recorded only the module
; string, so a barrel file or a `const { a } = require(...)` consumer named no
; symbol and vanished from that symbol's dependents.
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
