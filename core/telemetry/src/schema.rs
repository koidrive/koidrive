// SPDX-License-Identifier: Apache-2.0

//! The JSON a report holds. Every string in it is an enum's name, a key built from enum names, a version number or
//! the install id. Nothing here can hold a path, a name, file contents or an error message.

pub const SCHEMA: u16 = 5;

macro_rules! labels {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$doc:meta])* $variant:ident => $label:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$doc])* $variant,)*
        }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant,)*];

            pub fn name(self) -> &'static str {
                match self {
                    $($name::$variant => $label,)*
                }
            }
        }
    };
}

macro_rules! named {
    ($name:ident, $what:literal, $all:expr, $label:expr) => {
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&$label(*self))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                ($all)
                    .find(|value: &$name| $label(*value) == text)
                    .ok_or_else(|| serde::de::Error::custom(Invalid($what)))
            }
        }
    };
}

mod client;
mod counter;
mod crash;
mod event;
mod report;

pub use client::{
    Arch, Build, Channel, Client, Distro, ErrorCode, EventUuid, InstallId, Os, OsVersion,
    SourceFile, Version,
};
pub use counter::{
    Age, Asked, Counter, DaemonStart, Failure, FileCount, Halted, Level, Measure, Refusal,
    ServerCause, Synced, TooLarge, Transfer,
};
pub use crash::{BinaryId, CrashKind, CrashSignal, MachException, Symbol};
pub use event::{Command, Event, Origin, Outcome, RootProof, SetupFlow, SetupStep};
pub use report::{Histogram, HourlyReport, magnitude};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a valid {0}")]
pub struct Invalid(&'static str);

#[cfg(test)]
mod tests;
