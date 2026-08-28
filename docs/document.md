---
title: Grammar Reference
sidebar:
  order: 1
---

A complete description of the reader syntax, expression types, and special forms recognised by the evaluator defined in `reader.rs`, `eval.rs`, and `macros.rs`.

---

## 1 · Lexical Syntax

Source text is scanned character-by-character by `tokenize()` in `reader.rs`. The scanner produces five kinds of token:

| Token | Pattern | Notes |
|-------|---------|-------|
| `LParen` | `(` | Opens a list form. |
| `RParen` | `)` | Closes a list form. |
| `Quote` | `'` | Reader shorthand; expands to `(quote …)`. |
| `Str(s)` | `"…"` | String literal. Supports `\n \t \r \" \\`; may span multiple lines. |
| `Atom(s)` | any other run of non-delimiter chars | Parsed in order: `#t`/`#f` → `Bool`; integer (valid `i64`) → `Int`; floating-point (valid `f64`) → `Float`; complex (ends with `i`) → `Complex`; otherwise → `Symbol`. |

**Whitespace** (spaces, tabs, newlines, carriage returns) is only a token separator — it carries no structural meaning.

**Line comments** begin with `;` and run to the end of the line.

```lisp
; this is a comment — everything after ; is ignored
'x              ; reader shorthand → (quote x)
"hello\nworld"  ; a two-line string literal
42              ; integer atom  → Expr::Int(42)
3.14            ; float atom    → Expr::Float(3.14)
#t              ; boolean atom  → Expr::Bool(true)
#f              ; boolean atom  → Expr::Bool(false)
1+2i            ; complex atom  → Expr::Complex(1.0, 2.0)
3i              ; pure imaginary → Expr::Complex(0.0, 3.0)
+i              ; imaginary unit → Expr::Complex(0.0, 1.0)
-i              ; negative imaginary unit → Expr::Complex(0.0, -1.0)
my-symbol       ; symbol atom   → Expr::Symbol("my-symbol")
```

> **Note:** Bare `i` is now parsed as a **symbol** rather than the imaginary unit, so it can be used as a variable name. Use `+i` or `-i` for the imaginary unit in complex contexts.

---

## 2 · Expression Types

After scanning, `parse()` in `reader.rs` builds an `Expr` tree. There are nine variants (defined in `expr.rs`):

| Variant | Source syntax | Self-evaluates? |
|---------|--------------|-----------------|
| `Int(i64)` | `42`, `-7`, `0` | ✅ yes |
| `Float(f64)` | `3.14`, `-1.5e2` | ✅ yes |
| `Complex(f64, f64)` | `1+2i`, `-3i`, `+i`, `-i` | ✅ yes |
| `Bool(bool)` | `#t`, `#f` | ✅ yes |
| `Str(String)` | `"hello"` | ✅ yes |
| `Symbol(String)` | `x`, `my-var`, `+` | ❌ looks up name in the current environment |
| `List(Vec<Expr>)` | `(f a b)`, `()` | ❌ evaluated as a call or special form; `()` is nil |
| `Func` | built-in native function | ✅ yes (returned as-is) |
| `Lambda` / `Macro` | created by `lambda` / `defmacro` | ✅ yes (returned as-is) |

**Grammar rule:**

```
expr ::= integer | float | complex | bool | string | symbol | 'expr | ( expr* )
```

---

## 3 · Special Forms

When a list's first element is one of the symbols below, `eval()` handles it specially — arguments are *not* evaluated before dispatch. The forms `if`, `begin`, `let`, and `tailcall` participate in the trampoline loop described in §4 and never consume an extra Rust stack frame in tail position.

---

### `quote` — special form
*Reader shorthand: `'expr`*

**Rule:** `(quote expr)`

Returns `expr` unevaluated. The reader expands `'x` into `(quote x)` automatically.

```lisp
'hello        ; ⇒ hello  (symbol, not looked up)
'(1 2 3)     ; ⇒ (1 2 3)  (list, not called)
```

---

### `quasiquote` / `unquote` / `unquote-splicing` — special form

**Rule:**

```
(quasiquote template)
within template: (unquote expr) | (unquote-splicing list-expr)
```

Produces a list template in which most sub-forms are left unevaluated, but `(unquote e)` splices the evaluated value of `e` in-place, and `(unquote-splicing e)` splices every element of the list returned by `e` into the surrounding list. Nesting is tracked via a *depth counter*.

```lisp
(define x 42)
`(the answer is ,x)    ; ⇒ (the answer is 42)

