pub mod builtins;
pub mod env;
pub mod eval;
pub mod expr;
pub mod gc;
pub mod helper;
pub mod macros;
pub mod reader;
pub mod tinyasm;
pub mod vm;

pub use expr::{Expr, env_set, new_env, is_truthy};
pub use gc::Heap;
pub use eval::{eval, with_import_base};
pub use reader::{tokenize, parse, parse_all};
pub use builtins::global_env;

pub type Env = expr::Env;

pub struct PiLisp {
    heap: Heap,
    env: Env,
}

impl PiLisp {
    pub fn new() -> Self {
        let mut heap = Heap::new();
        let env = global_env(&mut heap);
        PiLisp { heap, env }
    }

    pub fn env(&self) -> Env {
        self.env
    }

    pub fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    pub fn eval(&mut self, src: &str) -> Result<Expr, String> {
        let exprs = parse_all(src)?;
        let mut last = Expr::List(vec![]);
        for e in exprs {
            last = eval(&e, self.env, &mut self.heap)?;
        }
        Ok(last)
    }

    pub fn eval_expr(&mut self, expr: &Expr) -> Result<Expr, String> {
        eval(expr, self.env, &mut self.heap)
    }

    pub fn parse(&self, src: &str) -> Result<Vec<Expr>, String> {
        parse_all(src)
    }
}

impl Default for PiLisp {
    fn default() -> Self {
        Self::new()
    }
}
