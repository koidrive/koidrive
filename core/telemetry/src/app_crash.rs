// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;

use crate::schema::{BinaryId, CrashKind, CrashSignal, Event, MachException};

const MAX_OFFSETS: usize = 24;

pub(crate) fn app_crash(report: &Value) -> Option<Event> {
    let error = report.get("crash")?.get("error")?;
    let process = report["report"]["process_name"].as_str();
    let image = (report["binary_images"].as_array().into_iter().flatten()).find(|image| {
        process.is_some()
            && image["name"]
                .as_str()
                .and_then(|path| path.rsplit('/').next())
                == process
    });
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
    let at = seconds(&report["report"]["timestamp"]).unwrap_or_else(crate::now);
    Some(Event::AppCrash {
        at,
        kind,
        exception,
        signal,
        binary,
        offsets,
    })
}

fn seconds(timestamp: &Value) -> Option<u64> {
    if let Some(micros) = timestamp.as_u64() {
        return Some(micros / 1_000_000);
    }
    let (date, time) = timestamp.as_str()?.strip_suffix('Z')?.split_once('T')?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut time = time
        .splitn(3, ':')
        .map(|part| part.split('.').next()?.parse::<u64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let days =
        u64::try_from(era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468).ok()?;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const EXECUTABLE: &str = "/Users/alice/Applications/Koidrive.app/Contents/MacOS/Koidrive";

    fn report() -> Value {
        json!({
            "report": { "id": "4D2B0160-69B6-4DC1-BFED-08A0554A67C9", "process_name": "Koidrive", "timestamp": "2026-11-12T18:25:23.456789Z", "type": "standard" },
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
            "system": { "CFBundleExecutablePath": "/Users/alice/Applications/Koidrive.app/Contents/MacOS", "device_app_hash": "abc" },
            "user": { "name": "alice" },
        })
    }

    #[test]
    fn a_crash_keeps_its_kind_exception_signal_time_build_and_the_app_frames_only() {
        let event = app_crash(&report()).unwrap();
        assert_eq!(
            event,
            Event::AppCrash {
                at: 1_794_507_923,
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
        }) = app_crash(&report)
        else {
            panic!("no crash");
        };
        assert_eq!(
            (kind, exception, signal, binary, offsets),
            (CrashKind::Other, None, None, None, vec![])
        );
        assert!(app_crash(&json!({ "report": {} })).is_none());
    }

    #[test]
    fn the_crash_time_is_read_as_kscrash_stores_it_and_as_its_store_hands_it_out() {
        for (timestamp, seconds) in [
            (json!(1_794_673_631_577_071u64), 1_794_673_631),
            (json!("2026-11-14T16:27:11.577071Z"), 1_794_673_631),
            (json!("2024-02-29T00:00:00Z"), 1_709_164_800),
        ] {
            let mut report = report();
            report["report"]["timestamp"] = timestamp;
            assert!(
                matches!(app_crash(&report), Some(Event::AppCrash { at, .. }) if at == seconds)
            );
        }
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
