; Pattern 0: method call -> foo()
(method_invocation
  name: (identifier) @ref)

; Pattern 1: new Foo() -> Instantiation
(object_creation_expression
  type: (type_identifier) @ref)

; Pattern 2: new Foo<T>() -> Instantiation
(object_creation_expression
  type: (generic_type
    (type_identifier) @ref))

; Pattern 3: new pkg.Foo() -> Instantiation
(object_creation_expression
  type: (scoped_type_identifier
    (type_identifier) @ref))

; Pattern 4: class Foo extends Bar -> Inheritance
(superclass
  (type_identifier) @ref)

; Pattern 5: class Foo extends Bar<T> -> Inheritance
(superclass
  (generic_type
    (type_identifier) @ref))

; Pattern 6: class Foo implements Bar -> Inheritance
(super_interfaces
  (type_list
    (type_identifier) @ref))

; Pattern 7: class Foo implements Bar<T> -> Inheritance
(super_interfaces
  (type_list
    (generic_type
      (type_identifier) @ref)))

; Pattern 8: interface Foo extends Bar -> Inheritance
(extends_interfaces
  (type_list
    (type_identifier) @ref))

; Pattern 9: interface Foo extends Bar<T> -> Inheritance
(extends_interfaces
  (type_list
    (generic_type
      (type_identifier) @ref)))

; Pattern 10: import pkg.Type -> Import
(import_declaration
  (scoped_identifier) @ref)

; Pattern 11: import Type -> Import
(import_declaration
  (identifier) @ref)
