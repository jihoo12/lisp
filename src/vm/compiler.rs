//! Bytecode compiler for pilisp (phase 1).
//!
//! Translates a fully macro-expanded `Expr` AST into a `Chunk` of `Op`
//! instructions, ready for the phase-2 VM to execute.
//!
//! # Public surface
//!
//! ```rust,ignore
//! let chunk = Compiler::compile(&expr, &heap)?;
//! ```
//!
//! # Design notes
//!
//! ## Macro expansion
//!
//! Macros are expanded **eagerly** before the compiler sees the expression.
//! `Compiler::compile` calls `expand_all` which recursively walks the AST and
//! expands any macro application it finds, using the same `eval`-time
//! environment lookup that the tree-walker uses.  This satisfies the
//! constraint that the compiler must operate on a fully expanded form.
//!
//! Because macro expansion requires an environment (to look up macro
//! definitions) and the compiler is otherwise environment-free at compile
//! time, we thread a `Heap` reference through the expansion phase.
//!
//! ## Tail-call detection
//!
//! The compiler tracks whether the sub-expression it is currently compiling
//! is in *tail position* via a boolean flag (`tail`).  A position is a tail
//! position when its value is the last thing the enclosing function will
//! return.  The rules are:
//!
//! * The body of a lambda is in tail position.
//! * The selected branch of `if` is in tail position when the `if` is itself
//!   in tail position.
//! * The last expression of a `begin` / `let` / `let*` body is in tail
//!   position when that whole form is in tail position.
//! * A function call expression is in tail position when the whole call is in
//!   tail position; in that case `TailCall` is emitted instead of `Call`.
//! * All other sub-expressions (condition of `if`, non-last `begin`
//!   expressions, binding initialisers) are **not** in tail position.
//!
//! ## `CubicalTerm` rejection
//!
//! Any node whose sub-tree contains an `Expr::CubicalTerm` causes the
//! compiler to return `Err("uncompilable: CubicalTerm")` immediately.  The
//! caller should catch this error and fall back to the tree-walking evaluator.
//!
//! ## `quote` and `quasiquote`
//!
//! `quote` simply converts the quoted datum to a `Value` with `expr_to_value`
//! and emits `LoadConst`.  `quasiquote` is handled by delegating to the
//! existing `eval_quasiquote` helper (which requires a heap and env) and then
//! converting the fully-spliced result to a `Value`.  This is a compile-time
//! quasi-quote evaluation — fine because quasiquote splicing can only read
//! from the environment, never mutate it in a way that would affect later
//! compilation.

use crate::expr::{Expr, env_get};
use crate::gc::GcHandle;
use crate::gc::Heap;
use crate::macros::expand_macro;
use crate::reader::parse_params;
use crate::vm::bytecode::{Chunk, Op, Value, expr_to_value};

// ── public entry point ────────────────────────────────────────────────────────

/// Stateless bytecode compiler.
///
/// All state is local to a single `compile` call; there is no mutable
/// per-instance state.  The struct exists only to group related methods under
/// a common namespace and to make `impl Compiler { ... }` natural Rust style.
pub struct Compiler;

impl Compiler {
    /// Compile `expr` into a `Chunk`.
    ///
    /// `heap` is needed only for macro expansion (looking up macro definitions
    /// and for any `eval` calls triggered by `quasiquote` unquoting).
    ///
    /// Returns `Err` if the expression is or contains `CubicalTerm`, or if
    /// any syntactic constraint is violated.
    pub fn compile(expr: &Expr, env: crate::expr::Env, heap: &mut Heap) -> Result<Chunk, String> {
        let expanded = expand_all(expr, env, heap)?;
        let mut chunk = Chunk::new();
        compile_expr(&expanded, &mut chunk, heap, env, /*tail=*/ true)?;
        // Every top-level chunk needs a terminal Return so the VM knows to stop.
        chunk.emit(Op::Return);
        Ok(chunk)
    }
}

// ── recursive macro expander ──────────────────────────────────────────────────

