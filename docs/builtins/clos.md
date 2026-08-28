---
title : CLOS Object System
sidebar:
  order: 11
---

A Common Lisp Object System (CLOS) for pi-lisp, implemented in `lib/clos.pi` with native Rust support from `src/builtins/clos.rs`.

Usage:

```lisp
(import "lib/clos.pi")
```

---

## Core forms

### `defclass`

Defines a new class with superclasses and slots.

```
(defclass name (super*) (slot*))
```

Each slot is a symbol or a list `(slot-name)`. Slot accessor methods are not automatically generated; use `slot-value` or `with-slots` instead.

```lisp
(defclass point () (x y))
```

### `defgeneric`

Declares a generic function.

```
(defgeneric name)
```

Creates a callable generic function and registers it for method dispatch.

```lisp
(defgeneric area)
```

### `defmethod`

Defines a primary method on a generic function with specializers.

```
(defmethod name (specializer*) body)
```

Each specializer is a symbol naming a class (e.g. `point`, `integer`, `string`), or a list `(var class)` for type-tagged parameters. `t` matches any type.

```lisp
(defmethod area ((p point))
  (* (slot-value p 'x) (slot-value p 'y)))

(defmethod area ((s string))
  (string-length s))
```

### `make-instance`

Creates a new instance of a class, with optional keyword-style initargs.

```
(make-instance class-name (:keyword value)*)
```

```lisp
(define p (make-instance 'point :x 3 :y 4))
```

---

## Method qualifiers

### `defmethod-before`

Defines a `:before` method — runs before the primary method.

```
(defmethod-before name (specializer*) body)
```

### `defmethod-after`

Defines an `:after` method — runs after the primary method.

```
(defmethod-after name (specializer*) body)
```

### `defmethod-around`

Defines an `:around` method — wraps the primary method and any `:before`/`:after` methods. The around method's body should use `call-next-method` to continue.

```
(defmethod-around name (specializer*) body)
```

Standard method combination order:

1. Most-specific `:around` method (if any)
2. `:before` methods (most-specific first)
3. Most-specific primary method
4. `:after` methods (most-specific last)

---

## Slot access

### `slot-value`

Reads a slot value from an instance.

```
(slot-value instance slot-name) → Expr
```

### `set-slot-value!`

Sets a slot value on an instance (returns a new instance — slots are immutable by default).

```
(set-slot-value! instance slot-name new-value) → Instance
```

### `with-slots`

Binds slot values to local variables within a body.

```
(with-slots (slot-name*) instance body)
```

```lisp
(with-slots (x y) p
  (+ x y))
```

### `with-accessors`

Binds accessor thunks to local variables within a body.

```
(with-accessors ((accessor-name slot-name)*) instance body)
```

---

## Introspection

### `class-of`

Returns the class name (as a symbol) of any value.

```
(class-of obj) → Symbol
```

| Object type | Class name |
|-------------|------------|
| Integer | `integer` |
| Float | `float` |
| Boolean | `boolean` |
| String | `string` |
| Symbol | `symbol` |
| Complex | `complex` |
| List | `list` |
| Empty list | `null` |
| CLOS instance | *class name* |
| Function / Lambda | `function` |
| Macro | `macro` |

### `subtypep`

Checks if one type is a subtype of another.

```
(subtypep child parent) → Bool
```

### `clos-instance?`

Returns `#t` if the argument is a CLOS instance.

```
(clos-instance? obj) → Bool
```

---

## Utility functions

### `gensym`

Generates a unique symbol.

```
(gensym) → Symbol
```

### `error`

Signals an error with a message string.

```
(error message) → ! (never returns)
```
