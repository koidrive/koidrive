use semver::Version;
use std::path::PathBuf;

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
pub(crate) struct CommitFact {
    pub(crate) path: PathBuf,
    #[serde(
        serialize_with = "serialize_version",
        deserialize_with = "deserialize_version"
    )]
    #[schemars(with = "String")]
    pub(crate) version: Version,
    pub(crate) change: CommitChange,
}
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommitChange {
    Installed,
    Replaced,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppLaunch {
    Open,
    Preserve,
}
fn serialize_version<S: serde::Serializer>(
    version: &Version,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(version)
}
fn deserialize_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Version, D::Error> {
    let text = <String as serde::Deserialize>::deserialize(deserializer)?;
    Version::parse(&text).map_err(serde::de::Error::custom)
}
