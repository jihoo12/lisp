//! Stack-based VM execution engine for pilisp (phase 2).
//!
//! # Architecture overview
//!
//! The VM is a classic stack machine with an explicit call-frame stack:
//!
//! ```text
//! VM {
//!   stack:  [ ... values ... ]   ← operand / value stack
//!   frames: [ CallFrame, ... ]   ← call-frame stack (grows with each call)
//!   heap:   &mut Heap            ← GC-managed environment frames
//! }
//! ```
//!
//! ## Stack layout for a call with N arguments
//!
//! Before `Call(N)` or `TailCall(N)` is dispatched the stack looks like:
//!
//! ```text
//! [ ... | callee | arg0 | arg1 | ... | argN-1 ]
//!                 ^--- stack_base of the new frame points here
//! ```
//!
//! The callee sits just below the arguments.  `stack_base` is set to
//! `stack.len() - N` so that locals (arguments + any temporaries pushed by the
//! body) are addressed relative to the base.
//!
//! ## Tail-call optimisation
//!
//! `TailCall(N)` reuses the *current* `CallFrame` instead of pushing a new one.
//! Concretely:
//!
//! 1. The new callee and arguments are popped from the stack top.
//! 2. The current frame's stack slice (`stack[frame.stack_base..]`) is cleared.
//! 3. The arguments are pushed back onto the (now-empty) frame slot.
//! 4. The frame's `chunk`, `ip`, `env`, and `stack_base` are updated in place.
//!
//! This keeps the `frames` vector the same length across any number of
//! tail-recursive iterations, achieving O(1) stack growth.
//!
//! ## Built-in dispatch (`Value::Builtin`)
//!
//! Built-in functions are stored as `Value::Builtin(Rc<dyn Fn(...)>)`.  When
//! the VM resolves a callee and finds a `Builtin`, it calls the function
//! directly and pushes the result — no new `CallFrame` is created, because
//! built-ins are opaque Rust closures that manage their own stack internally.
//!
//! ## Closure representation (`Value::Closure`)
//!
//! `MakeFunc` captures the current environment handle and pairs it with an
//! `Rc<Chunk>`.  The `Rc` lets multiple closures share (and cheaply clone) the
//! same compiled body without copying.  The `GcHandle` inside the closure is
//! what keeps the captured environment alive through GC cycles: the VM adds
//! all live handles in `frames` to the GC root set whenever a collection fires.

use std::rc::Rc;

use crate::expr::{Expr, BuiltinFn, env_get, env_set, new_env};
use crate::gc::{GcHandle, Heap};
use crate::vm::bytecode::{Chunk, Op, Value, expr_to_value, value_to_expr};

// ── Value extension: Builtin + Closure ───────────────────────────────────────
//
// The phase-1 `Value` enum only has data variants (Number, Str, Symbol, List,
// Nil).  The VM needs two more runtime-only variants that cannot be stored as
// compile-time constants:
//
//   • Builtin  — a built-in Rust function callable from the VM dispatch loop.
//   • Closure  — a compiled lambda: code + captured environment handle.
//
// Rather than touching bytecode.rs (phase 1) we extend `Value` locally here
// by using a newtype wrapper.  Since Rust enums are closed, we introduce a
// separate `VmValue` that carries all five phase-1 variants *plus* the two
// new ones.  The `from_value` / `into_value` helpers bridge the two types.

/// Extended value type used exclusively by the VM at runtime.
///
/// `VmValue` supersedes `Value` inside the dispatch loop: all stack slots hold
/// `VmValue`, and we convert to/from `Value` (and `Expr`) only at the
/// interpreter boundary.
#[derive(Clone)]
pub enum VmValue {
    Int(i64),
    Float(f64),
    Complex(f64, f64),
    Bool(bool),
    Str(String),
    Symbol(String),
    List(Vec<VmValue>),
    Nil,
    /// A wrapped built-in.
    Builtin(BuiltinFn),
    /// A compiled closure: shared code + captured GC environment.
    Closure {
        chunk: Rc<Chunk>,
        params: Vec<String>,
        body_expr: Box<Expr>,
        env: GcHandle,
    },
}

