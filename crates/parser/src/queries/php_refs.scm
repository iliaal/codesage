; Pattern 0: namespace use (use App\Models\User)
(namespace_use_declaration (namespace_use_clause (qualified_name) @ref))

; Pattern 1: use declaration (use SomeTrait)
(use_declaration (name) @ref)

; Pattern 2: function call
(function_call_expression function: (name) @ref)

; Pattern 3: object creation (new ClassName)
(object_creation_expression (name) @ref)

; Pattern 4: static method call (Class::method)
(scoped_call_expression scope: (name) @ref)

; Pattern 5: static method call method name (Class::method)
(scoped_call_expression name: (name) @ref)

; Pattern 6: instance method call ($object->method)
(member_call_expression name: (name) @ref)

; Pattern 7: nullsafe method call ($object?->method)
(nullsafe_member_call_expression name: (name) @ref)

; Pattern 8: class extends
(class_declaration (base_clause (name) @ref))

; Pattern 9: class implements
(class_declaration (class_interface_clause (name) @ref))

; Pattern 10: trait use in class body
(use_declaration (name) @ref)
