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
