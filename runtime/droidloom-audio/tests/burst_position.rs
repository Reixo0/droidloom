//! Protocol-level regression checks: count exchanges without a live audio device.
use droidloom_audio_client::Client;
use droidloom_audio_protocol::*;
use std::{os::fd::AsFd, thread, time::Duration};

fn receive(socket: &SeqPacket) -> Record {
    let mut bytes = [0; RECORD_BYTES];
    let received = socket.receive_record_into(&mut bytes).unwrap();
    assert!(received.descriptors.is_empty());
    Record::decode(received.bytes).unwrap()
}
fn reply(socket: &SeqPacket, request: Record, op: u32, values: [u64; 5]) {
    socket
        .send_record(
            &Record {
                op,
                values,
                ..request
            }
            .encode(),
            &[],
        )
        .unwrap();
}
fn fixture(
    legacy: bool,
    operations: Vec<u32>,
) -> (tempfile::TempDir, Client, thread::JoinHandle<()>) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audio.sock");
    let listener = SeqPacketListener::bind(&path).unwrap();
    let format = Format {
        direction: Direction::Playback,
        rate: RATE,
        channels: 2,
        frames: 1024,
    };
    let worker = thread::spawn(move || {
        let mut socket = listener.accept().unwrap();
        socket_timeout(&socket, 1000).unwrap();
        let mut open = receive(&socket);
        assert_eq!(open.op, OPEN);
        assert_eq!(open.values[4], BURST_POSITION);
        if legacy {
            reply(&socket, open, ERROR, [libc::EPROTO as u64, 0, 0, 0, 0]);
            drop(socket);
            socket = listener.accept().unwrap();
            socket_timeout(&socket, 1000).unwrap();
            open = receive(&socket);
            assert_eq!(open.values[4], 0);
        }
        let buffers: Vec<_> = (0..BUFFERS)
            .map(|_| SampleBuffer::allocate(format).unwrap())
            .collect();
        socket
            .send_record(
                &Record {
                    op: CONFIG,
                    request: 1,
                    generation: 42,
                    values: [
                        RATE as u64,
                        2,
                        1024,
                        BUFFERS as u64,
                        SHARED_POOL | if legacy { 0 } else { BURST_POSITION },
                    ],
                }
                .encode(),
                &buffers.iter().map(AsFd::as_fd).collect::<Vec<_>>(),
            )
            .unwrap();
        let mut previous = 1;
        for op in operations {
            let request = receive(&socket);
            assert_eq!(request.op, op, "unexpected extra exchange");
            assert_eq!(request.generation, 42);
            assert_eq!(request.request, previous + 1);
            previous = request.request;
            match op {
                ACQUIRE_POSITION => reply(
                    &socket,
                    request,
                    op,
                    [
                        pack_credit(1, 256, 0).unwrap(),
                        100,
                        1_000_000_000,
                        5_000_000,
                        0,
                    ],
                ),
                SUBMIT_POSITION => reply(
                    &socket,
                    request,
                    STATUS,
                    [200, 2_000_000_000, 5_000_000, 0, 0],
                ),
                STATUS => reply(
                    &socket,
                    request,
                    STATUS,
                    [300, 3_000_000_000, 4_000_000, 1, 0],
                ),
                ACQUIRE => reply(&socket, request, ACQUIRE, [1, 256, 0, 1024, 0]),
                SUBMIT | CONTROL => reply(&socket, request, ACK, [0; 5]),
                _ => panic!("bad fixture"),
            }
        }
    });
    let client = Client::connect(&path, format, Role::Media).unwrap();
    (directory, client, worker)
}

#[test]
fn playback_burst_and_repeated_position_queries_use_two_exchanges() {
    let (_directory, mut client, worker) =
        fixture(false, vec![CONTROL, ACQUIRE_POSITION, SUBMIT_POSITION]);
    client.control(START).unwrap();
    assert_eq!(client.acquire(256).unwrap().frames, 256);
    for _ in 0..3 {
        let p = client.position().unwrap();
        assert_eq!((p.frames, p.monotonic_ns), (100, 1_000_000_000));
    }
    client.submit(256).unwrap();
    for _ in 0..3 {
        let p = client.position().unwrap();
        assert_eq!((p.frames, p.monotonic_ns), (200, 2_000_000_000));
    }
    worker.join().unwrap();
}

#[test]
fn snapshots_expire_and_control_invalidates_them() {
    let (_directory, mut client, worker) = fixture(
        false,
        vec![
            CONTROL,
            ACQUIRE_POSITION,
            SUBMIT_POSITION,
            STATUS,
            CONTROL,
            STATUS,
        ],
    );
    client.control(START).unwrap();
    client.acquire(256).unwrap();
    client.submit(256).unwrap();
    thread::sleep(Duration::from_millis(12));
    assert_eq!(client.position().unwrap().frames, 300);
    client.control(FLUSH).unwrap();
    assert_eq!(client.position().unwrap().discontinuities, 1);
    worker.join().unwrap();
}

#[test]
fn legacy_broker_retries_open_and_uses_original_exchanges() {
    let (_directory, mut client, worker) =
        fixture(true, vec![CONTROL, ACQUIRE, STATUS, SUBMIT, STATUS]);
    client.control(START).unwrap();
    client.acquire(256).unwrap();
    assert_eq!(client.position().unwrap().frames, 300);
    client.submit(256).unwrap();
    assert_eq!(client.position().unwrap().frames, 300);
    worker.join().unwrap();
}

#[test]
fn credit_packing_preserves_offset_and_rejects_unbounded_metadata() {
    for slot in 0..BUFFERS as u32 {
        for frames in [1, 256, MAX_FRAMES] {
            for offset in [0, 4096, u32::MAX] {
                assert_eq!(
                    unpack_credit(pack_credit(slot, frames, offset).unwrap()).unwrap(),
                    (slot, frames, offset)
                );
            }
        }
    }
    assert!(pack_credit(BUFFERS as u32, 256, 0).is_err());
    assert!(pack_credit(0, 0, 0).is_err());
    assert!(unpack_credit(u64::MAX).is_err());
}