(define nums '(1 2 3))
`(a ,@nums b)           ; ⇒ (a 1 2 3 b)
```

---

### `if` — special form

**Rule:** `(if cond then [else])`

Evaluates `cond`. If truthy, evaluates and returns `then`; otherwise evaluates and returns `else` (or `()` / nil when `else` is omitted).

**Truthiness rules:** `#f` / `Bool(false)`, integer `0`, float `0.0`, the empty string `""`, and the empty list `()` are all falsy. Everything else — including every `Complex` — is truthy.

```lisp
(if (> x 0) "positive" "non-positive")
(if flag (do-something))   ; else branch is optional → returns ()
```

---

### `define` — special form

**Rule:** `(define symbol expr)`

Evaluates `expr` and binds the result to `symbol` in the *current* environment. The form returns the new value.

```lisp
(define pi 3.14159)
(define square (lambda (x) (* x x)))
```

---

### `lambda` — special form

**Rule:** `(lambda (param*) body)`

```lisp
(define add (lambda (a b) (+ a b)))
(add 3 4)   ; ⇒ 7

; Closures work as expected
(define make-adder
  (lambda (n) (lambda (x) (+ x n))))
(define add5 (make-adder 5))
(add5 10)    ; ⇒ 15
```

---

### `defmacro` — special form

**Rule:** `(defmacro name (param*) body)`

Defines a macro. At the *call site*, argument expressions are passed unevaluated and substituted textually into `body` (hygienic substitution via `expand_macro()` in `macros.rs`). The expanded form is then evaluated in the caller's environment.

Macros differ from lambdas in two ways: arguments are not pre-evaluated, and macro expansion happens at call time rather than producing a closure.

```lisp
(defmacro my-and (a b)
  `(if ,a ,b 0))

(my-and 1 2)   ; expands to (if 1 2 0)  ⇒ 2
(my-and 0 2)   ; expands to (if 0 2 0)  ⇒ 0
```

---

### `defstruct` — special form

**Rule:** `(defstruct name field*)`

Defines a structure type. Generates:
- **Constructor** `(name val1 val2 …)` — returns a tagged list `(struct name val1 val2 …)`.
- **Predicate** `(name? obj)` — returns `#t` if `obj` is a struct of this type.
- **Accessor** `(name-field obj)` — returns the value of `field` for the given struct instance.

`defstruct` always falls back to the tree-walker; it is not compiled to bytecode.

```lisp
(defstruct point x y)
(define p (point 3 4))
(point? p)      ; ⇒ #t
(point-x p)     ; ⇒ 3
(point-y p)     ; ⇒ 4
```

---

### `import` — special form

**Rule:** `(import "path")`

Reads another pi-lisp source file and evaluates each top-level form in the current environment. Definitions, macros, and other side effects from the imported file are therefore visible after the import. The form returns the last value produced by the imported file, or `()` if the file is empty.

Relative paths are resolved against the directory of the file doing the import when available, so a file can import a sibling with a simple relative path.

```lisp
; main.pi
(import "math.pi")
(square 9)   ; uses a definition from math.pi
```

> **VM note:** `import` always falls back to the tree-walker and is not compiled to bytecode.

---

### `begin` — special form

**Rule:** `(begin expr+)`

Evaluates each sub-expression in order and returns the value of the last one. Useful for sequencing side-effecting forms.

```lisp
(begin
  (define x 1)
  (define y 2)
  (+ x y))          ; ⇒ 3
```

---

### `let` — special form

**Rule:** `(let ((name expr)*) body*)`

Creates a new child environment, evaluates each binding's `expr` in the *outer* environment (not the new one — bindings cannot reference each other), and binds the results. Then evaluates each `body` form in the new environment and returns the last result.

```lisp
(let ((a 3)
      (b 4))
  (+ a b))           ; ⇒ 7
```

---

### `for` — special form

**Rule:** `(for var arg body*)`

Iterates over a numeric range or a list, binding `var` on each iteration and evaluating `body*` for side effects. Always returns `()`.

**Dispatch** depends on the number of arguments and the runtime types of the middle arguments:

| Shape | Semantics |
|-------|-----------|
| `(for var coll body*)` — **4 elements total** | List iteration: evaluate `coll`, bind `var` to each element in order, run `body*` |
| `(for var start end body*)` — **5+ elements**, and both `start` and `end` evaluate to integers | Numeric range: `var` runs from `start` up to but **not including** `end`, stepping by `1` |
| `(for var arg body*)` — **5+ elements**, but `start`/`end` are not both numbers | List iteration: `arg` is the collection, `body*` starts at the fourth argument |

The loop variable and any internal state live in a child environment that is discarded when the loop finishes — `var` is not visible outside the form.

