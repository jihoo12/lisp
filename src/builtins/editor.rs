use std::io::{Read, Write};
use std::rc::Rc;

use crate::{
    builtins::str_arg,
    env::{Env, env_set},
    expr::Expr,
    gc::Heap,
    reader::parse_all,
    vm::vm_eval,
};

pub fn register_terminal(env: Env, heap: &mut Heap) {
    // (write str) — write string to stdout without newline, return ()
    env_set(
        heap,
        env,
        "write".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("write: expects exactly 1 argument (a string)".into());
            }
            let s = str_arg(&args[0])?;
            std::io::stdout()
                .write_all(s.as_bytes())
                .and_then(|_| std::io::stdout().flush())
                .map_err(|e| format!("write: {}", e))?;
            Ok(Expr::List(vec![]))
        })),
    );

    // (writeline str) — write string + newline, return ()
    env_set(
        heap,
        env,
        "writeline".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err("writeline: expects exactly 1 argument (a string)".into());
            }
            let s = str_arg(&args[0])?;
            std::io::stdout()
                .write_all(s.as_bytes())
                .and_then(|_| std::io::stdout().write_all(b"\n"))
                .and_then(|_| std::io::stdout().flush())
                .map_err(|e| format!("writeline: {}", e))?;
            Ok(Expr::List(vec![]))
        })),
    );

    // (read-byte) — read one byte from stdin, return int 0-255, or -1 on EOF
    env_set(
        heap,
        env,
        "read-byte".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if !args.is_empty() {
                return Err("read-byte: expects 0 arguments".into());
            }
            let mut buf = [0u8; 1];
            match std::io::stdin().read_exact(&mut buf) {
                Ok(()) => Ok(Expr::Int(buf[0] as i64)),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    Ok(Expr::Int(-1))
                }
                Err(e) => Err(format!("read-byte: {}", e)),
            }
        })),
    );

    // (exit [code]) — exit the process
    env_set(
        heap,
        env,
        "exit".into(),
        Expr::Func(Rc::new(|args, _heap| {
            let code = if args.is_empty() {
                0
            } else if args.len() == 1 {
                match &args[0] {
                    Expr::Int(n) => *n as i32,
                    Expr::Float(n) => *n as i32,
                    _ => return Err("exit: expected a number or no arguments".into()),
                }
            } else {
                return Err("exit: expects 0 or 1 arguments".into());
            };
            std::process::exit(code);
        })),
    );
}

pub fn register_command_line(env: Env, heap: &mut Heap) {
    // (command-line) → list of argument strings (excluding the program name)
    env_set(
        heap,
        env,
        "command-line".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if !args.is_empty() {
                return Err("command-line: expects 0 arguments".into());
            }
            let raw: Vec<String> = std::env::args().skip(1).collect();
            let list: Vec<Expr> = raw.into_iter().map(Expr::Str).collect();
            Ok(Expr::List(list))
        })),
    );
}

pub fn register_equal(env: Env, heap: &mut Heap) {
    // (equal? a b) — structural equality for any two values
    env_set(
        heap,
        env,
        "equal?".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err("equal?: expects exactly 2 arguments".into());
            }
            Ok(Expr::Bool(expr_equal(&args[0], &args[1])))
        })),
    );
}

fn expr_equal(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Int(an), Expr::Int(bn)) => an == bn,
        (Expr::Float(af), Expr::Float(bf)) => (af - bf).abs() < f64::EPSILON,
        (Expr::Complex(ra, ia), Expr::Complex(rb, ib)) => {
            (ra - rb).abs() < f64::EPSILON && (ia - ib).abs() < f64::EPSILON
        }
        (Expr::Bool(ab), Expr::Bool(bb)) => ab == bb,
        (Expr::Str(as_), Expr::Str(bs_)) => as_ == bs_,
        (Expr::Symbol(as_), Expr::Symbol(bs_)) => as_ == bs_,
        (Expr::List(a_list), Expr::List(b_list)) => {
            a_list.len() == b_list.len()
                && a_list.iter().zip(b_list.iter()).all(|(x, y)| expr_equal(x, y))
        }
        _ => false,
    }
}

