use std::{
    cell::Cell,
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::Context,
};

use futures::FutureExt;

use super::{
    AmbientGuard,
    CURRENT,
    RunState,
    Source,
};
use crate::{
    Error,
    Result,
};

fn state() -> Rc<RunState> {
    Rc::new(RunState {
        operation: "op-1".into(),
        configuration: "{}".into(),
        metadata: std::cell::RefCell::new(BTreeMap::new()),
        pending: std::cell::RefCell::new(Vec::new()),
        uploads: std::cell::RefCell::new(Vec::new()),
        failed: std::cell::RefCell::new(None),
    })
}

fn completion(future: impl Future<Output = Result<()>> + 'static) -> super::Completion {
    future.boxed_local().shared()
}

/// Dispatch observes every accepted completion even after an earlier
/// completion fails; this prevents an ignored sibling write from escaping.
#[test]
fn drain_observes_all_pending_completions() {
    let run = state();
    let observed = Rc::new(Cell::new(0));
    let first = Rc::clone(&observed);
    let second = Rc::clone(&observed);
    run.register(completion(async move {
        first.set(first.get() + 1);
        Err(Error::TooLarge)
    }));
    run.register(completion(async move {
        second.set(second.get() + 1);
        Ok(())
    }));
    let mut drain = Box::pin(run.drain());
    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::new(&mut drain).poll(&mut context),
        std::task::Poll::Ready(())
    ));
    assert_eq!(observed.get(), 2);
    assert!(matches!(run.check(), Err(Error::TooLarge)));
    assert!(run.pending.borrow().is_empty());
}

#[test]
fn failure_is_sticky_while_returned_errors_remain_specific() {
    let run = state();
    assert_eq!(run.fail(Error::internal("first")), Error::Internal("first"));
    assert_eq!(run.fail(Error::internal("second")), Error::Internal("second"));
    assert_eq!(run.check(), Err(Error::Internal("first")));
}

#[test]
fn ambient_guard_restores_previous_run() {
    let outer = state();
    let inner = state();
    CURRENT.with(|current| *current.borrow_mut() = Some(Rc::clone(&outer)));
    {
        let _guard = AmbientGuard(CURRENT.with(|current| current.replace(Some(Rc::clone(&inner)))));
        assert!(CURRENT.with(|current| { current.borrow().as_ref().is_some_and(|run| Rc::ptr_eq(run, &inner)) }));
    }
    assert!(CURRENT.with(|current| { current.borrow().as_ref().is_some_and(|run| Rc::ptr_eq(run, &outer)) }));
    CURRENT.with(|current| *current.borrow_mut() = None);
}

#[test]
fn source_deserialization_uses_ambient_metadata() {
    let run = state();
    run.metadata.borrow_mut().insert(
        "source-1".into(),
        Source {
            id: "source-1".into(),
            filename: Some("report.pdf".into()),
            media_type: Some("application/pdf".into()),
            size: Some(12),
        },
    );
    let previous = CURRENT.with(|current| current.replace(Some(run)));
    let source: Source = serde_json::from_str("\"source-1\"").unwrap();
    CURRENT.with(|current| *current.borrow_mut() = previous);
    assert_eq!(source.filename(), Some("report.pdf"));
    assert_eq!(source.media_type(), Some("application/pdf"));
    assert_eq!(source.size(), Some(12));
}
