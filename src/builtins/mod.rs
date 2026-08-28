#[cfg(target_arch = "x86_64")]
mod asm;
mod base;
pub mod cffi;
mod clos;
mod editor;
mod network;
mod utils;

use std::rc::Rc;

use crate::env::{Env, new_env, env_set};
use crate::expr::Expr;
use crate::gc::Heap;

/// Extracts a real number from an Expr, or errors with context.
/// Complex numbers with zero imaginary part are accepted.
pub(crate) fn num(e: &Expr) -> Result<f64, String> {
    match e {
        Expr::Int(n) => Ok(*n as f64),
        Expr::Float(n) => Ok(*n),
        Expr::Complex(re, im) => {
            if *im == 0.0 {
                Ok(*re)
            } else {
                Err(format!(
                    "expected real number, got complex {:?}",
                    e
                ))
            }
        }
        Expr::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        other => Err(format!("expected number, got {:?}", other)),
    }
}

/// Extracts a complex number from an Expr, converting reals to complex.
pub(crate) fn complex_arg(e: &Expr) -> Result<(f64, f64), String> {
    match e {
        Expr::Int(n) => Ok((*n as f64, 0.0)),
        Expr::Float(n) => Ok((*n, 0.0)),
        Expr::Complex(re, im) => Ok((*re, *im)),
        Expr::Bool(b) => Ok((if *b { 1.0 } else { 0.0 }, 0.0)),
        other => Err(format!("expected number, got {:?}", other)),
    }
}

/// Returns true if any of the arguments is a complex number.
pub(crate) fn any_complex(args: &[Expr]) -> bool {
    args.iter().any(|a| matches!(a, Expr::Complex(_, _)))
}

/// Extracts a string slice from an Expr::Str, or errors with context.
pub(crate) fn str_arg(e: &Expr) -> Result<&str, String> {
    match e {
        Expr::Str(s) => Ok(s.as_str()),
        other => Err(format!("expected string, got {:?}", other)),
    }
}

/// Renders an Expr for `print`/`display`: strings print as their raw text
/// (no surrounding quotes), everything else uses its normal Debug form.
pub(crate) fn display_str(e: &Expr) -> String {
    match e {
        Expr::Str(s) => s.clone(),
        other => format!("{:?}", other),
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Builds the global environment populated with all builtin procedures.
///
/// ### Signature change from the Rc era
///
/// Previously returned a self-contained `Env` (`Rc<RefCell<EnvData>>`).
/// Now takes the interpreter's single `Heap` so the global frame is
/// allocated on the shared heap — the only place GC-managed frames may live.
/// Returns a `GcHandle` (aliased as `Env`) that the caller holds as the
/// permanent GC root.
pub fn global_env(heap: &mut Heap) -> Env {
    let env = new_env(heap, None);

    base::register_arithmetic(env, heap);
    base::register_comparisons(env, heap);
    base::register_lists(env, heap);
    base::register_higher_order(env, heap);
    base::register_complex(env, heap);
    utils::register_strings(env, heap);
    utils::register_misc(env, heap);
    utils::register_threading(env, heap);
    utils::register_file(env, heap);
    utils::register_io(env, heap);
    utils::register_os(env, heap);
    cffi::register_ffi(env, heap);
    network::register_network(env, heap);
    #[cfg(target_arch = "x86_64")]
    asm::register_assembler(env, heap);
    #[cfg(target_arch = "x86_64")]
    asm::register_load_asm(env, heap);
    #[cfg(target_arch = "x86_64")]
    asm::register_load_asm_parallel(env, heap);
    editor::register_terminal(env, heap);
    editor::register_command_line(env, heap);
    editor::register_equal(env, heap);
    editor::register_string_extras(env, heap);
    editor::register_read_key(env, heap);
    editor::register_load(env, heap);

    clos::register_clos(env, heap);

    register_aot(env, heap);

    env
}

fn register_aot(env: Env, heap: &mut Heap) {
    env_set(
        heap,
        env,
        "aot-compile".into(),
        Expr::Func(Rc::new(|args, heap| {
            if args.len() < 1 || args.len() > 3 {
                return Err(format!(
                    "aot-compile: expected 1-2 arguments (input [output]), got {}",
                    args.len()
                ));
            }
            let input = str_arg(&args[0])?.to_string();
            let output = if args.len() >= 2 {
                str_arg(&args[1])?.to_string()
            } else {
                let p = std::path::Path::new(&input);
                let stem = p.file_stem().unwrap_or_default().to_str().unwrap_or("out");
                format!("{}.aot", stem)
            };
            let global = crate::builtins::global_env(heap);
            crate::vm::aot_compile_file(&input, &output, global, heap)?;
            Ok(Expr::Str(output))
        })),
    );

    env_set(
        heap,
        env,
        "aot-load".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err(format!(
                    "aot-load: expected 1 argument (path), got {}",
                    args.len()
                ));
            }
            let path = str_arg(&args[0])?;
            crate::vm::aot_load_file(path)?;
            Ok(Expr::List(vec![]))
        })),
    );
}