pub fn register_string_extras(env: Env, heap: &mut Heap) {
    // (string ch ...) — convert integer codepoints to a string
    env_set(
        heap,
        env,
        "string".into(),
        Expr::Func(Rc::new(|args, _heap| {
            let mut s = String::new();
            for arg in args {
                match arg {
                    Expr::Int(n) => {
                        if let Some(ch) = char::from_u32(*n as u32) {
                            s.push(ch);
                        } else {
                            return Err(format!("string: invalid codepoint {}", n));
                        }
                    }
                    other => return Err(format!("string: expected integers, got {:?}", other)),
                }
            }
            Ok(Expr::Str(s))
        })),
    );

    // (string-ref s index) → int (Unicode codepoint), or -1 if out of range
    env_set(
        heap,
        env,
        "string-ref".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err("string-ref: expects (string-ref s index)".into());
            }
            let s = str_arg(&args[0])?;
            let idx = match &args[1] {
                Expr::Int(n) => *n,
                _ => return Err("string-ref: index must be an integer".into()),
            };
            let ch = s.chars().nth(idx as usize);
            match ch {
                Some(c) => Ok(Expr::Int(c as i64)),
                None => Ok(Expr::Int(-1)),
            }
        })),
    );

    // (string-split s separator) → list of strings
    env_set(
        heap,
        env,
        "string-split".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err("string-split: expects (string-split s separator)".into());
            }
            let s = str_arg(&args[0])?;
            let sep = str_arg(&args[1])?;
            let parts: Vec<Expr> = if sep.is_empty() {
                s.chars().map(|c| Expr::Str(c.to_string())).collect()
            } else {
                s.split(sep).map(|p| Expr::Str(p.to_string())).collect()
            };
            Ok(Expr::List(parts))
        })),
    );

    // (string-index-of s substr) → int index or -1 if not found
    env_set(
        heap,
        env,
        "string-index-of".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err("string-index-of: expects (string-index-of s substr)".into());
            }
            let s = str_arg(&args[0])?;
            let sub = str_arg(&args[1])?;
            match s.find(sub) {
                Some(pos) => Ok(Expr::Int(pos as i64)),
                None => Ok(Expr::Int(-1)),
            }
        })),
    );

    // (string-contains? s substr) → bool
    env_set(
        heap,
        env,
        "string-contains?".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err("string-contains?: expects (string-contains? s substr)".into());
            }
            let s = str_arg(&args[0])?;
            let sub = str_arg(&args[1])?;
            Ok(Expr::Bool(s.contains(sub)))
        })),
    );
}

pub fn register_load(env: Env, heap: &mut Heap) {
    // (load path) — load and evaluate a pi-lisp source file, return last value
    env_set(
        heap,
        env,
        "load".into(),
        Expr::Func(Rc::new(move |args, heap| {
            if args.len() != 1 {
                return Err("load: expects exactly 1 argument (a file path)".into());
            }
            let path = str_arg(&args[0])?.to_string();
            let source = std::fs::read_to_string(&path)
                .map_err(|e| format!("load: {}", e))?;
            let exprs = parse_all(&source)?;
            let mut result = Expr::List(vec![]);
            for expr in &exprs {
                result = vm_eval(expr, env, heap)?;
            }
            Ok(result)
        })),
    );
}

pub fn register_read_key(env: Env, heap: &mut Heap) {
    // (read-key) — read a single keypress, return an integer code
    // Regular bytes/control chars return their ASCII value (0-255).
    // Special keys return negative codes:
    //   -1  EOF
    //   -2  up arrow      -3  down arrow
    //   -4  right arrow    -5  left arrow
    //   -6  home           -7  end
    //   -8  page-up        -9  page-down
    //  -10  delete (Del)  -11  escape (Esc key alone)
    env_set(
        heap,
        env,
        "read-key".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if !args.is_empty() {
                return Err("read-key: expects 0 arguments".into());
            }
            Ok(Expr::Int(read_key_impl()?))
        })),
    );
}

