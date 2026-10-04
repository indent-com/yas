//! Deterministic ordering of the connection's internal queue versus incoming SPAWN.
use super::*;

async fn fixture() -> (
    Session,
    mpsc::Receiver<Internal>,
    TestOutbound,
    super::super::super::process::Server,
) {
    let server = super::super::super::process::Server::with_maxima(
        false,
        true,
        super::super::super::process::ProcessMaxima {
            per_session: 2,
            total: 2,
            ..super::super::super::process::ProcessMaxima::DEFAULT
        },
    );
    let mut services = test_services(None, unavailable_connector(), None);
    services.process = Some(super::super::super::yas_process::Runtime::new(
        server.clone(),
    ));
    let hello = ClientHello {
        min_minor: 1,
        max_minor: 1,
        receive: ReceiveLimits::recommended(0),
        client_instance: [61; 16],
        client_name: "completion-order-test".to_owned(),
        client_release: "1".to_owned(),
        families: [family::TRANSFER, family::PROCESS]
            .into_iter()
            .map(|family_id| FamilyOffer {
                family_id,
                versions: vec![1],
                required: true,
            })
            .collect(),
        codecs: Vec::new(),
        extensions: Extensions::default(),
    };
    let negotiated = negotiate(&hello, &services, false, 0).unwrap();
    let outbound = test_outbound(
        OUTBOUND_URGENT_BUFFERED,
        OUTBOUND_CONTROL_BUFFERED,
        u64::from(SERVER_MAX_DECODED),
    );
    let (internal, receiver) = mpsc::channel(32);
    let session = Session::new(
        services,
        negotiated,
        outbound.sender.clone(),
        ConnectionCancellation::default(),
        internal,
        mpsc::channel(1).0,
        mpsc::channel(1).0,
        mpsc::channel(1).0,
        None,
        None,
        None,
        None,
        Arc::new(CreditBudget::new(SERVER_MAX_BUFFERED)),
        Arc::new(CreditBudget::new(SERVER_MAX_BUFFERED)),
        Arc::new(InboundTransferRegistry::new()),
        Arc::new(AtomicBool::new(false)),
        None,
        Arc::default(),
    )
    .await;
    (session, receiver, outbound, server)
}

/// The real writer confirms guarded results. This test writer also keeps every frame for
/// assertions, and never services the internal queue unless the test explicitly chooses it.
async fn write_while(
    future: impl std::future::Future<Output = Result<(), ()>>,
    outbound: &mut TestOutbound,
    cancellation: &ConnectionCancellation,
) -> Vec<Frame> {
    let mut future = std::pin::pin!(future);
    let mut frames = Vec::new();
    timeout(TEST_TIMEOUT, async {
        loop {
            tokio::select! {
                biased;
                result = &mut future => {
                    result.unwrap();
                    break;
                }
                queued = outbound.receivers.recv(cancellation) => {
                    let mut queued = queued.unwrap();
                    if let Some(written) = queued.written.take() {
                        let _ = written.send(tokio::time::Instant::now());
                    }
                    frames.push(queued.frame);
                }
            }
        }
    })
    .await
    .unwrap();
    while let Some(mut queued) = outbound.receivers.try_ready() {
        if let Some(written) = queued.written.take() {
            let _ = written.send(tokio::time::Instant::now());
        }
        frames.push(queued.frame);
    }
    frames
}

fn request(id: u32, operation: u8) -> Frame {
    Frame {
        header: FrameHeader {
            sensitive: true,
            ..FrameHeader::request(family::PROCESS, yas_process_wire::request_kind::SPAWN, id)
        },
        payload: yas_process_wire::Spawn {
            operation_id: [operation; 16],
            flags: (yas_wire::schema::process::SPAWN_MERGE_STDERR
                | yas_wire::schema::process::SPAWN_REPORT_EXIT) as u16,
            environment_kind: yas_process_wire::EnvironmentKind::Empty,
            cwd: yas_process_wire::Cwd::ServerDefault,
            argv: vec![b"/bin/cat".to_vec()],
            env: Vec::new(),
            stdout_receive_credit: 1024,
            stderr_receive_credit: 0,
            extensions: Extensions::default(),
        }
        .encode()
        .unwrap(),
    }
}

fn result(frames: &[Frame], id: u32) -> ResultPrefix {
    let frame = frames
        .iter()
        .find(|frame| frame.header.request_id == Some(id))
        .unwrap();
    assert_eq!(frame.header.class, Class::Result);
    ResultPrefix::decode(&frame.payload).unwrap()
}

