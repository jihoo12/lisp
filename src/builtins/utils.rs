use std::rc::Rc;
use std::thread;

use crate::gc::Heap;
use crate::{
    builtins::{display_str, num, str_arg},
    env::{Env, env_set},
    expr::Expr,
};

pub fn register_strings(env: Env, heap: &mut Heap) {
    env_set(
        heap,
        env,
        "string?".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("string?: expects exactly 1 argument".into());
            }
            Ok(Expr::Bool(matches!(&args[0], Expr::Str(_))))
        })),
    );

    env_set(
        heap,
        env,
        "string-append".into(),
        Expr::Func(Rc::new(|args, _heap| {
            let mut out = String::new();
            for a in args {
                out.push_str(str_arg(a)?);
            }
            Ok(Expr::Str(out))
        })),
    );

    env_set(
        heap,
        env,
        "string-length".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("string-length: expects exactly 1 argument".into());
            }
            Ok(Expr::Int(str_arg(&args[0])?.chars().count() as i64))
        })),
    );

    macro_rules! string_cmp_fn {
        ($op:tt) => {
            Expr::Func(Rc::new(|args, _heap| {
                if args.len() != 2 {
                    return Err("string comparison expects exactly 2 arguments".into());
                }
                let a = str_arg(&args[0])?;
                let b = str_arg(&args[1])?;
                Ok(Expr::Bool(a $op b))
            }))
        };
    }

    env_set(heap, env, "string=?".into(), string_cmp_fn!(==));
    env_set(heap, env, "string<?".into(), string_cmp_fn!(<));
    env_set(heap, env, "string>?".into(), string_cmp_fn!(>));
    env_set(heap, env, "string<=?".into(), string_cmp_fn!(<=));
    env_set(heap, env, "string>=?".into(), string_cmp_fn!(>=));

    env_set(
        heap,
        env,
        "string->number".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("string->number: expects exactly 1 argument".into());
            }
            let s = str_arg(&args[0])?;
            s.parse::<f64>()
                .map(Expr::Float)
                .map_err(|_| format!("string->number: not a valid number: {:?}", s))
        })),
    );

    env_set(
        heap,
        env,
        "number->string".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("number->string: expects exactly 1 argument".into());
            }
            Ok(Expr::Str(format!("{}", num(&args[0])?)))
        })),
    );

    env_set(
        heap,
        env,
        "string->symbol".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("string->symbol: expects exactly 1 argument".into());
            }
            Ok(Expr::Symbol(str_arg(&args[0])?.to_string()))
        })),
    );

    env_set(
        heap,
        env,
        "symbol->string".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("symbol->string: expects exactly 1 argument".into());
            }
            match &args[0] {
                Expr::Symbol(s) => Ok(Expr::Str(s.clone())),
                other => Err(format!("symbol->string: expected symbol, got {:?}", other)),
            }
        })),
    );

    env_set(
        heap,
        env,
        "string-upcase".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("string-upcase: expects exactly 1 argument".into());
            }
            Ok(Expr::Str(str_arg(&args[0])?.to_uppercase()))
        })),
    );

    env_set(
        heap,
        env,
        "string-downcase".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("string-downcase: expects exactly 1 argument".into());
            }
            Ok(Expr::Str(str_arg(&args[0])?.to_lowercase()))
        })),
    );

    // (substring s start end) — character-indexed, end-exclusive, like Scheme.
    env_set(
        heap,
        env,
        "substring".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 3 {
                return Err("substring: expects (substring s start end)".into());
            }
            let s = str_arg(&args[0])?;
            let start = num(&args[1])? as usize;
            let end = num(&args[2])? as usize;
            let chars: Vec<char> = s.chars().collect();
            if start > end || end > chars.len() {
                return Err(format!(
                    "substring: index out of range (start={}, end={}, len={})",
                    start,
                    end,
                    chars.len()
                ));
            }
            Ok(Expr::Str(chars[start..end].iter().collect()))
        })),
    );
}

