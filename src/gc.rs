//! A minimal mark-and-sweep garbage collector for `EnvData` nodes.
//!
//! # How it works
//!
//! Every environment is stored inside a `Heap` as a pair of parallel
//! arrays, indexed by slot:
//!
//! ```text
//!   Heap.data: [ Some(EnvData), None,        Some(EnvData), ... ]
//!   Heap.meta: [ {generation: 0, ..},  {generation: 3, ..}, {generation: 1, ..},  ... ]
//! ```
//!
//! `data[i]` is `None` exactly when slot `i` is free. `meta[i].generation` is a
//! counter that increments every time slot `i` is *reused* by `alloc`,
//! independent of whether `data[i]` currently holds anything — this is
//! the key piece that lets us detect stale handles (see below).
//!
//! A `GcHandle` is `{ idx, generation }`: an index into those arrays plus the
//! generation it was issued at. It is `Copy` and has no destructor, so
//! it can live inside `Expr::Lambda` without any reference counting.
//! Both fields are private — the only way to obtain a `GcHandle` is via
//! `Heap::alloc`, so code outside this module can't conjure up an
//! arbitrary/out-of-thin-air handle.
//!
//! ## Allocation
//! `Heap::alloc` pops an index off a free-list if one exists (O(1)),
//! bumping that slot's generation; otherwise it grows the arrays.
//!
//! ## Stale handles vs. dangling handles
//! Two distinct failure modes are both detected, with distinct-ish
//! messages:
//! * **Freed, not yet reused** (`data[i] == None`) — same as before,
//!   "the slot was swept."
//! * **Reused by something else** (`data[i] == Some(..)` but
//!   `meta[i].generation != handle.generation`) — this is the dangerous case a plain
//!   `usize` handle can't distinguish: the index is alive, but it's
//!   *not the same environment* the handle was originally issued for.
//!   Without the generation counter this would silently return the
//!   wrong `EnvData` instead of erroring.
//!
//! Out-of-range indices (handle never allocated, or heap was somehow
//! given a handle from a different `Heap`) are also handled explicitly
//! via `.get()`, so they produce the same friendly panic message
//! instead of a raw "index out of bounds."
//!
//! ## Mark phase
//! The caller passes a set of *root* handles (the currently-reachable
//! environments — e.g. the global env and every env on the eval call
//! stack). `Heap::mark` walks each root's `parent` chain, and also walks
//! *into* every `Expr` stored in `vars` looking for `Expr::Lambda`
//! values — including ones nested inside `Expr::List`, the bodies of
//! other lambdas, and `Expr::Macro` bodies — not just top-level values.
//! Each lambda found contributes its captured-env `GcHandle` as
//! something to mark.
//!
//! The *heap-graph* walk (parent links, lambda-to-lambda chains) uses an
//! explicit work stack rather than recursion, since that graph can be
//! arbitrarily deep/cyclic. The *Expr-tree* walk inside a single value
//! is also an explicit stack (see `collect_lambda_envs`), for safety
//! against deeply nested ASTs.
//!
//! A handle that turns out to be out-of-range, freed, or stale while
//! walking is *not* a normal occurrence — `roots` should only ever
//! contain handles that are still genuinely alive — so hitting one is
//! treated as a bug and flagged via `debug_assert!` (loud in debug
//! builds, but the release build stays lenient and just skips it rather
//! than crashing the collector itself).
//!
//! ## Sweep phase
//! `Heap::sweep` iterates all slots. Any slot with `marked == false` is
//! freed (`data[i]` set to `None`, index pushed onto the free-list).
//! All remaining slots are then unmarked, ready for the next cycle.
//!
//! ## Compaction (optional)
//! We do *not* compact by default, so the `idx` half of a `GcHandle`
//! remains stable forever. Freed slots are reused by `alloc` via the
//! free-list (O(1) instead of the old O(n) first-fit scan).

use std::collections::HashMap;

use crate::expr::Expr;

// ── public handle ────────────────────────────────────────────────────────────