async fn spawn(
    session: &mut Session,
    receiver: &mut mpsc::Receiver<Internal>,
    outbound: &mut TestOutbound,
    id: u32,
) -> yas_process_wire::StreamBundle {
    let cancellation = session.cancellation.clone();
    let frames = write_while(
        session.spawn_process(request(id, id as u8)),
        outbound,
        &cancellation,
    )
    .await;
    assert!(frames.is_empty());
    let mut frames = frames;
    loop {
        let event = timeout(TEST_TIMEOUT, receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let complete = matches!(event, Internal::ProcessOperationComplete { request_id, .. } if request_id == id);
        // The real connection also handles its initial upload-GC tick and stdin progress.
        // Do not assume the operation reply is the first internal event in the queue, but
        // never consume a withheld final CLOSE as part of waiting for a replacement SPAWN.
        assert!(!matches!(event, Internal::ProcessOutputClosed { .. }));
        frames.extend(write_while(session.handle_internal(event), outbound, &cancellation).await);
        if complete {
            break;
        }
    }
    let result = result(&frames, id);
    assert_eq!(result.status, Status::Ok);
    yas_process_wire::StreamBundle::decode(&result.body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_close_retires_attachment_before_replacement_spawn() {
    let (mut session, mut receiver, mut outbound, server) = fixture().await;
    let cancellation = session.cancellation.clone();
    let mut bundles = Vec::new();
    for id in 1..=2 {
        bundles.push(spawn(&mut session, &mut receiver, &mut outbound, id).await);
    }
    assert_eq!(session.process.as_ref().unwrap().attachments.len(), 2);
    let mut frames = Vec::new();
    for bundle in &bundles {
        let transfer_id = bundle.stdin.as_ref().unwrap().transfer_id;
        for (kind, payload) in [
            (
                yas_wire::schema::transfer::event::BYTE_DATA,
                ByteData {
                    transfer_id,
                    offset: 0,
                    data: b"last".to_vec(),
                }
                .encode()
                .unwrap(),
            ),
            (
                yas_wire::schema::transfer::event::CLOSE,
                Close {
                    transfer_id,
                    final_data_bytes: 4,
                    status: Status::Ok.code(),
                    detail: Vec::new(),
                }
                .encode()
                .unwrap(),
            ),
        ] {
            frames.extend(
                write_while(
                    session.handle_transfer_event(Frame {
                        header: FrameHeader {
                            sensitive: true,
                            ..FrameHeader::event(family::TRANSFER, kind)
                        },
                        payload,
                    }),
                    &mut outbound,
                    &cancellation,
                )
                .await,
            );
        }
    }
    let mut closes = Vec::new();
    while closes.len() < 2 {
        let event = timeout(TEST_TIMEOUT, receiver.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event, Internal::ProcessOutputClosed { .. }) {
            closes.push(event);
        } else {
            frames.extend(
                write_while(session.handle_internal(event), &mut outbound, &cancellation).await,
            );
        }
    }
    // Withhold both output-close retirement messages, exactly as an incoming-channel select
    // can win over the ready internal queue. The output worker must NOT publish CLOSE itself.
    frames.extend(write_while(async { Ok(()) }, &mut outbound, &cancellation).await);
    assert_eq!(session.process.as_ref().unwrap().attachments.len(), 2);
    assert!(
        session
            .process
            .as_ref()
            .unwrap()
            .attachments
            .values()
            .all(|attachment| attachment.exited)
    );
    assert!(
        !frames
            .iter()
            .any(|frame| frame.header.family == family::TRANSFER
                && frame.header.kind == yas_wire::schema::transfer::event::CLOSE)
    );
    for bundle in &bundles {
        let output = frames
            .iter()
            .filter(|frame| {
                frame.header.family == family::TRANSFER
                    && frame.header.kind == yas_wire::schema::transfer::event::BYTE_DATA
            })
            .map(|frame| ByteData::decode(&frame.payload).unwrap())
            .filter(|data| data.transfer_id == bundle.stdout.transfer_id)
            .flat_map(|data| data.data)
            .collect::<Vec<_>>();
        assert_eq!(output, b"last");
        assert!(
            frames
                .iter()
                .any(|frame| frame.header.family == family::PROCESS
                    && frame.header.kind == yas_process_wire::event_kind::EXIT
                    && yas_process_wire::ExitReport::handle_of(&frame.payload)
                        == Some(bundle.process_handle))
        );
    }
    // Incoming work still executes while retirement is withheld, and the full attachment
    // bound still applies; no serialization, retry, sleep or relaxed capacity hides the race.
    let refused = write_while(
        session.spawn_process(request(10, 10)),
        &mut outbound,
        &cancellation,
    )
    .await;
    assert_eq!(result(&refused, 10).status, Status::ResourceExhausted);
    for (round, close) in closes.into_iter().enumerate() {
        let frames =
            write_while(session.handle_internal(close), &mut outbound, &cancellation).await;
        let close = frames
            .iter()
            .find(|frame| {
                frame.header.family == family::TRANSFER
                    && frame.header.kind == yas_wire::schema::transfer::event::CLOSE
            })
            .unwrap();
        assert!(close.header.sensitive);
        let close = Close::decode(&close.payload).unwrap();
        assert_eq!(
            (close.status, close.final_data_bytes),
            (Status::Ok.code(), 4)
        );
        assert_eq!(session.process.as_ref().unwrap().attachments.len(), 1);
        assert!(!session.outbound.contains_key(&close.transfer_id));
        assert!(
            !session
                .process
                .as_ref()
                .unwrap()
                .transfer_to_attachment
                .contains_key(&close.transfer_id)
        );
        spawn(&mut session, &mut receiver, &mut outbound, 3 + round as u32).await;
        assert_eq!(session.process.as_ref().unwrap().attachments.len(), 2);
        let refused = write_while(
            session.spawn_process(request(11, 11)),
            &mut outbound,
            &cancellation,
        )
        .await;
        assert_eq!(result(&refused, 11).status, Status::ResourceExhausted);
    }
    let replay = write_while(
        session.spawn_process(request(12, 1)),
        &mut outbound,
        &cancellation,
    )
    .await;
    assert_eq!(result(&replay, 12).status, Status::Stale);
    assert_eq!(
        session
            .process
            .as_ref()
            .unwrap()
            .service
            .snapshot(1)
            .unwrap()
            .records
            .len(),
        2
    );
    cancellation.cancel();
    timeout(TEST_TIMEOUT, session.shutdown()).await.unwrap();
    server.shutdown().await;
}
