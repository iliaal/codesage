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

; Pattern 6: method call (obj.method())
(call_expression function: (field_expression field: (field_identifier) @ref))

; Pattern 7: trait implemented for a type (impl Trait for Type)
(impl_item trait: (type_identifier) @ref)

; Pattern 8: type of an impl block (impl Type / impl Trait for Type)
(impl_item type: (type_identifier) @ref)

; Pattern 9: renamed use (use x::y as z) -- capture the source path
(use_declaration argument: (use_as_clause path: (_) @ref))

; Pattern 10: glob use (use a::b::*) -- capture the module path
(use_declaration argument: (use_wildcard [(scoped_identifier) (identifier)] @ref))

; Pattern 11: grouped use (use a::b::{X, Y}) -- one ref per name;
; the enclosing scoped_use_list path is prepended in references.rs.
(scoped_use_list list: (use_list [(identifier) (scoped_identifier)] @ref))

; Pattern 12: braced use without a leading path (use {a, b}) -- one ref per name
(use_declaration argument: (use_list [(identifier) (scoped_identifier)] @ref))