```lisp
; numeric: prints 0 1 2 3 4, then ⇒ ()
(for i 0 5 (print i))

; list: prints 1 2 3, then ⇒ ()
(for x '(1 2 3) (print x))

; multi-statement body
(for n 1 4
  (print n)
  (print (* n n)))   ; ⇒ ()

; dynamic bounds (evaluated at runtime)
(define start 0)
(define end 3)
(for j start end (print j))   ; prints 0 1 2
```

> **VM note:** when compiled with the `--features vm` bytecode backend, literal numeric bounds such as `(for i 0 5 …)` and four-element list forms are compiled to jump loops. Non-literal five-or-more-argument forms (e.g. `(for j start end …)`) fall back to the tree-walker so runtime dispatch can choose numeric vs list semantics.

---

### `tailcall` — special form

**Rule:** `(tailcall f arg*)`

Evaluates `f` and each `arg` left-to-right, then performs the call as a trampoline step — the body of a lambda `f` is evaluated on the *next iteration* of the `eval` loop rather than via a new Rust stack frame. This makes arbitrarily deep tail recursion use O(1) stack space.

**When is it needed?** Calls in tail position inside `if`, `begin`, `let`, and lambda bodies are already trampolined automatically — so for direct self-recursion you usually do not need `tailcall`. It becomes necessary for **mutual recursion**, where the optimizer cannot statically determine that a call to *another* function is in the tail position of the *current* lambda's frame.

```lisp
; Direct recursion — tailcall optional but harmless
(define count-down
  (lambda (n)
    (if (= n 0)
        "done"
        (tailcall count-down (- n 1)))))

; Mutual recursion — tailcall required for stack safety
(define is-even?
  (lambda (n)
    (if (= n 0) 1 (tailcall is-odd?  (- n 1)))))
(define is-odd?
  (lambda (n)
    (if (= n 0) 0 (tailcall is-even? (- n 1)))))

(is-even? 1000000)   ; ⇒ 1  (no stack overflow)
```

> **Builtin functions** are opaque Rust closures and are always called immediately — `tailcall` has no additional effect on them beyond normal argument evaluation.

---

### `set!` — special form

**Rule:** `(set! name expr)`

Evaluates `expr` and assigns the result to an existing variable binding `name`, walking up the environment chain to find it. The form returns `()`.

Unlike `define`, which creates a new binding in the current environment, `set!` mutates an existing binding and raises an error if no such binding exists.

```lisp
(define x 10)
(set! x 20)   ; x is now 20
(set! y 30)   ; error: y is not defined
```

---

### `ccall` — special form

**Rule:** `(ccall fn-ptr ret-type (arg-type val) …)`

Calls a C function pointer loaded via `lisp-dlsym`. The first argument is the function pointer (integer). The second is a return-type keyword (`:int`, `:float`, `:void`, or `:ptr`). Remaining arguments are typed argument pairs `(:type expr)` where `expr` is evaluated.

`ccall` is a special form because its typed argument pairs must not be evaluated as function applications — `(:ptr p)` is metadata, not a call to `:ptr`. It always falls back to the tree-walker.

```lisp
(ccall sqrt :float 9.0)       ; ⇒ 3.0
(ccall sum-point :int (:ptr p))
```

See [`docs/builtins/cffi.md`](builtins/cffi.md) for full details.

---

## 4 · Function Application

Any list that does not match a special form is treated as a function call:

**Rule:** `(operator arg*)`

1. Evaluate `operator` — must produce a `Func` or `Lambda`.
2. Evaluate each `arg` left-to-right.
3. Dispatch via `apply_step()` in `eval.rs`:
   - `Func(f)` — calls the native Rust closure `f` immediately and wraps the result in `Step::Value`.
   - `Lambda(params, body, env)` — creates a new child frame parented to the lambda's *closure* env (lexical scoping), binds arguments to parameters, and returns `Step::TailCall { expr: body, env: call_frame }`. The trampoline loop in `eval` then evaluates the body on the next iteration — no new Rust stack frame is allocated.

### Trampoline loop

`eval()` is implemented as a `loop` over a `(cur_expr, cur_env)` cursor. Each iteration calls the private `eval_step()`, which returns either:

| Step variant | Meaning |
|---|---|
| `Step::Value(v)` | Fully evaluated — the loop returns `v`. |
| `Step::TailCall { expr, env }` | Tail position — the loop updates the cursor and iterates without growing the stack. |

The forms that produce `TailCall` are: the selected branch of `if`, the last expression of `begin` and `let`, every lambda application, the result of macro expansion, and explicit `(tailcall …)` calls.

> **Macro vs. function:** if `operator` resolves to a `Macro`, the call is intercepted *before* argument evaluation — raw unevaluated expressions are substituted into the macro body, and the expanded form is returned as a `TailCall` so it is evaluated on the next trampoline iteration.