use crate::gc::GcHandle;
use crate::vm::bytecode::Chunk;
use crate::vm::machine::{VM, VmValue};
use std::ffi::CStr;
use std::ptr;
use std::rc::Rc;

/// Passed to every JIT-compiled chunk. The JIT frame gives the
/// native code access to the VM's operand stack and environment.
#[repr(C)]
pub struct JitFrame {
    /// Pointer to the base of the VM operand stack slice for this frame.
    pub stack_ptr: *mut u64,
    /// Number of values currently on the stack (in/out).
    pub stack_len: usize,
    /// Opaque handle to the GC environment (passed through to Rust helpers).
    pub env: GcHandle,
    /// Output: result tag (0 = Number, 1 = Nil, …)
    pub result_tag: u64,
    /// Output: result payload (f64 bits for Number, ptr for String, …)
    pub result_val: u64,
    /// Non-zero on runtime error; points to a static Rust error string.
    pub error: *const u8,

    // -- Rust-only fields below this line (not accessed by JIT code) --
    /// Parallel array for tags. JIT code doesn't touch this during numeric hot paths,
    /// but helpers will need it.
    pub tag_ptr: *mut u64,

    /// Parallel array for full VmValues (used by helpers that need to reconstruct Objects).
    pub val_ptr: *mut VmValue,

    pub capacity: usize,

    /// Raw pointer to the VM so helpers can access the Heap.
    pub vm_ptr: *mut std::ffi::c_void,

    /// Raw pointer to the current Chunk (needed by MakeFunc, StoreSelf, etc.).
    pub chunk_ptr: *const Chunk,
}

impl JitFrame {
    pub fn new(vm: &mut VM, chunk: &Chunk) -> Self {
        // We set up a separate parallel array for JIT execution.
        // For phase 1 we just allocate a fresh stack of size 1024.
        let capacity = 1024;
        let mut stack_vals = vec![0u64; capacity];
        let actual_capacity = stack_vals.capacity();
        let mut stack_tags = vec![0u64; actual_capacity];
        let mut val_refs = vec![VmValue::Nil; actual_capacity];

        let env = vm.frames.last().map(|f| f.env).expect("JIT needs an env");

        let frame = JitFrame {
            stack_ptr: stack_vals.as_mut_ptr(),
            tag_ptr: stack_tags.as_mut_ptr(),
            val_ptr: val_refs.as_mut_ptr(),
            stack_len: 0,
            capacity: actual_capacity,
            env,
            result_tag: 0,
            result_val: 0,
            error: ptr::null(),
            vm_ptr: vm as *mut VM as *mut std::ffi::c_void,
            chunk_ptr: chunk as *const Chunk,
        };

        // Leak the vecs so they stay alive during JIT execution
        std::mem::forget(stack_vals);
        std::mem::forget(stack_tags);
        std::mem::forget(val_refs);

        frame
    }

    pub fn into_vm_value(self) -> Result<VmValue, String> {
        // Recover the leaked vecs
        unsafe {
            let _ = Vec::from_raw_parts(self.stack_ptr, self.capacity, self.capacity);
            let _ = Vec::from_raw_parts(self.tag_ptr, self.capacity, self.capacity);
            let _ = Vec::from_raw_parts(self.val_ptr, self.capacity, self.capacity);
        }

        if !self.error.is_null() {
            let err_str = unsafe {
                CStr::from_ptr(self.error as *const i8)
                    .to_string_lossy()
                    .into_owned()
            };
            return Err(err_str);
        }

        match self.result_tag {
            0 => Ok(VmValue::Float(f64::from_bits(self.result_val))),
            1 => Ok(VmValue::Nil),
            // For strings, lists etc., we might retrieve them from a global or the helper would
            // have placed them somewhere. Phase 1 only returns Number/Nil directly.
            _ => Err("Unsupported JIT return tag".to_string()),
        }
    }

    pub fn push_val(&mut self, val: VmValue) {
        if self.stack_len >= self.capacity {
            self.error = c"JIT Stack overflow".as_ptr() as *const u8;
            return;
        }
        let idx = self.stack_len;
        unsafe {
            match val {
                VmValue::Int(n) => {
                    *self.tag_ptr.add(idx) = 0;
                    *self.stack_ptr.add(idx) = (n as f64).to_bits();
                }
                VmValue::Float(n) => {
                    *self.tag_ptr.add(idx) = 0;
                    *self.stack_ptr.add(idx) = n.to_bits();
                }
                VmValue::Bool(b) => {
                    *self.tag_ptr.add(idx) = 0;
                    *self.stack_ptr.add(idx) = f64::to_bits(if b { 1.0 } else { 0.0 });
                }
                VmValue::Nil => {
                    *self.tag_ptr.add(idx) = 1; // JitTag::Nil
                    *self.stack_ptr.add(idx) = 0;
                }
                other => {
                    *self.tag_ptr.add(idx) = 2; // Object
                    *self.stack_ptr.add(idx) = 0;
                    std::ptr::write(self.val_ptr.add(idx), other);
                }
            }
        }
        self.stack_len += 1;
    }

