//! Bytecode representation for the pilisp VM (phase 1: data structures only).
//!
//! # Design notes
//!
//! ## `Value` vs `Expr`
//!
//! `Expr` cannot be used directly as a VM value because:
//! * `Expr::Func` wraps an `Rc<dyn Fn(...)>` which is not `Clone` in a
//!   straightforward way (it _is_ `Clone` via `Rc::clone`, but the trait
//!   bound on `dyn Fn` makes it awkward to use generically).
//! * `Expr::Lambda` carries a `GcHandle` to a live GC frame, coupling the
//!   value type to the interpreter's GC lifecycle.
//! * `Expr::CubicalTerm` is intentionally opaque and uncompilable.
//!
//! `Value` is a clean, fully-owned, `Clone` type that the VM can move around
//! freely.  At the boundaries (loading constants, returning results) we convert
//! between `Value` and `Expr` via `expr_to_value` / `value_to_expr`.
//!
//! ## `Chunk` and `sub_chunks`
//!
//! Each lambda body is compiled into its own `Chunk` stored in the *parent*
//! chunk's `sub_chunks` vector.  `MakeFunc { code_offset, .. }` then refers to
//! that index.  This keeps all code in a contiguous, arena-like structure and
//! avoids heap-allocating separate code objects per lambda at compile time.
//!
//! ## Jump offsets
//!
//! `Jump` and `JumpIfFalse` carry absolute indices into `ops`.  The compiler
//! uses a two-pass technique within each chunk: it emits a placeholder with
//! offset `0` and then back-patches it once the target instruction is known
//! (see `compiler.rs`).

use crate::expr::{Expr, BuiltinFn};
use crate::gc::GcHandle;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

static CHUNK_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

// ── Value ─────────────────────────────────────────────────────────────────────

/// A VM-friendly mirror of `Expr`.
///
/// Every variant is `Clone` and owns its data — no `Rc`, no `GcHandle`.
/// Built-in functions are represented as a named string so they can be stored
/// as constants; the VM will look them up in the environment at runtime rather
/// than embedding the function pointer directly.
///
/// `CubicalTerm` is deliberately absent: the compiler rejects any expression
/// containing one before producing a `Value`.
#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Complex(f64, f64),
    Bool(bool),
    /// A string literal.
    Str(String),
    /// A symbol / identifier.  Used for quoted symbols and as the string form
    /// of unresolved names in constant data.
    Symbol(String),
    /// A proper list of values (the result of `quote` on a list, etc.).
    List(Vec<Value>),
    /// The empty list `()` — distinct from `List(vec![])` only for clarity;
    /// the two are semantically equivalent.
    Nil,
    /// A built-in function pointer.
    Builtin(BuiltinFn),
    /// A closure.
    Closure {
        params: Vec<String>,
        body_chunk: Rc<Chunk>,
        body_expr: Box<Expr>,
        env: GcHandle,
    },
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(n) => write!(f, "Int({})", n),
            Value::Float(n) => write!(f, "Float({})", n),
            Value::Complex(re, im) => write!(f, "Complex({}, {})", re, im),
            Value::Bool(b) => write!(f, "Bool({})", b),
            Value::Str(s) => write!(f, "Str({:?})", s),
            Value::Symbol(s) => write!(f, "Symbol({})", s),
            Value::List(items) => write!(f, "List({:?})", items),
            Value::Nil => write!(f, "Nil"),
            Value::Builtin(_) => write!(f, "Builtin(<builtin>)"),
            Value::Closure { params, .. } => write!(f, "Closure(<closure({})>)", params.join(", ")),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Complex(a, b), Value::Complex(c, d)) => a == c && b == d,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Symbol(a), Value::Symbol(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Nil, Value::Nil) => true,
            (Value::Builtin(a), Value::Builtin(b)) => Rc::ptr_eq(a, b),
            (
                Value::Closure {
                    params: p1,
                    env: e1,
                    body_chunk: c1,
                    ..
                },
                Value::Closure {
                    params: p2,
                    env: e2,
                    body_chunk: c2,
                    ..
                },
            ) => p1 == p2 && e1 == e2 && Rc::ptr_eq(c1, c2),
            _ => false,
        }
    }
}