pub fn register_misc(env: Env, heap: &mut Heap) {
    env_set(
        heap,
        env,
        "print".into(),
        Expr::Func(Rc::new(|args, _heap| {
            for a in args {
                print!("{} ", display_str(a));
            }
            println!();
            Ok(Expr::List(vec![]))
        })),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Threading — isolated worker interpreters
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum ThreadValue {
    Symbol(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    List(Vec<ThreadValue>),
}

impl ThreadValue {
    fn from_expr(expr: Expr) -> Result<Self, String> {
        match expr {
            Expr::Symbol(s) => Ok(ThreadValue::Symbol(s)),
            Expr::Int(n) => Ok(ThreadValue::Int(n)),
            Expr::Float(n) => Ok(ThreadValue::Float(n)),
            Expr::Bool(b) => Ok(ThreadValue::Bool(b)),
            Expr::Str(s) => Ok(ThreadValue::Str(s)),
            Expr::List(items) => items
                .into_iter()
                .map(ThreadValue::from_expr)
                .collect::<Result<Vec<_>, _>>()
                .map(ThreadValue::List),
            Expr::Complex(_, _) => {
                Err("worker returned a complex number, which cannot cross threads".into())
            }
            Expr::Func(_) => {
                Err("worker returned a builtin function, which cannot cross threads".into())
            }
            Expr::Lambda(..) => Err("worker returned a lambda, which cannot cross threads".into()),
            Expr::Macro(..) => Err("worker returned a macro, which cannot cross threads".into()),
        }
    }

    fn into_expr(self) -> Expr {
        match self {
            ThreadValue::Symbol(s) => Expr::Symbol(s),
            ThreadValue::Int(n) => Expr::Int(n),
            ThreadValue::Float(n) => Expr::Float(n),
            ThreadValue::Bool(b) => Expr::Bool(b),
            ThreadValue::Str(s) => Expr::Str(s),
            ThreadValue::List(items) => {
                Expr::List(items.into_iter().map(ThreadValue::into_expr).collect())
            }
        }
    }
}

fn eval_worker_source(src: String) -> Result<ThreadValue, String> {
    let exprs = crate::reader::parse_all(&src)?;
    let mut heap = Heap::new();
    let global_env = crate::builtins::global_env(&mut heap);
    let mut result = Expr::List(vec![]);

    for expr in exprs {
        result = crate::eval::eval(&expr, global_env, &mut heap)?;
    }

    ThreadValue::from_expr(result)
}

fn parallel_eval_sources(sources: Vec<String>) -> Result<Expr, String> {
    let handles: Vec<_> = sources
        .into_iter()
        .map(|src| thread::spawn(move || eval_worker_source(src)))
        .collect();

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        let value = handle
            .join()
            .map_err(|_| "parallel-eval: worker thread panicked".to_string())??;
        results.push(value.into_expr());
    }

    Ok(Expr::List(results))
}

pub fn register_threading(env: Env, heap: &mut Heap) {
    env_set(
        heap,
        env,
        "thread-eval".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("thread-eval: expects exactly 1 source string".into());
            }
            let results = parallel_eval_sources(vec![str_arg(&args[0])?.to_string()])?;
            match results {
                Expr::List(mut items) => Ok(items.pop().unwrap_or_else(|| Expr::List(vec![]))),
                other => Ok(other),
            }
        })),
    );

    env_set(
        heap,
        env,
        "parallel-eval".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("parallel-eval: expects exactly 1 list of source strings".into());
            }
            let sources = match &args[0] {
                Expr::List(items) => items
                    .iter()
                    .map(|item| str_arg(item).map(str::to_string))
                    .collect::<Result<Vec<_>, _>>()?,
                other => return Err(format!("parallel-eval: expected a list, got {:?}", other)),
            };
            parallel_eval_sources(sources)
        })),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// I/O — user input
// ─────────────────────────────────────────────────────────────────────────────

