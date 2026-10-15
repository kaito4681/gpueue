//! Golden and round-trip tests of the daemon ⇄ helper wire format.
//!
//! The golden strings pin the exact JSON shape. If one of these fails, the
//! wire format changed: bump `PROTOCOL_VERSION` if it is incompatible.

use std::collections::BTreeMap;
use std::fmt::Debug;

use gpueue_core::model::{GpuIndex, JobId, Uid};
use gpueue_protocol::helper::*;
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
        missing_programs: Vec::new(),
        programs: Vec::new(),
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
            log_path: "/var/lib/gpueue/logs/7.log".into(),
            no_checkpoint: false,
        }),
    );
    // The newline in the command is escaped, so the message stays one line.
    assert_golden_line(
        &msg,
        r#"{"type":"request","id":1,"op":"spawn","job":7,"uid":1000,"gid":1000,"cwd":"/home/alice","command":"python train.py\nsecond line","shell":["sh","-c"],"envs":{"PATH":"/usr/bin"},"gpus":[0],"log_path":"/var/lib/gpueue/logs/7.log"}"#,
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
                image_dir: "/var/lib/gpueue/checkpoints/7".into(),
                options: CheckpointOptions {
                    tcp_close: true,
                    leave_running: false,
                    timeout_secs: None,
                },
            },
        ),
        r#"{"type":"request","id":3,"op":"checkpoint","job":7,"image_dir":"/var/lib/gpueue/checkpoints/7","options":{"tcp_close":true,"leave_running":false}}"#,
    );
    assert_golden_line(
        &request(
            4,
            Request::Restore(RestoreRequest {
                job: JobId(7),
                uid: Uid(1000),
                image_dir: "/var/lib/gpueue/checkpoints/7".into(),
                gpus: vec![GpuIndex(0)],
                leave_stopped: false,
            }),
        ),
        r#"{"type":"request","id":4,"op":"restore","job":7,"uid":1000,"image_dir":"/var/lib/gpueue/checkpoints/7","gpus":[0]}"#,
    );
    assert_golden_line(
        &request(
            4,
            Request::Restore(RestoreRequest {
                job: JobId(7),
                uid: Uid(1000),
                image_dir: "/var/lib/gpueue/checkpoints/7".into(),
                gpus: vec![],
                leave_stopped: true,
            }),
        ),
        r#"{"type":"request","id":4,"op":"restore","job":7,"uid":1000,"image_dir":"/var/lib/gpueue/checkpoints/7","gpus":[],"leave_stopped":true}"#,
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
                image_dir: "/var/lib/gpueue/checkpoints/7".into(),
            },
        ),
        r#"{"type":"request","id":7,"op":"delete_checkpoint","job":7,"image_dir":"/var/lib/gpueue/checkpoints/7"}"#,
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
                    uid: None,
                    gpus: None,
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
    for (stopped, golden) in [
        (
            false,
            r#"{"type":"event","seq":47,"event":"restore_finished","job":7,"outcome":{"result":"success","pid":42}}"#,
        ),
        (
            true,
            r#"{"type":"event","seq":47,"event":"restore_finished","job":7,"outcome":{"result":"success","pid":42,"stopped":true}}"#,
        ),
    ] {
        assert_golden_line(
            &event(
                47,
                Event::RestoreFinished {
                    job: JobId(7),
                    outcome: RestoreOutcome::Success { pid: 42, stopped },
                },
            ),
            golden,
        );
    }
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

// ---------------------------------------------------------------------------
// Variant snapshot: guards the PROTOCOL_VERSION rule (see its docs)
// ---------------------------------------------------------------------------

/// Wire names of the variants of every enum without an `Unknown` fallback,
/// per protocol version. An older peer cannot decode a variant it does not
/// know, so adding one is incompatible: bump `PROTOCOL_VERSION` and add a
/// new entry here. Never edit an existing entry.
const VARIANT_SNAPSHOTS: &[(u32, &str)] = &[(
    1,
    "\
DaemonMessage: hello request
HelperMessage: hello response event
Request: spawn signal checkpoint restore delete_checkpoint list_running ack_events
ResponseResult: ok spawned accepted running error
Event: job_exited checkpoint_finished restore_finished
CheckpointOutcome: success failed
RestoreOutcome: success failed
",
)];