/// A GC-managed reference to an `EnvData` node.
///
/// `GcHandle` is `Copy` and has no destructor, so it can be embedded
/// anywhere (e.g. `Expr::Lambda`) without creating reference cycles.
///
/// Both fields are private. `idx` is the slot index; `generation` is the
/// generation the slot was at when this handle was issued. A handle
/// whose `generation` no longer matches the slot's current generation is
/// stale — the slot has been freed and reused for an unrelated
/// `EnvData` since this handle was created — and `Heap` will refuse to
/// resolve it rather than silently aliasing the wrong environment.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GcHandle {
    pub(crate) idx: usize,
    pub(crate) generation: u32,
}

// ── EnvData ──────────────────────────────────────────────────────────────────

/// The runtime data stored at each environment frame.
pub struct EnvData {
    pub vars: HashMap<String, Expr>,
    pub parent: Option<GcHandle>,
}

// ── internal slot metadata ──────────────────────────────────────────────────

/// Per-slot bookkeeping that must outlive the slot's *contents* being
/// freed — in particular `generation`, which has to keep counting even while
/// `data[idx]` is `None`, or a freed-then-reused slot would reset back
/// to a generation an old handle could match again.
///
/// # Performance: struct layout
/// `marked` is a `bool` (1 byte) and `generation` is a `u32` (4 bytes).
/// They are stored together in one cache line per slot to avoid a
/// separate array scan during mark/sweep.
struct SlotMeta {
    marked: bool,
    generation: u32,
}

// ── Heap ─────────────────────────────────────────────────────────────────────

/// The GC heap. There is typically one per interpreter instance.
///
/// Keep it alive for the entire lifetime of the interpreter; all
/// `GcHandle` values are indices into this `Heap`.
pub struct Heap {
    data: Vec<Option<EnvData>>,
    meta: Vec<SlotMeta>,
    /// Indices of currently-free slots, so `alloc` can reuse one in
    /// O(1) instead of scanning `data` for the first `None`.
    free_list: Vec<usize>,
    /// Maintained incrementally by `alloc`/`sweep` so `live_count()` is
    /// O(1) instead of re-scanning every slot.
    live_count: usize,
    /// Reusable scratch buffer for `mark` — allocated once and cleared
    /// between uses so we never pay for heap allocation in the hot mark
    /// loop. Stored on `Heap` rather than as a local so the capacity
    /// is retained across GC cycles.
    mark_stack: Vec<GcHandle>,
    /// When `live_count` exceeds this threshold, `maybe_collect` will
    /// trigger a collection. 0 disables automatic collection.
    /// Set via `set_gc_threshold`.
    gc_threshold: usize,
    /// Optional root stack for use by the evaluator/VM.  Roots pushed
    /// here are used by `alloc`'s safety-net collection (see `alloc`)
    /// and by `collect_registered_roots`.  Call `push_root` before
    /// allocating and `pop_root` afterward.
    roots: Vec<GcHandle>,
}

impl Heap {
    pub fn new() -> Self {
        Heap {
            data: Vec::new(),
            meta: Vec::new(),
            free_list: Vec::new(),
            live_count: 0,
            mark_stack: Vec::new(),
            gc_threshold: 1024,
            roots: Vec::new(),
        }
    }

    // ── configuration ────────────────────────────────────────────────────────

    /// Set the threshold that controls when `maybe_collect` triggers a
    /// collection.  The default is 1024.  Set to 0 to disable automatic
    /// collection entirely (call `collect` manually).
    #[allow(dead_code)]
    pub fn set_gc_threshold(&mut self, threshold: usize) {
        self.gc_threshold = threshold;
    }

    /// Return the current GC threshold.
    #[allow(dead_code)]
    pub fn gc_threshold(&self) -> usize {
        self.gc_threshold
    }

    // ── root registration ────────────────────────────────────────────────────

    /// Push `root` onto the internal root stack.  All roots in this stack
    /// are kept alive across any GC cycle triggered by `alloc` or
    /// `collect_registered_roots`.  Paired calls must be balanced:
    ///
    /// ```ignore
    /// heap.push_root(env);
    /// let child = heap.alloc(Some(env));
    /// heap.push_root(child);
    /// // ... work that may trigger GC ...
    /// heap.pop_root(); // child
    /// heap.pop_root(); // env
    /// ```
    #[allow(dead_code)]
    pub fn push_root(&mut self, root: GcHandle) {
        self.roots.push(root);
    }