    pub fn pop_val(&mut self) -> Result<VmValue, ()> {
        if self.stack_len == 0 {
            self.error = c"JIT Stack underflow".as_ptr() as *const u8;
            return Err(());
        }
        self.stack_len -= 1;
        let idx = self.stack_len;
        unsafe {
            let tag = *self.tag_ptr.add(idx);
            let val = *self.stack_ptr.add(idx);
            if tag == 0 {
                Ok(VmValue::Float(f64::from_bits(val)))
            } else if tag == 1 {
                Ok(VmValue::Nil)
            } else {
                Ok(std::ptr::replace(self.val_ptr.add(idx), VmValue::Nil))
            }
        }
    }
}

// -- Helpers called from JIT code --

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_load_var(
    frame_ptr: *mut JitFrame,
    name_ptr: *const u8,
    name_len: usize,
) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };
    let name =
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };

    match vm.heap_mut().env_get(frame.env, name) {
        Ok(expr) => match crate::vm::machine::expr_to_vm_value(&expr, vm.heap_mut()) {
            Ok(val) => frame.push_val(val),
            Err(_) => {
                frame.error = c"JIT LoadVar expr error".as_ptr() as *const u8;
                frame.push_val(crate::vm::machine::VmValue::Nil);
            }
        },
        Err(_) => {
            frame.error = c"JIT LoadVar undefined variable".as_ptr() as *const u8;
            frame.push_val(crate::vm::machine::VmValue::Nil);
        },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_store_var(
    frame_ptr: *mut JitFrame,
    name_ptr: *const u8,
    name_len: usize,
) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };
    let name =
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };

    if let Ok(val) = frame.pop_val() {
        if let Ok(expr) = crate::vm::machine::vm_value_to_expr(val, vm.heap_mut()) {
            vm.heap_mut().env_set(frame.env, name.to_string(), expr);
        } else {
            frame.error = c"JIT StoreVar vm_value_to_expr failed".as_ptr() as *const u8;
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_assign_var(
    frame_ptr: *mut JitFrame,
    name_ptr: *const u8,
    name_len: usize,
) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };
    let name =
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };

    if let Ok(val) = frame.pop_val() {
        if let Ok(expr) = crate::vm::machine::vm_value_to_expr(val, vm.heap_mut()) {
            if let Err(e) = vm.heap_mut().env_assign(frame.env, name, expr) {
                frame.error = c"JIT AssignVar failed".as_ptr() as *const u8;
                let _ = e; // error message is static
            }
        } else {
            frame.error = c"JIT AssignVar vm_value_to_expr failed".as_ptr() as *const u8;
        }
    }
}