impl std::fmt::Debug for VmValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmValue::Int(n) => write!(f, "{}", n),
            VmValue::Float(n) => write!(f, "{}", n),
            VmValue::Complex(re, im) => write!(f, "Complex({}, {})", re, im),
            VmValue::Bool(b) => write!(f, "{}", if *b { "#t" } else { "#f" }),
            VmValue::Str(s) => write!(f, "{:?}", s),
            VmValue::Symbol(s) => write!(f, "{}", s),
            VmValue::List(items) => {
                write!(f, "(")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{:?}", v)?;
                }
                write!(f, ")")
            }
            VmValue::Nil => write!(f, "()"),
            VmValue::Builtin(_) => write!(f, "<builtin>"),
            VmValue::Closure { params, .. } => write!(f, "<closure({})>", params.join(", ")),
        }
    }
}

/// Convert a phase-1 `Value` to a `VmValue` (always succeeds).
fn from_value(v: Value) -> VmValue {
    match v {
        Value::Int(n) => VmValue::Int(n),
        Value::Float(n) => VmValue::Float(n),
        Value::Complex(re, im) => VmValue::Complex(re, im),
        Value::Bool(b) => VmValue::Bool(b),
        Value::Str(s) => VmValue::Str(s),
        Value::Symbol(s) => VmValue::Symbol(s),
        Value::List(items) => VmValue::List(items.into_iter().map(from_value).collect()),
        Value::Nil => VmValue::Nil,
        Value::Builtin(f) => VmValue::Builtin(f),
        Value::Closure {
            params,
            body_chunk,
            body_expr,
            env,
        } => VmValue::Closure {
            chunk: body_chunk,
            params,
            body_expr,
            env,
        },
    }
}

/// Convert a `VmValue` back to a phase-1 `Value` (lossy: Builtin/Closure → Err).
fn into_value(v: VmValue) -> Result<Value, String> {
    match v {
        VmValue::Int(n) => Ok(Value::Int(n)),
        VmValue::Float(n) => Ok(Value::Float(n)),
        VmValue::Complex(re, im) => Ok(Value::Complex(re, im)),
        VmValue::Bool(b) => Ok(Value::Bool(b)),
        VmValue::Str(s) => Ok(Value::Str(s)),
        VmValue::Symbol(s) => Ok(Value::Symbol(s)),
        VmValue::List(items) => {
            let vs: Result<Vec<Value>, String> = items.into_iter().map(into_value).collect();
            Ok(Value::List(vs?))
        }
        VmValue::Nil => Ok(Value::Nil),
        VmValue::Builtin(f) => Ok(Value::Builtin(f)),
        VmValue::Closure {
            chunk,
            params,
            body_expr,
            env,
        } => Ok(Value::Closure {
            params,
            body_chunk: chunk,
            body_expr,
            env,
        }),
    }
}

/// Convert a `VmValue` to an `Expr` for the interpreter boundary.
pub fn vm_value_to_expr(v: VmValue, _heap: &mut Heap) -> Result<Expr, String> {
    match v {
        VmValue::Complex(re, im) => Ok(Expr::Complex(re, im)),
        VmValue::Builtin(f) => Ok(Expr::Func(f)),
        VmValue::Closure {
            params,
            body_expr,
            env,
            ..
        } => Ok(Expr::Lambda(params, body_expr, env)),
        other => {
            let v = into_value(other)?;
            Ok(value_to_expr(v))
        }
    }
}

