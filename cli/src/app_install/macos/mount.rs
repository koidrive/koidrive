use crate::app_install::error::{AppInstallError, AppInstallFailure, failure};
use crate::app_install::macos::config::Config;
use crate::app_install::macos::control::{Control, io_error, passed, process};
use crate::app_install::macos::filesystem::{directory, identity, regular_at};
use crate::app_install::macos::state::{Identity, MountState, OwnedMount};
use crate::program::Arg;
use std::path::Path;

impl OwnedMount {
    fn check_image(&self, expected: Identity) -> Result<(), AppInstallError> {
        let parent = directory(
            self.image
                .parent()
                .ok_or_else(|| io_error("Image has no parent"))?,
            false,
        )?;
        let file = regular_at(
            &parent,
            self.image
                .file_name()
                .ok_or_else(|| io_error("Image has no name"))?,
        )?;
        if identity(&file.metadata().map_err(io_error)?) != expected {
            return Err(failure(
                AppInstallFailure::ConcurrentChange,
                "Mounted image identity changed",
            ));
        }
        Ok(())
    }

    pub(super) async fn attach(
        &self,
        config: &Config,
        expected: Identity,
        control: &Control,
    ) -> Result<(), AppInstallError> {
        self.check_image(expected)?;
        process(
            "Opening the downloaded disk image",
            config.limits.tool,
            &config.hdiutil,
            &[
                Arg::from("attach"),
                Arg::from("-readonly"),
                Arg::from("-nobrowse"),
                Arg::from("-noautoopen"),
                Arg::from("-mountpoint"),
                passed(&self.point)?,
                Arg::from("-plist"),
                passed(&self.image)?,
            ],
            control,
        )
        .await?;
        match self.state(config, control).await? {
            MountState::Owned => Ok(()),
            _ => Err(failure(
                AppInstallFailure::Verification,
                "Disk image did not attach at its owned mountpoint",
            )),
        }
    }

    pub(super) async fn state(
        &self,
        config: &Config,
        control: &Control,
    ) -> Result<MountState, AppInstallError> {
        let info = process(
            "Listing open disk images",
            config.limits.tool,
            &config.hdiutil,
            &[Arg::from("info"), Arg::from("-plist")],
            control,
        )
        .await?;
        let plist =
            plist::Value::from_reader(std::io::Cursor::new(info.stdout)).map_err(io_error)?;
        let images = plist
            .as_dictionary()
            .and_then(|dict| dict.get("images"))
            .and_then(plist::Value::as_array)
            .ok_or_else(|| io_error("Cannot inspect mounted disk images"))?;
        for image in images {
            let Some(dict) = image.as_dictionary() else {
                return Err(io_error("Invalid disk image inventory"));
            };
            let entities = dict
                .get("system-entities")
                .and_then(plist::Value::as_array)
                .ok_or_else(|| io_error("Invalid disk image entities"))?;
            for entity in entities {
                let point = entity
                    .as_dictionary()
                    .and_then(|dict| dict.get("mount-point"))
                    .and_then(plist::Value::as_string);
                if point.map(Path::new) == Some(self.point.as_path()) {
                    return Ok(
                        if dict
                            .get("image-path")
                            .and_then(plist::Value::as_string)
                            .map(Path::new)
                            == Some(self.image.as_path())
                        {
                            MountState::Owned
                        } else {
                            MountState::Foreign
                        },
                    );
                }
            }
        }
        Ok(MountState::Absent)
    }

    pub(super) async fn cleanup(
        &self,
        config: &Config,
        expected: Identity,
        control: &Control,
    ) -> Result<(), AppInstallError> {
        match self.state(config, control).await? {
            MountState::Absent => return Ok(()),
            MountState::Foreign => {
                return Err(failure(
                    AppInstallFailure::UnsafePath,
                    "Unexpected image occupies installer mountpoint",
                ));
            }
            MountState::Owned => (),
        }
        self.check_image(expected)?;
        process(
            "Closing the downloaded disk image",
            config.limits.tool,
            &config.hdiutil,
            &[Arg::from("detach"), passed(&self.point)?],
            control,
        )
        .await?;
        match self.state(config, control).await? {
            MountState::Absent => Ok(()),
            _ => Err(io_error("Installer disk image remains mounted")),
        }
    }
}
