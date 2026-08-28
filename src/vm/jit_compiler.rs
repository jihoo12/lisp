use crate::tinyasm::{
    assembler::Assembler,
    encoder::{Instruction, MemoryAddr, Operand},
    jit::JitMemory,
    registers::{Register, XmmRegister},
};
use crate::vm::bytecode::{Chunk, Op, Value};

pub struct JitCompiler;

impl JitCompiler {
    pub fn compile_chunk(
        chunk: &Chunk,
    ) -> Result<
        (
            JitMemory,
            unsafe extern "C" fn(*mut crate::vm::jit_abi::JitFrame),
        ),
        String,
    > {
        let mut asm = Assembler::new();

        // Register usage:
        // RDI = frame_ptr (argument 1)
        // RBX = stack_ptr
        // R12 = stack_len
        // R13 = tag_ptr

        let frame_offset_stack_ptr = 0;
        let frame_offset_stack_len = 8;
        let frame_offset_result_tag = 24;
        let frame_offset_result_val = 32;
        let _frame_offset_error = 40;
        let frame_offset_tag_ptr = 48;
        let _frame_offset_chunk_ptr = 80; // after vm_ptr at 72

        // Prologue — save callee-saved GPRs
        asm.add_instruction(Instruction::Push(Operand::Reg(Register::RBP)));
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::RBP),
            Operand::Reg(Register::RSP),
        ));
        asm.add_instruction(Instruction::Push(Operand::Reg(Register::RBX)));
        asm.add_instruction(Instruction::Push(Operand::Reg(Register::R12)));
        asm.add_instruction(Instruction::Push(Operand::Reg(Register::R13)));
        asm.add_instruction(Instruction::Push(Operand::Reg(Register::R14)));
        asm.add_instruction(Instruction::Push(Operand::Reg(Register::R15)));
        // align stack to 16 bytes and allocate XMM save area (10 × 16 bytes)
        let xmm_save_size: i32 = 160;
        asm.add_instruction(Instruction::Sub(
            Operand::Reg(Register::RSP),
            Operand::Imm32(8 + xmm_save_size),
        ));
        // Save callee-saved XMM6–XMM15 (System V AMD64 ABI)
        let xmm_callee: [XmmRegister; 10] = [
            XmmRegister::XMM6,  XmmRegister::XMM7,  XmmRegister::XMM8,
            XmmRegister::XMM9,  XmmRegister::XMM10, XmmRegister::XMM11,
            XmmRegister::XMM12, XmmRegister::XMM13, XmmRegister::XMM14,
            XmmRegister::XMM15,
        ];
        for (i, xmm) in xmm_callee.iter().enumerate() {
            let offset: i32 = (i as i32) * 16;
            asm.add_instruction(Instruction::Movdqa(
                Operand::Mem(MemoryAddr::base_disp(Register::RSP, offset)),
                Operand::Xmm(*xmm),
            ));
        }

        // Load fields from JitFrame (RDI)
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::RBX),
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, frame_offset_stack_ptr)),
        ));
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::R12),
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, frame_offset_stack_len)),
        ));
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::R13),
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, frame_offset_tag_ptr)),
        ));

        // Iterate ops
        for (i, op) in chunk.ops.iter().enumerate() {
            asm.add_instruction(Instruction::Label(format!("op_{}", i)));

            match op {
                Op::LoadConst(Value::Int(n)) => {
                    let bits = (*n as f64).to_bits();
                    asm.add_instruction(Instruction::Mov(
                        Operand::Reg(Register::RAX),
                        Operand::Imm64(bits),
                    ));

                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::R13),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Imm32(0),
                    ));
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::RBX),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Reg(Register::RAX),
                    ));
                    asm.add_instruction(Instruction::Add(
                        Operand::Reg(Register::R12),
                        Operand::Imm32(1),
                    ));
                }
                Op::LoadConst(Value::Float(n)) => {
                    let bits = n.to_bits();
                    asm.add_instruction(Instruction::Mov(
                        Operand::Reg(Register::RAX),
                        Operand::Imm64(bits),
                    ));

                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::R13),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Imm32(0),
                    ));
                    // stack_ptr[r12*8] = bits
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::RBX),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Reg(Register::RAX),
                    ));

                    asm.add_instruction(Instruction::Add(
                        Operand::Reg(Register::R12),
                        Operand::Imm32(1),
                    ));
                }
                Op::LoadConst(Value::Bool(b)) => {
                    let val: f64 = if *b { 1.0 } else { 0.0 };
                    let bits = val.to_bits();
                    asm.add_instruction(Instruction::Mov(
                        Operand::Reg(Register::RAX),
                        Operand::Imm64(bits),
                    ));
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::R13),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Imm32(0),
                    ));
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::RBX),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Reg(Register::RAX),
                    ));
                    asm.add_instruction(Instruction::Add(
                        Operand::Reg(Register::R12),
                        Operand::Imm32(1),
                    ));
                }
                Op::LoadConst(Value::Nil) | Op::LoadNil => {
                    // tag_ptr[r12*8] = 1 (Nil)
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::R13),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Imm32(1),
                    ));
                    // stack_ptr[r12*8] = 0
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::RBX),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                        Operand::Imm32(0),
                    ));
                    asm.add_instruction(Instruction::Add(
                        Operand::Reg(Register::R12),
                        Operand::Imm32(1),
                    ));
                }
                // Non-numeric LoadConst variants → delegate to helper
                Op::LoadConst(_) => {
                    Self::emit_make_const_helper(&mut asm, op);
                }
                Op::Pop => {
                    asm.add_instruction(Instruction::Sub(
                        Operand::Reg(Register::R12),
                        Operand::Imm32(1),
                    ));
                }
                Op::Jump(target) => {
                    asm.add_instruction(Instruction::JmpLabel(format!("op_{}", target)));
                }
                Op::JumpIfFalse(target) => {
                    asm.add_instruction(Instruction::Sub(
                        Operand::Reg(Register::R12),
                        Operand::Imm32(1),
                    ));

                    // rdx = tag
                    asm.add_instruction(Instruction::Mov(
                        Operand::Reg(Register::RDX),
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::R13),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                    ));
                    // rax = val
                    asm.add_instruction(Instruction::Mov(
                        Operand::Reg(Register::RAX),
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::RBX),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                    ));

                    // tag == 1 (Nil) -> jump
                    asm.add_instruction(Instruction::Cmp(
                        Operand::Reg(Register::RDX),
                        Operand::Imm32(1),
                    ));
                    asm.add_instruction(Instruction::JeLabel(format!("op_{}", target)));

                    // tag == 0 (Number)
                    asm.add_instruction(Instruction::Cmp(
                        Operand::Reg(Register::RDX),
                        Operand::Imm32(0),
                    ));
                    asm.add_instruction(Instruction::JneLabel(format!("jif_cont_{}", i)));

                    // is val 0? (0.0 has bits 0)
                    asm.add_instruction(Instruction::Cmp(
                        Operand::Reg(Register::RAX),
                        Operand::Imm32(0),
                    ));
                    asm.add_instruction(Instruction::JeLabel(format!("op_{}", target)));

                    asm.add_instruction(Instruction::Label(format!("jif_cont_{}", i)));
                }
                Op::Return => {
                    asm.add_instruction(Instruction::Sub(
                        Operand::Reg(Register::R12),
                        Operand::Imm32(1),
                    ));

                    // Result tag
                    asm.add_instruction(Instruction::Mov(
                        Operand::Reg(Register::RDX),
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::R13),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                    ));
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr::base_disp(
                            Register::RDI,
                            frame_offset_result_tag,
                        )),
                        Operand::Reg(Register::RDX),
                    ));

                    // Result val
                    asm.add_instruction(Instruction::Mov(
                        Operand::Reg(Register::RAX),
                        Operand::Mem(MemoryAddr {
                            base: Some(Register::RBX),
                            index: Some(Register::R12),
                            scale: 8,
                            disp: 0,
                        }),
                    ));
                    asm.add_instruction(Instruction::Mov(
                        Operand::Mem(MemoryAddr::base_disp(
                            Register::RDI,
                            frame_offset_result_val,
                        )),
                        Operand::Reg(Register::RAX),
                    ));

                    asm.add_instruction(Instruction::JmpLabel("epilogue".to_string()));
                }
                Op::LoadVar(name) => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_load_var as *const () as usize,
                        Some(name.as_ptr() as u64),
                        Some(name.len() as u64),
                    );
                }
                Op::StoreVar(name) => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_store_var as *const () as usize,
                        Some(name.as_ptr() as u64),
                        Some(name.len() as u64),
                    );
                }
                Op::AssignVar(name) => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_assign_var as *const () as usize,
                        Some(name.as_ptr() as u64),
                        Some(name.len() as u64),
                    );
                }
                Op::Call(n_args) => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_call as *const () as usize,
                        Some(*n_args as u64),
                        None,
                    );
                }
                Op::TailCall(n_args) => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_tail_call as *const () as usize,
                        Some(*n_args as u64),
                        None,
                    );
                }
                Op::MakeFunc { code_offset, .. } => {
                    Self::emit_make_func_helper(&mut asm, chunk, *code_offset, i);
                }
                Op::MakeList(n) => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_make_list as *const () as usize,
                        Some(*n as u64),
                        None,
                    );
                }
                Op::PrependList => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_prepend_list as *const () as usize,
                        None,
                        None,
                    );
                }
                Op::AppendSplice => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_append_splice as *const () as usize,
                        None,
                        None,
                    );
                }
                Op::TreeEval(expr) => {
                    // Pin the expression so the pointer remains valid
                    let expr = Box::leak(Box::new(expr.clone()));
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_tree_eval as *const () as usize,
                        Some(expr as *const _ as u64),
                        None,
                    );
                }
                Op::PushEnv => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_push_env as *const () as usize,
                        None,
                        None,
                    );
                }
                Op::PopEnv => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_pop_env as *const () as usize,
                        None,
                        None,
                    );
                }
                Op::StoreSelf(name) => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_store_self as *const () as usize,
                        Some(name.as_ptr() as u64),
                        Some(name.len() as u64),
                    );
                }

                // ── Inline arithmetic: pop two numbers, push result ──────
                Op::NumAdd | Op::NumSub | Op::NumMul | Op::NumDiv => {
                    Self::emit_num_binop(&mut asm, op);
                }
                Op::NumEq => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_num_eq as *const () as usize,
                        None,
                        None,
                    );
                }
                Op::NumLt => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_num_lt as *const () as usize,
                        None,
                        None,
                    );
                }
                Op::NumGt => {
                    Self::emit_helper_call(
                        &mut asm,
                        crate::vm::jit_abi::jit_helper_num_gt as *const () as usize,
                        None,
                        None,
                    );
                }
            }
        }

        asm.add_instruction(Instruction::Label("epilogue".to_string()));
        // Epilogue
        // Write back stack_len
        asm.add_instruction(Instruction::Mov(
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, frame_offset_stack_len)),
            Operand::Reg(Register::R12),
        ));

        // Restore callee-saved XMM6–XMM15
        let xmm_callee: [XmmRegister; 10] = [
            XmmRegister::XMM6,  XmmRegister::XMM7,  XmmRegister::XMM8,
            XmmRegister::XMM9,  XmmRegister::XMM10, XmmRegister::XMM11,
            XmmRegister::XMM12, XmmRegister::XMM13, XmmRegister::XMM14,
            XmmRegister::XMM15,
        ];
        for (i, xmm) in xmm_callee.iter().enumerate() {
            let offset: i32 = (i as i32) * 16;
            asm.add_instruction(Instruction::Movdqa(
                Operand::Xmm(*xmm),
                Operand::Mem(MemoryAddr::base_disp(Register::RSP, offset)),
            ));
        }
        // Free XMM save area + unalign stack
        asm.add_instruction(Instruction::Add(
            Operand::Reg(Register::RSP),
            Operand::Imm32(8 + xmm_save_size),
        ));
        asm.add_instruction(Instruction::Pop(Operand::Reg(Register::R15)));
        asm.add_instruction(Instruction::Pop(Operand::Reg(Register::R14)));
        asm.add_instruction(Instruction::Pop(Operand::Reg(Register::R13)));
        asm.add_instruction(Instruction::Pop(Operand::Reg(Register::R12)));
        asm.add_instruction(Instruction::Pop(Operand::Reg(Register::RBX)));
        asm.add_instruction(Instruction::Pop(Operand::Reg(Register::RBP)));
        asm.add_instruction(Instruction::Ret);

        let bytes = asm.assemble().map_err(|e| e.to_string())?;
        let mut mem = JitMemory::new(bytes.len()).map_err(|e| e.to_string())?;
        mem.write(&bytes).map_err(|e| e.to_string())?;
        mem.make_executable().map_err(|e| e.to_string())?;

        let raw_fn = unsafe { mem.as_fn().map_err(|e| e.to_string())? };
        let fp: unsafe extern "C" fn(*mut crate::vm::jit_abi::JitFrame) =
            unsafe { std::mem::transmute(raw_fn) };

        Ok((mem, fp))
    }

    fn emit_helper_call(
        asm: &mut Assembler,
        helper_ptr: usize,
        arg1: Option<u64>,
        arg2: Option<u64>,
    ) {
        // flush stack_len
        asm.add_instruction(Instruction::Mov(
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, 8)),
            Operand::Reg(Register::R12),
        ));

        // Push RDI because it's caller-saved in System V AMD64 ABI, and we need it back after call
        asm.add_instruction(Instruction::Push(Operand::Reg(Register::RDI)));

        // Align stack to 16 bytes before call (since Push RDI misaligned it by 8 bytes)
        asm.add_instruction(Instruction::Sub(
            Operand::Reg(Register::RSP),
            Operand::Imm32(8),
        ));

        if let Some(a1) = arg1 {
            asm.add_instruction(Instruction::Mov(
                Operand::Reg(Register::RSI),
                Operand::Imm64(a1),
            ));
        }
        if let Some(a2) = arg2 {
            asm.add_instruction(Instruction::Mov(
                Operand::Reg(Register::RDX),
                Operand::Imm64(a2),
            ));
        }

        // Call helper
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::RAX),
            Operand::Imm64(helper_ptr as u64),
        ));
        asm.add_instruction(Instruction::Call(Operand::Reg(Register::RAX)));

        // Unalign stack
        asm.add_instruction(Instruction::Add(
            Operand::Reg(Register::RSP),
            Operand::Imm32(8),
        ));

        // Pop RDI
        asm.add_instruction(Instruction::Pop(Operand::Reg(Register::RDI)));

        // reload rbx, r12, r13 (stack might have reallocated, though for Phase 1 it's static capacity)
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::RBX),
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, 0)),
        ));
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::R12),
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, 8)),
        ));
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::R13),
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, 48)),
        ));

        // Check for error
        asm.add_instruction(Instruction::Mov(
            Operand::Reg(Register::RAX),
            Operand::Mem(MemoryAddr::base_disp(Register::RDI, 40)),
        ));
        asm.add_instruction(Instruction::Cmp(
            Operand::Reg(Register::RAX),
            Operand::Imm32(0),
        ));
        asm.add_instruction(Instruction::JneLabel("epilogue".to_string()));
    }

    /// Emit a helper call for non-numeric LoadConst variants.
    fn emit_make_const_helper(asm: &mut Assembler, op: &Op) {
        // For LoadConst with non-numeric values, we create the value through a helper.
        // We store the Op in a thread-local and pass a pointer to it.
        // This is a bit hacky but works for Phase 2.
        thread_local! {
            static PENDING_OP: std::cell::RefCell<Option<Op>> = const { std::cell::RefCell::new(None) };
        }

        PENDING_OP.with(|cell| {
            *cell.borrow_mut() = Some(op.clone());
        });

        // Pass a dummy pointer; the helper will read from the thread-local
        Self::emit_helper_call(
            asm,
            crate::vm::jit_abi::jit_helper_make_const as *const () as usize,
            Some(0),
            None,
        );
    }

    /// Emit MakeFunc by delegating to a helper that reads from the chunk's sub_chunks.
    fn emit_make_func_helper(
        asm: &mut Assembler,
        chunk: &Chunk,
        code_offset: usize,
        op_index: usize,
    ) {
        // We need to pass the code_offset to the helper.
        // The helper reads the sub-chunk from the chunk and creates a closure.
        //
        // Problem: the helper needs params and body_expr from the Op::MakeFunc.
        // Solution: store them in a thread-local indexed by op_index.
        thread_local! {
            static MAKE_FUNC_OPS: std::cell::RefCell<Vec<(usize, Vec<String>, crate::expr::Expr)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }

        // Find the MakeFunc op to extract params and body_expr
        if let Op::MakeFunc { params, body_expr, .. } = &chunk.ops[op_index] {
            MAKE_FUNC_OPS.with(|cell| {
                let mut ops = cell.borrow_mut();
                // Clear old entries (we only need the current one)
                ops.clear();
                ops.push((code_offset, params.clone(), (**body_expr).clone()));
            });
        }

        Self::emit_helper_call(
            asm,
            crate::vm::jit_abi::jit_helper_make_func_from_op as *const () as usize,
            Some(op_index as u64),
            None,
        );
    }

    /// Emit inline binary numeric operation (Add/Sub/Mul/Div).
    ///
    /// All numbers on the JIT stack are stored as f64 bit patterns.
    /// Strategy:
    /// 1. Pop two values: b (top), a (below).
    /// 2. Load both into XMM registers.
    /// 3. Perform the SSE2 scalar double operation.
    /// 4. Store the result back and push one value.
    fn emit_num_binop(asm: &mut Assembler, op: &Op) {
        // stack_len -= 2 (pop both operands)
        asm.add_instruction(Instruction::Sub(
            Operand::Reg(Register::R12),
            Operand::Imm32(2),
        ));

        // Load a = stack_ptr[stack_len] into xmm0
        // Memory: [RBX + R12 * 8]
        asm.add_instruction(Instruction::Movsd(
            Operand::Xmm(XmmRegister::XMM0),
            Operand::Mem(MemoryAddr {
                base: Some(Register::RBX),
                index: Some(Register::R12),
                scale: 8,
                disp: 0,
            }),
        ));

        // Load b = stack_ptr[stack_len + 1] into xmm1
        // Memory: [RBX + R12 * 8 + 8]
        asm.add_instruction(Instruction::Movsd(
            Operand::Xmm(XmmRegister::XMM1),
            Operand::Mem(MemoryAddr {
                base: Some(Register::RBX),
                index: Some(Register::R12),
                scale: 8,
                disp: 8,
            }),
        ));

        // Perform the operation: xmm0 = xmm0 OP xmm1
        match op {
            Op::NumAdd => {
                asm.add_instruction(Instruction::Addsd(
                    Operand::Xmm(XmmRegister::XMM0),
                    Operand::Xmm(XmmRegister::XMM1),
                ));
            }
            Op::NumSub => {
                // Note: we want a - b, and xmm0=a, xmm1=b
                asm.add_instruction(Instruction::Subsd(
                    Operand::Xmm(XmmRegister::XMM0),
                    Operand::Xmm(XmmRegister::XMM1),
                ));
            }
            Op::NumMul => {
                asm.add_instruction(Instruction::Mulsd(
                    Operand::Xmm(XmmRegister::XMM0),
                    Operand::Xmm(XmmRegister::XMM1),
                ));
            }
            Op::NumDiv => {
                asm.add_instruction(Instruction::Divsd(
                    Operand::Xmm(XmmRegister::XMM0),
                    Operand::Xmm(XmmRegister::XMM1),
                ));
            }
            _ => unreachable!(),
        }

        // Store result at stack_ptr[stack_len] (top of stack after push)
        asm.add_instruction(Instruction::Movsd(
            Operand::Mem(MemoryAddr {
                base: Some(Register::RBX),
                index: Some(Register::R12),
                scale: 8,
                disp: 0,
            }),
            Operand::Xmm(XmmRegister::XMM0),
        ));

        // stack_len += 1 (push result)
        asm.add_instruction(Instruction::Add(
            Operand::Reg(Register::R12),
            Operand::Imm32(1),
        ));
    }
}
