//! Golden and round-trip tests of the daemon ⇄ helper wire format.
//!
//! The golden strings pin the exact JSON shape. If one of these fails, the
//! wire format changed: bump `PROTOCOL_VERSION` if it is incompatible.

use std::collections::BTreeMap;
use std::fmt::Debug;

use puzuz_core::model::{GpuIndex, JobId, Uid};
use puzuz_protocol::helper::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Checks the encoded line and that it decodes back to the same value.
fn assert_golden_line<T>(value: &T, golden: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let line = encode_line(value).unwrap();
    assert_eq!(
        String::from_utf8(line.clone()).unwrap(),
        format!("{golden}\n")
    );
    let back: T = decode_line(&line).unwrap();
    assert_eq!(&back, value);
}

fn request(id: u64, request: Request) -> DaemonMessage {
    DaemonMessage::Request(RequestEnvelope {
        id: RequestId(id),
        request,
    })
}

#[test]
fn hello_golden() {
    let hello = Hello {
        protocol_version: 1,
        binary_version: "0.1.0".into(),
    };
    assert_golden_line(
        &DaemonMessage::Hello(hello.clone()),
        r#"{"type":"hello","protocol_version":1,"binary_version":"0.1.0"}"#,
    );
    assert_golden_line(
        &HelperMessage::Hello(hello.clone()),
        r#"{"type":"hello","protocol_version":1,"binary_version":"0.1.0"}"#,
    );
    assert!(Hello::current().is_compatible(&hello));
    let future = Hello {
        protocol_version: 2,
        ..hello
    };
    assert!(!Hello::current().is_compatible(&future));
}

#[test]
fn spawn_golden() {
    let msg = request(
        1,
        Request::Spawn(SpawnRequest {
            job: JobId(7),
            uid: Uid(1000),
            gid: Gid(1000),
            groups: None,
            cwd: "/home/alice".into(),
            command: "python train.py\nsecond line".into(),
            shell: vec!["sh".into(), "-c".into()],
            envs: BTreeMap::from([("PATH".into(), "/usr/bin".into())]),
            gpus: vec![GpuIndex(0)],
            log_path: "/var/lib/puzuz/logs/7.log".into(),
            no_checkpoint: false,
        }),
    );
    // The newline in the command is escaped, so the message stays one line.
    assert_golden_line(
        &msg,
        r#"{"type":"request","id":1,"op":"spawn","job":7,"uid":1000,"gid":1000,"cwd":"/home/alice","command":"python train.py\nsecond line","shell":["sh","-c"],"envs":{"PATH":"/usr/bin"},"gpus":[0],"log_path":"/var/lib/puzuz/logs/7.log"}"#,
    );
}

#[test]
fn other_requests_golden() {
    assert_golden_line(
        &request(
            2,
            Request::Signal {
                job: JobId(7),
                signal: signal::SIGTERM,
            },
        ),
        r#"{"type":"request","id":2,"op":"signal","job":7,"signal":15}"#,
    );
    assert_golden_line(
        &request(
            3,
            Request::Checkpoint {
                job: JobId(7),
                image_dir: "/var/lib/puzuz/checkpoints/7".into(),
                options: CheckpointOptions {
                    tcp_close: true,
                    leave_running: false,
                    timeout_secs: None,
                },
            },
        ),
        r#"{"type":"request","id":3,"op":"checkpoint","job":7,"image_dir":"/var/lib/puzuz/checkpoints/7","options":{"tcp_close":true,"leave_running":false}}"#,
    );
    assert_golden_line(
        &request(
            4,
            Request::Restore(RestoreRequest {
                job: JobId(7),
                uid: Uid(1000),
                image_dir: "/var/lib/puzuz/checkpoints/7".into(),
                gpus: vec![GpuIndex(0)],
            }),
        ),
        r#"{"type":"request","id":4,"op":"restore","job":7,"uid":1000,"image_dir":"/var/lib/puzuz/checkpoints/7","gpus":[0]}"#,
    );
    assert_golden_line(
        &request(5, Request::ListRunning),
        r#"{"type":"request","id":5,"op":"list_running"}"#,
    );
    assert_golden_line(
        &request(
            6,
            Request::AckEvents {
                up_to: EventSeq(41),
            },
        ),
        r#"{"type":"request","id":6,"op":"ack_events","up_to":41}"#,
    );
    assert_golden_line(
        &request(
            7,
            Request::DeleteCheckpoint {
                job: JobId(7),
                image_dir: "/var/lib/puzuz/checkpoints/7".into(),
            },
        ),
        r#"{"type":"request","id":7,"op":"delete_checkpoint","job":7,"image_dir":"/var/lib/puzuz/checkpoints/7"}"#,
    );
}

#[test]
fn responses_golden() {
    let resp = |id, result| {
        HelperMessage::Response(Response {
            id: RequestId(id),
            result,
        })
    };
    assert_golden_line(
        &resp(1, ResponseResult::Spawned { pid: 4242 }),
        r#"{"type":"response","id":1,"result":"spawned","pid":4242}"#,
    );
    assert_golden_line(
        &resp(2, ResponseResult::Ok),
        r#"{"type":"response","id":2,"result":"ok"}"#,
    );
    assert_golden_line(
        &resp(3, ResponseResult::Accepted),
        r#"{"type":"response","id":3,"result":"accepted"}"#,
    );
    assert_golden_line(
        &resp(
            5,
            ResponseResult::Running {
                jobs: vec![RunningJob {
                    job: JobId(7),
                    pid: Some(4242),
                    phase: RunningPhase::Running,
                }],
            },
        ),
        r#"{"type":"response","id":5,"result":"running","jobs":[{"job":7,"pid":4242,"phase":"running"}]}"#,
    );
    assert_golden_line(
        &resp(
            9,
            ResponseResult::Error {
                code: HelperErrorCode::InvalidRequest,
                message: "refusing to spawn as uid 0".into(),
            },
        ),
        r#"{"type":"response","id":9,"result":"error","code":"invalid_request","message":"refusing to spawn as uid 0"}"#,
    );
}

