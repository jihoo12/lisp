use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::env::{Env, env_set};
use crate::eval::eval_tree;
use crate::expr::{Expr, new_env};
use crate::gc::Heap;

// ── Thread-local method registry ──────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ClosMethod {
    specializers: Vec<String>,
    qualifier: String,
    body: Expr,
    gf_name: String,
}

thread_local! {
    static GF_METHODS: RefCell<HashMap<String, Vec<ClosMethod>>> = RefCell::new(HashMap::new());
    static CLASS_CPL: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
}

fn register_gf(gf_name: &str) {
    GF_METHODS.with(|m| {
        let mut map = m.borrow_mut();
        if !map.contains_key(gf_name) {
            map.insert(gf_name.to_string(), Vec::new());
        }
    });
}

fn register_method(
    gf_name: &str,
    specializers: Vec<String>,
    qualifier: &str,
    body: Expr,
) {
    let method = ClosMethod {
        specializers,
        qualifier: qualifier.to_string(),
        body,
        gf_name: gf_name.to_string(),
    };
    GF_METHODS.with(|m| {
        let mut map = m.borrow_mut();
        map.entry(gf_name.to_string())
            .or_default()
            .push(method);
    });
}

fn get_methods(gf_name: &str) -> Vec<ClosMethod> {
    GF_METHODS.with(|m| {
        m.borrow()
            .get(gf_name)
            .cloned()
            .unwrap_or_default()
    })
}

fn get_cpl(class_name: &str) -> Vec<String> {
    CLASS_CPL.with(|c| {
        c.borrow()
            .get(class_name)
            .cloned()
            .unwrap_or_else(|| vec![class_name.to_string(), "t".to_string()])
    })
}

fn set_cpl(class_name: &str, cpl: Vec<String>) {
    CLASS_CPL.with(|c| {
        c.borrow_mut().insert(class_name.to_string(), cpl);
    });
}

// ── Class-of helper ────────────────────────────────────────────────────────────

fn classify(expr: &Expr) -> String {
    match expr {
        Expr::Int(_) => "integer".to_string(),
        Expr::Float(_) => "float".to_string(),
        Expr::Bool(_) => "boolean".to_string(),
        Expr::Str(_) => "string".to_string(),
        Expr::Symbol(_) => "symbol".to_string(),
        Expr::Complex(_, _) => "complex".to_string(),
        Expr::List(items) if !items.is_empty() => {
            if let Expr::Symbol(tag) = &items[0] {
                if tag == "clos-instance" && items.len() >= 2 {
                    if let Expr::Symbol(name) = &items[1] {
                        return name.clone();
                    }
                }
            }
            "list".to_string()
        }
        Expr::List(_) => "null".to_string(),
        Expr::Func(_) | Expr::Lambda(..) => "function".to_string(),
        Expr::Macro(..) => "macro".to_string(),
    }
}

// ── Method dispatch engine ─────────────────────────────────────────────────────

fn method_applicable(m: &ClosMethod, arg_classes: &[String]) -> bool {
    if m.specializers.len() != arg_classes.len() {
        return false;
    }
    for (spec, cls) in m.specializers.iter().zip(arg_classes.iter()) {
        if spec != "t" && spec != cls && !is_subtype(cls, spec) {
            return false;
        }
    }
    true
}

fn is_subtype(child: &str, parent: &str) -> bool {
    if child == parent || parent == "t" {
        return true;
    }
    // Walk the child's CPL looking for parent
    let cpl = get_cpl(child);
    for cls in &cpl {
        if cls == parent {
            return true;
        }
    }
    false
}

/// Compare two methods for sorting. Returns true if `a` is more specific than `b`.
fn method_more_specific(a: &ClosMethod, b: &ClosMethod) -> bool {
    let specs_a = &a.specializers;
    let specs_b = &b.specializers;
    for (sa, sb) in specs_a.iter().zip(specs_b.iter()) {
        if sa != sb {
            // If sa is a subtype of sb (and sb is not t, or sa is not t),
            // then a is more specific at this position.
            // If sb is t and sa is not, a is more specific.
            // If sa is t and sb is not, b is more specific.
            if sb == "t" {
                return true; // a is more specific (sa is not t)
            }
            if sa == "t" {
                return false; // b is more specific (sb is not t)
            }
            // Neither is t: check if sa is more specific (a subtype of sb)
            if is_subtype(sa, sb) {
                return true;
            }
            // Check if sb is a subtype of sa (meaning b is more specific)
            if is_subtype(sb, sa) {
                return false;
            }
            // If neither is a subtype of the other, the order is unspecified.
            // We still need to return something deterministic.
            // a is more specific if sa appears before sb in the CPL of an
            // argument's class. But we don't have the arg class here.
            // Fall back to a simple heuristic.
            return sa < sb; // arbitrary but deterministic
        }
    }
    false // equal specificity
}

