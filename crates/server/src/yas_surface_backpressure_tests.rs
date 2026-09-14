#[cfg(unix)]
async fn backpressured_surface_session(
    capacity: u8,
) -> (
    AppState,
    DuplexStream,
    FrameCodec,
    yas_surface::ViewResult,
    tokio::task::JoinHandle<()>,
    mpsc::UnboundedReceiver<FrameHeader>,
) {
    let state = super::super::tests::process_transport::test_state(
        super::super::process::Server::new(false, true),
    );
    let surface_handle = {
        let mut shared = state.session.lock().await;
        add_test_surface(&mut shared, 7)
    };
    // Match the hosted endpoint instead of hiding four MiB inside a test pipe.
    let (mut client, server) = tokio::io::duplex(super::super::LOCAL_SESSION_BUFFER);
    let cancellation = ConnectionCancellation::default();
    let registration = state.connections.register(cancellation.clone()).unwrap();
    let mut services = Services::from_state(&state);
    services.receive_max_buffered_override = Some(TEST_PEER_MAX_BUFFERED);
    let (queued_tx, queued_rx) = mpsc::unbounded_channel();
    services.result_queued_probe = Some(queued_tx);
    let task = tokio::spawn(serve_registered(
        server,
        services,
        cancellation,
        Some(registration),
        None,
        None,
        ConnectionOrigin::Network,
    ));
    let (codec, _) = handshake(&mut client, &[family::SURFACE]).await;
    write_request(
        &mut client,
        &codec,
        family::SURFACE,
        yas_wire::schema::surface::request::OPEN_VIEW,
        10,
        &yas_surface::OpenView {
            surface_handle,
            width: 320,
            height: 180,
            max_fps: 60,
            decoder_capacity: capacity,
            codec_versions: vec![yas_wire::schema::surface::CODEC_H264_V1 as u16],
            extensions: Extensions::default(),
        },
    )
    .await;
    let opened = next_result(
        &mut client,
        &codec,
        family::SURFACE,
        yas_wire::schema::surface::request::OPEN_VIEW,
        10,
    )
    .await;
    assert_eq!(opened.status, Status::Ok);
    let view = yas_surface::ViewResult::decode(&opened.body).unwrap();
    (state, client, codec, view, task, queued_rx)
}

