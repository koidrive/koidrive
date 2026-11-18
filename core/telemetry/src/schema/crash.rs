// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::Invalid;

/// A function of Koidrive's own code, such as `koidrive_driver::folder::Folder::run::{{closure}}`, without its generic
/// arguments. Built from a demangled name only when that names one of Koidrive's crates, so a dependency's function,
/// a path or a value cannot become one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Symbol(String);

impl Symbol {
    pub fn ours(demangled: &str) -> Option<Self> {
        let mut name = demangled.trim();
        let mut qualified = String::new();
        if let Some(inner) = name.strip_prefix('<') {
            let end = closing(inner)?;
            let path = &inner[..end];
            qualified.push_str(path.split(" as ").next()?);
            name = &inner[end + 1..];
        }
        qualified.push_str(name);
        let mut plain = String::new();
        let mut depth = 0usize;
        for char in qualified.chars() {
            match char {
                '<' => depth += 1,
                '>' => depth = depth.checked_sub(1)?,
                _ if depth == 0 => plain.push(char),
                _ => {}
            }
        }
        let plain = plain.replace("::::", "::");
        let plain = plain.trim_end_matches("::");
        let crate_name = plain.split("::").next()?;
        let ours = (crate_name.starts_with("koidrive_") || crate_name == "koi")
            && plain.len() <= 160
            && plain.bytes().all(|byte| {
                matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b':' | b'{' | b'}')
            });
        ours.then(|| Symbol(plain.to_owned()))
    }
}

fn closing(text: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, char) in text.char_indices() {
        match char {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

impl TryFrom<String> for Symbol {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        Symbol::ours(&value)
            .filter(|symbol| symbol.0 == value)
            .ok_or(Invalid("symbol"))
    }
}

impl From<Symbol> for String {
    fn from(symbol: Symbol) -> String {
        symbol.0
    }
}

impl Symbol {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

labels! {
    /// How KSCrash caught a crash of the Mac app.
    pub enum CrashKind {
        Mach => "mach",
        Signal => "signal",
        NsException => "nsexception",
        CppException => "cpp_exception",
        Other => "other",
    }
}

labels! {
    /// The Mach exception of a crash of the Mac app.
    pub enum MachException {
        BadAccess => "bad_access",
        BadInstruction => "bad_instruction",
        Arithmetic => "arithmetic",
        Emulation => "emulation",
        Software => "software",
        Breakpoint => "breakpoint",
        Syscall => "syscall",
        MachSyscall => "mach_syscall",
        RpcAlert => "rpc_alert",
        Crash => "crash",
        Resource => "resource",
        Guard => "guard",
        CorpseNotify => "corpse_notify",
        Other => "other",
    }
}

labels! {
    /// The signal of a crash of the Mac app.
    pub enum CrashSignal {
        Ill => "sigill",
        Trap => "sigtrap",
        Abort => "sigabrt",
        FloatingPoint => "sigfpe",
        Bus => "sigbus",
        Segv => "sigsegv",
        Sys => "sigsys",
        Other => "other",
    }
}

named!(
    CrashKind,
    "crash kind",
    CrashKind::ALL.iter().copied(),
    |kind: CrashKind| kind.name().to_owned()
);
named!(
    MachException,
    "mach exception",
    MachException::ALL.iter().copied(),
    |exception: MachException| exception.name().to_owned()
);
named!(
    CrashSignal,
    "crash signal",
    CrashSignal::ALL.iter().copied(),
    |signal: CrashSignal| signal.name().to_owned()
);

/// The build UUID of the Mac app's executable, which matches a crash to the build's debug symbols.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BinaryId(String);

impl TryFrom<String> for BinaryId {
    type Error = Invalid;

    fn try_from(value: String) -> Result<Self, Invalid> {
        let uuid = value.len() == 36
            && value.char_indices().all(|(index, char)| match index {
                8 | 13 | 18 | 23 => char == '-',
                _ => char.is_ascii_hexdigit(),
            });
        uuid.then(|| BinaryId(value.to_ascii_uppercase()))
            .ok_or(Invalid("binary id"))
    }
}

impl From<BinaryId> for String {
    fn from(id: BinaryId) -> String {
        id.0
    }
}