fn sort_methods(methods: &[ClosMethod]) -> Vec<ClosMethod> {
    let mut sorted = methods.to_vec();
    // Simple insertion sort for determinism
    for i in 1..sorted.len() {
        let mut j = i;
        while j > 0 && method_more_specific(&sorted[j], &sorted[j - 1]) {
            sorted.swap(j, j - 1);
            j -= 1;
        }
    }
    sorted
}

/// Apply a ClosMethod (which wraps an Expr::Lambda) to the given args.
fn apply_method(
    method: &ClosMethod,
    args: &[Expr],
    heap: &mut Heap,
) -> Result<Expr, String> {
    match &method.body {
        Expr::Lambda(params, body, closure_env) => {
            // Create a call frame with the closure's captured env as parent
            let call_frame = new_env(heap, Some(*closure_env));
            if params.len() != args.len() {
                return Err(format!(
                    "arity mismatch in method of {}: expected {} args, got {}",
                    method.gf_name,
                    params.len(),
                    args.len()
                ));
            }
            for (p, a) in params.iter().zip(args.iter()) {
                heap.env_set(call_frame, p.clone(), a.clone());
            }
            // Use eval_tree to avoid re-entering the VM
            eval_tree(body, call_frame, heap)
        }
        _ => Err("method body is not a lambda".into()),
    }
}

/// Full generic function dispatch with standard method combination.
fn dispatch_gf(
    gf_name: &str,
    args: &[Expr],
    heap: &mut Heap,
) -> Result<Expr, String> {
    let methods = get_methods(gf_name);
    if methods.is_empty() {
        return Err(format!("{}: no methods defined", gf_name));
    }

    let arg_classes: Vec<String> = args.iter().map(|a| classify(a)).collect();

    // Find applicable methods
    let applicable: Vec<&ClosMethod> = methods.iter().filter(|m| method_applicable(m, &arg_classes)).collect();

    if applicable.is_empty() {
        return Err(format!(
            "{}: no applicable method for args ({})",
            gf_name,
            arg_classes.join(" ")
        ));
    }

    // Sort by specificity (most specific first for primary and :before,
    // most specific last for :after)
    let sorted = sort_methods(
        &applicable.into_iter().cloned().collect::<Vec<_>>()
    );

    let primary: Vec<&ClosMethod> = sorted.iter().filter(|m| m.qualifier == "primary").collect();
    let before: Vec<&ClosMethod> = sorted.iter().filter(|m| m.qualifier == ":before").collect();
    let after: Vec<&ClosMethod> = sorted.iter().filter(|m| m.qualifier == ":after").collect();
    let around: Vec<&ClosMethod> = sorted.iter().filter(|m| m.qualifier == ":around").collect();

    // If there are :around methods, call the most specific one.
    // The :around method body should use call-next-method to continue.
    if !around.is_empty() {
        // For now, just call the most specific :around method.
        // :around methods are expected to handle the full call chain.
        return apply_method(around[0], args, heap);
    }

    // Standard method combination: :before, primary, :after
    for m in &before {
        apply_method(m, args, heap)?;
    }

    let result = if primary.is_empty() {
        Expr::List(vec![]) // empty list = nil
    } else {
        apply_method(primary[0], args, heap)?
    };

    for m in after.iter().rev() {
        apply_method(m, args, heap)?;
    }

    Ok(result)
}

// ── Builtin registrations ─────────────────────────────────────────────────────

