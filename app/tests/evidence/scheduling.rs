// Deterministic scheduling/crash seams only. Hooks run at entry to the actual Session::persist and
// immediately after the actual CurrentNote::rewrite_transcript succeeds. No operation is replaced.
type Hook = Box<dyn FnOnce()>;
thread_local! {
    static BEFORE_PERSIST: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
    static AFTER_REWRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub(crate) fn before_persist(hook: Hook) {
    BEFORE_PERSIST.with(|slot| *slot.borrow_mut() = Some(hook));
}
pub(crate) fn persist_entry() {
    let hook = BEFORE_PERSIST.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}
pub(crate) fn crash_after_rewrite() {
    AFTER_REWRITE.set(true);
}
pub(crate) fn rewrite_complete() {
    if AFTER_REWRITE.replace(false) {
        panic!("synthetic crash after actual note rewrite");
    }
}