/// Fails to compile when a variant is added to one of the snapshotted
/// enums, so [`variant_samples`] cannot silently miss it.
#[allow(dead_code)]
fn exhaustive(
    d: &DaemonMessage,
    h: &HelperMessage,
    r: &Request,
    rr: &ResponseResult,
    e: &Event,
    c: &CheckpointOutcome,
    ro: &RestoreOutcome,
) {
    match d {
        DaemonMessage::Hello(_) | DaemonMessage::Request(_) => {}
    }
    match h {
        HelperMessage::Hello(_) | HelperMessage::Response(_) | HelperMessage::Event(_) => {}
    }
    match r {
        Request::Spawn(_)
        | Request::Signal { .. }
        | Request::Checkpoint { .. }
        | Request::Restore(_)
        | Request::DeleteCheckpoint { .. }
        | Request::ListRunning
        | Request::AckEvents { .. } => {}
    }
    match rr {
        ResponseResult::Ok
        | ResponseResult::Spawned { .. }
        | ResponseResult::Accepted
        | ResponseResult::Running { .. }
        | ResponseResult::Error { .. } => {}
    }
    match e {
        Event::JobExited { .. }
        | Event::CheckpointFinished { .. }
        | Event::RestoreFinished { .. } => {}
    }
    match c {
        CheckpointOutcome::Success { .. } | CheckpointOutcome::Failed { .. } => {}
    }
    match ro {
        RestoreOutcome::Success { .. } | RestoreOutcome::Failed { .. } => {}
    }
}

/// One value of every variant of the snapshotted enums, as
/// `(enum, tag field, encoded value)`.
fn variant_samples() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    fn json(value: impl Serialize) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }
    let job = JobId(1);
    let spawn = SpawnRequest {
        job,
        uid: Uid(1000),
        gid: Gid(1000),
        groups: None,
        cwd: "/".into(),
        command: "true".into(),
        shell: vec!["sh".into(), "-c".into()],
        envs: BTreeMap::new(),
        gpus: vec![],
        log_path: "/l".into(),
        no_checkpoint: false,
    };
    let response = Response {
        id: RequestId(1),
        result: ResponseResult::Ok,
    };
    let event = EventEnvelope {
        seq: EventSeq(1),
        event: Event::JobExited {
            job,
            status: ExitStatus::Unknown,
        },
    };
    let mut out = vec![
        (
            "DaemonMessage",
            "type",
            json(DaemonMessage::Hello(Hello::current())),
        ),
        (
            "DaemonMessage",
            "type",
            json(DaemonMessage::Request(RequestEnvelope {
                id: RequestId(1),
                request: Request::ListRunning,
            })),
        ),
        (
            "HelperMessage",
            "type",
            json(HelperMessage::Hello(Hello::current())),
        ),
        (
            "HelperMessage",
            "type",
            json(HelperMessage::Response(response)),
        ),
        ("HelperMessage", "type", json(HelperMessage::Event(event))),
    ];
    for request in [
        Request::Spawn(spawn),
        Request::Signal { job, signal: 9 },
        Request::Checkpoint {
            job,
            image_dir: "/i".into(),
            options: CheckpointOptions::default(),
        },
        Request::Restore(RestoreRequest {
            job,
            uid: Uid(1000),
            image_dir: "/i".into(),
            gpus: vec![],
            leave_stopped: false,
        }),
        Request::DeleteCheckpoint {
            job,
            image_dir: "/i".into(),
        },
        Request::ListRunning,
        Request::AckEvents { up_to: EventSeq(1) },
    ] {
        out.push(("Request", "op", json(request)));
    }
    for result in [
        ResponseResult::Ok,
        ResponseResult::Spawned { pid: 1 },
        ResponseResult::Accepted,
        ResponseResult::Running { jobs: vec![] },
        ResponseResult::Error {
            code: HelperErrorCode::Internal,
            message: String::new(),
        },
    ] {
        out.push(("ResponseResult", "result", json(result)));
    }
    for event in [
        Event::JobExited {
            job,
            status: ExitStatus::Unknown,
        },
        Event::CheckpointFinished {
            job,
            outcome: CheckpointOutcome::failed("x"),
        },
        Event::RestoreFinished {
            job,
            outcome: RestoreOutcome::failed("x"),
        },
    ] {
        out.push(("Event", "event", json(event)));
    }
    for outcome in [
        CheckpointOutcome::Success {
            image_size_bytes: 1,
        },
        CheckpointOutcome::failed("x"),
    ] {
        out.push(("CheckpointOutcome", "result", json(outcome)));
    }
    for outcome in [
        RestoreOutcome::Success {
            pid: 1,
            stopped: false,
        },
        RestoreOutcome::failed("x"),
    ] {
        out.push(("RestoreOutcome", "result", json(outcome)));
    }
    out
}

