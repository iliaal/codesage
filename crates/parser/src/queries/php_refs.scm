; Pattern 0: namespace use (use App\Models\User)
(namespace_use_declaration (namespace_use_clause (qualified_name) @ref))

; Pattern 1: function call
(function_call_expression function: (name) @ref)

; Pattern 2: object creation (new ClassName)
(object_creation_expression (name) @ref)

; Pattern 3: static method call (Class::method)
(scoped_call_expression scope: (name) @ref)

; Pattern 4: static method call method name (Class::method)
(scoped_call_expression name: (name) @ref)

; Pattern 5: instance method call ($object->method)
(member_call_expression name: (name) @ref)

; Pattern 6: nullsafe method call ($object?->method)
(nullsafe_member_call_expression name: (name) @ref)

; Pattern 7: class extends
(class_declaration (base_clause (name) @ref))

; Pattern 8: class implements
(class_declaration (class_interface_clause (name) @ref))

; Pattern 9: trait use in class body (use SomeTrait;)
(use_declaration (name) @ref)

; Pattern 10: parameter type hint (function f(PDO $db))
(simple_parameter type: (named_type [(name) (qualified_name)] @ref))

; Pattern 11: constructor-promoted property type hint (public function __construct(private PDO $db))
(property_promotion_parameter type: (named_type [(name) (qualified_name)] @ref))

; Pattern 12: function return type hint (function f(): PDO)
(function_definition return_type: (named_type [(name) (qualified_name)] @ref))

; Pattern 13: method return type hint (public function f(): PDO)
(method_declaration return_type: (named_type [(name) (qualified_name)] @ref))

; Pattern 14: group use (use App\Models\{User, Post};): one ref per clause;
; the declaration's base namespace_name is prepended in references.rs.
(namespace_use_declaration
  (namespace_use_group
    (namespace_use_clause [(name) (qualified_name)] @ref)))

; Patterns 15-18: class types inside composite type hints, in any position that
; takes a type (parameters, promoted properties, typed properties, function,
; method, closure and arrow-function returns). Each class name is its own row.
; Pattern 15: nullable (?Foo)
(optional_type (named_type [(name) (qualified_name)] @ref))

; Pattern 16: union (Foo|Bar)
(union_type (named_type [(name) (qualified_name)] @ref))

; Pattern 17: intersection (Foo&Bar), including a DNF group's (Foo&Bar)
(intersection_type (named_type [(name) (qualified_name)] @ref))

; Pattern 18: DNF top level ((Foo&Bar)|Baz): the bare Baz arm
(disjunctive_normal_form_type (named_type [(name) (qualified_name)] @ref))

; Pattern 19: typed property (private Foo $p)
(property_declaration type: (named_type [(name) (qualified_name)] @ref))

; Pattern 20: variadic parameter type (Foo ...$xs)
(variadic_parameter type: (named_type [(name) (qualified_name)] @ref))

; Pattern 21: closure return type (function (): Foo {})
(anonymous_function return_type: (named_type [(name) (qualified_name)] @ref))

; Pattern 22: arrow-function return type (fn(): Foo => ...)
(arrow_function return_type: (named_type [(name) (qualified_name)] @ref))
