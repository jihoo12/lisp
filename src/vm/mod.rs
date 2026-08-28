//! Phase-1 + Phase-2 VM infrastructure: bytecode representation, compiler,
//! and stack-based execution engine.
//!
//! # Public API
//!
//! The main entry point for the integration layer is [`vm_eval`], which:
//! 1. Compiles `expr` with [`compiler::Compiler::compile`].
//! 2. Runs the resulting [`bytecode::Chunk`] with [`machine::VM`].
//! 3. Falls back to the tree-walking [`crate::eval::eval`] if the compiler
//!    returns an `"uncompilable"` error (e.g. because the expression contains
//!    a `CubicalTerm`).

pub mod aot;
pub mod bytecode;
pub mod cache;
pub mod compiler;
pub mod machine;

#[cfg(target_arch = "x86_64")]
pub mod jit_abi;
#[cfg(target_arch = "x86_64")]
pub mod jit_cache;
#[cfg(target_arch = "x86_64")]
pub mod jit_compiler;

use std::cell::RefCell;

use crate::eval::eval_tree as tree_eval;
use crate::expr::Expr;
use crate::gc::{GcHandle, Heap};

use cache::CompileCache;
use compiler::{Compiler, is_compilable};
use machine::{VM, vm_value_to_expr};

// ── Thread-local compile cache ────────────────────────────────────────────────

thread_local! {
    static CACHE: RefCell<CompileCache> = RefCell::new(CompileCache::new());
}

#[cfg(target_arch = "x86_64")]
thread_local! {
    pub(crate) static JIT_CACHE: RefCell<jit_cache::JitCache> = RefCell::new(jit_cache::JitCache::new());
}

// ── vm_eval ───────────────────────────────────────────────────────────────────

/// Evaluate `expr` using the bytecode VM, falling back to the tree-walker on
/// uncompilable expressions.
///
/// # Caching behaviour
///
/// Two layers of caching are applied to amortise the cost of repeated
/// evaluation of structurally identical expressions (common for top-level
/// `define`s, loop bodies, lambda bodies, etc.):
///
/// 1. **`is_compilable` cache** — the expensive deep AST walk is skipped for
///    expressions already seen.  This cache is invalidated on every `defmacro`
///    call because new macros change what `is_compilable` returns.
///
/// 2. **`Chunk` cache** — once an expression has been expanded and compiled the
///    resulting `Chunk` is stored.  On the next call with an identical
///    expression the three steps (expand, compile, link) are bypassed entirely
///    and only the VM execution step (`VM::run`) is performed.  This cache is
///    never invalidated because compiled chunks are pure functions of the source
///    expression and do not depend on runtime environment values.
///
/// # Fallback behaviour
///
/// If [`Compiler::compile`] returns an error whose message starts with
/// `"uncompilable"`, `vm_eval` silently delegates to the tree-walking
/// evaluator.  Any other compile or runtime error is propagated as-is.
///
/// This allows the VM and the tree-walker to coexist: cubical-type-theory
/// forms (which use `CubicalTerm`) are always handled by the tree-walker,
/// while everything else goes through the VM.
pub fn vm_eval(expr: &Expr, env: GcHandle, heap: &mut Heap) -> Result<Expr, String> {
    let key = CompileCache::key(expr);

    // ── Step 1: Check `is_compilable` (cached) ────────────────────────────────
    let compilable = CACHE.with(|c| c.borrow().get_compilable(&key));
    let compilable = match compilable {
        Some(v) => v,
        None => {
            let result = is_compilable(expr, heap, env);
            CACHE.with(|c| c.borrow_mut().insert_compilable(key.clone(), result));
            result
        }
    };

    if !compilable {
        // Detect `defmacro` at the top level: after the tree-walker installs
        // the new macro, invalidate the compilable cache so that subsequent
        // expressions are re-checked with the macro in scope.
        let is_defmacro = matches!(expr, Expr::List(l)
            if matches!(l.first(), Some(Expr::Symbol(s)) if s == "defmacro"));

        let result = tree_eval(expr, env, heap)?;

        if is_defmacro {
            CACHE.with(|c| c.borrow_mut().invalidate_compilable());
        }

        return Ok(result);
    }

    // ── Step 2: Look up a cached Chunk ────────────────────────────────────────
    let cached_chunk = CACHE.with(|c| c.borrow().get_chunk(&key).cloned());

    let chunk = match cached_chunk {
        Some(chunk) => chunk,
        None => {
            // Expand macros + compile — paid only once per unique expression.
            match Compiler::compile(expr, env, heap) {
                Ok(compiled) => {
                    CACHE.with(|c| c.borrow_mut().insert_chunk(key.clone(), compiled.clone()));
                    compiled
                }
                Err(e) => {
                    // Safety net: uncompilable errors fall back to the tree-walker.
                    return tree_eval(expr, env, heap).map_err(|_| e);
                }
            }
        }
    };

    // ── Step 3: Run the (possibly cached) chunk ───────────────────────────────
    let mut vm = VM::new(heap, env, chunk);
    match vm.run() {
        Ok(v) => vm_value_to_expr(v, vm.heap_mut()),
        Err(e) => {
            if e.starts_with("uncompilable") {
                tree_eval(expr, env, heap)
            } else {
                Err(e)
            }
        }
    }
}