pub fn register_clos(env: Env, heap: &mut Heap) {
    // class-of
    env_set(
        heap,
        env,
        "class-of".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err(format!("class-of: expected 1 argument, got {}", args.len()));
            }
            Ok(Expr::Symbol(classify(&args[0])))
        })),
    );

    // subtypep
    env_set(
        heap,
        env,
        "subtypep".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err(format!("subtypep: expected 2 arguments, got {}", args.len()));
            }
            let child = match &args[0] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!("subtypep: first arg must be a symbol, got {:?}", other)),
            };
            let parent = match &args[1] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!("subtypep: second arg must be a symbol, got {:?}", other)),
            };
            Ok(Expr::Bool(is_subtype(&child, &parent)))
        })),
    );

    // clos-instance?
    env_set(
        heap,
        env,
        "clos-instance?".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err(format!("clos-instance?: expected 1 argument, got {}", args.len()));
            }
            let is_inst = matches!(&args[0], Expr::List(items)
                if items.len() >= 2
                && matches!(&items[0], Expr::Symbol(s) if s == "clos-instance"));
            Ok(Expr::Bool(is_inst))
        })),
    );

    // clos-allocate-instance
    env_set(
        heap,
        env,
        "clos-allocate-instance".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err(format!(
                    "clos-allocate-instance: expected 2 arguments (class-name slot-count), got {}",
                    args.len()
                ));
            }
            let class_name = match &args[0] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!(
                    "clos-allocate-instance: class name must be a symbol, got {:?}", other
                )),
            };
            let slot_count = match &args[1] {
                Expr::Int(n) => *n as usize,
                other => return Err(format!(
                    "clos-allocate-instance: slot count must be an integer, got {:?}", other
                )),
            };
            let slots: Vec<Expr> = (0..slot_count).map(|_| Expr::List(vec![])).collect();
            let mut instance = vec![
                Expr::Symbol("clos-instance".into()),
                Expr::Symbol(class_name),
            ];
            instance.extend(slots);
            Ok(Expr::List(instance))
        })),
    );

    // defgeneric-register — create a new generic function
    env_set(
        heap,
        env,
        "defgeneric-register".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err(format!(
                    "defgeneric-register: expected 1 argument (gf-name), got {}",
                    args.len()
                ));
            }
            let name = match &args[0] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!(
                    "defgeneric-register: gf-name must be a symbol, got {:?}", other
                )),
            };
            register_gf(&name);
            Ok(Expr::Symbol(name))
        })),
    );

    // make-generic-function — create a callable generic function as Expr::Func
    env_set(
        heap,
        env,
        "make-generic-function".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 1 {
                return Err(format!(
                    "make-generic-function: expected 1 argument (gf-name), got {}",
                    args.len()
                ));
            }
            let gf_name = match &args[0] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!(
                    "make-generic-function: gf-name must be a symbol, got {:?}", other
                )),
            };
            register_gf(&gf_name);

            let name_for_closure = gf_name.clone();
            let func = Expr::Func(Rc::new(move |call_args, heap| {
                dispatch_gf(&name_for_closure, call_args, heap)
            }));
            Ok(func)
        })),
    );

    // defmethod-register — add a method to a generic function
    env_set(
        heap,
        env,
        "defmethod-register".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 4 {
                return Err(format!(
                    "defmethod-register: expected 4 arguments (gf-name specializers qualifier body), got {}",
                    args.len()
                ));
            }
            let gf_name = match &args[0] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!(
                    "defmethod-register: gf-name must be a symbol, got {:?}", other
                )),
            };
            let specializers = match &args[1] {
                Expr::List(specs) => {
                    let mut v = Vec::new();
                    for s in specs {
                        match s {
                            Expr::Symbol(name) => v.push(name.clone()),
                            other => return Err(format!(
                                "defmethod-register: specializer must be a symbol, got {:?}", other
                            )),
                        }
                    }
                    v
                }
                other => return Err(format!(
                    "defmethod-register: specializers must be a list, got {:?}", other
                )),
            };
            let qualifier = match &args[2] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!(
                    "defmethod-register: qualifier must be a symbol, got {:?}", other
                )),
            };
            let body = args[3].clone();

            // Ensure the body is a lambda or callable
            match &body {
                Expr::Lambda(..) | Expr::Func(..) => {}
                other => return Err(format!(
                    "defmethod-register: body must be a lambda or function, got {:?}", other
                )),
            }

            // Register the class CPL if not already known (for built-in types)
            // Store the method
            register_method(&gf_name, specializers, &qualifier, body);
            Ok(Expr::Symbol(gf_name))
        })),
    );

    // error — signal an error from Lisp
    env_set(
        heap,
        env,
        "error".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.is_empty() {
                return Err("error: no message".into());
            }
            let msg = match &args[0] {
                Expr::Str(s) => s.clone(),
                other => format!("{:?}", other),
            };
            Err(msg)
        })),
    );

    // compute-cpl — compute and store class precedence list
    env_set(
        heap,
        env,
        "compute-and-store-cpl!".into(),
        Expr::Func(Rc::new(|args, _heap| {
            if args.len() != 2 {
                return Err(format!(
                    "compute-and-store-cpl!: expected 2 arguments (class-name direct-supers), got {}",
                    args.len()
                ));
            }
            let class_name = match &args[0] {
                Expr::Symbol(s) => s.clone(),
                other => return Err(format!(
                    "compute-and-store-cpl!: class name must be a symbol, got {:?}", other
                )),
            };
            let direct_supers = match &args[1] {
                Expr::List(items) => {
                    let mut v = Vec::new();
                    for item in items {
                        match item {
                            Expr::Symbol(s) => v.push(s.clone()),
                            other => return Err(format!(
                                "compute-and-store-cpl!: superclass must be a symbol, got {:?}", other
                            )),
                        }
                    }
                    v
                }
                other => return Err(format!(
                    "compute-and-store-cpl!: direct-supers must be a list, got {:?}", other
                )),
            };

            // Compute CPL: simple topological merge
            // Walk superclasses in order, adding classes not already in CPL
            let mut all: Vec<String> = vec![class_name.clone()];
            for s in &direct_supers {
                if !all.contains(s) {
                    all.push(s.clone());
                    let stored = get_cpl(s);
                    for cls in &stored {
                        if cls != s && !all.contains(cls) {
                            all.push(cls.clone());
                        }
                    }
                }
            }
            // Ensure standard-object and t are included
            if !all.contains(&"standard-object".to_string()) && class_name != "t" {
                all.push("standard-object".to_string());
            }
            if !all.contains(&"t".to_string()) && class_name != "t" {
                all.push("t".to_string());
            }

            set_cpl(&class_name, all.clone());
            Ok(Expr::List(all.into_iter().map(Expr::Symbol).collect()))
        })),
    );
}
