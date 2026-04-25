; Pattern 0: #include <system>
(preproc_include path: (system_lib_string) @ref)

; Pattern 1: #include "local"
(preproc_include path: (string_literal) @ref)

; Pattern 2: bare call -> foo()
(call_expression function: (identifier) @ref)

; Pattern 3: qualified call -> ns::foo()
(call_expression function: (qualified_identifier) @ref)

; Pattern 4: member call -> obj.foo() / obj->foo()
(call_expression function: (field_expression field: (field_identifier) @ref))

; Pattern 5: template-function call -> foo<T>()
(call_expression function: (template_function name: (identifier) @ref))

; Pattern 6: new T() -> Instantiation
(new_expression type: (type_identifier) @ref)

; Pattern 7: new ns::T() -> Instantiation
(new_expression type: (qualified_identifier) @ref)

; Pattern 8: new T<U>() -> Instantiation
(new_expression type: (template_type name: (type_identifier) @ref))

; Pattern 9: class Foo : Bar { ... } -> Inheritance (bare base)
(base_class_clause (type_identifier) @ref)

; Pattern 10: class Foo : ns::Bar { ... } -> Inheritance (qualified base)
(base_class_clause (qualified_identifier) @ref)

; Pattern 11: class Foo : Bar<T> { ... } -> Inheritance (template base)
(base_class_clause (template_type name: (type_identifier) @ref))

; Pattern 12: using ns::name; -> Import
(using_declaration (qualified_identifier) @ref)

; Pattern 13: using namespace ns; -> Import
(using_declaration (identifier) @ref)
