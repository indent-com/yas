use super::*;

fn ping_reply() -> Frame {
    Frame {
        header: FrameHeader::result(
            family::CORE,
            yas_wire::core::request_kind::PING,
            heartbeat::REQUEST_ID,
        ),
        payload: ResultPrefix {
            status: Status::Ok,
            detail: Extensions::default(),
            body: PingResult {
                receiver_receive_ns: 1,
                receiver_send_ns: 2,
            }
            .encode()
            .unwrap(),
        }
        .encode()
        .unwrap(),
    }
}

#[tokio::test(start_paused = true)]
async fn ping_rtt_excludes_writer_queue_and_heartbeat_scheduling_delay() {
    let mut outbound = test_outbound(1024, 1024, 1024);
    let heartbeat = Arc::new(heartbeat::Heartbeat::default());
    let task = tokio::spawn({
        let heartbeat = Arc::clone(&heartbeat);
        let sender = outbound.sender.clone();
        async move { heartbeat.run(&sender, Duration::from_secs(1)).await }
    });
    for expected in [0, 200_000] {
        let mut ping = outbound.receivers.control.recv().await.unwrap();
        assert_eq!(ping.frame.header.request_id, Some(heartbeat::REQUEST_ID));
        tokio::time::advance(Duration::from_millis(500)).await;
        ping.written
            .take()
            .unwrap()
            .send(tokio::time::Instant::now())
            .unwrap();
        tokio::time::advance(Duration::from_millis(200)).await;
        assert_eq!(heartbeat.receive(&ping_reply()), Ok(true));
        // No await between receive and advancing the clock: the run future
        // cannot turn its own delayed wakeup into a larger RTT observation.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(heartbeat.rtt_us.load(Ordering::Relaxed), expected);
    }
    task.abort();
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn ping_rtt_reaches_active_native_surface_views_and_drops_with_connection() {
    let state = super::super::super::tests::process_transport::test_state(
        super::super::super::process::Server::new(false, true),
    );
    let surface_handle = add_test_surface(&mut *state.session.lock().await, 7);
    let mut services = Services::from_state(&state);
    services.ping_interval = Duration::from_secs(1);
    let (mut client, codec, _, task) = start_session(services, &[family::SURFACE]).await;
    let request = yas_surface::OpenView {
        surface_handle,
        width: 320,
        height: 180,
        max_fps: 60,
        decoder_capacity: 3,
        codec_versions: vec![yas_wire::schema::surface::CODEC_H264_V1 as u16],
        extensions: Extensions::default(),
    };
    for request_id in [1, 2] {
        write_request(
            &mut client,
            &codec,
            family::SURFACE,
            yas_wire::schema::surface::request::OPEN_VIEW,
            request_id,
            &request,
        )
        .await;
        assert_eq!(
            next_result(
                &mut client,
                &codec,
                family::SURFACE,
                yas_wire::schema::surface::request::OPEN_VIEW,
                request_id
            )
            .await
            .status,
            Status::Ok
        );
    }
    let (mut ids, estimate) = {
        let mut shared = state.session.lock().await;
        let ids: Vec<_> = shared
            .clients
            .iter()
            .filter_map(|(&id, client)| client.native_surface.as_ref().map(|_| id))
            .collect();
        assert_eq!(ids.len(), 2);
        let estimate = Arc::clone(
            &shared.clients[&ids[0]]
                .native_surface
                .as_ref()
                .unwrap()
                .transport_rtt_us,
        );
        for id in &ids {
            let view = shared.clients.get_mut(id).unwrap();
            assert!(Arc::ptr_eq(
                &estimate,
                &view.native_surface.as_ref().unwrap().transport_rtt_us
            ));
            view.surface_ack_timing.baseline_ms = Some(75.0);
            super::super::super::record_surface_frame_sent(view, 7, 10_000, false, Instant::now());
        }
        (ids, estimate)
    };
    for (delay_ms, expected_ms) in [
        (200, 0),
        (200, 200),
        (800, 200),
        (200, 200),
        (500, 200),
        (500, 500),
        (25, 25),
        (1500, 25),
    ] {
        let ping = next_frame(&mut client, &codec).await;
        assert_eq!(ping.header.request_id, Some(heartbeat::REQUEST_ID));
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        client
            .write_all(&codec.encode_stream(&ping_reply()).unwrap())
            .await
            .unwrap();
        // Drain a client-originated round trip as a reader/dispatcher barrier.
        write_request(
            &mut client,
            &codec,
            family::CORE,
            yas_wire::core::request_kind::PING,
            42,
            &Ping {
                sender_monotonic_ns: 0,
            },
        )
        .await;
        next_result(
            &mut client,
            &codec,
            family::CORE,
            yas_wire::core::request_kind::PING,
            42,
        )
        .await;
        tokio::task::yield_now().await;
        assert_eq!(estimate.load(Ordering::Relaxed), expected_ms * 1_000);
        let shared = state.session.lock().await;
        for id in &ids {
            let view = &shared.clients[id];
            assert_eq!(
                super::super::super::surface_ack_window_ms(view),
                (expected_ms as f32).max(75.0)
            );
            assert_eq!(view.surface_inflight_frames.len(), 1);
        }
    }
    write_request(
        &mut client,
        &codec,
        family::SURFACE,
        yas_wire::schema::surface::request::OPEN_VIEW,
        3,
        &yas_surface::OpenView {
            decoder_capacity: 1,
            ..request
        },
    )
    .await;
    assert_eq!(
        next_result(
            &mut client,
            &codec,
            family::SURFACE,
            yas_wire::schema::surface::request::OPEN_VIEW,
            3,
        )
        .await
        .status,
        Status::Ok
    );
    {
        let shared = state.session.lock().await;
        let (&id, view) = shared
            .clients
            .iter()
            .find(|(id, view)| !ids.contains(id) && view.native_surface.is_some())
            .unwrap();
        assert!(view.surface_ack_timing.baseline_ms.is_none());
        assert_eq!(super::super::super::surface_ack_window_ms(view), 25.0);
        ids.push(id);
    }
    drop(client);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    let shared = state.session.lock().await;
    assert!(ids.iter().all(|id| !shared.clients.contains_key(id)));
    assert_eq!(Arc::strong_count(&estimate), 1);
}

fn services_with_heartbeat() -> Services {
    let mut services = test_services(None, unavailable_connector(), None);
    services.ping_interval = Duration::from_secs(1);
    services
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn peer_expiry_releases_only_its_client_record_and_surface_claims() {
    let state = super::super::super::tests::process_transport::test_state(
        super::super::super::process::Server::new(false, true),
    );
    let mut services = Services::from_state(&state);
    services.ping_interval = Duration::from_millis(100);
    let (mut client, server) = tokio::io::duplex(2 * 1024 * 1024);
    let cancellation = ConnectionCancellation::default();
    let registration = state.connections.register(cancellation.clone()).unwrap();
    let task = tokio::spawn(serve_registered(
        server,
        services,
        cancellation,
        Some(registration),
        None,
        None,
        ConnectionOrigin::Network,
    ));
    let (_, hello) = handshake(&mut client, &[]).await;
    timeout(TEST_TIMEOUT, async {
        while !state
            .session
            .lock()
            .await
            .native_yas_clients
            .contains_key(&hello.session_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let other_owner = [0xab; 16];
    {
        let mut shared = state.session.lock().await;
        assert!(shared.native_yas_clients.contains_key(&hello.session_id));
        shared
            .native_surface_claims
            .insert((hello.session_id, 1), (800, 600, 240));
        shared
            .native_surface_claims
            .insert((other_owner, 1), (1920, 1080, 120));
    }
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    timeout(TEST_TIMEOUT, async {
        loop {
            let shared = state.session.lock().await;
            if !shared.native_yas_clients.contains_key(&hello.session_id) {
                assert!(
                    !shared
                        .native_surface_claims
                        .contains_key(&(hello.session_id, 1))
                );
                assert_eq!(
                    shared.mediated_size_for_surface(1, &[]),
                    Some((1920, 1080, 120))
                );
                break;
            }
            drop(shared);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(client);
}

#[tokio::test(start_paused = true)]
async fn pings_preserve_an_otherwise_idle_session() {
    let (mut client, codec, hello, task) = start_session(services_with_heartbeat(), &[]).await;
    assert!(
        hello
            .families
            .iter()
            .find(|family| family.family_id == family::CORE)
            .unwrap()
            .operations
            .iter()
            .any(
                |operation| operation.kind == yas_wire::core::request_kind::PING
                    && operation.server_sends
            )
    );
    for _ in 0..5 {
        let ping = next_frame(&mut client, &codec).await;
        assert_eq!(
            ping.header,
            FrameHeader::request(
                family::CORE,
                yas_wire::core::request_kind::PING,
                heartbeat::REQUEST_ID
            )
        );
        Ping::decode(&ping.payload).unwrap();
        let reply = Frame {
            header: FrameHeader::result(
                family::CORE,
                yas_wire::core::request_kind::PING,
                heartbeat::REQUEST_ID,
            ),
            payload: ResultPrefix {
                status: Status::Ok,
                detail: Extensions::default(),
                body: PingResult {
                    receiver_receive_ns: 1,
                    receiver_send_ns: 2,
                }
                .encode()
                .unwrap(),
            }
            .encode()
            .unwrap(),
        };
        client
            .write_all(&codec.encode_stream(&reply).unwrap())
            .await
            .unwrap();
    }
    // Client-originated pings still work on the same live session.
    write_request(
        &mut client,
        &codec,
        family::CORE,
        yas_wire::core::request_kind::PING,
        42,
        &Ping {
            sender_monotonic_ns: 3,
        },
    )
    .await;
    assert_eq!(
        next_result(
            &mut client,
            &codec,
            family::CORE,
            yas_wire::core::request_kind::PING,
            42
        )
        .await
        .status,
        Status::Ok
    );
    assert!(!task.is_finished());
    drop(client);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn silent_peer_expires_even_while_it_keeps_reading() {
    let (mut client, codec, _, task) = start_session(services_with_heartbeat(), &[]).await;
    let start = tokio::time::Instant::now();
    let ping = next_frame(&mut client, &codec).await;
    assert_eq!(ping.header.class, Class::Request);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    assert!(read_frame(&mut client, &codec).await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn peer_expiry_interrupts_a_blocked_writer_and_handler() {
    let gate = Arc::new(OutboundWriterGate::result(
        family::CORE,
        yas_wire::core::request_kind::PING,
        42,
        false,
    ));
    let mut services = services_with_heartbeat();
    services.outbound_writer_gate = Some(Arc::clone(&gate));
    let (mut client, codec, _, task) = start_session(services, &[]).await;
    // Park the writer and saturate its control queue, forcing the dispatcher
    // itself to await an outbound send. Neither can service a timeout branch.
    for request_id in 42..42 + OUTBOUND_CONTROL_QUEUE as u32 + 20 {
        write_request(
            &mut client,
            &codec,
            family::CORE,
            yas_wire::core::request_kind::PING,
            request_id,
            &Ping {
                sender_monotonic_ns: 0,
            },
        )
        .await;
    }
    gate.wait_until_reached().await;
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
    assert!(read_frame(&mut client, &codec).await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn zero_interval_disables_peer_expiry() {
    let services = test_services(None, unavailable_connector(), None);
    let (client, _, _, task) = start_session(services, &[]).await;
    tokio::time::advance(Duration::from_secs(120)).await;
    assert!(!task.is_finished());
    drop(client);
    timeout(TEST_TIMEOUT, task).await.unwrap().unwrap();
}