#[cfg(unix)]
async fn surface_test_ping(client: &mut DuplexStream, codec: &FrameCodec, id: u32) {
    write_request(
        client,
        codec,
        family::CORE,
        yas_wire::core::request_kind::PING,
        id,
        &Ping {
            sender_monotonic_ns: 1,
        },
    )
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn surface_eos_waits_for_decoder_credit_without_blocking_ping() {
    let (state, mut client, codec, view, task, _) = backpressured_surface_session(1).await;
    send_hidden_surface_frame(&state, 7, (320, 180), 1, 0, true, large_h264_test_frame(16)).await;
    let data = next_frame(&mut client, &codec).await;
    assert_eq!(
        yas_surface::SurfaceFrame::decode(&data.payload)
            .unwrap()
            .sequence,
        1
    );
    state
        .session
        .lock()
        .await
        .compositor
        .as_mut()
        .unwrap()
        .surfaces
        .remove(&7);
    state
        .surface_catalogue_updates
        .send_modify(|revision| *revision = revision.wrapping_add(1));
    timeout(TEST_TIMEOUT, async {
        while state
            .session
            .lock()
            .await
            .clients
            .values()
            .any(|client| !client.catalog_visible && client.surface_subscriptions.contains(&7))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    surface_test_ping(&mut client, &codec, 11).await;
    let reply = next_frame(&mut client, &codec).await;
    assert_eq!(
        reply.header,
        FrameHeader::result(family::CORE, yas_wire::core::request_kind::PING, 11),
        "EOS must not exceed the occupied one-frame window"
    );
    write_event(
        &mut client,
        &codec,
        family::SURFACE,
        yas_wire::schema::surface::event::FRAME_ACK,
        &yas_surface::FrameAck {
            view_id: view.view_id,
            feedback: yas_surface::FrameFeedback {
                presented_sequence: 1,
                decoder_queue_depth: 0,
                available_slots: 1,
            },
        },
    )
    .await;
    let frame = next_frame(&mut client, &codec).await;
    let eos = yas_surface::SurfaceFrame::decode(&frame.payload).unwrap();
    assert_eq!(eos.sequence, 2);
    assert_ne!(
        eos.flags & yas_wire::schema::surface::FRAME_END_OF_STREAM as u16,
        0
    );
    // An EOS ACK races the catalogue boundary; it must not disconnect the peer.
    write_event(
        &mut client,
        &codec,
        family::SURFACE,
        yas_wire::schema::surface::event::FRAME_ACK,
        &yas_surface::FrameAck {
            view_id: view.view_id,
            feedback: yas_surface::FrameFeedback {
                presented_sequence: 2,
                decoder_queue_depth: 0,
                available_slots: 1,
            },
        },
    )
    .await;
    surface_test_ping(&mut client, &codec, 12).await;
    assert_eq!(
        next_result(
            &mut client,
            &codec,
            family::CORE,
            yas_wire::core::request_kind::PING,
            12
        )
        .await
        .status,
        Status::Ok
    );
    drop(client);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn surface_bulk_and_lifetime_barriers_do_not_queue_megabytes_ahead_of_ping() {
    for operation in [
        0,
        yas_wire::schema::surface::request::CONFIGURE_VIEW,
        yas_wire::schema::surface::request::RESET_VIEW,
        yas_wire::schema::surface::request::CLOSE_VIEW,
    ] {
        let (state, mut client, codec, view, task, mut queued) =
            backpressured_surface_session(4).await;
        send_hidden_surface_frame(
            &state,
            7,
            (320, 180),
            1,
            0,
            true,
            large_h264_test_frame(3 * 1024 * 1024),
        )
        .await;
        // Allow the producer to reach transport backpressure before probing.
        // The queue bound below is deliberately independent of its configured
        // capacity: using LOCAL_SESSION_BUFFER there hid a one-MiB FIFO.
        tokio::time::sleep(Duration::from_millis(50)).await;
        match operation {
            yas_wire::schema::surface::request::CONFIGURE_VIEW => {
                write_request(
                    &mut client,
                    &codec,
                    family::SURFACE,
                    operation,
                    11,
                    &yas_surface::ConfigureView {
                        view_id: view.view_id,
                        width: 320,
                        height: 180,
                        max_fps: 30,
                        decoder_capacity: 4,
                        latency_target_ns: 0,
                        extensions: Extensions::default(),
                    },
                )
                .await
            }
            yas_wire::schema::surface::request::RESET_VIEW => {
                write_request(
                    &mut client,
                    &codec,
                    family::SURFACE,
                    operation,
                    11,
                    &yas_surface::ResetView {
                        view_id: view.view_id,
                    },
                )
                .await
            }
            yas_wire::schema::surface::request::CLOSE_VIEW => {
                write_request(
                    &mut client,
                    &codec,
                    family::SURFACE,
                    operation,
                    11,
                    &yas_surface::CloseView {
                        view_id: view.view_id,
                    },
                )
                .await
            }
            _ => {}
        }
        surface_test_ping(&mut client, &codec, 12).await;
        // Keep the transport blocked until Ping reaches the control lane.
        // Draining immediately races request dispatch and measures scheduling
        // rather than bytes buffered downstream of the priority writer.
        timeout(TEST_TIMEOUT, async {
            while queued.recv().await.unwrap()
                != FrameHeader::result(family::CORE, yas_wire::core::request_kind::PING, 12)
            {
            }
        })
        .await
        .expect("Ping must be queued while the transport is blocked");
        let mut bytes_before_ping = 0usize;
        let mut fragments = 0u16;
        let mut fragment_count = 0u16;
        loop {
            let frame = next_frame(&mut client, &codec).await;
            if frame.header.class == Class::Result {
                assert_eq!(
                    frame.header,
                    FrameHeader::result(family::CORE, yas_wire::core::request_kind::PING, 12),
                    "lifetime Result overtook its frame boundary"
                );
                break;
            }
            let fragment = yas_surface::SurfaceFrame::decode(&frame.payload).unwrap();
            assert_eq!(fragment.fragment_index, fragments);
            fragments += 1;
            fragment_count = fragment.fragment_count;
            bytes_before_ping += frame.payload.len();
        }
        eprintln!("surface operation {operation}: {bytes_before_ping} bytes precede Ping");
        assert!(
            bytes_before_ping <= 96 * 1024,
            "{bytes_before_ping} bytes ahead of ping for operation {operation}"
        );
        // Complete the frame and verify that asynchronous retirement/configure
        // cannot truncate it or publish its Result ahead of remaining fragments.
        while fragments < fragment_count {
            let frame = next_frame(&mut client, &codec).await;
            let fragment = yas_surface::SurfaceFrame::decode(&frame.payload).unwrap();
            assert_eq!(fragment.fragment_index, fragments);
            fragments += 1;
        }
        if operation != 0 {
            assert_eq!(
                next_result(&mut client, &codec, family::SURFACE, operation, 11)
                    .await
                    .status,
                Status::Ok
            );
        }
        drop(client);
        timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn surface_open_queue_pressure_does_not_block_ping() {
    let (state, mut client, codec, _view, task, _) = backpressured_surface_session(4).await;
    let (commands, receiver) = std::sync::mpsc::sync_channel(1);
    commands
        .try_send(yas_compositor::CompositorCommand::DragLeave)
        .unwrap();
    let (original, surface_handle) = {
        let mut session = state.session.lock().await;
        let handle = session.surface_handles.get_or_insert(7).unwrap();
        let original = std::mem::replace(
            &mut session.compositor.as_mut().unwrap().handle.command_tx,
            commands,
        );
        (original, handle)
    };
    let (release, released) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let _ = released.recv_timeout(Duration::from_secs(2));
        while receiver.recv().is_ok() {}
    });
    let responsive = timeout(Duration::from_millis(250), async {
        write_request(
            &mut client,
            &codec,
            family::SURFACE,
            yas_wire::schema::surface::request::OPEN_VIEW,
            11,
            &yas_surface::OpenView {
                surface_handle,
                width: 320,
                height: 180,
                max_fps: 60,
                decoder_capacity: 4,
                codec_versions: vec![yas_wire::schema::surface::CODEC_H264_V1 as u16],
                extensions: Extensions::default(),
            },
        )
        .await;
        let opened = next_result(
            &mut client,
            &codec,
            family::SURFACE,
            yas_wire::schema::surface::request::OPEN_VIEW,
            11,
        )
        .await;
        assert_eq!(opened.status, Status::Ok);
        write_request(
            &mut client,
            &codec,
            family::SURFACE,
            yas_wire::schema::surface::request::FOCUS,
            13,
            &yas_surface::Focus {
                surface_handle,
                operation_id: [13; 16],
                focused: true,
                extensions: Extensions::default(),
            },
        )
        .await;
        assert_eq!(
            next_result(
                &mut client,
                &codec,
                family::SURFACE,
                yas_wire::schema::surface::request::FOCUS,
                13
            )
            .await
            .status,
            Status::Ok
        );
        surface_test_ping(&mut client, &codec, 12).await;
        next_result(
            &mut client,
            &codec,
            family::CORE,
            yas_wire::core::request_kind::PING,
            12,
        )
        .await
    })
    .await;
    release.send(()).unwrap();
    state
        .session
        .lock()
        .await
        .compositor
        .as_mut()
        .unwrap()
        .handle
        .command_tx = original;
    drain.join().unwrap();
    drop(client);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    assert_eq!(
        responsive
            .expect("opening a view must leave Ping responsive with a full compositor queue")
            .status,
        Status::Ok
    );
}