    /// Pop the most-recently-pushed root.  Panics if the root stack is
    /// empty (indicating an unbalanced push/pop in the caller).
    #[allow(dead_code)]
    pub fn pop_root(&mut self) {
        self.roots
            .pop()
            .expect("pop_root: root stack is empty (unbalanced push/pop)");
    }

    /// Return the number of registered roots.
    #[allow(dead_code)]
    pub fn root_count(&self) -> usize {
        self.roots.len()
    }

    // ── allocation ───────────────────────────────────────────────────────────

    /// Allocate a new `EnvData` node and return its handle.
    ///
    /// Reuses a free slot off the free-list if one exists (O(1));
    /// otherwise grows the heap. Reusing a slot bumps its generation,
    /// which is what invalidates any old `GcHandle` still pointing at
    /// that index.
    ///
    /// # Safety-net collection
    ///
    /// When the heap has no free slots *and* registered roots exist, a
    /// collection is triggered automatically as a safety net for callers
    /// that forgot to call `maybe_collect` or `collect`.  This prevents
    /// unbounded heap growth even if some code path never explicitly
    /// triggers GC.
    pub fn alloc(&mut self, parent: Option<GcHandle>) -> GcHandle {
        // Safety-net: if we are about to grow the heap (no free slots),
        // try collecting first using registered roots.  This catches
        // callers that forget to trigger GC explicitly.
        if self.free_list.is_empty() && !self.roots.is_empty() && self.live_count > self.gc_threshold
        {
            let roots = self.roots.clone();
            self.collect(&roots);
        }

        let env_data = EnvData {
            vars: HashMap::new(),
            parent,
        };
        self.live_count += 1;

        if let Some(idx) = self.free_list.pop() {
            let new_generation = self.meta[idx].generation.wrapping_add(1);
            self.meta[idx] = SlotMeta {
                marked: false,
                generation: new_generation,
            };
            self.data[idx] = Some(env_data);
            return GcHandle {
                idx,
                generation: new_generation,
            };
        }

        let idx = self.data.len();
        self.data.push(Some(env_data));
        self.meta.push(SlotMeta {
            marked: false,
            generation: 0,
        });
        GcHandle { idx, generation: 0 }
    }

    // ── slot access ──────────────────────────────────────────────────────────

    /// Borrow the `EnvData` for `handle`.
    ///
    /// # Panics
    /// Panics with a descriptive message if the handle is out of
    /// range, points at a freed slot, or is stale (the slot has since
    /// been reused for a different `EnvData`).
    pub fn get(&self, handle: GcHandle) -> &EnvData {
        self.check(handle).unwrap_or_else(|msg| panic!("{}", msg));
        // SAFETY: check() already verified the slot is Some.
        unsafe { self.data[handle.idx].as_ref().unwrap_unchecked() }
    }

    /// Mutably borrow the `EnvData` for `handle`. Same panic conditions
    /// as `get`.
    pub fn get_mut(&mut self, handle: GcHandle) -> &mut EnvData {
        self.check(handle).unwrap_or_else(|msg| panic!("{}", msg));
        // SAFETY: check() already verified the slot is Some.
        unsafe { self.data[handle.idx].as_mut().unwrap_unchecked() }
    }

    /// Validate a handle without panicking, returning a descriptive
    /// error otherwise. Uses `.get()` rather than direct indexing so an
    /// out-of-range `idx` produces this message instead of a raw
    /// "index out of bounds" panic.
    fn check(&self, handle: GcHandle) -> Result<(), String> {
        let meta = match self.meta.get(handle.idx) {
            Some(m) => m,
            None => {
                return Err(format!(
                    "GcHandle is dangling: index {} is out of range (heap capacity {})",
                    handle.idx,
                    self.data.len()
                ));
            }
        };
        if meta.generation != handle.generation {
            return Err(format!(
                "GcHandle is dangling: slot {} was freed and reused (handle generation {}, slot generation {})",
                handle.idx, handle.generation, meta.generation
            ));
        }
        // Note: if generation matches, the slot is guaranteed to be Some —
        // sweep() bumps generation when freeing, so a matching generation
        // implies the slot has not been swept since this handle was issued.
        // The is_none() branch below is therefore unreachable in practice;
        // it exists as a belt-and-suspenders guard against future refactors.
        if self.data[handle.idx].is_none() {
            return Err(format!(
                "GcHandle is dangling: slot {} was freed (generation matched — this is a bug)",
                handle.idx
            ));
        }
        Ok(())
    }

