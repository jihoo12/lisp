use crate::expr::Expr;
use crate::gc::{GcHandle, Heap};
use crate::vm::bytecode::{Chunk, Op, Value};
use crate::vm::cache::CompileCache;
use crate::vm::compiler::{Compiler, is_compilable};

const MAGIC: &[u8; 4] = b"PIAO";
const VERSION: u32 = 1;

// ── Writer ───────────────────────────────────────────────────────────────

struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn w_u8(&mut self, v: u8) { self.buf.push(v); }
    fn w_u32(&mut self, v: u32) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn w_u64(&mut self, v: u64) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn w_i64(&mut self, v: i64) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn w_f64(&mut self, v: f64) { self.buf.extend_from_slice(&v.to_le_bytes()); }
    fn w_bool(&mut self, v: bool) { self.buf.push(if v { 1 } else { 0 }); }

    fn w_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        self.w_u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }

    fn w_value(&mut self, val: &Value) -> Result<(), String> {
        match val {
            Value::Int(n) => { self.w_u8(0); self.w_i64(*n); }
            Value::Float(n) => { self.w_u8(1); self.w_f64(*n); }
            Value::Complex(re, im) => { self.w_u8(2); self.w_f64(*re); self.w_f64(*im); }
            Value::Bool(b) => { self.w_u8(3); self.w_bool(*b); }
            Value::Str(s) => { self.w_u8(4); self.w_str(s); }
            Value::Symbol(s) => { self.w_u8(5); self.w_str(s); }
            Value::List(items) => {
                self.w_u8(6);
                self.w_u32(items.len() as u32);
                for item in items { self.w_value(item)?; }
            }
            Value::Nil => { self.w_u8(7); }
            Value::Builtin(_) => return Err("Cannot serialize Builtin value".into()),
            Value::Closure { .. } => return Err("Cannot serialize Closure value".into()),
        }
        Ok(())
    }

    fn w_expr(&mut self, expr: &Expr) -> Result<(), String> {
        match expr {
            Expr::Symbol(s) => { self.w_u8(0); self.w_str(s); }
            Expr::Int(n) => { self.w_u8(1); self.w_i64(*n); }
            Expr::Float(n) => { self.w_u8(2); self.w_f64(*n); }
            Expr::Complex(re, im) => { self.w_u8(3); self.w_f64(*re); self.w_f64(*im); }
            Expr::Bool(b) => { self.w_u8(4); self.w_bool(*b); }
            Expr::Str(s) => { self.w_u8(5); self.w_str(s); }
            Expr::List(items) => {
                self.w_u8(6);
                self.w_u32(items.len() as u32);
                for item in items { self.w_expr(item)?; }
            }
            Expr::Func(_) | Expr::Lambda(..) | Expr::Macro(..) => {
                return Err("Cannot serialize runtime-only Expr variant".into());
            }
        }
        Ok(())
    }

    fn w_op(&mut self, op: &Op) -> Result<(), String> {
        match op {
            Op::LoadConst(v) => { self.w_u8(0); self.w_value(v)?; }
            Op::LoadVar(s) => { self.w_u8(1); self.w_str(s); }
            Op::StoreVar(s) => { self.w_u8(2); self.w_str(s); }
            Op::Jump(t) => { self.w_u8(3); self.w_u64(*t as u64); }
            Op::JumpIfFalse(t) => { self.w_u8(4); self.w_u64(*t as u64); }
            Op::Return => { self.w_u8(5); }
            Op::MakeFunc { code_offset, params, body_expr } => {
                self.w_u8(6);
                self.w_u64(*code_offset as u64);
                self.w_u32(params.len() as u32);
                for p in params { self.w_str(p); }
                self.w_expr(body_expr)?;
            }
            Op::Call(n) => { self.w_u8(7); self.w_u64(*n as u64); }
            Op::TailCall(n) => { self.w_u8(8); self.w_u64(*n as u64); }
            Op::TreeEval(e) => { self.w_u8(9); self.w_expr(e)?; }
            Op::MakeList(n) => { self.w_u8(10); self.w_u64(*n as u64); }
            Op::PrependList => { self.w_u8(11); }
            Op::AppendSplice => { self.w_u8(12); }
            Op::LoadNil => { self.w_u8(13); }
            Op::Pop => { self.w_u8(14); }
            Op::PushEnv => { self.w_u8(15); }
            Op::PopEnv => { self.w_u8(16); }
            Op::StoreSelf(s) => { self.w_u8(17); self.w_str(s); }
            Op::AssignVar(s) => { self.w_u8(18); self.w_str(s); }
        }
        Ok(())
    }

    fn w_chunk(&mut self, chunk: &Chunk) -> Result<(), String> {
        self.w_u64(chunk.id);
        self.w_u32(chunk.ops.len() as u32);
        for op in &chunk.ops { self.w_op(op)?; }
        self.w_u32(chunk.sub_chunks.len() as u32);
        for sub in &chunk.sub_chunks { self.w_chunk(sub)?; }
        Ok(())
    }

    fn into_bytes(self) -> Vec<u8> { self.buf }
}

