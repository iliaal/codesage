; Pattern 0: import path
(import_spec path: (interpreted_string_literal) @ref)

; Pattern 1: function call (simple name)
(call_expression function: (identifier) @ref)

; Pattern 2: function call (selector expression, e.g. pkg.Func or obj.Method)
(call_expression function: (selector_expression) @ref)