/// Recursively expand all macro applications in `expr`.
///
/// The expansion loop follows the same logic as the tree-walker in `eval_step`:
/// if the head of a list is a symbol that names a macro in `env`, substitute
/// the arguments and expand the result again (macros may expand to macro calls).
///
/// Non-macro list heads are left structurally intact; only their sub-expressions
/// are recursively expanded.  Atoms are returned unchanged.
fn expand_all(expr: &Expr, env: crate::expr::Env, heap: &mut Heap) -> Result<Expr, String> {
    match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Complex(_, _) | Expr::Bool(_) | Expr::Str(_)
        | Expr::Symbol(_) => Ok(expr.clone()),

        // Func / Lambda / Macro are runtime values; they shouldn't appear as
        // raw AST nodes in the source being compiled, but handle gracefully.
        Expr::Func(_) | Expr::Lambda(..) | Expr::Macro(..) => Ok(expr.clone()),

        Expr::List(list) if list.is_empty() => Ok(expr.clone()),

        Expr::List(list) => {
            // Special forms that create bindings or have their own quoting
            // rules need to suppress expansion in the "quoted" positions.
            if let Some(Expr::Symbol(head)) = list.first() {
                match head.as_str() {
                    // These forms are not user-defined macros; expand their
                    // sub-expressions (except quoted positions).
                    "quote" => return Ok(expr.clone()), // datum is opaque
                    "quasiquote" => return Ok(expr.clone()), // handled at compile time
                    // defmacro / defstruct are always handled by the tree-walker;
                    // never reach here in normal flow (is_compilable blocks
                    // them), but guard defensively so expand_all never mangles
                    // them.
                    "defmacro" | "defstruct" => return Ok(expr.clone()),
                    "if" | "begin" | "define" | "set!" | "let" | "let*" | "for" => {
                        // Expand sub-expressions, keeping the special-form head.
                        let mut expanded = vec![list[0].clone()];
                        for sub in &list[1..] {
                            expanded.push(expand_all(sub, env, heap)?);
                        }
                        return Ok(Expr::List(expanded));
                    }
                    "lambda" => {
                        // Don't expand the parameter list; do expand the body.
                        if list.len() < 3 {
                            return Ok(expr.clone());
                        }
                        let mut out = vec![list[0].clone(), list[1].clone()];
                        for body_expr in &list[2..] {
                            out.push(expand_all(body_expr, env, heap)?);
                        }
                        return Ok(Expr::List(out));
                    }
                    _ => {
                        // Check if the head names a macro.
                        if let Ok(Expr::Macro(params, body)) = env_get(heap, env, head) {
                            let args = &list[1..];
                            let substituted = expand_macro(&params, &body, args)?;
                            // Expand the result recursively (macros can return macros).
                            return expand_all(&substituted, env, heap);
                        }
                    }
                }
            }
            // Generic list: expand every element.
            let expanded: Result<Vec<Expr>, String> =
                list.iter().map(|e| expand_all(e, env, heap)).collect();
            Ok(Expr::List(expanded?))
        }
    }
}

// ── core compiler ─────────────────────────────────────────────────────────────