#[cfg(unix)]
fn read_key_impl() -> Result<i64, String> {
    use std::io::Read;

    fn read_byte() -> Result<i64, String> {
        let mut buf = [0u8];
        match std::io::stdin().read_exact(&mut buf) {
            Ok(()) => Ok(buf[0] as i64),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(-1),
            Err(e) => Err(format!("read-key: {}", e)),
        }
    }

    fn poll_stdin(timeout_ms: i32) -> Result<bool, String> {
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if ret < 0 {
            Err(format!("read-key: poll failed: {}", std::io::Error::last_os_error()))
        } else {
            Ok(ret > 0)
        }
    }

    let b = read_byte()?;
    if b != 27 {
        // Not ESC — return as-is (backspace=127, enter=13, tab=9)
        return Ok(b);
    }

    // ESC pressed — check if more data follows
    if !poll_stdin(10)? {
        return Ok(-11); // Escape key alone
    }

    // Read the next byte
    let b2 = read_byte()?;
    if b2 == -1 {
        return Ok(-11);
    }

    if b2 == 91 {
        // CSI sequence: ESC [
        let b3 = read_byte()?;
        if b3 == -1 { return Ok(-11); }
        match b3 {
            65 => Ok(-2),   // A = up
            66 => Ok(-3),   // B = down
            67 => Ok(-4),   // C = right
            68 => Ok(-5),   // D = left
            72 => Ok(-6),   // H = home
            70 => Ok(-7),   // F = end
            49 => {         // 1 = possible home (1~), etc.
                let b4 = read_byte()?;
                if b4 == 126 { Ok(-10) } else { Ok(-11) } // 1~ = home (treat as delete for now)
            }
            51 => {          // 3~ = Delete
                let b4 = read_byte()?;
                if b4 == 126 { Ok(-10) } else { Ok(-11) }
            }
            53 => {          // 5~ = Page Up
                let b4 = read_byte()?;
                if b4 == 126 { Ok(-8) } else { Ok(-11) }
            }
            54 => {          // 6~ = Page Down
                let b4 = read_byte()?;
                if b4 == 126 { Ok(-9) } else { Ok(-11) }
            }
            _ => Ok(-11),
        }
    } else if b2 == 79 {
        // SS3 sequence: ESC O
        let b3 = read_byte()?;
        if b3 == -1 { return Ok(-11); }
        match b3 {
            72 => Ok(-6),   // H = home
            70 => Ok(-7),   // F = end
            _ => Ok(-11),
        }
    } else {
        Ok(-11) // stray ESC
    }
}

#[cfg(not(unix))]
fn read_key_impl() -> Result<i64, String> {
    let mut buf = [0u8];
    match std::io::stdin().read_exact(&mut buf) {
        Ok(()) => Ok(buf[0] as i64),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(-1),
        Err(e) => Err(format!("read-key: {}", e)),
    }
}

// ── Platform-specific helpers ────────────────────────────────────────────

#[cfg(unix)]
fn set_raw_mode(enable: bool) -> Result<(), String> {
    use std::mem::MaybeUninit;
    let fd = 0; // stdin

    // Get current terminal attributes
    let mut termios = MaybeUninit::<libc::termios>::uninit();
    let ret = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if ret != 0 {
        return Err(format!("tcgetattr failed: {}", std::io::Error::last_os_error()));
    }
    let mut t = unsafe { termios.assume_init() };

    if enable {
        // Raw mode: disable canonical mode, echo, signals, etc.
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        t.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        t.c_oflag &= !(libc::OPOST);
        t.c_cflag |= libc::CS8;
        // VMIN = 1, VTIME = 0: read returns as soon as 1 byte is available
        t.c_cc[libc::VMIN as usize] = 1;
        t.c_cc[libc::VTIME as usize] = 0;
    } else {
        // Cooked mode: sensible defaults (echo, canonical, signals)
        t.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
        t.c_iflag |= libc::IXON | libc::ICRNL;
        t.c_oflag |= libc::OPOST;
    }

    let ret = unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &t) };
    if ret != 0 {
        return Err(format!("tcsetattr failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_raw_mode(_enable: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn terminal_size_impl() -> Result<(i64, i64), String> {
    use std::mem::MaybeUninit;
    let mut ws = MaybeUninit::<libc::winsize>::uninit();
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, ws.as_mut_ptr()) };
    if ret != 0 {
        return Err(format!(
            "ioctl(TIOCGWINSZ) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let ws = unsafe { ws.assume_init() };
    Ok((ws.ws_row as i64, ws.ws_col as i64))
}

#[cfg(not(unix))]
fn terminal_size_impl() -> Result<(i64, i64), String> {
    Ok((24, 80))
}