/// Registers interactive-input builtins.
///
/// `(read-line)`            — reads one line from stdin, returns `Expr::Str`.
/// `(read-line prompt)`     — prints `prompt` (no newline) first, then reads.
pub fn register_io(env: Env, heap: &mut Heap) {
    env_set(
        heap,
        env,
        "read-line".into(),
        Expr::Func(Rc::new(|args, _heap| {
            use std::io::Write;
            if args.len() > 1 {
                return Err("read-line: expects 0 or 1 arguments".into());
            }
            // Optional prompt — flush so it appears before blocking.
            if let Some(p) = args.first() {
                print!("{}", crate::builtins::display_str(p));
                std::io::stdout().flush().map_err(|e| e.to_string())?;
            }
            // Use the shared BufReader from main so we never race with the REPL.
            match crate::helper::shared_read_line()? {
                Some(line) => Ok(Expr::Str(line)),
                None => Ok(Expr::Str(String::new())), // EOF → empty string
            }
        })),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// File I/O
// ─────────────────────────────────────────────────────────────────────────────

/// Registers file-system builtins.
///
/// `(file-read   path)`          — read whole file, return `Expr::Str`.
/// `(file-write  path content)`  — overwrite file with string content.
/// `(file-append path content)`  — append string content to file.
/// `(file-exists? path)`         — return 1.0 / 0.0.
/// `(file-delete  path)`         — delete file; returns `()`.
pub fn register_file(env: Env, heap: &mut Heap) {
    // (file-read path)
    env_set(
        heap,
        env,
        "file-read".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("file-read: expects exactly 1 argument".into());
            }
            let path = str_arg(&args[0])?;
            std::fs::read_to_string(path)
                .map(Expr::Str)
                .map_err(|e| format!("file-read: {}: {}", path, e))
        })),
    );

    // (file-write path content)
    env_set(
        heap,
        env,
        "file-write".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err("file-write: expects (file-write path content)".into());
            }
            let path = str_arg(&args[0])?;
            let content = str_arg(&args[1])?;
            std::fs::write(path, content)
                .map(|_| Expr::List(vec![]))
                .map_err(|e| format!("file-write: {}: {}", path, e))
        })),
    );

    // (file-append path content)
    env_set(
        heap,
        env,
        "file-append".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err("file-append: expects (file-append path content)".into());
            }
            let path = str_arg(&args[0])?;
            let content = str_arg(&args[1])?;
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| f.write_all(content.as_bytes()))
                .map(|_| Expr::List(vec![]))
                .map_err(|e| format!("file-append: {}: {}", path, e))
        })),
    );

    // (file-exists? path)
    env_set(
        heap,
        env,
        "file-exists?".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("file-exists?: expects exactly 1 argument".into());
            }
            let path = str_arg(&args[0])?;
            Ok(Expr::Bool(std::path::Path::new(path).exists()))
        })),
    );

    // (file-delete path)
    env_set(
        heap,
        env,
        "file-delete".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("file-delete: expects exactly 1 argument".into());
            }
            let path = str_arg(&args[0])?;
            std::fs::remove_file(path)
                .map(|_| Expr::List(vec![]))
                .map_err(|e| format!("file-delete: {}: {}", path, e))
        })),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// OS — shell execution
// ─────────────────────────────────────────────────────────────────────────────

/// Registers OS / process builtins.
///
/// `(shell cmd)`                    — run `cmd` via `sh -c`, block until done,
///                                    return captured stdout as `Expr::Str`.
/// `(shell-status cmd)`             — same but return exit code as `Expr::Number`.
pub fn register_os(env: Env, heap: &mut Heap) {
    use std::process::Command;

    // (shell cmd-string) → Expr::Str  (captured stdout)
    env_set(
        heap,
        env,
        "shell".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("shell: expects exactly 1 argument".into());
            }
            let cmd = str_arg(&args[0])?;
            let out = Command::new("sh")
                .args(["-c", cmd])
                .output()
                .map_err(|e| format!("shell: {}", e))?;
            // Combine stdout; ignore stderr (available via shell redirection if needed).
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            Ok(Expr::Str(stdout))
        })),
    );

    // (shell-status cmd-string) → Expr::Number  (exit code)
    env_set(
        heap,
        env,
        "shell-status".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("shell-status: expects exactly 1 argument".into());
            }
            let cmd = str_arg(&args[0])?;
            let status = Command::new("sh")
                .args(["-c", cmd])
                .status()
                .map_err(|e| format!("shell-status: {}", e))?;
            Ok(Expr::Int(status.code().unwrap_or(-1) as i64))
        })),
    );
}
