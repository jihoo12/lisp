---
title : Embedding & Library Usage
sidebar:
  order: 3
---

pi-lisp can be used as a Rust library crate to embed a Lisp interpreter in your own application. All public API is available through the `pilisp` crate.

---

## Adding the dependency

```toml
[dependencies]
pilisp = { git = "https://github.com/pi-lisp/pi-lisp" }
```

---

## Quick start

```rust
use pilisp::PiLisp;

fn main() {
    let mut lisp = PiLisp::new();
    let result = lisp.eval("(+ 1 2 3)").unwrap();
    println!("{:?}", result); // => 6
}
```

`PiLisp::new()` creates a new garbage-collected heap and a global environment with all builtins (arithmetic, lists, strings, CLOS, C FFI, assembly, etc.) pre-registered.

---

## Evaluating code

### Multi-expression evaluation

`eval` parses and evaluates every top-level expression in the source string, returning the value of the last one (or `()` if the source is empty):

```rust
let result = lisp.eval("
    (define square (lambda (x) (* x x)))
    (square 9)
").unwrap();
// => Expr::Int(81)
```

### Parsing and evaluating separately

```rust
use pilisp::{parse_all, eval};

let exprs = parse_all("(+ 1 2) (* 3 4)")?;
for expr in exprs {
    let result = eval(&expr, lisp.env(), lisp.heap())?;
    println!("{:?}", result);
}
```

### Tokenizing source text

```rust
use pilisp::tokenize;

let tokens = tokenize("(+ 1 2)").unwrap();
// => [LParen, Atom("+"), Atom("1"), Atom("2"), RParen]
```

---

## The `PiLisp` API

```rust
impl PiLisp {
    /// Create a new interpreter with a GC heap and global environment.
    pub fn new() -> Self;

    /// Evaluate Lisp source, returning the last expression's value.
    pub fn eval(&mut self, src: &str) -> Result<Expr, String>;

    /// Evaluate a pre-parsed expression tree.
    pub fn eval_expr(&mut self, expr: &Expr) -> Result<Expr, String>;

    /// Parse source into a list of expressions.
    pub fn parse(&self, src: &str) -> Result<Vec<Expr>, String>;

    /// Access the garbage-collected heap.
    pub fn heap(&mut self) -> &mut Heap;

    /// Access the global environment handle.
    pub fn env(&self) -> Env;
}
```

---

## Working with `Expr` values

The core expression type `Expr` represents all Lisp values:

```rust
pub enum Expr {
    Symbol(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Complex(f64, f64),
    Str(String),
    List(Vec<Expr>),
    Func(BuiltinFn),
    Lambda(Vec<String>, Box<Expr>, GcHandle),
    Macro(Vec<String>, Box<Expr>),
}
```

Pattern-match to inspect results:

```rust
use pilisp::Expr;

match result {
    Expr::Int(n) => println!("integer: {}", n),
    Expr::Float(f) => println!("float: {}", f),
    Expr::List(items) => println!("list of {} elements", items.len()),
    Expr::Str(s) => println!("string: {}", s),
    other => println!("{:?}", other),
}
```

### Constructing expressions

Passing Lisp values programmatically:

```rust
use pilisp::Expr;

let sum = Expr::List(vec![
    Expr::Symbol("+".into()),
    Expr::Int(1),
    Expr::Int(2),
]);

lisp.eval_expr(&sum).unwrap(); // => Expr::Int(3)
```

---

## Advanced: working with modules directly

All internal modules are public and can be used independently:

| Module | Key exports | Purpose |
|--------|-------------|---------|
| `pilisp::eval` | `eval`, `with_import_base` | Evaluation and `import` base path |
| `pilisp::reader` | `tokenize`, `parse`, `parse_all` | Scanner and parser |
| `pilisp::expr` | `Expr`, `Env`, `BuiltinFn` | Core types |
| `pilisp::gc` | `Heap`, `GcHandle`, `EnvData` | Garbage collector |
| `pilisp::builtins` | `global_env` | Global environment factory |
| `pilisp::vm` | `vm_eval`, `aot_compile_file`, `cache_stats` | Bytecode VM and AOT |
| `pilisp::macros` | `expand_macro`, `eval_quasiquote` | Macro expansion |

### Custom environments

Create a fresh environment with only the builtins you need:

```rust
use pilisp::{Heap, global_env};

let mut heap = Heap::new();
let env = global_env(&mut heap);
```

Or build a clean environment with no builtins:

```rust
use pilisp::{Expr, new_env, env_set};

let mut heap = Heap::new();
let env = new_env(&mut heap, None);
heap.env_set(env, "my-var".into(), Expr::Int(42));
```

### `Heap` and GC

The `Heap` uses mark-and-sweep garbage collection. When embedding:

- Hold the heap alive for the duration of your interpreter session
- `Env` is a `GcHandle` (a `Copy` index into the heap) — no reference counting
- The global environment must be a GC root (keep it reachable)
- `env_set` and `env_get` are available directly on the heap

```rust
// Manually managing GC roots
use pilisp::{Heap, Expr, global_env};

let mut heap = Heap::new();
let root = heap.alloc(Expr::new_env(None));    // root frame
let child = heap.alloc(Expr::new_env(Some(root))); // child
heap.root(root); // keep root alive across collections
```

---

## AOT compilation from Rust

Compile a `.pi` file to bytecode ahead-of-time:

```rust
use pilisp::{Heap, global_env, vm};

let mut heap = Heap::new();
let env = global_env(&mut heap);
vm::aot_compile_file("input.pi", "output.aot", env, &mut heap).unwrap();
```

Load cached bytecode at startup:

```rust
vm::aot_load_file("output.aot").unwrap();
```

---

## Thread safety

The `Heap` and garbage collector are **not** `Send` or `Sync`. Each interpreter session — `PiLisp` + its heap — is single-threaded. Use `std::thread::spawn` with a fresh `PiLisp::new()` per thread for parallel evaluation.

The builtin `thread-eval` and `parallel-eval` functions handle this internally by spawning worker threads with isolated global environments.

---

## C FFI from embedded Rust

Load shared libraries and call C functions:

```rust
use pilisp::{Heap, global_env, eval, Expr, parse_all};

let mut heap = Heap::new();
let env = global_env(&mut heap);
let src = "
    (define lib (lisp-dlopen \"libm.so.6\"))
    (define sqrt (lisp-dlsym lib \"sqrt\"))
    (ccall sqrt 9.0)
";
let exprs = parse_all(src).unwrap();
let result = eval(&exprs[2], env, &mut heap).unwrap();
// => Expr::Float(3.0)
```