/// Compile a single `Expr` into `chunk`.
///
/// `tail` indicates whether this expression is in tail position.  When `true`
/// and the expression is a function call, `TailCall` is emitted instead of
/// `Call`; when the expression is a `Return`-valued form (already handled by
/// the caller's append of `Op::Return`) nothing extra is emitted.
fn compile_expr(
    expr: &Expr,
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    match expr {
        // ── self-evaluating atoms ─────────────────────────────────────────────
        Expr::Int(n) => {
            chunk.emit(Op::LoadConst(Value::Int(*n)));
            Ok(())
        }
        Expr::Float(n) => {
            chunk.emit(Op::LoadConst(Value::Float(*n)));
            Ok(())
        }
        Expr::Complex(re, im) => {
            chunk.emit(Op::LoadConst(Value::Complex(*re, *im)));
            Ok(())
        }
        Expr::Bool(b) => {
            chunk.emit(Op::LoadConst(Value::Bool(*b)));
            Ok(())
        }
        Expr::Str(s) => {
            chunk.emit(Op::LoadConst(Value::Str(s.clone())));
            Ok(())
        }

        // ── symbol lookup ─────────────────────────────────────────────────────
        Expr::Symbol(s) => {
            if s.starts_with(':') {
                chunk.emit(Op::LoadConst(Value::Symbol(s.clone())));
            } else {
                chunk.emit(Op::LoadVar(s.clone()));
            }
            Ok(())
        }

        // ── runtime values (should not appear raw in compiled source) ─────────
        Expr::Func(_) | Expr::Lambda(..) | Expr::Macro(..) => {
            // If somehow a runtime value ends up in the AST, convert it or err.
            match expr_to_value(expr) {
                Ok(v) => {
                    chunk.emit(Op::LoadConst(v));
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }

        // ── list forms ────────────────────────────────────────────────────────
        Expr::List(list) => compile_list(list, chunk, heap, env, tail),
    }
}

/// Compile a list form `(head arg...)`.
fn compile_list(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    // Empty list → push Nil constant.
    if list.is_empty() {
        chunk.emit(Op::LoadConst(Value::Nil));
        return Ok(());
    }

    // Check for special forms first.
    if let Expr::Symbol(head) = &list[0] {
        match head.as_str() {
            // ── quote ─────────────────────────────────────────────────────────
            "quote" => return compile_quote(list, chunk),

            // ── quasiquote ────────────────────────────────────────────────────
            "quasiquote" => {
                if list.len() != 2 {
                    return Err(format!(
                        "quasiquote expects 1 argument, got {}",
                        list.len() - 1
                    ));
                }
                return compile_quasiquote(&list[1], 1, chunk, heap, env);
            }

            // ── if ────────────────────────────────────────────────────────────
            "if" => return compile_if(list, chunk, heap, env, tail),

            // ── define ────────────────────────────────────────────────────────
            "define" => return compile_define(list, chunk, heap, env),

            // ── set! ──────────────────────────────────────────────────────────
            "set!" => return compile_set(list, chunk, heap, env),

            // ── begin ─────────────────────────────────────────────────────────
            "begin" => return compile_begin(list, chunk, heap, env, tail),

            // ── let ───────────────────────────────────────────────────────────
            "let" => return compile_let(list, chunk, heap, env, tail),

            // ── let* ──────────────────────────────────────────────────────────
            "let*" => return compile_let_star(list, chunk, heap, env, tail),

            // ── for ───────────────────────────────────────────────────────────
            "for" => return compile_for(list, chunk, heap, env, tail),

            // ── lambda ────────────────────────────────────────────────────────
            "lambda" => return compile_lambda(list, chunk, heap, env, None),

            // ── tailcall ──────────────────────────────────────────────────────
            "tailcall" => {
                if list.len() < 2 {
                    return Err("tailcall expects at least a function".into());
                }
                return compile_call(&list[1..], chunk, heap, env, true);
            }

            // Anything else falls through to function-call handling below.
            _ => {}
        }
    }

    // ── function call ─────────────────────────────────────────────────────────
    compile_call(list, chunk, heap, env, tail)
}

// ── quote ─────────────────────────────────────────────────────────────────────

fn compile_quote(list: &[Expr], chunk: &mut Chunk) -> Result<(), String> {
    if list.len() != 2 {
        return Err(format!("quote expects 1 argument, got {}", list.len() - 1));
    }
    let v = expr_to_value(&list[1]).map_err(|e| format!("quote: cannot compile datum: {}", e))?;
    chunk.emit(Op::LoadConst(v));
    Ok(())
}

fn qq_op_expr(expr: &Expr) -> Option<&str> {
    if let Expr::List(l) = expr
        && let Some(Expr::Symbol(s)) = l.first() {
            return Some(s.as_str());
        }
    None
}

/// Compile a `quasiquote` expression at the given nesting depth entirely within the VM.
fn compile_quasiquote(
    expr: &Expr,
    depth: usize,
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
) -> Result<(), String> {
    match expr {
        Expr::List(list) if !list.is_empty() => {
            match qq_op_expr(expr) {
                Some("unquote") => {
                    if list.len() != 2 {
                        return Err(format!(
                            "unquote expects 1 argument, got {}",
                            list.len() - 1
                        ));
                    }
                    if depth == 1 {
                        // Fully escape: compile the inner expression normally.
                        compile_expr(&list[1], chunk, heap, env, false)
                    } else {
                        // Still nested — descend one level but keep the
                        // `unquote` wrapper for the outer quasiquote to see.
                        chunk.emit(Op::LoadConst(Value::Symbol("unquote".into())));
                        compile_quasiquote(&list[1], depth - 1, chunk, heap, env)?;
                        chunk.emit(Op::MakeList(2));
                        Ok(())
                    }
                }

                Some("quasiquote") => {
                    if list.len() != 2 {
                        return Err(format!(
                            "quasiquote expects 1 argument, got {}",
                            list.len() - 1
                        ));
                    }
                    // Entering a nested quasiquote — increment depth.
                    chunk.emit(Op::LoadConst(Value::Symbol("quasiquote".into())));
                    compile_quasiquote(&list[1], depth + 1, chunk, heap, env)?;
                    chunk.emit(Op::MakeList(2));
                    Ok(())
                }

                _ => {
                    // Start with empty accumulator.
                    chunk.emit(Op::LoadNil);
                    // Iterate items in reverse.
                    for item in list.iter().rev() {
                        if qq_op_expr(item) == Some("unquote-splicing") {
                            let inner = match item {
                                Expr::List(l) => l,
                                _ => unreachable!(),
                            };
                            if inner.len() != 2 {
                                return Err(format!(
                                    "unquote-splicing expects 1 argument, got {}",
                                    inner.len() - 1
                                ));
                            }
                            if depth == 1 {
                                // Evaluate and splice the resulting list inline.
                                compile_expr(&inner[1], chunk, heap, env, false)?;
                                chunk.emit(Op::AppendSplice);
                            } else {
                                // At depth > 1 reconstruct the form.
                                chunk.emit(Op::LoadConst(Value::Symbol("unquote-splicing".into())));
                                compile_quasiquote(&inner[1], depth - 1, chunk, heap, env)?;
                                chunk.emit(Op::MakeList(2));
                                chunk.emit(Op::PrependList);
                            }
                        } else {
                            compile_quasiquote(item, depth, chunk, heap, env)?;
                            chunk.emit(Op::PrependList);
                        }
                    }
                    Ok(())
                }
            }
        }
        // Atoms (numbers, strings, symbols not in unquote position) are
        // converted to constants and loaded.
        _ => {
            let val = expr_to_value(expr)
                .map_err(|e| format!("quasiquote: cannot convert atom to Value: {}", e))?;
            chunk.emit(Op::LoadConst(val));
            Ok(())
        }
    }
}

// ── if ────────────────────────────────────────────────────────────────────────

/// Compile `(if cond then [else])`.
///
/// Emits:
/// ```text
/// <cond>
/// JumpIfFalse → else_label
/// <then>
/// Jump → end_label
/// else_label:
/// <else>   (or LoadConst Nil if absent)
/// end_label:
/// ```
fn compile_if(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    if list.len() < 3 || list.len() > 4 {
        return Err(format!(
            "if expects 2 or 3 arguments, got {}",
            list.len() - 1
        ));
    }

    // Compile the condition (never in tail position).
    compile_expr(&list[1], chunk, heap, env, false)?;

    // Emit a placeholder JumpIfFalse; record where it is so we can patch it.
    let jump_if_false_idx = chunk.emit(Op::JumpIfFalse(0));

    // Compile the "then" branch (in tail position if the whole `if` is).
    compile_expr(&list[2], chunk, heap, env, tail)?;

    // Emit a placeholder unconditional jump past the else branch.
    let jump_over_else_idx = chunk.emit(Op::Jump(0));

    // Patch the JumpIfFalse to land here (start of the else branch).
    let else_start = chunk.ops.len();
    chunk.patch_jump(jump_if_false_idx, else_start);

    // Compile the "else" branch (or push Nil if absent).
    if list.len() == 4 {
        compile_expr(&list[3], chunk, heap, env, tail)?;
    } else {
        chunk.emit(Op::LoadConst(Value::Nil));
    }

    // Patch the unconditional jump to skip past the else branch.
    let end = chunk.ops.len();
    chunk.patch_jump(jump_over_else_idx, end);

    Ok(())
}

// ── define ────────────────────────────────────────────────────────────────────

/// Compile `(define name expr)`.
///
/// Emits: `<expr>` then `StoreVar(name)` then `LoadConst(Nil)`.
/// `StoreVar` does not push a value; the explicit `LoadConst(Nil)` ensures
/// `define` always leaves `()` on the stack as its return value.
///
/// Special case: `(define name (lambda ...))` is detected so that
/// `compile_lambda` can emit `StoreSelf(name)` at the top of the sub-chunk
/// to support direct recursion.
fn compile_define(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
) -> Result<(), String> {
    if list.len() != 3 {
        return Err(format!(
            "define expects 2 arguments, got {}",
            list.len() - 1
        ));
    }
    let name = match &list[1] {
        Expr::Symbol(s) => s.clone(),
        other => return Err(format!("define: expected a symbol name, got {:?}", other)),
    };

    // Detect `(define name (lambda ...))`  — compile with self-name so the
    // closure can bind itself for direct recursion.
    let is_lambda_rhs = matches!(
        &list[2],
        Expr::List(inner)
            if !inner.is_empty()
                && matches!(&inner[0], Expr::Symbol(s) if s == "lambda")
    );

    if is_lambda_rhs {
        // Pass the binding name into compile_lambda so it emits StoreSelf.
        if let Expr::List(lambda_list) = &list[2] {
            compile_lambda(lambda_list, chunk, heap, env, Some(name.clone()))?;
        }
    } else {
        compile_expr(&list[2], chunk, heap, env, false)?;
    }

    chunk.emit(Op::StoreVar(name));
    // define returns () — StoreVar does not push, so push it explicitly.
    chunk.emit(Op::LoadConst(Value::Nil));
    Ok(())
}

// ── set! ──────────────────────────────────────────────────────────────────────

/// Compile `(set! name expr)`.
///
/// Semantically identical bytecode to `define` — the difference (walking the
/// parent chain to find an existing binding rather than creating a new one) is
/// enforced by the VM at runtime, not by different opcodes.
///
/// Design decision: we could introduce a separate `SetVar` opcode to preserve
/// the `set!` vs `define` distinction at the bytecode level, but since the
/// VM has access to the full environment chain at runtime it can enforce the
/// semantics without a distinct opcode.  For phase 1 we re-use `StoreVar`.
fn compile_set(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
) -> Result<(), String> {
    if list.len() != 3 {
        return Err(format!("set! expects 2 arguments, got {}", list.len() - 1));
    }
    let name = match &list[1] {
        Expr::Symbol(s) => s.clone(),
        other => return Err(format!("set!: expected a symbol name, got {:?}", other)),
    };
    compile_expr(&list[2], chunk, heap, env, false)?;
    chunk.emit(Op::AssignVar(name));
    // set! returns ()
    chunk.emit(Op::LoadConst(Value::Nil));
    Ok(())
}

// ── begin ─────────────────────────────────────────────────────────────────────

/// Compile `(begin expr...)`.
///
/// All expressions except the last are compiled in non-tail position and their
/// result popped.  The last expression is compiled in the caller-supplied tail
/// position.
fn compile_begin(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    let body = &list[1..];
    if body.is_empty() {
        chunk.emit(Op::LoadConst(Value::Nil));
        return Ok(());
    }
    for expr in &body[..body.len() - 1] {
        compile_expr(expr, chunk, heap, env, false)?;
        chunk.emit(Op::Pop);
    }
    compile_expr(body.last().unwrap(), chunk, heap, env, tail)
}

// ── for ───────────────────────────────────────────────────────────────────────

const FOR_END: &str = "__for-end";
const FOR_LST: &str = "__for-lst";

/// Compile `(for var arg body...)`.
///
/// Literal numeric bounds `(for var N M body...)` compile to a jump loop.
/// Single-collection forms `(for var coll body...)` compile to a cdr walk.
/// Non-literal 5+ arg forms fall back to `TreeEval` so runtime dispatch in
/// `eval_for` can choose numeric vs list semantics.
fn compile_for(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    if list.len() < 4 {
        return Err(format!(
            "for expects at least 3 arguments (var collection body), got {}",
            list.len() - 1
        ));
    }

    let var = match &list[1] {
        Expr::Symbol(name) => name.clone(),
        other => {
            return Err(format!(
                "for: loop variable must be a symbol, got {:?}",
                other
            ));
        }
    };

    let numeric_literals = list.len() >= 5
        && matches!(&list[2], Expr::Int(_))
        && matches!(&list[3], Expr::Int(_));

    if list.len() >= 5 && !numeric_literals {
        chunk.emit(Op::TreeEval(Expr::List(list.to_vec())));
        let _ = tail;
        return Ok(());
    }

    if numeric_literals {
        compile_for_numeric(&list[2], &list[3], &var, &list[4..], chunk, heap, env)
    } else {
        compile_for_list(&list[2], &var, &list[3..], chunk, heap, env)
    }
}

fn compile_for_body(
    body: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
) -> Result<(), String> {
    for expr in body {
        compile_expr(expr, chunk, heap, env, false)?;
        chunk.emit(Op::Pop);
    }
    Ok(())
}

fn compile_for_numeric(
    start: &Expr,
    end: &Expr,
    var: &str,
    body: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
) -> Result<(), String> {
    chunk.emit(Op::PushEnv);
    compile_expr(start, chunk, heap, env, false)?;
    chunk.emit(Op::StoreVar(var.to_string()));
    compile_expr(end, chunk, heap, env, false)?;
    chunk.emit(Op::StoreVar(FOR_END.into()));

    let loop_start = chunk.ops.len();
    compile_call(
        &[
            Expr::Symbol("<".into()),
            Expr::Symbol(var.into()),
            Expr::Symbol(FOR_END.into()),
        ],
        chunk,
        heap,
        env,
        false,
    )?;
    let jump_end_idx = chunk.emit(Op::JumpIfFalse(0));

    compile_for_body(body, chunk, heap, env)?;

    compile_call(
        &[
            Expr::Symbol("+".into()),
            Expr::Symbol(var.into()),
            Expr::Int(1),
        ],
        chunk,
        heap,
        env,
        false,
    )?;
    chunk.emit(Op::StoreVar(var.to_string()));
    chunk.emit(Op::Jump(loop_start));

    let end_label = chunk.ops.len();
    chunk.patch_jump(jump_end_idx, end_label);
    chunk.emit(Op::PopEnv);
    chunk.emit(Op::LoadConst(Value::Nil));
    Ok(())
}

fn compile_for_list(
    collection: &Expr,
    var: &str,
    body: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
) -> Result<(), String> {
    chunk.emit(Op::PushEnv);
    compile_expr(collection, chunk, heap, env, false)?;
    chunk.emit(Op::StoreVar(FOR_LST.into()));

    let loop_start = chunk.ops.len();
    compile_call(
        &[
            Expr::Symbol("not".into()),
            Expr::List(vec![
                Expr::Symbol("null?".into()),
                Expr::Symbol(FOR_LST.into()),
            ]),
        ],
        chunk,
        heap,
        env,
        false,
    )?;
    let jump_end_idx = chunk.emit(Op::JumpIfFalse(0));

    compile_call(
        &[Expr::Symbol("car".into()), Expr::Symbol(FOR_LST.into())],
        chunk,
        heap,
        env,
        false,
    )?;
    chunk.emit(Op::StoreVar(var.to_string()));

    compile_for_body(body, chunk, heap, env)?;

    compile_call(
        &[Expr::Symbol("cdr".into()), Expr::Symbol(FOR_LST.into())],
        chunk,
        heap,
        env,
        false,
    )?;
    chunk.emit(Op::StoreVar(FOR_LST.into()));
    chunk.emit(Op::Jump(loop_start));

    let end_label = chunk.ops.len();
    chunk.patch_jump(jump_end_idx, end_label);
    chunk.emit(Op::PopEnv);
    chunk.emit(Op::LoadConst(Value::Nil));
    Ok(())
}

// ── let ───────────────────────────────────────────────────────────────────────

/// Compile `(let ((name expr)...) body...)`.
///
/// Standard `let` semantics: all binding initialisers are evaluated in the
/// *outer* environment before any binding is created.  The bindings are
/// visible only inside the body — we bracket them with `PushEnv`/`PopEnv`
/// so the VM allocates a fresh child frame before storing them and restores
/// the parent frame after the body.
///
/// Bytecode layout:
/// ```text
/// [compile all RHS in outer env]     ; all initialisers evaluated first
/// PushEnv                            ; allocate child frame
/// StoreVar(nameN-1) … StoreVar(name0); store in reverse order
/// [body expressions]
/// PopEnv                             ; restore parent frame
/// ```
fn compile_let(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    if list.len() < 3 {
        return Err(format!(
            "let expects at least 2 arguments (bindings body), got {}",
            list.len() - 1
        ));
    }
    let bindings = match &list[1] {
        Expr::List(b) => b,
        other => return Err(format!("let: expected a list of bindings, got {:?}", other)),
    };

    // Phase 1: Evaluate all initialisers in the *outer* environment.
    let mut names = Vec::new();
    for b in bindings {
        match b {
            Expr::List(pair) if pair.len() == 2 => {
                if let Expr::Symbol(name) = &pair[0] {
                    names.push(name.clone());
                    compile_expr(&pair[1], chunk, heap, env, false)?;
                } else {
                    return Err(format!(
                        "let binding name must be a symbol, got {:?}",
                        pair[0]
                    ));
                }
            }
            other => {
                return Err(format!(
                    "let binding must be a (name expr) pair, got {:?}",
                    other
                ));
            }
        }
    }

    // Phase 2: Open a child scope, then store the values.
    // All RHS values are on the stack in push order; store in reverse so
    // the stack pops them back in declaration order.
    chunk.emit(Op::PushEnv);
    for name in names.iter().rev() {
        chunk.emit(Op::StoreVar(name.clone()));
    }

    // Phase 3: Compile the body expressions.
    let body = &list[2..];
    if body.is_empty() {
        chunk.emit(Op::LoadConst(Value::Nil));
        chunk.emit(Op::PopEnv);
        return Ok(());
    }
    for expr in &body[..body.len() - 1] {
        compile_expr(expr, chunk, heap, env, false)?;
        chunk.emit(Op::Pop);
    }
    // Last body expr — not in tail position because we need PopEnv after it.
    compile_expr(body.last().unwrap(), chunk, heap, env, false)?;
    chunk.emit(Op::PopEnv);
    // If the caller wanted tail position, the value is already on the stack.
    let _ = tail; // tail TCO inside let is deferred to future work
    Ok(())
}

// ── let* ──────────────────────────────────────────────────────────────────────

/// Compile `(let* ((name expr)...) body...)`.
///
/// Sequential `let` semantics: each binding can see the previous ones.
/// The entire `let*` body runs in a child scope opened by `PushEnv`.
/// Each binding is evaluated then immediately stored so that later bindings
/// can reference earlier ones.
///
/// Bytecode layout:
/// ```text
/// PushEnv
/// [compile rhs0] ; StoreVar(name0)
/// [compile rhs1] ; StoreVar(name1)   ← can reference name0
/// …
/// [body expressions]
/// PopEnv
/// ```
fn compile_let_star(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    if list.len() < 3 {
        return Err(format!(
            "let* expects at least 2 arguments (bindings body), got {}",
            list.len() - 1
        ));
    }
    let bindings = match &list[1] {
        Expr::List(b) => b,
        other => {
            return Err(format!(
                "let*: expected a list of bindings, got {:?}",
                other
            ));
        }
    };

    // Open the child scope before any bindings.
    chunk.emit(Op::PushEnv);

    // Each pair: evaluate in the current (extended) env, then bind immediately.
    for b in bindings {
        match b {
            Expr::List(pair) if pair.len() == 2 => {
                if let Expr::Symbol(name) = &pair[0] {
                    compile_expr(&pair[1], chunk, heap, env, false)?;
                    chunk.emit(Op::StoreVar(name.clone()));
                } else {
                    return Err(format!(
                        "let* binding name must be a symbol, got {:?}",
                        pair[0]
                    ));
                }
            }
            other => {
                return Err(format!(
                    "let* binding must be a (name expr) pair, got {:?}",
                    other
                ));
            }
        }
    }

    // Compile body.
    let body = &list[2..];
    if body.is_empty() {
        chunk.emit(Op::LoadConst(Value::Nil));
        chunk.emit(Op::PopEnv);
        return Ok(());
    }
    for expr in &body[..body.len() - 1] {
        compile_expr(expr, chunk, heap, env, false)?;
        chunk.emit(Op::Pop);
    }
    // Compile last body expr (not in full tail position — PopEnv follows).
    compile_expr(body.last().unwrap(), chunk, heap, env, false)?;
    chunk.emit(Op::PopEnv);
    let _ = tail;
    Ok(())
}

// ── lambda ────────────────────────────────────────────────────────────────────

/// Compile `(lambda (params...) body...)` into a `MakeFunc` instruction.
///
/// The body is compiled into a fresh sub-chunk stored in the parent chunk's
/// `sub_chunks` vector.  A terminal `Op::Return` is appended automatically.
///
/// `self_name` — when `Some(name)`, a `StoreSelf(name)` is prepended to the
/// sub-chunk body so the closure can look up its own name for direct recursion
/// (as in `(define factorial (lambda (n) ...))`).  Pass `None` for anonymous
/// lambdas.
fn compile_lambda(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    self_name: Option<String>,
) -> Result<(), String> {
    if list.len() < 3 {
        return Err(format!(
            "lambda expects at least 2 arguments (params body...), got {}",
            list.len() - 1
        ));
    }
    let params =
        parse_params(&list[1]).map_err(|e| format!("lambda: invalid parameter list: {}", e))?;

    // Compile the body into a new sub-chunk.
    let mut body_chunk = Chunk::new();

    // If this is a named lambda (from `define`), bind the closure to its own
    // name at the very start of the sub-chunk so recursive calls resolve.
    if let Some(ref name) = self_name {
        body_chunk.emit(Op::StoreSelf(name.clone()));
    }

    let body_exprs = &list[2..];
    if body_exprs.is_empty() {
        body_chunk.emit(Op::LoadConst(Value::Nil));
    } else {
        for expr in &body_exprs[..body_exprs.len() - 1] {
            compile_expr(expr, &mut body_chunk, heap, env, false)?;
            body_chunk.emit(Op::Pop);
        }
        // Last body expression is in tail position.
        compile_expr(body_exprs.last().unwrap(), &mut body_chunk, heap, env, true)?;
    }
    // Every lambda body ends with Return.
    body_chunk.emit(Op::Return);

    // Register the sub-chunk in the parent chunk and record its index.
    let code_offset = chunk.add_sub_chunk(body_chunk);

    let body_expr = if body_exprs.len() == 1 {
        body_exprs[0].clone()
    } else {
        Expr::List(
            std::iter::once(Expr::Symbol("begin".into()))
                .chain(body_exprs.iter().cloned())
                .collect(),
        )
    };

    chunk.emit(Op::MakeFunc {
        code_offset,
        params,
        body_expr: Box::new(body_expr),
    });
    Ok(())
}

// ── function call ─────────────────────────────────────────────────────────────

/// Compile a function call `(func arg...)`.
///
/// Strategy:
/// 1. Compile the function expression (push the callee).
/// 2. Compile each argument left-to-right (push args).
/// 3. Emit `TailCall(n)` if in tail position, else `Call(n)`.
fn compile_call(
    list: &[Expr],
    chunk: &mut Chunk,
    heap: &mut Heap,
    env: crate::expr::Env,
    tail: bool,
) -> Result<(), String> {
    let n_args = list.len() - 1;

    // Compile the callee.
    compile_expr(&list[0], chunk, heap, env, false)?;

    // Compile arguments left-to-right.
    for arg in &list[1..] {
        compile_expr(arg, chunk, heap, env, false)?;
    }

    if tail {
        chunk.emit(Op::TailCall(n_args));
    } else {
        chunk.emit(Op::Call(n_args));
    }
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn is_compilable_rec(expr: &Expr, qq_depth: usize, heap: &Heap, env: GcHandle) -> bool {
    match expr {
        Expr::Int(_) | Expr::Float(_) | Expr::Complex(_, _) | Expr::Bool(_) => true,
        Expr::Str(_) => true,
        Expr::Symbol(s) => {
            if qq_depth == 0 {
                s != "unquote" && s != "unquote-splicing"
            } else {
                true
            }
        }
        Expr::Func(_) => true,
        Expr::Lambda(_, body, env) => is_compilable_rec(body, qq_depth, heap, *env),
        Expr::Macro(..) => false,
        Expr::List(items) => {
            if items.is_empty() {
                return true;
            }
            if qq_depth > 0 {
                // Inside quasiquote: unquoted positions escape back to normal code.
                match qq_op_expr(expr) {
                    Some("unquote") => {
                        if items.len() != 2 {
                            return false;
                        }
                        if qq_depth == 1 {
                            is_compilable_rec(&items[1], 0, heap, env)
                        } else {
                            is_compilable_rec(&items[1], qq_depth - 1, heap, env)
                        }
                    }
                    Some("quasiquote") => {
                        if items.len() != 2 {
                            return false;
                        }
                        is_compilable_rec(&items[1], qq_depth + 1, heap, env)
                    }
                    _ => {
                        // General list in quasiquote
                        for item in items {
                            if qq_op_expr(item) == Some("unquote-splicing") {
                                if let Expr::List(inner) = item {
                                    if inner.len() != 2 {
                                        return false;
                                    }
                                    let next_depth = if qq_depth == 1 { 0 } else { qq_depth - 1 };
                                    if !is_compilable_rec(&inner[1], next_depth, heap, env) {
                                        return false;
                                    }
                                } else {
                                    unreachable!();
                                }
                            } else {
                                if !is_compilable_rec(item, qq_depth, heap, env) {
                                    return false;
                                }
                            }
                        }
                        true
                    }
                }
            } else {
                // Outside quasiquote (qq_depth == 0)
                if let Expr::Symbol(s) = &items[0] {
                    match s.as_str() {
                        "unquote" | "unquote-splicing" => return false,
                        // defmacro / defstruct / ccall always fall back to the tree-walker.
                        "defmacro" | "defstruct" | "ccall" | "import" => return false,
                        "lambda" => {
                            return items.iter().all(|e| is_compilable_rec(e, 0, heap, env));
                        }
                        "define" | "let" | "for" => {
                            return items.iter().all(|e| is_compilable_rec(e, 0, heap, env));
                        }
                        "quasiquote" => {
                            if items.len() != 2 {
                                return false;
                            }
                            return is_compilable_rec(&items[1], 1, heap, env);
                        }
                        "quote" => {
                            return items.len() == 2;
                        }
                        _ => {
                            // Key change: if the head symbol resolves to a macro
                            // in the current environment, this expression is not
                            // compilable by the VM — fall back to the tree-walker.
                            if let Ok(Expr::Macro(..)) = env_get(heap, env, s) {
                                return false;
                            }
                        }
                    }
                }
                // Recurse into all sub-expressions.
                items.iter().all(|e| is_compilable_rec(e, 0, heap, env))
            }
        }
    }
}

/// Recursively check if the expression is compilable in the conservative VM scope.
///
/// `heap` and `env` are required so the deep walk can detect macro calls at
/// any nesting depth — if any list node's head symbol resolves to
/// `Expr::Macro` in the current env, the whole expression is not compilable
/// and must be handed to the tree-walker.
pub fn is_compilable(expr: &Expr, heap: &Heap, env: GcHandle) -> bool {
    is_compilable_rec(expr, 0, heap, env)
}