/// Convert an `Expr` to a `VmValue`, bridging built-in and lambda values.
pub fn expr_to_vm_value(expr: &Expr, heap: &mut Heap) -> Result<VmValue, String> {
    match expr {
        Expr::Int(n) => Ok(VmValue::Int(*n)),
        Expr::Float(n) => Ok(VmValue::Float(*n)),
        Expr::Complex(re, im) => Ok(VmValue::Complex(*re, *im)),
        Expr::Bool(b) => Ok(VmValue::Bool(*b)),
        Expr::Str(s) => Ok(VmValue::Str(s.clone())),
        Expr::Symbol(s) => Ok(VmValue::Symbol(s.clone())),
        Expr::List(items) => {
            let mut vs = Vec::with_capacity(items.len());
            for e in items {
                vs.push(expr_to_vm_value(e, heap)?);
            }
            Ok(VmValue::List(vs))
        }
        Expr::Func(f) => Ok(VmValue::Builtin(Rc::clone(f))),
        Expr::Lambda(params, body_expr, captured_env) => {
            let key = crate::vm::cache::CompileCache::key(body_expr);
            let chunk = crate::vm::CACHE
                .with(|c| c.borrow().get_chunk(&key).cloned())
                .unwrap_or_else(|| {
                    if !crate::vm::compiler::is_compilable(body_expr, heap, *captured_env) {
                        let mut c = Chunk::new();
                        c.emit(Op::TreeEval((**body_expr).clone()));
                        return c;
                    }
                    let chunk =
                        crate::vm::compiler::Compiler::compile(body_expr, *captured_env, heap)
                            .unwrap_or_else(|_| {
                                let mut c = Chunk::new();
                                c.emit(Op::TreeEval((**body_expr).clone()));
                                c
                            });
                    crate::vm::CACHE
                        .with(|c| c.borrow_mut().insert_chunk(key.clone(), chunk.clone()));
                    chunk
                });
            Ok(VmValue::Closure {
                chunk: Rc::new(chunk),
                params: params.clone(),
                body_expr: body_expr.clone(),
                env: *captured_env,
            })
        }
        Expr::Macro(..) => Err("uncompilable: Macro".into()),
    }
}

// ── is_truthy ─────────────────────────────────────────────────────────────────

fn is_truthy(v: &VmValue) -> bool {
    match v {
        VmValue::Bool(b) => *b,
        VmValue::Int(n) => *n != 0,
        VmValue::Float(n) => *n != 0.0,
        VmValue::Complex(_, _) => true,
        VmValue::Nil => false,
        VmValue::Str(s) => !s.is_empty(),
        VmValue::List(l) => !l.is_empty(),
        _ => true,
    }
}

// ── CallFrame ────────────────────────────────────────────────────────────────

/// A single call frame on the VM's frame stack.
pub struct CallFrame {
    /// The compiled code being executed.  `Rc` so closures can share code.
    chunk: Rc<Chunk>,
    /// Instruction pointer: index into `chunk.ops`.
    ip: usize,
    /// Index into `VM::stack` where this frame's locals start (just after the
    /// arguments; the callee value itself is *below* `stack_base`).
    stack_base: usize,
    /// The environment frame for variable lookup and binding.
    pub(crate) env: GcHandle,
    /// Parameter names of the closure executing in this frame.
    /// Empty for the top-level chunk (which is not a closure call).
    /// Used by `Op::StoreSelf` to reconstruct the closure value.
    params: Vec<String>,
}

// ── VM ───────────────────────────────────────────────────────────────────────

/// Stack-based bytecode virtual machine.
///
/// Lifetime `'h` ties the VM to the single `Heap` that owns all
/// `EnvData` frames.  There is exactly one `VM` per `vm_eval` call;
/// it is not meant to be reused across top-level evaluations.
pub struct VM<'h> {
    /// The operand / value stack.
    stack: Vec<VmValue>,
    /// The call-frame stack.  The innermost (most-recently-pushed) frame is
    /// `frames.last_mut()`.
    pub(crate) frames: Vec<CallFrame>,
    /// GC heap shared with the tree-walking evaluator.
    heap: &'h mut Heap,
}

impl<'h> VM<'h> {
    /// Create a new VM that will execute `chunk` in environment `env`.
    pub fn new(heap: &'h mut Heap, env: GcHandle, chunk: Chunk) -> Self {
        let initial_frame = CallFrame {
            chunk: Rc::new(chunk),
            ip: 0,
            stack_base: 0,
            env,
            params: Vec::new(), // top-level chunk is not a closure
        };
        VM {
            stack: Vec::new(),
            frames: vec![initial_frame],
            heap,
        }
    }

    /// Expose the heap reference for post-run conversions.
    pub fn heap_mut(&mut self) -> &mut Heap {
        self.heap
    }