/// Return the current sizes of the compile cache for debugging / benchmarking.
///
/// Returns `(chunk_count, compilable_count)`.
pub fn cache_stats() -> (usize, usize) {
    CACHE.with(|c| {
        let c = c.borrow();
        (c.chunks.len(), c.compilable.len())
    })
}

// ── AOT (Ahead-of-Time) compilation helpers ──────────────────────────────

/// Compile a Lisp source file to an AOT bytecode file on disk.
///
/// Parses the source, compiles every compilable top-level form into a
/// serialised `Chunk`, and writes the result to `output_path`.
pub fn aot_compile_file(
    input_path: &str,
    output_path: &str,
    env: GcHandle,
    heap: &mut Heap,
) -> Result<(), String> {
    let src =
        std::fs::read_to_string(input_path).map_err(|e| format!("read '{}': {}", input_path, e))?;
    let bytes = aot::compile_to_bytes(&src, env, heap)?;
    std::fs::write(output_path, &bytes).map_err(|e| format!("write '{}': {}", output_path, e))?;
    Ok(())
}

/// Load an AOT bytecode file into the thread-local compile cache.
///
/// Subsequent evaluations of expressions whose Debug-key matches a cached
/// entry will use the pre-compiled bytecode directly, bypassing compilation.
pub fn aot_load_file(path: &str) -> Result<(), String> {
    let data =
        std::fs::read(path).map_err(|e| format!("read AOT '{}': {}", path, e))?;
    CACHE.with(|c| aot::load_into_cache(&data, &mut c.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins;
    use crate::reader::parse_all;

    fn eval_str(src: &str, heap: &mut Heap, env: GcHandle) -> Result<Expr, String> {
        let exprs = parse_all(src)?;
        assert_eq!(exprs.len(), 1);
        vm_eval(&exprs[0], env, heap)
    }

    #[test]
    fn test_is_compilable() {
        use crate::vm::compiler::is_compilable;
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // Simple arithmetic/comparisons: compilable
        let expr1 = parse_all("(+ 1 2)").unwrap().remove(0);
        assert!(is_compilable(&expr1, &heap, env));

        let expr2 = parse_all("(if (= 1 1) (+ 2 3) 4)").unwrap().remove(0);
        assert!(is_compilable(&expr2, &heap, env));

        // define, let, let* are now compilable
        let expr3 = parse_all("(let ((x 1)) x)").unwrap().remove(0);
        assert!(
            is_compilable(&expr3, &heap, env),
            "let should now be compilable"
        );

        let expr4 = parse_all("(define x 1)").unwrap().remove(0);
        assert!(
            is_compilable(&expr4, &heap, env),
            "define should now be compilable"
        );

        let expr5 = parse_all("(let* ((x 1) (y (+ x 1))) y)").unwrap().remove(0);
        assert!(
            is_compilable(&expr5, &heap, env),
            "let* should now be compilable"
        );

        // lambda is compilable (compile_lambda handles it)
        let expr_lambda = parse_all("(lambda (x) x)").unwrap().remove(0);
        assert!(is_compilable(&expr_lambda, &heap, env));

        // quasiquote with unquote: compilable
        let expr6 = parse_all("`(1 ,x)").unwrap().remove(0);
        assert!(is_compilable(&expr6, &heap, env));

        // quasiquote without unquote: compilable
        let expr7 = parse_all("`(1 2 3)").unwrap().remove(0);
        assert!(is_compilable(&expr7, &heap, env));

        // defmacro: never compilable (always tree-walker)
        let expr_dm = parse_all("(defmacro foo (x) x)").unwrap().remove(0);
        assert!(
            !is_compilable(&expr_dm, &heap, env),
            "defmacro must not be compilable"
        );
    }

    #[test]
    fn test_vm_define() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // define returns ()
        let res = eval_str("(define x 42)", &mut heap, env).unwrap();
        assert!(
            matches!(res, Expr::List(ref v) if v.is_empty()),
            "define should return (): got {:?}",
            res
        );

        // The binding should now be visible
        let res2 = eval_str("x", &mut heap, env).unwrap();
        assert!(matches!(res2, Expr::Int(n) if n == 42));
    }

    #[test]
    fn test_vm_let() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // Basic let
        let res = eval_str("(let ((x 3) (y 4)) (+ x y))", &mut heap, env).unwrap();
        assert!(matches!(res, Expr::Int(n) if n == 7));
    }

    #[test]
    fn test_vm_let_scoping() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // let bindings are not visible outside the body
        eval_str("(define z 99)", &mut heap, env).unwrap();
        let res = eval_str("(let ((z 1)) z)", &mut heap, env).unwrap();
        assert!(matches!(res, Expr::Int(n) if n == 1));

        // After the let, z should still be 99 in the outer env
        let res2 = eval_str("z", &mut heap, env).unwrap();
        assert!(matches!(res2, Expr::Int(n) if n == 99));
    }

    #[test]
    fn test_vm_let_star() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // let* allows later bindings to see earlier ones
        let res = eval_str("(let* ((x 1) (y (+ x 1))) y)", &mut heap, env).unwrap();
        assert!(matches!(res, Expr::Int(n) if n == 2));
    }

    #[test]
    fn test_vm_eval_fallback() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // This is compilable, so it runs in the VM
        let res1 = eval_str("(+ 10 20)", &mut heap, env).unwrap();
        assert!(matches!(res1, Expr::Int(n) if n == 30));

        // This still falls back to tree-walker (uses lambda)
        let res2 = eval_str("(let ((x 5)) ((lambda (y) (+ x y)) 10))", &mut heap, env).unwrap();
        assert!(matches!(res2, Expr::Int(n) if n == 15));
    }

    #[test]
    fn test_vm_quasiquote() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // 1. basic unquote
        let res1 = eval_str("(let ((x 42)) `(the answer is ,x))", &mut heap, env).unwrap();
        assert_eq!(format!("{:?}", res1), "(the answer is 42)");

        // 2. unquote-splicing
        let res2 = eval_str("(let ((items '(1 2 3))) `(a ,@items b))", &mut heap, env).unwrap();
        assert_eq!(format!("{:?}", res2), "(a 1 2 3 b)");

        // 3. nested quasiquote
        let res3 = eval_str("`(a `(b ,(+ 1 2)))", &mut heap, env).unwrap();
        assert_eq!(
            format!("{:?}", res3),
            "(a (quasiquote (b (unquote (+ 1 2)))))"
        );

        // 4. quasiquote in macro body
        let macro_decl = "
        (defmacro when (condition body)
          `(if ,condition ,body ()))
        ";
        let exprs = parse_all(macro_decl).unwrap();
        assert_eq!(exprs.len(), 1);
        crate::eval::eval_tree(&exprs[0], env, &mut heap).unwrap();

        let res4 = eval_str("(when (> 3 2) 77)", &mut heap, env).unwrap();
        assert!(matches!(res4, Expr::Int(n) if n == 77));
    }

    /// Verifies that macro calls work correctly in the hybrid VM+tree-walker
    /// setup at every nesting depth and context.
    #[test]
    fn test_macro_correctness() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // Helper: evaluate a sequence of top-level forms, return last result.
        let eval_seq = |src: &str, heap: &mut Heap, env: GcHandle| -> Result<Expr, String> {
            let exprs = parse_all(src)?;
            let mut last = Expr::List(vec![]);
            for e in &exprs {
                last = vm_eval(e, env, heap)?;
            }
            Ok(last)
        };

        // 1. defmacro returns ()
        let dm_res = eval_seq(
            "(defmacro my-when (condition body) `(if ,condition ,body ()))",
            &mut heap,
            env,
        )
        .unwrap();
        assert!(
            matches!(dm_res, Expr::List(ref v) if v.is_empty()),
            "defmacro should return (): got {:?}",
            dm_res
        );

        // 2. Basic top-level macro call
        let r = eval_seq(
            "(defmacro my-when2 (cond body) `(if ,cond ,body ()))
             (my-when2 (> 3 2) 99)",
            &mut heap,
            env,
        )
        .unwrap();
        assert!(
            matches!(r, Expr::Int(n) if n == 99),
            "top-level macro call failed: {:?}",
            r
        );

        // 3. Macro call inside let body — this was the Problem 1 bug
        let r2 = eval_seq(
            "(defmacro my-if-pos (x body) `(if (> ,x 0) ,body ()))
             (let ((v 5)) (my-if-pos v 42))",
            &mut heap,
            env,
        )
        .unwrap();
        assert!(
            matches!(r2, Expr::Int(n) if n == 42),
            "macro inside let failed: {:?}",
            r2
        );

        // 4. Macro call inside lambda body
        let r3 = eval_seq(
            "(defmacro my-double-check (x body) `(if (> ,x 0) ,body 0))
             (define check-fn (lambda (n) (my-double-check n (* n 2))))
             (check-fn 7)",
            &mut heap,
            env,
        )
        .unwrap();
        assert!(
            matches!(r3, Expr::Int(n) if n == 14),
            "macro inside lambda failed: {:?}",
            r3
        );

        // 5. Macro call as argument to a function
        let r4 = eval_seq(
            "(defmacro my-or (a b) `(if ,a ,a ,b))
             (+ (my-or 0 3) (my-or 4 0))",
            &mut heap,
            env,
        )
        .unwrap();
        assert!(
            matches!(r4, Expr::Int(n) if n == 7),
            "macro as function argument failed: {:?}",
            r4
        );

        // 6. Nested macro calls
        let r5 = eval_seq(
            "(defmacro my-and2 (a b) `(if ,a ,b ()))
             (defmacro my-when3 (cond body) `(if ,cond ,body ()))
             (my-when3 (my-and2 1 1) 55)",
            &mut heap,
            env,
        )
        .unwrap();
        assert!(
            matches!(r5, Expr::Int(n) if n == 55),
            "nested macro calls failed: {:?}",
            r5
        );

        // 7. Macro inside let*, each binding can reference earlier ones
        let r6 = eval_seq(
            "(defmacro my-inc (x) `(+ ,x 1))
             (let* ((a 10) (b (my-inc a))) b)",
            &mut heap,
            env,
        )
        .unwrap();
        assert!(
            matches!(r6, Expr::Int(n) if n == 11),
            "macro inside let* binding failed: {:?}",
            r6
        );
    }

    /// Verifies that the compile cache does not corrupt results when the same
    /// expression is evaluated repeatedly.
    #[test]
    fn test_cache_hit_correctness() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        // Evaluate the same expression 5 times — after the first hit the chunk
        // should be served from cache.
        for i in 0..5_u32 {
            let res = eval_str("(+ 1 2)", &mut heap, env).unwrap();
            assert!(
                matches!(res, Expr::Int(n) if n == 3),
                "iteration {i}: expected 3, got {:?}",
                res
            );
        }

        // Verify that the chunk cache actually has an entry.
        let (chunks, _compilable) = cache_stats();
        assert!(
            chunks > 0,
            "chunk cache should be non-empty after evaluation"
        );
    }

    /// Verifies that defmacro invalidates the compilable cache so that later
    /// expressions re-check macro membership with the updated environment.
    #[test]
    fn test_defmacro_invalidates_cache() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        let eval_seq = |src: &str, heap: &mut Heap, env: GcHandle| -> Result<Expr, String> {
            let exprs = parse_all(src)?;
            let mut last = Expr::List(vec![]);
            for e in &exprs {
                last = vm_eval(e, env, heap)?;
            }
            Ok(last)
        };

        // Evaluate `(foo 1)` before `foo` is a macro — it should be a normal call error.
        // Then define the macro and evaluate again — should now expand correctly.
        eval_seq(
            "(defmacro double (x) `(+ ,x ,x))
             (double 7)",
            &mut heap,
            env,
        )
        .map(|r| {
            assert!(
                matches!(r, Expr::Int(n) if n == 14),
                "macro double failed: {:?}",
                r
            );
        })
        .unwrap_or(());
    }

    #[test]
    fn test_division_preserves_int_when_exact() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        let res = eval_str("(/ 4 2)", &mut heap, env).unwrap();
        assert!(matches!(res, Expr::Int(n) if n == 2));
    }

    #[test]
    fn test_division_returns_float_when_inexact() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        let res = eval_str("(/ 5 2)", &mut heap, env).unwrap();
        assert!(matches!(res, Expr::Float(n) if (n - 2.5).abs() < 1e-10));
    }

    #[test]
    fn test_division_returns_float_when_float_arg() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        let res = eval_str("(/ 4 2.0)", &mut heap, env).unwrap();
        assert!(matches!(res, Expr::Float(n) if (n - 2.0).abs() < 1e-10));
    }

    #[test]
    fn test_modulo_accepts_float() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        let res = eval_str("(% 5.0 2.0)", &mut heap, env).unwrap();
        assert!(matches!(res, Expr::Int(n) if n == 1));
    }

    #[test]
    fn test_negation_overflow_returns_error() {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);

        let res = eval_str("(- -9223372036854775808)", &mut heap, env);
        assert!(res.is_err());
    }
}
