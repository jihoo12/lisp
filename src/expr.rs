use std::fmt;
use std::rc::Rc;

use crate::gc::{GcHandle, Heap};

/// Type alias for built-in functions.
pub type BuiltinFn = Rc<dyn Fn(&[Expr], &mut Heap) -> Result<Expr, String>>;

/// Core value/expression type for the Lisp evaluator.
///
/// # Environment representation
///
/// The old design used `Rc<RefCell<EnvData>>` for shared ownership and
/// `Weak<RefCell<EnvData>>` inside `Lambda` to break cycles.  This worked
/// but had two problems:
///
/// 1. **Cycle leaks** — if two closures mutually capture each other through
///    a shared env frame, `Weak` alone doesn't save you; the `Rc` counts
///    never reach zero.
/// 2. **No compaction / visibility** — the allocator has no idea which
///    frames are still live, so it can never report heap pressure or run a
///    proper collection.
///
/// The new design stores a plain `GcHandle` (a `usize` index into the
/// interpreter's `Heap`).  Because `GcHandle` is `Copy` and has *no
/// destructor*, there are no reference counts and no cycles at the Rust
/// level.  Liveness is determined by the GC's mark phase instead.
#[derive(Clone)]
pub enum Expr {
    Symbol(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// A complex number: real and imaginary parts as f64.
    Complex(f64, f64),
    /// A string literal, e.g. `"hello world"`.  Self-evaluating.
    Str(String),
    List(Vec<Expr>),
    /// A built-in function.  Still uses `Rc` because function pointers are
    /// not GC-managed (they hold no `EnvData` references).
    Func(BuiltinFn),
    /// A user-defined closure.
    ///
    /// Fields:
    /// * parameter names
    /// * body expression
    /// * `GcHandle` of the captured environment frame — **not** a `Weak`;
    ///   the GC keeps this frame alive as long as the `Lambda` is reachable.
    Lambda(Vec<String>, Box<Expr>, GcHandle),
    Macro(Vec<String>, Box<Expr>),
}

impl fmt::Debug for Expr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Expr::Symbol(s) => write!(f, "{}", s),
            Expr::Int(n) => write!(f, "{}", n),
            Expr::Float(n) => write!(f, "{}", n),
            Expr::Bool(b) => write!(f, "{}", if *b { "#t" } else { "#f" }),
            Expr::Complex(re, im) => {
                if *im == 0.0 {
                    write!(f, "{}", re)
                } else if *re == 0.0 {
                    write!(f, "{}i", im)
                } else if *im < 0.0 {
                    write!(f, "{}{}i", re, im)
                } else {
                    write!(f, "{}+{}i", re, im)
                }
            }
            Expr::Str(s) => write!(f, "{:?}", s),
            Expr::List(l) => {
                write!(f, "(")?;
                for (i, e) in l.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{:?}", e)?;
                }
                write!(f, ")")
            }
            Expr::Func(_) => write!(f, "<builtin>"),
            Expr::Lambda(..) => write!(f, "<lambda>"),
            Expr::Macro(..) => write!(f, "<macro>"),
        }
    }
}

pub fn is_truthy(e: &Expr) -> bool {
    match e {
        Expr::Bool(b) => *b,
        Expr::Int(n) => *n != 0,
        Expr::Float(n) => *n != 0.0,
        Expr::Complex(_, _) => true,
        Expr::Str(s) => !s.is_empty(),
        Expr::List(l) => !l.is_empty(),
        _ => true,
    }
}

// ── Environment helpers ───────────────────────────────────────────────────────
//
// Previously `Env` was `Rc<RefCell<EnvData>>` and `WeakEnv` was the weak
// counterpart.  Now both collapse to `GcHandle`; the `Heap` owns the data.
//
// The public type alias keeps call-sites short and makes future changes
// (e.g. swapping the GC implementation) a one-line edit.

/// A handle to a live environment frame on the GC heap.
pub type Env = GcHandle;

/// Allocate a new, empty environment frame with the given parent.
///
/// The returned `Env` (`GcHandle`) is immediately live; the caller is
/// responsible for registering it as a GC root before the next collection.
pub fn new_env(heap: &mut Heap, parent: Option<Env>) -> Env {
    heap.alloc(parent)
}

/// Look up `name` in `env`, walking the parent chain.
pub fn env_get(heap: &Heap, env: Env, name: &str) -> Result<Expr, String> {
    heap.env_get(env, name)
}

/// Bind `name` → `val` in the innermost frame of `env` (no parent walk).
pub fn env_set(heap: &mut Heap, env: Env, name: String, val: Expr) {
    heap.env_set(env, name, val);
}