/// Sync the JIT frame's stack into the VM's stack, execute do_call, then
/// sync the result back. This is the "slow path" that handles all function
/// calls (builtins and closures) by delegating to the VM.
unsafe extern "C" fn sync_and_call(
    frame: &mut JitFrame,
    n_args: usize,
    tail: bool,
) {
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };

    // 1. Sync JIT stack → VM stack
    vm.stack.clear();
    for i in 0..frame.stack_len {
        let tag = unsafe { *frame.tag_ptr.add(i) };
        match tag {
            0 => {
                let bits = unsafe { *frame.stack_ptr.add(i) };
                let f = f64::from_bits(bits);
                // Distinguish int vs float: check if it's a whole number in i64 range
                if f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
                    vm.stack.push(VmValue::Int(f as i64));
                } else {
                    vm.stack.push(VmValue::Float(f));
                }
            }
            1 => vm.stack.push(VmValue::Nil),
            _ => {
                let val = unsafe { std::ptr::replace(frame.val_ptr.add(i), VmValue::Nil) };
                vm.stack.push(val);
            }
        }
    }

    // 2. Call
    let call_result = vm.do_call(n_args, tail);

    // 3. Sync VM stack → JIT frame
    frame.stack_len = 0;
    match call_result {
        Ok(()) => {
            if let Some(result) = vm.stack.pop() {
                frame.push_val(result);
            } else {
                frame.push_val(VmValue::Nil);
            }
        }
        Err(e) => {
            let boxed = e.into_boxed_str();
            let ptr = Box::into_raw(boxed) as *const u8;
            frame.error = ptr;
            frame.push_val(VmValue::Nil);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_call(frame_ptr: *mut JitFrame, n_args: usize) {
    unsafe {
        let frame = &mut *frame_ptr;
        sync_and_call(frame, n_args, false);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_tail_call(frame_ptr: *mut JitFrame, n_args: usize) {
    unsafe {
        let frame = &mut *frame_ptr;
        sync_and_call(frame, n_args, true);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_tree_eval(
    frame_ptr: *mut JitFrame,
    expr_ptr: *const crate::expr::Expr,
) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };
    let expr = unsafe { &*expr_ptr };

    match crate::eval::eval_tree(expr, frame.env, vm.heap_mut()) {
        Ok(result) => {
            match crate::vm::machine::expr_to_vm_value(&result, vm.heap_mut()) {
                Ok(val) => frame.push_val(val),
                Err(_) => {
                    frame.error = c"JIT TreeEval: conversion failed".as_ptr() as *const u8;
                    frame.push_val(VmValue::Nil);
                }
            }
        }
        Err(e) => {
            // Leak the error string
            let boxed = e.into_boxed_str();
            let ptr = Box::into_raw(boxed) as *const u8;
            frame.error = ptr;
            frame.push_val(VmValue::Nil);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_push_env(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };
    let child = crate::expr::new_env(vm.heap_mut(), Some(frame.env));
    frame.env = child;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_pop_env(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };
    if let Some(parent) = vm.heap_mut().parent_of(frame.env) {
        frame.env = parent;
    } else {
        frame.error = c"JIT PopEnv: no parent environment".as_ptr() as *const u8;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_store_self(
    frame_ptr: *mut JitFrame,
    name_ptr: *const u8,
    name_len: usize,
) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };
    let name =
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len)) };

    // Reconstruct the closure from the current chunk
    let chunk = unsafe { &*frame.chunk_ptr };
    let self_val = VmValue::Closure {
        chunk: Rc::new(chunk.clone()),
        params: Vec::new(),
        body_expr: Box::new(crate::expr::Expr::List(vec![])),
        env: frame.env,
    };
    if let Ok(expr) = crate::vm::machine::vm_value_to_expr(self_val, vm.heap_mut()) {
        vm.heap_mut().env_set(frame.env, name.to_string(), expr);
    } else {
        frame.error = c"JIT StoreSelf: conversion failed".as_ptr() as *const u8;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_make_list(
    frame_ptr: *mut JitFrame,
    n: usize,
) {
    let frame = unsafe { &mut *frame_ptr };
    if frame.stack_len < n {
        frame.error = c"JIT MakeList: stack underflow".as_ptr() as *const u8;
        return;
    }
    let start = frame.stack_len - n;
    let mut items = Vec::with_capacity(n);
    for i in start..frame.stack_len {
        let tag = unsafe { *frame.tag_ptr.add(i) };
        let val_bits = unsafe { *frame.stack_ptr.add(i) };
        match tag {
            0 => {
                let f = f64::from_bits(val_bits);
                if f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
                    items.push(VmValue::Int(f as i64));
                } else {
                    items.push(VmValue::Float(f));
                }
            }
            1 => items.push(VmValue::Nil),
            _ => {
                let val = unsafe { std::ptr::replace(frame.val_ptr.add(i), VmValue::Nil) };
                items.push(val);
            }
        }
    }
    frame.stack_len = start;
    frame.push_val(VmValue::List(items));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_prepend_list(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    if frame.stack_len < 2 {
        frame.error = c"JIT PrependList: stack underflow".as_ptr() as *const u8;
        return;
    }
    let list_val = frame.pop_val().unwrap_or(VmValue::Nil);
    let item_val = frame.pop_val().unwrap_or(VmValue::Nil);
    match list_val {
        VmValue::List(mut items) => {
            items.insert(0, item_val);
            frame.push_val(VmValue::List(items));
        }
        _ => {
            frame.error = c"JIT PrependList: expected list".as_ptr() as *const u8;
            frame.push_val(VmValue::Nil);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_append_splice(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    if frame.stack_len < 2 {
        frame.error = c"JIT AppendSplice: stack underflow".as_ptr() as *const u8;
        return;
    }
    let splice_val = frame.pop_val().unwrap_or(VmValue::Nil);
    let acc_val = frame.pop_val().unwrap_or(VmValue::Nil);
    match (splice_val, acc_val) {
        (VmValue::List(mut s), VmValue::List(a)) => {
            s.extend(a);
            frame.push_val(VmValue::List(s));
        }
        _ => {
            frame.error = c"JIT AppendSplice: expected two lists".as_ptr() as *const u8;
            frame.push_val(VmValue::Nil);
        }
    }
}

/// Helper for non-numeric LoadConst. Reads the pending Op from a thread-local.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_make_const(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    let vm = unsafe { &mut *(frame.vm_ptr as *mut VM<'_>) };

    thread_local! {
        static PENDING_OP: std::cell::RefCell<Option<crate::vm::bytecode::Op>> =
            const { std::cell::RefCell::new(None) };
    }

    let op = PENDING_OP.with(|cell| cell.borrow_mut().take());
    match op {
        Some(crate::vm::bytecode::Op::LoadConst(val)) => {
            match crate::vm::machine::expr_to_vm_value(
                &crate::vm::bytecode::value_to_expr(val),
                vm.heap_mut(),
            ) {
                Ok(val) => frame.push_val(val),
                Err(_) => {
                    frame.error = c"JIT MakeConst: conversion failed".as_ptr() as *const u8;
                    frame.push_val(VmValue::Nil);
                }
            }
        }
        Some(crate::vm::bytecode::Op::LoadNil) => {
            frame.push_val(VmValue::Nil);
        }
        _ => {
            frame.error = c"JIT MakeConst: unexpected op".as_ptr() as *const u8;
            frame.push_val(VmValue::Nil);
        }
    }
}

/// Helper for MakeFunc. Reads the op data from a thread-local.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_make_func_from_op(
    frame_ptr: *mut JitFrame,
    op_index: usize,
) {
    let frame = unsafe { &mut *frame_ptr };
    let chunk = unsafe { &*frame.chunk_ptr };

    thread_local! {
        static MAKE_FUNC_OPS: std::cell::RefCell<Vec<(usize, Vec<String>, crate::expr::Expr)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    let op_data = MAKE_FUNC_OPS.with(|cell| cell.borrow_mut().pop());
    if let Some((code_offset, params, body_expr)) = op_data {
        if code_offset < chunk.sub_chunks.len() {
            let sub_chunk = chunk.sub_chunks[code_offset].clone();
            let closure = VmValue::Closure {
                chunk: Rc::new(sub_chunk),
                params,
                body_expr: Box::new(body_expr),
                env: frame.env,
            };
            frame.push_val(closure);
        } else {
            frame.error = c"JIT MakeFunc: code_offset out of range".as_ptr() as *const u8;
            frame.push_val(VmValue::Nil);
        }
    } else {
        frame.error = c"JIT MakeFunc: no op data".as_ptr() as *const u8;
        frame.push_val(VmValue::Nil);
    }
    let _ = op_index;
}

/// Fast inline helpers for comparisons — operate directly on JIT stack, no VM sync.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_num_eq(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    if frame.stack_len < 2 {
        frame.error = c"JIT NumEq: stack underflow".as_ptr() as *const u8;
        return;
    }
    let idx = frame.stack_len - 2;
    let a_tag = unsafe { *frame.tag_ptr.add(idx) };
    let b_tag = unsafe { *frame.tag_ptr.add(idx + 1) };
    let a_bits = unsafe { *frame.stack_ptr.add(idx) };
    let b_bits = unsafe { *frame.stack_ptr.add(idx + 1) };
    let equal = if a_tag == 0 && b_tag == 0 {
        let a = f64::from_bits(a_bits);
        let b = f64::from_bits(b_bits);
        a == b
    } else if a_tag == 1 && b_tag == 1 {
        true // nil == nil
    } else if a_tag != 0 && b_tag != 0 {
        // Both objects: compare pointers
        let a_val = unsafe { std::ptr::read(frame.val_ptr.add(idx)) };
        let b_val = unsafe { std::ptr::read(frame.val_ptr.add(idx + 1)) };
        let eq = match (&a_val, &b_val) {
            (VmValue::Str(a), VmValue::Str(b)) => a == b,
            (VmValue::Bool(a), VmValue::Bool(b)) => a == b,
            _ => false,
        };
        std::mem::forget(a_val);
        std::mem::forget(b_val);
        eq
    } else {
        false
    };
    frame.stack_len = idx;
    frame.push_val(VmValue::Bool(equal));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_num_lt(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    if frame.stack_len < 2 {
        frame.error = c"JIT NumLt: stack underflow".as_ptr() as *const u8;
        return;
    }
    let idx = frame.stack_len - 2;
    let a_bits = unsafe { *frame.stack_ptr.add(idx) };
    let b_bits = unsafe { *frame.stack_ptr.add(idx + 1) };
    let a = f64::from_bits(a_bits);
    let b = f64::from_bits(b_bits);
    frame.stack_len = idx;
    frame.push_val(VmValue::Bool(a < b));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_helper_num_gt(frame_ptr: *mut JitFrame) {
    let frame = unsafe { &mut *frame_ptr };
    if frame.stack_len < 2 {
        frame.error = c"JIT NumGt: stack underflow".as_ptr() as *const u8;
        return;
    }
    let idx = frame.stack_len - 2;
    let a_bits = unsafe { *frame.stack_ptr.add(idx) };
    let b_bits = unsafe { *frame.stack_ptr.add(idx + 1) };
    let a = f64::from_bits(a_bits);
    let b = f64::from_bits(b_bits);
    frame.stack_len = idx;
    frame.push_val(VmValue::Bool(a > b));
}