    /// Run the VM until the outermost frame executes `Return`.
    ///
    /// Returns the final value or a runtime error string.
    pub fn run(&mut self) -> Result<VmValue, String> {
        #[cfg(target_arch = "x86_64")]
        if self.frames.len() == 1 {
            let chunk = Rc::clone(&self.frames[0].chunk);
            let frame_key = format!("{}", chunk.id);
            let fp = crate::vm::JIT_CACHE.with(|c| c.borrow_mut().tick(&frame_key, &chunk));
            if let Some(fp) = fp {
                return self.run_jit(fp);
            }
        }

        loop {
            // Safety: frames is never empty while running.
            let frame_idx = self.frames.len() - 1;

            // Fetch the next opcode.
            let op = {
                let frame = &self.frames[frame_idx];
                if frame.ip >= frame.chunk.ops.len() {
                    return Err("VM: instruction pointer out of bounds".into());
                }
                frame.chunk.ops[frame.ip].clone()
            };
            // Advance ip *before* dispatching so that jumps simply overwrite it.
            self.frames[frame_idx].ip += 1;

            match op {
                // ── LoadConst ────────────────────────────────────────────────
                Op::LoadConst(v) => {
                    self.stack.push(from_value(v));
                }

                // ── LoadVar ──────────────────────────────────────────────────
                Op::LoadVar(name) => {
                    let env = self.frames[frame_idx].env;
                    let expr = env_get(self.heap, env, &name)
                        .map_err(|e| format!("VM LoadVar '{}' in env {:?}: {}", name, env, e))?;
                    let val = expr_to_vm_value(&expr, self.heap)?;
                    self.stack.push(val);
                }

                // ── StoreVar ─────────────────────────────────────────────────
                Op::StoreVar(name) => {
                    let val = self.pop()?;
                    let env = self.frames[frame_idx].env;
                    let expr = self.vm_value_to_expr_inner(val)?;
                    env_set(self.heap, env, name, expr);
                    // StoreVar is a pure side-effect: it does NOT push a value.
                    // Callers that need a return value (e.g. `define`) emit an
                    // explicit LoadConst(Nil) afterwards.
                }

                // ── AssignVar ────────────────────────────────────────────────
                Op::AssignVar(name) => {
                    let val = self.pop()?;
                    let env = self.frames[frame_idx].env;
                    let expr = self.vm_value_to_expr_inner(val)?;
                    self.heap.env_assign(env, &name, expr)
                        .map_err(|e| format!("{}: {}", name, e))?;
                }

                // ── Pop ──────────────────────────────────────────────────────
                Op::Pop => {
                    self.pop()?;
                }

                // ── Jump ─────────────────────────────────────────────────────
                Op::Jump(target) => {
                    self.frames[frame_idx].ip = target;
                }

                // ── JumpIfFalse ──────────────────────────────────────────────
                Op::JumpIfFalse(target) => {
                    let val = self.pop()?;
                    if !is_truthy(&val) {
                        self.frames[frame_idx].ip = target;
                    }
                }

                // ── Return ───────────────────────────────────────────────────
                Op::Return => {
                    let result = self.pop()?;
                    if self.frames.len() == 1 {
                        // Returning from the outermost frame — we're done.
                        return Ok(result);
                    }
                    // Pop the current frame and restore the caller's stack.
                    let frame = self.frames.pop().unwrap();
                    // Discard all locals pushed by the returning frame (including
                    // any arguments that were placed there).
                    self.stack.truncate(frame.stack_base);
                    // Push the return value into the caller's stack.
                    self.stack.push(result);
                }

                // ── MakeFunc ─────────────────────────────────────────────────
                Op::MakeFunc {
                    code_offset,
                    params,
                    body_expr,
                } => {
                    let env = self.frames[frame_idx].env;
                    // Clone the sub-chunk out of the current chunk.
                    let sub_chunk = {
                        let frame = &self.frames[frame_idx];
                        frame.chunk.sub_chunks[code_offset].clone()
                    };
                    self.stack.push(VmValue::Closure {
                        chunk: Rc::new(sub_chunk),
                        params,
                        body_expr,
                        env,
                    });
                }

                // ── MakeList ─────────────────────────────────────────────────
                Op::MakeList(n) => {
                    let start = self
                        .stack
                        .len()
                        .checked_sub(n)
                        .ok_or("VM MakeList: stack underflow")?;
                    let items: Vec<VmValue> = self.stack.drain(start..).collect();
                    self.stack.push(VmValue::List(items));
                }

                // ── PrependList ──────────────────────────────────────────────
                Op::PrependList => {
                    let item = self.pop()?;
                    let list = self.pop()?;
                    match list {
                        VmValue::List(mut items) => {
                            items.insert(0, item);
                            self.stack.push(VmValue::List(items));
                        }
                        other => {
                            return Err(format!("PrependList: expected list, got {:?}", other));
                        }
                    }
                }

                // ── AppendSplice ─────────────────────────────────────────────
                Op::AppendSplice => {
                    let splice = self.pop()?;
                    let acc = self.pop()?;
                    match (splice, acc) {
                        (VmValue::List(mut s), VmValue::List(a)) => {
                            s.extend(a);
                            self.stack.push(VmValue::List(s));
                        }
                        other => {
                            return Err(format!(
                                "AppendSplice: expected two lists, got {:?}",
                                other
                            ));
                        }
                    }
                }

                // ── LoadNil ──────────────────────────────────────────────────
                Op::LoadNil => {
                    self.stack.push(VmValue::List(vec![]));
                }

                // ── PushEnv ──────────────────────────────────────────────────
                Op::PushEnv => {
                    let parent = self.frames[frame_idx].env;
                    let child = new_env(self.heap, Some(parent));
                    self.frames[frame_idx].env = child;
                }

                // ── PopEnv ───────────────────────────────────────────────────
                Op::PopEnv => {
                    let current = self.frames[frame_idx].env;
                    // Retrieve the parent handle stored by new_env.
                    let parent = self.heap.parent_of(current).ok_or_else(|| {
                        "VM PopEnv: no parent environment (already at root)".to_string()
                    })?;
                    self.frames[frame_idx].env = parent;
                }

                // ── StoreSelf ───────────────────────────────────────────────
                Op::StoreSelf(name) => {
                    // Reconstruct the closure value from the current frame's
                    // chunk and params, capturing the current env.
                    let frame = &self.frames[frame_idx];
                    let self_val = VmValue::Closure {
                        chunk: Rc::clone(&frame.chunk),
                        params: frame.params.clone(),
                        body_expr: Box::new(crate::expr::Expr::List(vec![])),
                        env: frame.env,
                    };
                    let expr = self.vm_value_to_expr_inner(self_val)?;
                    let env = self.frames[frame_idx].env;
                    env_set(self.heap, env, name, expr);
                }

                // ── Call ─────────────────────────────────────────────────────
                Op::Call(n_args) => {
                    self.do_call(n_args, /*tail=*/ false)?;
                }

                // ── TailCall ─────────────────────────────────────────────────
                Op::TailCall(n_args) => {
                    self.do_call(n_args, /*tail=*/ true)?;
                }

                // ── TreeEval ─────────────────────────────────────────────────
                Op::TreeEval(expr) => {
                    let env = self.frames[frame_idx].env;
                    let result = crate::eval::eval_tree(&expr, env, self.heap)?;
                    let val = expr_to_vm_value(&result, self.heap).unwrap_or(VmValue::List(vec![]));

                    if self.frames.len() == 1 {
                        return Ok(val);
                    }
                    let frame = self.frames.pop().unwrap();
                    self.stack.truncate(frame.stack_base);
                    self.stack.push(val);
                }
            }
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Pop the top value from the stack.
    fn pop(&mut self) -> Result<VmValue, String> {
        self.stack
            .pop()
            .ok_or_else(|| "VM: stack underflow".to_string())
    }

    /// Convert a `VmValue` to an `Expr` for storage in the GC heap.
    fn vm_value_to_expr_inner(&self, v: VmValue) -> Result<Expr, String> {
        match v {
            VmValue::Builtin(f) => Ok(Expr::Func(f)),
            VmValue::Closure {
                chunk: _,
                params,
                body_expr,
                env,
            } => {
                // Represent as a Lambda with the stored body expression.
                Ok(Expr::Lambda(params, body_expr, env))
            }
            other => {
                let val = into_value(other)?;
                Ok(value_to_expr(val))
            }
        }
    }

    /// Core logic for `Call` and `TailCall`.
    ///
    /// Stack before: `[ ... | callee | arg0 | arg1 | ... | argN-1 ]`
    ///
    /// For a normal `Call`, a new `CallFrame` is pushed.
    /// For a `TailCall`, the current frame is updated in place (no new frame).
    fn do_call(&mut self, n_args: usize, tail: bool) -> Result<(), String> {
        let stack_len = self.stack.len();
        if stack_len < n_args + 1 {
            return Err(format!(
                "VM Call: stack underflow (need {} + callee, have {})",
                n_args, stack_len
            ));
        }

        // The callee is just below the arguments.
        let callee_idx = stack_len - n_args - 1;
        let callee = self.stack[callee_idx].clone();

        match callee {
            VmValue::Builtin(f) => {
                // Collect arguments (they sit above the callee on the stack).
                let args: Vec<VmValue> = self.stack.drain(callee_idx + 1..).collect();
                // Remove the callee itself.
                self.stack.pop();

                // Convert VmValue arguments to Value, then to Expr.
                let mut expr_args = Vec::with_capacity(args.len());
                for arg in args {
                    let val = into_value(arg)?;
                    expr_args.push(value_to_expr(val));
                }

                // Call the built-in directly.
                let result = f(&expr_args, self.heap)?;

                // Convert result back to Value, then VmValue, and push.
                let val_res = expr_to_value(&result)?;
                self.stack.push(from_value(val_res));
                Ok(())
            }

            VmValue::Closure {
                chunk,
                params,
                body_expr: _,
                env: closure_env,
            } => {
                let n_params = params.len();
                if n_args != n_params {
                    return Err(format!(
                        "arity mismatch: closure expects {} arguments, got {}",
                        n_params, n_args
                    ));
                }

                // Collect arguments from the stack.
                let args: Vec<VmValue> = self.stack.drain(callee_idx + 1..).collect();
                // Remove the callee.
                self.stack.pop(); // now stack.len() == callee_idx

                // Allocate a new environment frame, child of the closure's env.
                let call_env = new_env(self.heap, Some(closure_env));

                // Bind parameters.
                for (name, val) in params.iter().zip(args) {
                    let expr = self.vm_value_to_expr_inner(val)?;
                    env_set(self.heap, call_env, name.clone(), expr);
                }

                if tail && !self.frames.is_empty() {
                    // ── Tail-call: reuse current frame ────────────────────────
                    //
                    // The current frame's locals sit in stack[stack_base..].
                    // We've already restored the stack to `callee_idx` above,
                    // so stack[stack_base..] is now empty (callee + args were
                    // above stack_base or exactly at it).
                    //
                    // Simply overwrite the frame fields; the stack_base stays
                    // the same (the frame's slot in the stack is unchanged).
                    let frame_idx = self.frames.len() - 1;
                    let frame = &mut self.frames[frame_idx];
                    frame.chunk = chunk;
                    frame.ip = 0;
                    frame.env = call_env;
                    frame.params = params;
                    // stack_base stays as-is — the frame's stack window begins
                    // where it always did; args are now bound in the env.
                } else {
                    // ── Normal call: push a new frame ─────────────────────────
                    let stack_base = self.stack.len(); // no locals on stack yet
                    self.frames.push(CallFrame {
                        chunk,
                        ip: 0,
                        stack_base,
                        env: call_env,
                        params,
                    });
                }

                // ── Maybe collect ───────────────────────────────────────────
                //
                // Trigger GC if the heap has grown past the threshold.
                // All frame environments are passed as roots so that
                // nothing reachable from the call stack is collected.
                let frame_roots: Vec<GcHandle> = self.frames.iter().map(|f| f.env).collect();
                self.heap.maybe_collect(&frame_roots);

                Ok(())
            }

            other => Err(format!("VM: not callable: {:?}", other)),
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn run_jit(
        &mut self,
        fp: unsafe extern "C" fn(*mut crate::vm::jit_abi::JitFrame),
    ) -> Result<VmValue, String> {
        let mut frame = crate::vm::jit_abi::JitFrame::new(self);
        unsafe { fp(&mut frame) };
        frame.into_vm_value()
    }
}