#[test]
fn events_golden() {
    let event = |seq, event| {
        HelperMessage::Event(EventEnvelope {
            seq: EventSeq(seq),
            event,
        })
    };
    assert_golden_line(
        &event(
            40,
            Event::JobExited {
                job: JobId(7),
                status: ExitStatus::Exited { code: 0 },
            },
        ),
        r#"{"type":"event","seq":40,"event":"job_exited","job":7,"status":{"kind":"exited","code":0}}"#,
    );
    assert_golden_line(
        &event(
            41,
            Event::JobExited {
                job: JobId(8),
                status: ExitStatus::Signaled { signal: 9 },
            },
        ),
        r#"{"type":"event","seq":41,"event":"job_exited","job":8,"status":{"kind":"signaled","signal":9}}"#,
    );
    assert_golden_line(
        &event(
            42,
            Event::CheckpointFinished {
                job: JobId(7),
                outcome: CheckpointOutcome::Success {
                    image_size_bytes: 1024,
                },
            },
        ),
        r#"{"type":"event","seq":42,"event":"checkpoint_finished","job":7,"outcome":{"result":"success","image_size_bytes":1024}}"#,
    );
    assert_golden_line(
        &event(
            43,
            Event::RestoreFinished {
                job: JobId(7),
                outcome: RestoreOutcome::failed("GPU model changed"),
            },
        ),
        r#"{"type":"event","seq":43,"event":"restore_finished","job":7,"outcome":{"result":"failed","reason":"GPU model changed"}}"#,
    );
    assert_golden_line(
        &event(
            44,
            Event::RestoreFinished {
                job: JobId(7),
                outcome: RestoreOutcome::Failed {
                    reason: "the helper is stopping".into(),
                    code: Some(HelperErrorCode::ShuttingDown),
                },
            },
        ),
        r#"{"type":"event","seq":44,"event":"restore_finished","job":7,"outcome":{"result":"failed","reason":"the helper is stopping","code":"shutting_down"}}"#,
    );
    assert_golden_line(
        &event(
            45,
            Event::CheckpointFinished {
                job: JobId(7),
                outcome: CheckpointOutcome::failed("criu dump failed"),
            },
        ),
        r#"{"type":"event","seq":45,"event":"checkpoint_finished","job":7,"outcome":{"result":"failed","reason":"criu dump failed"}}"#,
    );
    assert_golden_line(
        &event(
            46,
            Event::CheckpointFinished {
                job: JobId(7),
                outcome: CheckpointOutcome::refused("not enough free space"),
            },
        ),
        r#"{"type":"event","seq":46,"event":"checkpoint_finished","job":7,"outcome":{"result":"failed","reason":"not enough free space","code":"checkpoint_refused"}}"#,
    );
    assert!(CheckpointOutcome::refused("x").is_refused());
    assert!(!CheckpointOutcome::failed("x").is_refused());
    assert!(ExitStatus::Exited { code: 0 }.success());
    assert!(!ExitStatus::Signaled { signal: 9 }.success());
}

#[test]
fn checkpoint_options_default_when_omitted() {
    let msg: DaemonMessage =
        decode_line(br#"{"type":"request","id":3,"op":"checkpoint","job":7,"image_dir":"/x"}"#)
            .unwrap();
    let DaemonMessage::Request(env) = msg else {
        panic!("not a request");
    };
    assert_eq!(env.request.op(), "checkpoint");
    assert_eq!(
        env.request,
        Request::Checkpoint {
            job: JobId(7),
            image_dir: "/x".into(),
            options: CheckpointOptions::default(),
        }
    );
}

#[test]
fn decode_accepts_crlf_and_rejects_garbage() {
    let msg: DaemonMessage =
        decode_line(b"{\"type\":\"request\",\"id\":5,\"op\":\"list_running\"}\r\n").unwrap();
    assert_eq!(msg, request(5, Request::ListRunning));
    assert!(decode_line::<DaemonMessage>(b"{\"type\":\"nope\"}").is_err());
    assert!(decode_line::<DaemonMessage>(b"not json").is_err());
}

#[test]
fn line_buffer_splits_stream() {
    let a = encode_line(&request(1, Request::ListRunning)).unwrap();
    let b = encode_line(&request(2, Request::AckEvents { up_to: EventSeq(3) })).unwrap();
    let mut stream = a.clone();
    stream.extend_from_slice(&b);

    let mut buf = LineBuffer::new();
    // Feed byte by byte to exercise partial reads.
    let mut lines = Vec::new();
    for byte in &stream {
        buf.push(std::slice::from_ref(byte));
        while let Some(line) = buf.next_line().unwrap() {
            lines.push(decode_line::<DaemonMessage>(&line).unwrap());
        }
    }
    assert_eq!(
        lines,
        vec![
            request(1, Request::ListRunning),
            request(2, Request::AckEvents { up_to: EventSeq(3) }),
        ]
    );
    assert_eq!(buf.pending(), 0);
}

#[test]
fn line_buffer_rejects_overlong_line() {
    let mut buf = LineBuffer::new();
    buf.push(&vec![b'x'; MAX_LINE_LEN + 1]);
    assert!(matches!(buf.next_line(), Err(FrameError::TooLong { .. })));
}