    /// Read a variable from `handle`'s env, walking the parent chain.
    ///
    /// Iterative rather than recursive, so a very deep environment
    /// chain (e.g. many nested `let`s) can't blow the Rust call stack —
    /// the same reasoning `mark` already applies to the heap graph.
    pub fn env_get(&self, handle: GcHandle, name: &str) -> Result<Expr, String> {
        let mut current = handle;
        loop {
            let data = self.get(current);
            if let Some(v) = data.vars.get(name) {
                return Ok(v.clone());
            }
            match data.parent {
                Some(parent) => current = parent,
                None => return Err(format!("undefined symbol: {}", name)),
            }
        }
    }

    /// Bind `name` → `val` in `handle`'s frame (does not walk parents).
    /// This is `define` semantics: always creates/overwrites a binding
    /// in the innermost frame.
    pub fn env_set(&mut self, handle: GcHandle, name: String, val: Expr) {
        self.get_mut(handle).vars.insert(name, val);
    }

    /// `set!` semantics: walk outward from `handle` looking for an
    /// *existing* binding of `name` and mutate it in place wherever
    /// it's found. Unlike `env_set`, this does not create a new binding
    /// in the local frame — if no existing binding is found anywhere up
    /// the parent chain, it returns an error instead of silently
    /// falling back to a local `define`.
    ///
    /// # Performance
    /// Uses `get_mut` with `entry()` to avoid double-lookup when the
    /// key is found.
    #[allow(dead_code)]
    pub fn env_assign(&mut self, handle: GcHandle, name: &str, val: Expr) -> Result<(), String> {
        let mut current = handle;
        loop {
            // Check for the key before taking a mutable borrow, so we can
            // still read `parent` afterward without fighting the borrow checker.
            if self.get(current).vars.contains_key(name) {
                // Key exists — take a mutable borrow and update in place.
                // `get_mut` is safe here because we just confirmed the handle
                // is valid via `get` one line above.
                self.get_mut(current).vars.insert(name.to_string(), val);
                return Ok(());
            }
            let parent = self.get(current).parent;
            match parent {
                Some(p) => current = p,
                None => return Err(format!("set!: undefined symbol: {}", name)),
            }
        }
    }

    // ── GC cycle ─────────────────────────────────────────────────────────────

    /// **Mark phase.**
    ///
    /// Mark every slot reachable from `roots`: all ancestors reachable
    /// via `parent` links, plus every `GcHandle` found inside any
    /// `Expr::Lambda` reachable from a slot's `vars` — including
    /// lambdas nested inside `Expr::List`, lambda bodies, and macro
    /// bodies, not just top-level values.
    ///
    /// `roots` should contain every `GcHandle` that is directly
    /// reachable from the Rust stack (global env, current call-stack
    /// envs, etc.). A root that's out of range, freed, or stale
    /// indicates a bug in the caller (it should never have been
    /// reachable in the first place); this is flagged via
    /// `debug_assert!` rather than either panicking unconditionally or
    /// silently ignoring it, since dropping a still-needed root
    /// silently would otherwise manifest later as a confusing "use
    /// after collection" panic far from the actual bug.
    ///
    /// # Performance
    /// Uses a persistent `mark_stack` buffer stored on `Heap` to avoid
    /// allocating a new `Vec` on every GC cycle. Lambda-env handles are
    /// pushed directly onto the mark stack rather than collected into a
    /// temporary `Vec<GcHandle>` first.
    pub fn mark(&mut self, roots: &[GcHandle]) {
        // Reuse the persistent mark_stack buffer (capacity is retained).
        debug_assert!(
            self.mark_stack.is_empty(),
            "mark_stack was not cleared after last cycle"
        );
        self.mark_stack.extend_from_slice(roots);

        while let Some(h) = self.mark_stack.pop() {
            // Validate the handle and pull out plain (Copy) values right
            // away rather than holding a `&SlotMeta`/`&EnvData` across
            // the branches below — keeps the borrows trivially short
            // and side-steps any question of exactly when they end.
            let (slot_generation, already_marked) = match self.meta.get(h.idx) {
                Some(m) => (m.generation, m.marked),
                None => {
                    debug_assert!(false, "mark: handle {:?} has an out-of-range index", h);
                    continue;
                }
            };
            if slot_generation != h.generation {
                debug_assert!(
                    false,
                    "mark: handle {:?} is stale (slot generation is {})",
                    h, slot_generation
                );
                continue;
            }
            if already_marked {
                continue; // already visited
            }

            // Pull out parent and lambda handles before marking, to avoid
            // holding a borrow across the mutable mark below.
            let parent = match self.data[h.idx].as_ref() {
                Some(slot_data) => {
                    // Push lambda-captured env handles *directly* onto the
                    // mark_stack — no intermediate Vec allocation.
                    for expr in slot_data.vars.values() {
                        collect_lambda_envs(expr, &mut self.mark_stack);
                    }
                    slot_data.parent
                }
                None => {
                    debug_assert!(false, "mark: handle {:?} points at a freed slot", h);
                    continue;
                }
            };

            self.meta[h.idx].marked = true;

            if let Some(p) = parent {
                self.mark_stack.push(p);
            }
        }
        // mark_stack is now empty; capacity is retained for the next cycle.
    }

