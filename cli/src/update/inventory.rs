use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installs {
    pub(super) homebrew: Vec<PathBuf>,
    pub(super) script: Option<PathBuf>,
    pub(super) apps: Vec<PathBuf>,
}

impl Installs {
    pub fn of_this_machine() -> Installs {
        let absolute = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let prefix = absolute("HOMEBREW_PREFIX");
        let home = absolute("HOME");
        let installs = Installs::under(prefix.as_deref(), home.as_deref());
        #[cfg(feature = "test-install-inventory")]
        let installs = {
            let mut installs = installs;
            if let Some(root) = absolute("KOIDRIVE_TEST_INSTALL_INVENTORY") {
                installs.homebrew = prefix
                    .into_iter()
                    .map(|prefix| prefix.join("opt/koidrive/bin/koi"))
                    .collect();
                installs.apps = vec![root.join("Applications/Koidrive.app/Contents/Helpers/koi")];
            }
            installs
        };
        installs
    }

    pub(super) fn under(homebrew_prefix: Option<&Path>, home: Option<&Path>) -> Installs {
        let mut homebrew: Vec<PathBuf> = Vec::new();
        let prefixes = [
            homebrew_prefix,
            Some(Path::new("/opt/homebrew")),
            Some(Path::new("/usr/local")),
            Some(Path::new("/home/linuxbrew/.linuxbrew")),
        ];
        for prefix in prefixes.into_iter().flatten() {
            let koi = prefix.join("opt/koidrive/bin/koi");
            if !homebrew.contains(&koi) {
                homebrew.push(koi);
            }
        }
        Installs {
            homebrew,
            script: home.map(|home| home.join(".local/bin/koi")),
            apps: std::iter::once(PathBuf::from(
                "/Applications/Koidrive.app/Contents/Helpers/koi",
            ))
            .chain(home.map(|home| home.join("Applications/Koidrive.app/Contents/Helpers/koi")))
            .collect(),
        }
    }

    pub(super) fn all(&self) -> Vec<PathBuf> {
        let mut all = self.homebrew.clone();
        all.extend(self.script.clone());
        all.extend(self.apps.iter().cloned());
        all
    }
}

