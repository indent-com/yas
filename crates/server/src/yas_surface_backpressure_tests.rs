#[cfg(unix)]
async fn backpressured_surface_session(
    capacity: u8,
) -> (
    AppState,
    DuplexStream,
    FrameCodec,
    yas_surface::ViewResult,
    tokio::task::JoinHandle<()>,
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
    (state, client, codec, view, task)
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
    let (state, mut client, codec, view, task) = backpressured_surface_session(1).await;
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
        let (state, mut client, codec, view, task) = backpressured_surface_session(4).await;
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
        timeout(TEST_TIMEOUT, async {
            loop {
                let bytes = state
                    .session
                    .lock()
                    .await
                    .native_yas_clients
                    .values()
                    .next()
                    .unwrap()
                    .outbound_bytes
                    .load(Ordering::Relaxed);
                if bytes >= 48 * 1024 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
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
        assert!(
            bytes_before_ping
                <= super::super::LOCAL_SESSION_BUFFER + 3 * (SURFACE_RELIABLE_FRAGMENT_BYTES + 64),
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