    /// **Sweep phase.**
    ///
    /// Free every unmarked slot (pushing it onto the free-list for
    /// reuse), then clear all mark bits.
    /// Returns the number of slots that were freed.
    ///
    /// # Performance
    /// Iterates `meta` and `data` in lockstep via `zip` to give the
    /// compiler and CPU the best shot at auto-vectorising the scan.
    pub fn sweep(&mut self) -> usize {
        let mut freed = 0;

        for (idx, (data, meta)) in self.data.iter_mut().zip(self.meta.iter_mut()).enumerate() {
            if data.is_some() {
                if meta.marked {
                    meta.marked = false; // reset for next cycle
                } else {
                    *data = None;
                    self.free_list.push(idx);
                    self.live_count -= 1;
                    freed += 1;
                }
            }
        }
        freed
    }

    /// Convenience: run a full mark-and-sweep cycle.
    ///
    /// Returns the number of freed slots.
    pub fn collect(&mut self, roots: &[GcHandle]) -> usize {
        self.mark(roots);
        self.sweep()
    }

    /// Like `collect`, but uses the current root stack as the root set.
    /// Returns the number of freed slots.
    #[allow(dead_code)]
    pub fn collect_registered_roots(&mut self) -> usize {
        if self.roots.is_empty() {
            return 0;
        }
        let roots = self.roots.clone();
        self.collect(&roots)
    }

    /// Run a collection cycle only if `live_count` exceeds the configured
    /// threshold.  Returns `true` if a collection was actually performed.
    ///
    /// This is the idiomatic way to trigger GC; call it at strategic
    /// points (e.g. after allocating a new frame) with the handles that
    /// are live on the Rust stack.
    pub fn maybe_collect(&mut self, roots: &[GcHandle]) -> bool {
        if self.live_count > self.gc_threshold {
            self.collect(roots);
            true
        } else {
            false
        }
    }

    /// Like `maybe_collect`, but uses the current root stack.
    /// Returns `true` if a collection was performed.
    #[allow(dead_code)]
    pub fn maybe_collect_registered_roots(&mut self) -> bool {
        if self.live_count > self.gc_threshold && !self.roots.is_empty() {
            let roots = self.roots.clone();
            self.collect(&roots);
            true
        } else {
            false
        }
    }

    // ── diagnostics ──────────────────────────────────────────────────────────

    /// Return the parent handle of the environment at `handle`, or `None`
    /// if it is a root frame.  Used by the VM's `PopEnv` instruction to
    /// restore the enclosing scope after a `let` / `let*` body completes.
    pub fn parent_of(&self, handle: GcHandle) -> Option<GcHandle> {
        self.get(handle).parent
    }

