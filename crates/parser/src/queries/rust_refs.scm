; Pattern 0: use declaration with scoped path (use std::io::Read)
(use_declaration argument: (scoped_identifier) @ref)

; Pattern 1: use declaration with simple identifier (use SomeTrait)
(use_declaration argument: (identifier) @ref)

; Pattern 2: function call (simple name)
(call_expression function: (identifier) @ref)

; Pattern 3: function call (scoped path like module::func)
(call_expression function: (scoped_identifier) @ref)

; Pattern 4: macro invocation (simple name like println!)
(macro_invocation macro: (identifier) @ref)

; Pattern 5: macro invocation (scoped like std::println!)
(macro_invocation macro: (scoped_identifier) @ref)
