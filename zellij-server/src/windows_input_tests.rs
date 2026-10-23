use super::super::windows_input_pipe::input_pipe;
use super::*;
use std::{io::Read, time::Instant};

fn pair() -> (InputWriter, std::fs::File) {
    let (read, write) = input_pipe(4096).unwrap();
    let writer = InputWriter::with_policy(
        write,
        Policy {
            primary: Duration::from_millis(50),
            recovery: Duration::from_millis(500),
        },
    )
    .unwrap();
    (writer, std::fs::File::from(read))
}
fn wait_until(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < until, "test event deadline");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn write_all(writer: &InputWriter, bytes: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "test write deadline",
            ));
        }
        match writer.try_write(&bytes[offset..])? {
            0 => std::thread::sleep(Duration::from_millis(1)),
            n => offset += n,
        }
    }
    Ok(())
}
fn fill_and_stall(writer: &InputWriter) {
    // Fill one pipe buffer, then issue one request that cannot finish yet.
    write_all(writer, &[b'a'; CHUNK_SIZE]).unwrap();
    assert_eq!(writer.try_write(&[b'b'; CHUNK_SIZE]).unwrap(), 0);
    wait_until(|| writer.state.stalled.load(Ordering::Acquire));
}

#[test]
fn exact_bytes_and_fifo_order_survive_many_partial_scheduler_polls() {
    let (writer, mut reader) = pair();
    let expected: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
    let read = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    write_all(&writer, &expected).unwrap();
    writer.enqueue_control(b"tail").unwrap();
    writer.drain().unwrap();
    writer.shutdown();
    let mut expected = expected;
    expected.extend_from_slice(b"tail");
    assert_eq!(read.join().unwrap(), expected);
}

#[test]
fn large_bracketed_paste_preserves_unicode_and_delimiters_across_chunks() {
    let (writer, mut reader) = pair();
    let payload = format!(
        "\x1b[200~{}한😀{}\x1b[201~",
        "x".repeat(4088),
        "한😀\n".repeat(20000)
    );
    let read = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    write_all(&writer, payload.as_bytes()).unwrap();
    writer.shutdown();
    assert_eq!(read.join().unwrap(), payload.as_bytes());
}

#[test]
fn temporary_stall_recovers_without_replaying_pending_bytes() {
    let (writer, mut reader) = pair();
    fill_and_stall(&writer);
    let read = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    write_all(&writer, &[b'b'; CHUNK_SIZE]).unwrap();
    writer.shutdown();
    assert_eq!(
        read.join().unwrap(),
        [vec![b'a'; CHUNK_SIZE], vec![b'b'; CHUNK_SIZE]].concat()
    );
}

#[test]
fn control_events_do_not_steal_the_pending_user_acknowledgement() {
    let (writer, mut reader) = pair();
    fill_and_stall(&writer);
    writer.enqueue_control(b"\x1b[1;1R").unwrap();
    let read = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    write_all(&writer, &[b'b'; CHUNK_SIZE]).unwrap();
    writer.drain().unwrap();
    writer.shutdown();
    assert_eq!(
        read.join().unwrap(),
        [
            vec![b'a'; CHUNK_SIZE],
            vec![b'b'; CHUNK_SIZE],
            b"\x1b[1;1R".to_vec()
        ]
        .concat()
    );
}

#[test]
fn permanent_stall_is_reported_without_hanging_the_caller() {
    let (writer, _reader) = pair();
    fill_and_stall(&writer);
    let mut failure = None;
    wait_until(|| {
        failure = writer.try_write(&[b'b'; CHUNK_SIZE]).err();
        failure.is_some()
    });
    assert_eq!(failure.unwrap().kind(), io::ErrorKind::TimedOut);
    assert!(writer.try_write(b"not-retried").is_err());
    writer.shutdown();
}

#[test]
fn shutdown_cancels_live_write_and_drain_without_closing_reader() {
    let (writer, _reader) = pair();
    let writer = Arc::new(writer);
    fill_and_stall(&writer);
    let draining = writer.clone();
    let drain = std::thread::spawn(move || draining.drain());
    let at = Instant::now();
    writer.shutdown();
    assert!(at.elapsed() < Duration::from_secs(2));
    assert!(drain.join().unwrap().is_err());
    writer.shutdown();
    assert!(writer.enqueue_control(b"late").is_err());
}

#[test]
fn stalled_pane_does_not_block_another_writer() {
    let (first, _first_reader) = pair();
    fill_and_stall(&first);
    let (second, mut reader) = pair();
    let read = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    write_all(&second, b"second-pane").unwrap();
    second.shutdown();
    assert_eq!(read.join().unwrap(), b"second-pane");
    first.shutdown();
}

#[test]
fn control_queue_is_bounded_and_never_silently_accepts_overflow() {
    let (writer, _reader) = pair();
    fill_and_stall(&writer);
    for _ in 0..QUEUE_CAPACITY {
        writer.enqueue_control(b"x").unwrap();
    }
    assert_eq!(
        writer.enqueue_control(b"overflow").unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    writer.shutdown();
}

#[test]
fn changed_pending_data_is_not_acknowledged_as_a_new_write() {
    let (writer, _reader) = pair();
    fill_and_stall(&writer);
    assert_eq!(
        writer.try_write(b"different-data").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    writer.shutdown();
}

#[test]
fn broken_pipe_is_reported_and_worker_releases_handles() {
    let (writer, reader) = pair();
    drop(reader);
    assert!(write_all(&writer, b"closed-reader").is_err());
    writer.shutdown();
}

#[test]
fn timeout_rounding_is_finite() {
    assert_eq!(timeout_ms(Duration::ZERO), 0);
    assert_eq!(timeout_ms(Duration::from_nanos(1)), 1);
    assert_eq!(timeout_ms(Duration::MAX), u32::MAX - 1);
}
