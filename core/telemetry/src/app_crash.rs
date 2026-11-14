// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;

use crate::schema::{BinaryId, CrashKind, CrashSignal, Event, MachException};

const MAX_OFFSETS: usize = 24;

/// The event for a crash report written by KSCrash 2.6, or none when it holds no crash. Only the crash's kind, Mach
/// exception, signal and time are read, with the build UUID of `executable` and the offsets of the crashed thread's
/// frames in it; everything else in the report, such as paths, names, messages and memory, is never read.
pub fn app_crash(report: &Value, executable: &str) -> Option<Event> {
    let error = report.get("crash")?.get("error")?;
    let image = (report["binary_images"].as_array().into_iter().flatten())
        .find(|image| image["name"].as_str() == Some(executable));
    let base = image.and_then(|image| image["image_addr"].as_u64());
    let binary = image
        .and_then(|image| image["uuid"].as_str())
        .and_then(|uuid| BinaryId::try_from(uuid.to_owned()).ok());
    let crashed = (report["crash"]["threads"].as_array().into_iter().flatten())
        .find(|thread| thread["crashed"] == true);
    let offsets = (crashed.and_then(|thread| thread["backtrace"]["contents"].as_array()))
        .into_iter()
        .flatten()
        .filter(|frame| base.is_some() && frame["object_addr"].as_u64() == base)
        .filter_map(|frame| frame["instruction_addr"].as_u64()?.checked_sub(base?))
        .take(MAX_OFFSETS)
        .collect();
    let kind = match error["type"].as_str() {
        Some("mach") => CrashKind::Mach,
        Some("signal") => CrashKind::Signal,
        Some("nsexception") => CrashKind::NsException,
        Some("cpp_exception") => CrashKind::CppException,
        _ => CrashKind::Other,
    };
    let exception = error["mach"]["exception"]
        .as_u64()
        .map(|number| match number {
            1 => MachException::BadAccess,
            2 => MachException::BadInstruction,
            3 => MachException::Arithmetic,
            4 => MachException::Emulation,
            5 => MachException::Software,
            6 => MachException::Breakpoint,
            7 => MachException::Syscall,
            8 => MachException::MachSyscall,
            9 => MachException::RpcAlert,
            10 => MachException::Crash,
            11 => MachException::Resource,
            12 => MachException::Guard,
            13 => MachException::CorpseNotify,
            _ => MachException::Other,
        });
    let signal = error["signal"]["signal"]
        .as_u64()
        .map(|number| match number {
            4 => CrashSignal::Ill,
            5 => CrashSignal::Trap,
            6 => CrashSignal::Abort,
            8 => CrashSignal::FloatingPoint,
            10 => CrashSignal::Bus,
            11 => CrashSignal::Segv,
            12 => CrashSignal::Sys,
            _ => CrashSignal::Other,
        });
    let at = (report["report"]["timestamp"].as_u64())
        .map_or_else(crate::now, |micros| micros / 1_000_000);
    Some(Event::AppCrash {
        at,
        kind,
        exception,
        signal,
        binary,
        offsets,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const EXECUTABLE: &str = "/Users/alice/Applications/Koidrive.app/Contents/MacOS/Koidrive";

    fn report() -> Value {
        json!({
            "report": { "id": "4D2B0160-69B6-4DC1-BFED-08A0554A67C9", "process_name": "Koidrive", "timestamp": 1_794_527_123_456_789u64, "type": "standard" },
            "binary_images": [
                { "name": "/usr/lib/libSystem.B.dylib", "image_addr": 4096, "uuid": "11111111-2222-3333-4444-555555555555" },
                { "name": EXECUTABLE, "image_addr": 1_048_576, "uuid": "17d33ed4-005a-358b-ba17-36b819625055" },
            ],
            "crash": {
                "error": {
                    "type": "nsexception",
                    "mach": { "exception": 10, "exception_name": "EXC_CRASH", "code": 0, "subcode": 0 },
                    "signal": { "signal": 6, "name": "SIGABRT", "code": 0 },
                    "reason": "could not open /Users/alice/Private Notes.docx",
                    "nsexception": { "name": "AliceException", "userInfo": "Alice Notes" },
                },
                "threads": [
                    { "crashed": false, "backtrace": { "contents": [{ "object_name": "Koidrive", "object_addr": 1_048_576, "instruction_addr": 1_048_999 }] } },
                    { "crashed": true, "name": "Alice's queue", "backtrace": { "contents": [
                        { "object_name": "libSystem.B.dylib", "object_addr": 4096, "instruction_addr": 5000, "symbol_name": "abort" },
                        { "object_name": "Koidrive", "object_addr": 1_048_576, "instruction_addr": 1_049_576, "symbol_name": "Alice.open(file:)" },
                        { "object_name": "Koidrive", "object_addr": 1_048_576, "instruction_addr": 1_050_576 },
                    ] } },
                ],
            },
            "system": { "CFBundleExecutablePath": EXECUTABLE, "device_app_hash": "abc" },
            "user": { "name": "alice" },
        })
    }

    #[test]
    fn a_crash_keeps_its_kind_exception_signal_time_build_and_the_app_frames_only() {
        let event = app_crash(&report(), EXECUTABLE).unwrap();
        assert_eq!(
            event,
            Event::AppCrash {
                at: 1_794_527_123,
                kind: CrashKind::NsException,
                exception: Some(MachException::Crash),
                signal: Some(CrashSignal::Abort),
                binary: Some(
                    BinaryId::try_from("17D33ED4-005A-358B-BA17-36B819625055".to_owned()).unwrap()
                ),
                offsets: vec![1000, 2000],
            }
        );
        let text = serde_json::to_string(&event).unwrap().to_lowercase();
        for leaked in [
            "alice",
            "private",
            "/users",
            "koidrive.app",
            "abort",
            "libsystem",
            "4d2b0160",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }

    #[test]
    fn a_report_without_the_app_image_or_numbers_still_counts_the_crash() {
        let mut report = report();
        report["binary_images"] = json!([]);
        report["crash"]["error"] = json!({ "type": "something_new" });
        report["report"] = json!({});
        let Some(Event::AppCrash {
            kind,
            exception,
            signal,
            binary,
            offsets,
            ..
        }) = app_crash(&report, EXECUTABLE)
        else {
            panic!("no crash");
        };
        assert_eq!(
            (kind, exception, signal, binary, offsets),
            (CrashKind::Other, None, None, None, vec![])
        );
        assert!(app_crash(&json!({ "report": {} }), EXECUTABLE).is_none());
    }

    #[test]
    fn a_build_id_is_a_uuid() {
        for bad in [
            "",
            "17D33ED4",
            "17D33ED4-005A-358B-BA17-36B81962505Z",
            "/Users/alice/Koidrive",
        ] {
            assert!(BinaryId::try_from(bad.to_owned()).is_err(), "{bad}");
        }
    }
}