// ── Reader ───────────────────────────────────────────────────────────────

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self { Self { buf, pos: 0 } }

    fn r_u8(&mut self) -> Result<u8, String> {
        if self.pos + 1 > self.buf.len() {
            return Err("Unexpected end of data".into());
        }
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn r_u32(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.buf.len() {
            return Err("Unexpected end of data".into());
        }
        let bytes: [u8; 4] = self.buf[self.pos..self.pos + 4].try_into().unwrap();
        self.pos += 4;
        Ok(u32::from_le_bytes(bytes))
    }

    fn r_u64(&mut self) -> Result<u64, String> {
        if self.pos + 8 > self.buf.len() {
            return Err("Unexpected end of data".into());
        }
        let bytes: [u8; 8] = self.buf[self.pos..self.pos + 8].try_into().unwrap();
        self.pos += 8;
        Ok(u64::from_le_bytes(bytes))
    }

    fn r_i64(&mut self) -> Result<i64, String> {
        if self.pos + 8 > self.buf.len() {
            return Err("Unexpected end of data".into());
        }
        let bytes: [u8; 8] = self.buf[self.pos..self.pos + 8].try_into().unwrap();
        self.pos += 8;
        Ok(i64::from_le_bytes(bytes))
    }

    fn r_f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_bits(self.r_u64()?))
    }

    fn r_bool(&mut self) -> Result<bool, String> {
        match self.r_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(format!("Invalid bool value: {}", other)),
        }
    }

    fn r_str(&mut self) -> Result<String, String> {
        let len = self.r_u32()? as usize;
        if self.pos + len > self.buf.len() {
            return Err("Unexpected end of data reading string".into());
        }
        let s = std::str::from_utf8(&self.buf[self.pos..self.pos + len])
            .map_err(|e| format!("Invalid UTF-8: {}", e))?;
        self.pos += len;
        Ok(s.to_string())
    }

    fn r_value(&mut self) -> Result<Value, String> {
        let tag = self.r_u8()?;
        match tag {
            0 => Ok(Value::Int(self.r_i64()?)),
            1 => Ok(Value::Float(self.r_f64()?)),
            2 => { let re = self.r_f64()?; let im = self.r_f64()?; Ok(Value::Complex(re, im)) }
            3 => Ok(Value::Bool(self.r_bool()?)),
            4 => Ok(Value::Str(self.r_str()?)),
            5 => Ok(Value::Symbol(self.r_str()?)),
            6 => {
                let len = self.r_u32()? as usize;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len { items.push(self.r_value()?); }
                Ok(Value::List(items))
            }
            7 => Ok(Value::Nil),
            _ => Err(format!("Unknown Value tag: {}", tag)),
        }
    }

    fn r_expr(&mut self) -> Result<Expr, String> {
        let tag = self.r_u8()?;
        match tag {
            0 => Ok(Expr::Symbol(self.r_str()?)),
            1 => Ok(Expr::Int(self.r_i64()?)),
            2 => Ok(Expr::Float(self.r_f64()?)),
            3 => { let re = self.r_f64()?; let im = self.r_f64()?; Ok(Expr::Complex(re, im)) }
            4 => Ok(Expr::Bool(self.r_bool()?)),
            5 => Ok(Expr::Str(self.r_str()?)),
            6 => {
                let len = self.r_u32()? as usize;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len { items.push(self.r_expr()?); }
                Ok(Expr::List(items))
            }
            _ => Err(format!("Unknown Expr tag: {}", tag)),
        }
    }

    fn r_op(&mut self) -> Result<Op, String> {
        let tag = self.r_u8()?;
        match tag {
            0 => { let v = self.r_value()?; Ok(Op::LoadConst(v)) }
            1 => { let s = self.r_str()?; Ok(Op::LoadVar(s)) }
            2 => { let s = self.r_str()?; Ok(Op::StoreVar(s)) }
            3 => { let t = self.r_u64()? as usize; Ok(Op::Jump(t)) }
            4 => { let t = self.r_u64()? as usize; Ok(Op::JumpIfFalse(t)) }
            5 => Ok(Op::Return),
            6 => {
                let code_offset = self.r_u64()? as usize;
                let nparams = self.r_u32()? as usize;
                let mut params = Vec::with_capacity(nparams);
                for _ in 0..nparams { params.push(self.r_str()?); }
                let body_expr = Box::new(self.r_expr()?);
                Ok(Op::MakeFunc { code_offset, params, body_expr })
            }
            7 => { let n = self.r_u64()? as usize; Ok(Op::Call(n)) }
            8 => { let n = self.r_u64()? as usize; Ok(Op::TailCall(n)) }
            9 => { let e = self.r_expr()?; Ok(Op::TreeEval(e)) }
            10 => { let n = self.r_u64()? as usize; Ok(Op::MakeList(n)) }
            11 => Ok(Op::PrependList),
            12 => Ok(Op::AppendSplice),
            13 => Ok(Op::LoadNil),
            14 => Ok(Op::Pop),
            15 => Ok(Op::PushEnv),
            16 => Ok(Op::PopEnv),
            17 => { let s = self.r_str()?; Ok(Op::StoreSelf(s)) }
            18 => { let s = self.r_str()?; Ok(Op::AssignVar(s)) }
            _ => Err(format!("Unknown Op tag: {}", tag)),
        }
    }

    fn r_chunk(&mut self) -> Result<Chunk, String> {
        let id = self.r_u64()?;
        let nops = self.r_u32()? as usize;
        let mut ops = Vec::with_capacity(nops);
        for _ in 0..nops { ops.push(self.r_op()?); }
        let nsub = self.r_u32()? as usize;
        let mut sub_chunks = Vec::with_capacity(nsub);
        for _ in 0..nsub { sub_chunks.push(self.r_chunk()?); }
        Ok(Chunk { ops, sub_chunks, id })
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Compile a complete source string into a serialized AOT byte blob.
pub fn compile_to_bytes(src: &str, env: GcHandle, heap: &mut Heap) -> Result<Vec<u8>, String> {
    let exprs = crate::reader::parse_all(src)?;
    let mut writer = Writer::new();
    writer.buf.extend_from_slice(MAGIC);
    writer.w_u32(VERSION);
    let mut count: u32 = 0;
    for expr in &exprs {
        if is_compilable(expr, heap, env) {
            count += 1;
        }
    }
    writer.w_u32(count);
    for expr in &exprs {
        if is_compilable(expr, heap, env) {
            let key = CompileCache::key(expr);
            let chunk = Compiler::compile(expr, env, heap)?;
            writer.w_str(&key);
            writer.w_chunk(&chunk)?;
        }
    }
    Ok(writer.into_bytes())
}

/// Deserialize an AOT blob and insert all entries into a [`CompileCache`].
///
/// Both `chunks` and `compilable` maps are populated so that
/// [`vm_eval`](crate::vm::vm_eval) finds cached bytecode without re-checking
/// compilability.
pub fn load_into_cache(data: &[u8], cache: &mut CompileCache) -> Result<(), String> {
    if data.len() < 4 || &data[..4] != MAGIC {
        return Err("Not a valid AOT file (bad magic)".into());
    }
    let mut reader = Reader::new(data);
    reader.pos = 4;
    let version = reader.r_u32()?;
    if version != VERSION {
        return Err(format!("Unsupported AOT version: {}", version));
    }
    let num_entries = reader.r_u32()? as usize;
    for _ in 0..num_entries {
        let key = reader.r_str()?;
        let chunk = reader.r_chunk()?;
        cache.chunks.insert(key.clone(), chunk);
        cache.compilable.insert(key, true);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins;
    use crate::gc::Heap;
    use crate::reader::parse_all;
    use crate::vm::machine::{VM, vm_value_to_expr};
    use crate::vm::compiler::Compiler;

    fn setup() -> (Heap, GcHandle) {
        let mut heap = Heap::new();
        let env = builtins::global_env(&mut heap);
        (heap, env)
    }

    /// Round-trip a simple expression through serialize/deserialize and check
    /// the VM produces the same result as a fresh compile+run.
    fn roundtrip_and_run(src: &str) -> Result<Expr, String> {
        let (mut heap, env) = setup();
        let exprs = parse_all(src)?;
        assert_eq!(exprs.len(), 1);

        let chunk = Compiler::compile(&exprs[0], env, &mut heap)?;

        let mut w = Writer::new();
        w.w_chunk(&chunk).unwrap();
        let bytes = w.into_bytes();

        let mut r = Reader::new(&bytes);
        let restored = r.r_chunk().unwrap();

        let mut vm = VM::new(&mut heap, env, restored);
        let val = vm.run()?;
        vm_value_to_expr(val, &mut heap)
    }

    #[test]
    fn test_aot_roundtrip_add() {
        let res = roundtrip_and_run("(+ 1 2)").unwrap();
        assert!(matches!(res, Expr::Int(n) if n == 3));
    }

    #[test]
    fn test_aot_roundtrip_if() {
        let res = roundtrip_and_run("(if (= 1 1) 42 0)").unwrap();
        assert!(matches!(res, Expr::Int(n) if n == 42));
    }

    #[test]
    fn test_aot_roundtrip_lambda() {
        let (mut heap, env) = setup();
        let src = "(define double (lambda (x) (* x 2)))";
        let exprs = parse_all(src).unwrap();
        assert_eq!(exprs.len(), 1);

        let chunk = Compiler::compile(&exprs[0], env, &mut heap).unwrap();

        let mut w = Writer::new();
        w.w_chunk(&chunk).unwrap();
        let bytes = w.into_bytes();

        let mut r = Reader::new(&bytes);
        let restored = r.r_chunk().unwrap();

        let mut vm = VM::new(&mut heap, env, restored);
        let val = vm.run().unwrap();
        let result = vm_value_to_expr(val, &mut heap).unwrap();
        assert!(matches!(result, Expr::List(ref v) if v.is_empty()));
    }

    #[test]
    fn test_aot_compile_and_load_integration() {
        let (mut heap, env) = setup();
        let src = "(+ 1 2)";

        let bytes = compile_to_bytes(src, env, &mut heap).unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.len() > 8);

        let mut cache = CompileCache::new();
        load_into_cache(&bytes, &mut cache).unwrap();
        assert_eq!(cache.chunks.len(), 1);
        assert_eq!(cache.compilable.len(), 1);
    }

    #[test]
    fn test_aot_invalid_magic() {
        let mut cache = CompileCache::new();
        let result = load_into_cache(b"BADMAGIC", &mut cache);
        assert!(result.is_err());
    }
}