    /// Total number of slots ever allocated (including free slots).
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Number of live (non-freed) slots. O(1) — tracked incrementally
    /// rather than rescanned on every call.
    #[allow(dead_code)]
    pub fn live_count(&self) -> usize {
        self.live_count
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

// ── Expr traversal for marking ──────────────────────────────────────────────

/// Walk `expr`'s tree looking for every `Expr::Lambda`, appending each
/// one's captured-env handle directly to `out` (which is the mark
/// phase's work stack). Descends into compound variants that can
/// *contain* a lambda (lists, lambda/macro bodies).
///
/// # Performance & safety
/// Uses an explicit local stack rather than recursion to avoid stack
/// overflow on deeply-nested ASTs (e.g. machine-generated code with
/// hundreds of nested lists).
// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::Expr;

    fn heap() -> Heap {
        Heap::new()
    }

    #[test]
    fn alloc_and_get() {
        let mut h = heap();
        let e1 = h.alloc(None);
        let e2 = h.alloc(Some(e1));
        assert_eq!(h.live_count(), 2);
        assert_eq!(h.get(e1).parent, None);
        assert_eq!(h.get(e2).parent, Some(e1));
    }

    #[test]
    fn alloc_reuses_free_slots() {
        let mut h = heap();
        let _e1 = h.alloc(None);
        let _e2 = h.alloc(None);
        assert_eq!(h.capacity(), 2);
        h.collect(&[]); // no roots → both freed
        assert_eq!(h.live_count(), 0);
        let e3 = h.alloc(None);
        // Should have reused slot 0 or 1, not grown.
        assert_eq!(h.capacity(), 2, "should reuse freed slot, not grow");
        assert!(e3.idx == 0 || e3.idx == 1);
    }

    #[test]
    fn collect_frees_unreachable() {
        let mut h = heap();
        let e1 = h.alloc(None);
        let _e2 = h.alloc(Some(e1));
        assert_eq!(h.live_count(), 2);
        // Collect with e1 as the only root.  e2 has e1 as parent, but parent
        // links go *upward* — the mark phase walks from a root through its
        // ancestors (parents), not its children.  e2 is therefore unreachable.
        h.collect(&[e1]);
        assert_eq!(h.live_count(), 1, "only e1 should survive");
    }

    #[test]
    fn lambda_handle_is_marked() {
        let mut h = heap();
        let captured = h.alloc(None);
        // Store a lambda whose captured env is `captured` inside a var.
        let lam = Expr::Lambda(vec!["x".into()], Box::new(Expr::Int(42)), captured);
        let env = h.alloc(None);
        h.env_set(env, "f".into(), lam);
        // Collect with env as root → captured should survive.
        h.collect(&[env]);
        assert!(h.get(captured).vars.is_empty());
        assert_eq!(h.live_count(), 2); // env + captured
        // Collect without roots → everything freed.
        h.collect(&[]);
        assert_eq!(h.live_count(), 0);
    }

    #[test]
    fn stale_handle_detected() {
        let mut h = heap();
        let e1 = h.alloc(None);
        let handle = e1; // save the handle
        h.collect(&[]); // frees e1
        let _e2 = h.alloc(None); // reuses the slot, bumps generation
        assert!(h.check(handle).is_err(), "stale handle should error");
    }

    #[test]
    fn out_of_range_handle_detected() {
        let h = heap();
        let bogus = GcHandle {
            idx: 999,
            generation: 0,
        };
        assert!(h.check(bogus).is_err(), "out-of-range handle should error");
    }

    #[test]
    fn env_get_walks_parent_chain() {
        let mut h = heap();
        let parent = h.alloc(None);
        h.env_set(parent, "x".into(), Expr::Int(1));
        let child = h.alloc(Some(parent));
        match h.env_get(child, "x") {
            Ok(Expr::Int(1)) => {}
            other => panic!("expected Ok(Int(1)), got {:?}", other),
        }
        match h.env_get(child, "y") {
            Err(ref s) if s == "undefined symbol: y" => {}
            other => panic!("expected Err(undefined symbol: y), got {:?}", other),
        }
    }

    #[test]
    fn env_assign_walks_parent_chain() {
        let mut h = heap();
        let parent = h.alloc(None);
        h.env_set(parent, "x".into(), Expr::Int(1));
        let child = h.alloc(Some(parent));
        assert!(h.env_assign(child, "x", Expr::Int(99)).is_ok());
        match h.env_get(child, "x") {
            Ok(Expr::Int(99)) => {}
            other => panic!("expected Ok(Int(99)), got {:?}", other),
        }
        assert!(h.env_assign(child, "y", Expr::Int(0)).is_err());
    }

    #[test]
    fn maybe_collect_respects_threshold() {
        let mut h = heap();
        h.set_gc_threshold(2);
        let e1 = h.alloc(None);
        let _e2 = h.alloc(None);
        assert_eq!(h.live_count(), 2);
        // live_count (2) is NOT > threshold (2), so no collection.
        assert!(!h.maybe_collect(&[e1]));
        assert_eq!(h.live_count(), 2);
        // Now live_count (3) > threshold (2).
        let _e3 = h.alloc(None);
        // _e3 pushes it to 3, but we haven't collected yet.  Now:
        assert!(h.maybe_collect(&[e1]));
        // e1 is a root, _e2 and _e3 are unreachable (no parent chain from e1).
        assert_eq!(h.live_count(), 1);
    }

    #[test]
    fn push_pop_root() {
        let mut h = heap();
        assert_eq!(h.root_count(), 0);
        let e1 = h.alloc(None);
        h.push_root(e1);
        assert_eq!(h.root_count(), 1);
        h.pop_root();
        assert_eq!(h.root_count(), 0);
    }

    #[test]
    #[should_panic(expected = "root stack is empty")]
    fn pop_root_on_empty_panics() {
        let mut h = heap();
        h.pop_root();
    }

    #[test]
    fn root_stack_protects_from_alloc_safety_net() {
        let mut h = heap();
        h.set_gc_threshold(2);

        // Allocate 3 envs, registering them as roots.
        let e1 = h.alloc(None);
        h.push_root(e1);
        let e2 = h.alloc(None);
        h.push_root(e2);
        let e3 = h.alloc(None);
        h.push_root(e3);

        assert_eq!(h.live_count(), 3);

        // Allocate a 4th — this should trigger the safety-net collection
        // in `alloc` since free_list is empty, we have roots, and
        // live_count (3) > threshold (2).
        let e4 = h.alloc(None);
        h.push_root(e4);

        // All 4 should be alive (all were rooted).
        assert_eq!(h.live_count(), 4);

        // Pop and collect with no roots → everything freed.
        h.pop_root();
        h.pop_root();
        h.pop_root();
        h.pop_root();
        assert_eq!(h.root_count(), 0);
        h.collect(&[]);
        assert_eq!(h.live_count(), 0);
    }

    #[test]
    fn generational_wrapping_does_not_match_stale_handle() {
        let mut h = heap();
        // Fill the heap with many allocations to drive up generations.
        let mut handles = Vec::new();
        for _ in 0..10 {
            let e = h.alloc(None);
            handles.push(e);
            h.collect(&[]);
        }
        // Now all slots are freed.  Reallocate and verify the old handles
        // are stale.
        let fresh = h.alloc(None);
        for old in &handles {
            if old.idx == fresh.idx {
                assert!(h.check(*old).is_err(), "old handle {:?} should be stale", old);
            }
        }
    }
}

fn collect_lambda_envs(root: &Expr, out: &mut Vec<GcHandle>) {
    // Small, short-lived stack for the Expr tree walk. In the common case
    // (shallow ASTs) this will have at most a handful of entries and
    // avoids a heap allocation entirely if the compiler elides it.
    let mut stack: Vec<&Expr> = vec![root];

    while let Some(expr) = stack.pop() {
        match expr {
            Expr::Lambda(_, body, env_handle) => {
                out.push(*env_handle);
                stack.push(body);
            }
            Expr::Macro(_, body) => {
                stack.push(body);
            }
            Expr::List(items) => {
                stack.extend(items.iter());
            }
            Expr::Symbol(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Complex(_, _)
            | Expr::Bool(_)
            | Expr::Str(_)
            | Expr::Func(_) => {}
        }
    }
}