pub fn homebrew_link(binary: &Path) -> Option<PathBuf> {
    if binary.ends_with("opt/koidrive/bin/koi") {
        return Some(binary.to_path_buf());
    }
    if !binary.ends_with("bin/koi") {
        return None;
    }
    let version = binary.parent()?.parent()?;
    let formula = version.parent()?;
    let cellar = formula.parent()?;
    let prefix = cellar.parent()?;
    let named = |path: &Path, name: &str| path.file_name().is_some_and(|file| file == name);
    (named(formula, "koidrive") && named(cellar, "Cellar") && version.file_name().is_some())
        .then(|| prefix.join("opt/koidrive/bin/koi"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Cask { brew: PathBuf },
    CaskElsewhere { brew: PathBuf },
    Formula { brew: PathBuf },
    Installer,
    App,
    AppDownload,
    Development,
}

impl Method {
    pub(super) fn via(&self) -> Option<Via> {
        match self {
            Method::CaskElsewhere { .. } | Method::Formula { .. } => Some(Via::Homebrew),
            Method::Installer => Some(Via::Installer),
            Method::Cask { .. } | Method::App | Method::AppDownload => Some(Via::App),
            Method::Development => None,
        }
    }

    fn brew(&self) -> Option<&Path> {
        match self {
            Method::Cask { brew } | Method::CaskElsewhere { brew } | Method::Formula { brew } => {
                Some(brew)
            }
            Method::Installer | Method::App | Method::AppDownload | Method::Development => None,
        }
    }
}

pub(crate) struct Install {
    pub(super) binary: PathBuf,
    pub(super) method: Method,
}

impl Install {
    fn prefixes(installs: &Installs) -> Vec<PathBuf> {
        let mut prefixes: Vec<PathBuf> = installs
            .homebrew
            .iter()
            .filter_map(|koi| koi.ancestors().nth(4).map(Path::to_path_buf))
            .collect();
        #[cfg(feature = "test-install-inventory")]
        let isolated = std::env::var_os("KOIDRIVE_TEST_INSTALL_INVENTORY").is_some();
        #[cfg(not(feature = "test-install-inventory"))]
        let isolated = false;
        if !isolated && let Some(path) = std::env::var_os("PATH") {
            prefixes.extend(
                std::env::split_paths(&path)
                    .filter(|bin| {
                        bin.is_absolute() && bin.file_name().is_some_and(|name| name == "bin")
                    })
                    .filter_map(|bin| bin.parent().map(Path::to_path_buf)),
            );
        }
        prefixes
    }

    pub(crate) fn every(also: &[PathBuf]) -> Vec<Self> {
        let installs = Installs::of_this_machine();
        let prefixes = Self::prefixes(&installs);
        let known = installs.all();
        let running = std::env::current_exe().ok();
        let mut found: Vec<Install> = Vec::new();
        for binary in running.iter().chain(also).chain(&known) {
            let Ok(resolved) = fs::canonicalize(binary) else {
                continue;
            };
            let development = Some(binary) == running.as_ref()
                && update::is_development_version(env!("CARGO_PKG_VERSION"))
                && !known
                    .iter()
                    .any(|path| fs::canonicalize(path).is_ok_and(|path| path == resolved));
            let install = match development {
                true => Install {
                    binary: resolved,
                    method: Method::Development,
                },
                false => match Self::locate(binary, &prefixes, &installs.apps) {
                    Ok(install) => install,
                    Err(_) => continue,
                },
            };
            let homebrew = |method: &Method| method.brew().is_some();
            match found
                .iter()
                .position(|known| known.binary == install.binary)
            {
                Some(index) if homebrew(&install.method) && !homebrew(&found[index].method) => {
                    found[index] = install;
                }
                Some(_) => {}
                None => found.push(install),
            }
        }
        found
    }

    pub(crate) fn binary(&self) -> &Path {
        &self.binary
    }

    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    pub(crate) fn current() -> Result<Self, CliError> {
        let installs = Installs::of_this_machine();
        let prefixes = Self::prefixes(&installs);
        let binary = std::env::current_exe()
            .map_err(|error| failed(format!("Cannot locate this koi: {error}")))?;
        let invoked = std::env::args_os()
            .next()
            .and_then(|arg| Self::invoked(&binary, &arg, std::env::var_os("PATH").as_deref()));
        let install = Self::resolve(
            invoked.as_deref().unwrap_or(&binary),
            env!("CARGO_PKG_VERSION"),
            &prefixes,
            &installs.apps,
        )?;
        if invoked.is_none()
            && matches!(
                install.method,
                Method::Installer | Method::Cask { .. } | Method::App | Method::AppDownload
            )
        {
            return Err(failed(
                "Cannot identify the invoked koi; run its full path to update".to_owned(),
            ));
        }
        Ok(install)
    }

    pub(super) fn invoked(binary: &Path, arg: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
        if arg.is_empty() {
            return None;
        }
        let invoked = Path::new(arg);
        let invoked = if invoked.is_absolute() || invoked.components().count() > 1 {
            invoked.to_path_buf()
        } else {
            on_path(arg, path)?
        };
        (fs::canonicalize(&invoked).ok()? == fs::canonicalize(binary).ok()?).then_some(invoked)
    }

    pub(super) fn resolve(
        binary: &Path,
        version: &str,
        prefixes: &[PathBuf],
        apps: &[PathBuf],
    ) -> Result<Self, CliError> {
        if update::is_development_version(version) {
            return Ok(Install {
                binary: binary.to_path_buf(),
                method: Method::Development,
            });
        }
        Self::locate(binary, prefixes, apps)
    }

    pub(super) fn locate(
        binary: &Path,
        prefixes: &[PathBuf],
        apps: &[PathBuf],
    ) -> Result<Self, CliError> {
        let resolved = fs::canonicalize(binary)
            .map_err(|error| failed(format!("Cannot resolve {}: {error}", binary.display())))?;
        let mut candidates = Vec::new();
        let mut link = binary.to_path_buf();
        for _ in 0..40 {
            if let Some(prefix) =
                homebrew_link(&link).and_then(|opt| opt.ancestors().nth(4).map(Path::to_path_buf))
            {
                candidates.push(prefix);
            }
            if let Some(prefix) = link
                .parent()
                .filter(|bin| bin.ends_with("bin"))
                .and_then(Path::parent)
            {
                candidates.push(prefix.to_path_buf());
            }
            let Ok(target) = fs::read_link(&link) else {
                break;
            };
            link = match target.is_absolute() {
                true => target,
                false => link.parent().unwrap_or(Path::new("/")).join(target),
            };
        }
        candidates.extend_from_slice(prefixes);
        let formula = candidates.iter().find(|prefix| {
            fs::canonicalize(prefix.join("opt/koidrive/bin/koi"))
                .is_ok_and(|helper| helper == resolved)
        });
        let method = if let Some(prefix) = formula {
            Method::Formula {
                brew: fs::canonicalize(prefix)
                    .map_err(|error| {
                        failed(format!(
                            "Cannot resolve Homebrew at {}: {error}",
                            prefix.display()
                        ))
                    })?
                    .join("bin/brew"),
            }
        } else if let Some(opt) = homebrew_link(&resolved) {
            Method::Formula {
                brew: opt
                    .ancestors()
                    .nth(4)
                    .expect("a formula path has a prefix")
                    .join("bin/brew"),
            }
        } else if resolved.ends_with("Koidrive.app/Contents/Helpers/koi") {
            let owned = candidates.iter().find(|prefix| {
                fs::read_dir(prefix.join("Caskroom/koidrive")).is_ok_and(|entries| {
                    entries.flatten().any(|entry| {
                        fs::canonicalize(entry.path().join("Koidrive.app/Contents/Helpers/koi"))
                            .is_ok_and(|helper| helper == resolved)
                    })
                })
            });
            let standard = apps
                .iter()
                .any(|helper| fs::canonicalize(helper).is_ok_and(|helper| helper == resolved));
            match owned {
                Some(prefix) => {
                    let brew = fs::canonicalize(prefix)
                        .map_err(|error| {
                            failed(format!(
                                "Cannot resolve Homebrew at {}: {error}",
                                prefix.display()
                            ))
                        })?
                        .join("bin/brew");
                    match standard {
                        true => Method::Cask { brew },
                        false => Method::CaskElsewhere { brew },
                    }
                }
                None if standard => Method::App,
                None => Method::AppDownload,
            }
        } else if resolved.ends_with("bin/koi")
            && resolved
                .ancestors()
                .nth(2)
                .is_some_and(|keg| keg.join("INSTALL_RECEIPT.json").is_file())
        {
            return Err(failed("Cannot locate the Homebrew that owns this koi; set its HOMEBREW_PREFIX or put its bin directory on PATH".to_owned()));
        } else if resolved.file_name().is_some_and(|name| name == "koi")
            && !resolved
                .components()
                .any(|part| part.as_os_str().to_string_lossy().ends_with(".app"))
        {
            Method::Installer
        } else {
            return Err(failed(format!(
                "{} is not a recognized Koidrive installation",
                resolved.display()
            )));
        };
        Ok(Install {
            binary: resolved,
            method,
        })
    }

    pub(super) fn command(&self) -> Option<Vec<&str>> {
        match self.method {
            Method::CaskElsewhere { .. } | Method::Formula { .. } => self.fallback(),
            Method::Cask { .. } | Method::Installer | Method::App => {
                Some(vec![self.binary.to_str()?, "update"])
            }
            Method::AppDownload | Method::Development => None,
        }
    }

    pub(super) fn fallback(&self) -> Option<Vec<&str>> {
        let brew = self.method.brew()?;
        let path = std::env::var_os("PATH");
        let selected = on_path(OsStr::new("brew"), path.as_deref())
            .and_then(|selected| selected.parent().and_then(|bin| fs::canonicalize(bin).ok()));
        let owned = brew.parent().and_then(|bin| fs::canonicalize(bin).ok());
        let executable = if owned.is_some() && selected == owned {
            "brew"
        } else {
            brew.to_str()?
        };
        let mut command = vec![executable, "upgrade"];
        if !matches!(self.method, Method::Formula { .. }) {
            command.push("--cask");
        }
        command.push("koidrive");
        Some(command)
    }

    pub(crate) fn notice(&self, update: &update::Update, version: &str) -> UpdateNoticeData {
        let command = self.command().map(|command| command_text(&command));
        let update = if self.method == Method::Development {
            &update::Update::Off
        } else {
            update
        };
        UpdateNoticeData::of(update, version, command.as_deref())
    }
}

pub(super) fn command_text(command: &[&str]) -> String {
    command
        .iter()
        .map(|word| crate::pairing::shell_word(word))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn on_path(name: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|bin| bin.join(Path::new(name)))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}