#[test]
fn variants_change_only_with_a_protocol_version_bump() {
    let mut snapshot = String::new();
    let mut current: Option<&str> = None;
    for (name, tag, value) in variant_samples() {
        if current != Some(name) {
            if current.is_some() {
                snapshot.push('\n');
            }
            snapshot.push_str(name);
            snapshot.push(':');
            current = Some(name);
        }
        let variant = value[tag]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {value}"));
        snapshot.push(' ');
        snapshot.push_str(variant);
    }
    snapshot.push('\n');

    let versions: Vec<u32> = VARIANT_SNAPSHOTS.iter().map(|(v, _)| *v).collect();
    assert!(
        versions.windows(2).all(|w| w[0] < w[1]),
        "snapshots must be in increasing version order"
    );
    let Some((_, expected)) = VARIANT_SNAPSHOTS
        .iter()
        .find(|(version, _)| *version == PROTOCOL_VERSION)
    else {
        panic!("no variant snapshot for PROTOCOL_VERSION {PROTOCOL_VERSION}; add one:\n{snapshot}");
    };
    assert_eq!(
        snapshot, *expected,
        "the helper protocol's variants changed: bump PROTOCOL_VERSION and add a new \
         snapshot entry (do not edit the existing one)"
    );
}

#[test]
fn unknown_codes_and_phases_decode_to_the_fallback() {
    let msg: HelperMessage = decode_line(
        br#"{"type":"response","id":1,"result":"error","code":"from_the_future","message":"m"}"#,
    )
    .unwrap();
    assert_eq!(
        msg,
        HelperMessage::Response(Response {
            id: RequestId(1),
            result: ResponseResult::Error {
                code: HelperErrorCode::Unknown,
                message: "m".into(),
            },
        })
    );
    let msg: HelperMessage = decode_line(
        br#"{"type":"response","id":2,"result":"running","jobs":[{"job":3,"phase":"hibernating"}]}"#,
    )
    .unwrap();
    assert_eq!(
        msg,
        HelperMessage::Response(Response {
            id: RequestId(2),
            result: ResponseResult::Running {
                jobs: vec![RunningJob {
                    job: JobId(3),
                    pid: None,
                    phase: RunningPhase::Unknown,
                    uid: None,
                    gpus: None,
                }],
            },
        })
    );
    let msg: HelperMessage = decode_line(
        br#"{"type":"event","seq":4,"event":"job_exited","job":3,"status":{"kind":"vanished","extra":1}}"#,
    )
    .unwrap();
    assert_eq!(
        msg,
        HelperMessage::Event(EventEnvelope {
            seq: EventSeq(4),
            event: Event::JobExited {
                job: JobId(3),
                status: ExitStatus::Unknown,
            },
        })
    );
    let msg: HelperMessage = decode_line(
        br#"{"type":"event","seq":5,"event":"checkpoint_finished","job":3,"outcome":{"result":"failed","reason":"r","code":"new_code"}}"#,
    )
    .unwrap();
    assert!(matches!(
        msg,
        HelperMessage::Event(EventEnvelope {
            event: Event::CheckpointFinished {
                outcome: CheckpointOutcome::Failed {
                    code: Some(HelperErrorCode::Unknown),
                    ..
                },
                ..
            },
            ..
        })
    ));
}
