mod support {
    pub(crate) mod host_fixture;
}

use std::{
    sync::{
        Arc,
        atomic::Ordering,
    },
    time::{
        Duration,
        Instant,
    },
};

use sloper_extension_host::{
    Engine,
    Error,
    Host,
    validate_component,
};
use support::host_fixture::{
    TestHost,
    fixture,
    request,
    run,
};
use tokio::{
    sync::watch,
    time::timeout,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_deadline_interrupts_computation_without_interrupting_another_attempt() {
    let engine = Engine::new().expect("engine config is valid");
    let spin_engine = engine.clone();
    let spin_host = Arc::new(TestHost::default());
    let spinning = Arc::clone(&spin_host);
    let (_stopping, stopping) = watch::channel(None);
    let (native, native_receiver) = watch::channel(Some(Instant::now() + Duration::from_secs(90)));
    let spinner = tokio::spawn(async move {
        let bytes = fixture();
        let manifest = validate_component(&bytes).expect("fixture manifest is valid");
        spin_engine
            .run(&bytes, &manifest, request("spin"), spinning, stopping, native_receiver)
            .await
    });
    timeout(Duration::from_secs(60), spin_host.started.notified())
        .await
        .expect("guest starts computing");
    native
        .send(Some(Instant::now() + Duration::from_millis(200)))
        .expect("computing attempt owns receiver");
    let host = Arc::new(TestHost {
        token_delay: Duration::from_millis(500),
        ..TestHost::default()
    });
    let survivor = timeout(
        Duration::from_secs(15),
        run(&engine, request("wait"), Arc::clone(&host), None),
    )
    .await
    .expect("independent attempt remains live");
    survivor.expect("shared epochs do not interrupt another store");
    let stopped = spinner.await.expect("computing task returns normally");
    assert!(matches!(stopped, Err(Error::Deadline)), "result={stopped:?}");
    assert_eq!(host.finishes.load(Ordering::Acquire), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_deadline_update_interrupts_a_guest_making_no_calls() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost::default());
    let spinning = Arc::clone(&host);
    let (stopping, stopping_receiver) = watch::channel(None);
    let (_native, native) = watch::channel(Some(Instant::now() + Duration::from_secs(90)));
    let spinner = tokio::spawn(async move {
        let bytes = fixture();
        let manifest = validate_component(&bytes).expect("fixture manifest is valid");
        engine
            .run(&bytes, &manifest, request("spin"), spinning, stopping_receiver, native)
            .await
    });
    timeout(Duration::from_secs(60), host.started.notified())
        .await
        .expect("guest starts computing");
    stopping
        .send(Some(Instant::now() + Duration::from_millis(100)))
        .expect("computing attempt owns cancellation receiver");
    let result = timeout(Duration::from_secs(3), spinner)
        .await
        .expect("cancellation interrupts computation")
        .expect("computing task returns normally");
    assert!(matches!(result, Err(Error::Stopped)), "result={result:?}");
    assert_eq!(host.finishes.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_guest_settlement_uses_the_original_native_deadline() {
    let engine = Engine::new().expect("engine config is valid");
    let host = Arc::new(TestHost {
        finish_delay: Duration::from_secs(60),
        ..TestHost::default()
    });
    let settling = Arc::clone(&host);
    let (_stopping, stopping) = watch::channel(None);
    let (native, native_receiver) = watch::channel(Some(Instant::now() + Duration::from_secs(90)));
    let attempt = tokio::spawn(async move {
        let bytes = fixture();
        let manifest = validate_component(&bytes).expect("fixture manifest is valid");
        engine
            .run(
                &bytes,
                &manifest,
                request("counter"),
                settling,
                stopping,
                native_receiver,
            )
            .await
    });
    timeout(Duration::from_secs(60), host.finish_started.notified())
        .await
        .expect("successful guest begins settlement");
    native
        .send(Some(Instant::now() + Duration::from_millis(100)))
        .expect("settlement retains native deadline receiver");
    let result = timeout(Duration::from_secs(3), attempt)
        .await
        .expect("settlement does not get a fresh time allowance")
        .expect("attempt returns normally");
    assert!(matches!(result, Err(Error::Deadline)), "result={result:?}");
    assert_eq!(host.finishes.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn settlement_success_cannot_override_a_simultaneously_expired_deadline() {
    let engine = Engine::new().expect("engine config is valid");
    let bytes = fixture();
    let manifest = validate_component(&bytes).expect("fixture manifest is valid");
    let (_stopping, stopping) = watch::channel(None);
    let (native, native_receiver) = watch::channel(None);
    let host = Arc::new(TestHost {
        finish_expiry: Some(native),
        ..TestHost::default()
    });
    let result = engine
        .run(
            &bytes,
            &manifest,
            request("counter"),
            Arc::clone(&host) as Arc<dyn Host>,
            stopping,
            native_receiver,
        )
        .await;
    assert!(matches!(result, Err(Error::Deadline)), "result={result:?}");
    assert_eq!(host.finishes.load(Ordering::Acquire), 1);
}
