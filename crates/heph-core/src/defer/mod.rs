pub struct ScopeCall<F: FnOnce()> {
    pub c: Option<F>,
}

impl<F: FnOnce()> Drop for ScopeCall<F> {
    fn drop(&mut self) {
        // c is always Some when drop is called; it can only be None if drop runs twice
        #[expect(
            clippy::unwrap_used,
            reason = "c is always Some at drop time; the defer! macro guarantees this"
        )]
        self.c.take().unwrap()()
    }
}

#[macro_export]
macro_rules! defer {
    ($($data: tt)*) => {
        let _scope_call = $crate::defer::ScopeCall {
            c: Some(|| -> () { { $($data)* } }),
        };
    };
}