// ── Expr ↔ Value conversion ───────────────────────────────────────────────────

/// Convert an `Expr` to a `Value`.
///
/// Returns `Err` if the expression contains anything that has no `Value`
/// equivalent: `Expr::Func`, `Expr::Lambda`, `Expr::Macro`, or
/// `Expr::CubicalTerm`.
pub fn expr_to_value(expr: &Expr) -> Result<Value, String> {
    match expr {
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Float(n) => Ok(Value::Float(*n)),
        Expr::Complex(re, im) => Ok(Value::Complex(*re, *im)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::Symbol(s) => Ok(Value::Symbol(s.clone())),
        Expr::List(items) => {
            let vs: Result<Vec<Value>, String> = items.iter().map(expr_to_value).collect();
            Ok(Value::List(vs?))
        }
        Expr::Func(f) => Ok(Value::Builtin(Rc::clone(f))),
        Expr::Lambda(params, body, env) => {
            let mut body_chunk = Chunk::new();
            body_chunk.emit(Op::LoadConst(Value::Nil));
            body_chunk.emit(Op::Return);
            Ok(Value::Closure {
                params: params.clone(),
                body_chunk: Rc::new(body_chunk),
                body_expr: body.clone(),
                env: *env,
            })
        }
        Expr::Macro(..) => Err("cannot convert macro to Value".into()),
    }
}

/// Convert a `Value` back to an `Expr`.
///
/// This is always total — every `Value` variant has an `Expr` counterpart.
pub fn value_to_expr(val: Value) -> Expr {
    match val {
        Value::Int(n) => Expr::Int(n),
        Value::Float(n) => Expr::Float(n),
        Value::Complex(re, im) => Expr::Complex(re, im),
        Value::Bool(b) => Expr::Bool(b),
        Value::Str(s) => Expr::Str(s),
        Value::Symbol(s) => Expr::Symbol(s),
        Value::List(items) => Expr::List(items.into_iter().map(value_to_expr).collect()),
        Value::Nil => Expr::List(vec![]),
        Value::Builtin(f) => Expr::Func(f),
        Value::Closure {
            params,
            body_expr,
            env,
            ..
        } => Expr::Lambda(params, body_expr, env),
    }
}

// ── Op ───────────────────────────────────────────────────────────────────────

/// A single VM instruction.
///
/// All operands are embedded directly in the variant (no separate constant
/// pool), which keeps the instruction set simple for phase 1.  A future
/// optimising VM could split large constants into a pool.
#[derive(Debug, Clone)]
pub enum Op {
    /// Push an immediate constant onto the value stack.
    LoadConst(Value),

    /// Look up a variable by name in the current environment and push the
    /// result.
    LoadVar(String),

    /// Pop the top of the stack and bind it to `name` in the current
    /// environment frame (equivalent to `define`).
    StoreVar(String),

    /// Pop the top of the stack and assign to `name`, walking the parent
    /// chain to find an existing binding (equivalent to `set!`).
    /// Returns an error if no existing binding is found.
    AssignVar(String),

    /// Unconditional jump to an absolute instruction index within this chunk.
    Jump(usize),

    /// Pop the top of the stack; if falsy, jump to the given absolute index.
    JumpIfFalse(usize),

    /// Return from the current call frame: pop and return the top of the stack.
    Return,

    /// Create a closure from `sub_chunks[code_offset]` and the current
    /// environment, then push it onto the stack.
    ///
    /// `code_offset` is an index into the *parent* `Chunk`'s `sub_chunks`
    /// vector, not an instruction offset.
    MakeFunc {
        /// Index into the parent chunk's `sub_chunks` array.
        code_offset: usize,
        /// The formal parameter names the closure expects.
        params: Vec<String>,
        /// The original expression body of the lambda.
        body_expr: Box<Expr>,
    },

    /// Call the function on top of the stack with `n` arguments (which are
    /// already on the stack below the function, pushed left-to-right).
    Call(usize),

    /// Like `Call`, but signals that this is in tail position.  The VM
    /// (phase 2) will reuse the current stack frame instead of allocating a
    /// new one.
    TailCall(usize),

    /// Fall back to tree-walker for this expression.
    TreeEval(Expr),

    /// Pop `n` values from the stack (right-to-left, so the first-pushed ends
    /// up at the front of the list) and push the resulting `Value::List`.
    MakeList(usize),

    /// Pop item and list, and push the prepended list.
    PrependList,
    /// Pop splice and acc-list, and splice the items.
    AppendSplice,
    /// Push an empty list.
    LoadNil,

    /// Discard the top of the stack (used to throw away the value of a
    /// non-tail expression in a `begin` sequence or a `define` side-effect).
    Pop,

    /// Push a new child environment frame onto the environment stack.
    /// The new frame's parent is the current frame.  After this instruction
    /// the VM's current `env` is the newly allocated child.
    PushEnv,

    /// Restore the parent of the current environment frame as the VM's
    /// current `env`.  Used to exit the scope created by `let` / `let*`.
    PopEnv,

    /// Bind the currently-executing closure to `name` in the current
    /// environment frame.  Emitted at the top of a named lambda's sub-chunk
    /// (produced by `(define name (lambda ...))`) so that recursive calls
    /// inside the body can resolve `name`.
    ///
    /// The VM implements this by re-wrapping the current frame's `chunk` +
    /// `params` into a fresh `VmValue::Closure` and storing it under `name`.
    StoreSelf(String),

    // ── Inline arithmetic (JIT fast-path) ──────────────────────────────────
    /// Pop two numbers, add them, push the result.
    /// Supports Int+Int→Int, Int+Float→Float, Float+Float→Float.
    NumAdd,
    /// Pop two numbers, subtract (second-from-top − top), push result.
    NumSub,
    /// Pop two numbers, multiply, push result.
    NumMul,
    /// Pop two numbers, divide (second-from-top ÷ top), push result.
    NumDiv,
    /// Pop two values, push Bool(top-of-stack equality).
    /// Numbers compare by value; strings by content; identity for others.
    NumEq,
    /// Pop two numbers, push Bool(second-from-top < top).
    NumLt,
    /// Pop two numbers, push Bool(second-from-top > top).
    NumGt,
}

// ── Chunk ────────────────────────────────────────────────────────────────────

/// A compiled unit of code: a flat instruction sequence plus any nested
/// lambda bodies (stored as sub-chunks so the parent can reference them by
/// index via `MakeFunc`).
#[derive(Debug, Clone)]
pub struct Chunk {
    /// The instruction sequence for this code unit.
    pub ops: Vec<Op>,
    /// Lambda bodies compiled from `lambda` forms inside this chunk.
    /// `Op::MakeFunc { code_offset, .. }` indexes into this vector.
    pub sub_chunks: Vec<Chunk>,
    /// Globally unique ID for this chunk.
    pub id: u64,
}

impl Chunk {
    /// Create a new, empty chunk.
    pub fn new() -> Self {
        Chunk {
            ops: Vec::new(),
            sub_chunks: Vec::new(),
            id: CHUNK_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Append an instruction and return its index.
    pub fn emit(&mut self, op: Op) -> usize {
        let idx = self.ops.len();
        self.ops.push(op);
        idx
    }

    /// Back-patch the jump target at `patch_idx` (must be a `Jump` or
    /// `JumpIfFalse` instruction) with the real destination `target`.
    pub fn patch_jump(&mut self, patch_idx: usize, target: usize) {
        match &mut self.ops[patch_idx] {
            Op::Jump(dest) | Op::JumpIfFalse(dest) => *dest = target,
            other => panic!(
                "patch_jump: instruction at {} is not a jump: {:?}",
                patch_idx, other
            ),
        }
    }

    /// Add a sub-chunk (a compiled lambda body) and return its index in
    /// `self.sub_chunks`.
    pub fn add_sub_chunk(&mut self, chunk: Chunk) -> usize {
        let idx = self.sub_chunks.len();
        self.sub_chunks.push(chunk);
        idx
    }
}

impl Default for Chunk {
    fn default() -> Self {
        Self::new()
    }
}
