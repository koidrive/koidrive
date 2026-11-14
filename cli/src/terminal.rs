// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::future::Future;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use cliclack::Theme as _;
use koidrive_ipc::Code;
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::process::Child;
use tokio::sync::{Mutex, oneshot};

use crate::error::CliError;

const READY: &str = "\u{1b}]777;koi-prompt-ready\u{7}";

#[derive(Default)]
struct ReadyTheme {
    ready: AtomicBool,
    compact: bool,
}

struct DefaultTheme;

impl cliclack::Theme for DefaultTheme {}

impl cliclack::Theme for ReadyTheme {
    fn format_footer_with_message(&self, state: &cliclack::ThemeState, message: &str) -> String {
        let mut footer = if self.compact && matches!(state, cliclack::ThemeState::Submit) {
            String::new()
        } else {
            <DefaultTheme as cliclack::Theme>::format_footer_with_message(
                &DefaultTheme,
                state,
                message,
            )
        };
        if !self.ready.swap(true, Ordering::Relaxed) {
            footer.push_str(READY);
        }
        footer
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Interaction {
    Direct,
    Plain,
    Rich,
}

#[derive(Serialize, Deserialize)]
enum Prompt {
    Input {
        question: String,
        secret: bool,
    },
    Select {
        question: String,
        items: Vec<String>,
    },
    Confirm {
        question: String,
        default: bool,
    },
}

#[derive(Serialize, Deserialize)]
enum Answer {
    Text(String),
    Selection(usize),
    Confirmation(bool),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum PromptFailure {
    Cancelled,
    Closed,
    Io(String),
}

impl From<io::Error> for PromptFailure {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::Interrupted => Self::Cancelled,
            io::ErrorKind::UnexpectedEof => Self::Closed,
            _ => Self::Io(error.to_string()),
        }
    }
}

impl From<PromptFailure> for CliError {
    fn from(error: PromptFailure) -> Self {
        match error {
            PromptFailure::Cancelled => {
                CliError::new(Code::Interrupted, "cancelled on the terminal")
            }
            PromptFailure::Closed => CliError::from("stdin closed before an answer"),
            PromptFailure::Io(message) => CliError::from(message),
        }
    }
}

struct ActivePrompt {
    child: Child,
    saved: SavedTerminal,
}

struct InputBridge {
    terminal: AsyncFd<OwnedFd>,
    helper: AsyncFd<OwnedFd>,
}

#[cfg(target_os = "macos")]
fn controlling_terminal_name() -> io::Result<std::ffi::CString> {
    unsafe extern "C" {
        fn devname_r(
            device: libc::dev_t,
            mode: libc::mode_t,
            buffer: *mut libc::c_char,
            length: libc::c_int,
        ) -> *mut libc::c_char;
    }

    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of_val(&info) as libc::c_int;
    let count = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if count <= 0 {
        return Err(io::Error::last_os_error());
    }
    if count != size {
        return Err(io::Error::other(
            "the process terminal information is incomplete",
        ));
    }
    let info = unsafe { info.assume_init() };
    let mut buffer = [0_u8; libc::PATH_MAX as usize];
    let name = unsafe {
        devname_r(
            info.e_tdev as libc::dev_t,
            libc::S_IFCHR,
            buffer.as_mut_ptr().cast(),
            buffer.len() as libc::c_int,
        )
    };
    if name.is_null() {
        return Err(io::Error::last_os_error());
    }
    let name = std::ffi::CStr::from_bytes_until_nul(&buffer).map_err(io::Error::other)?;
    let mut path = b"/dev/".to_vec();
    path.extend_from_slice(name.to_bytes());
    std::ffi::CString::new(path).map_err(io::Error::other)
}

impl InputBridge {
    fn new(
        input: &OwnedFd,
        output: impl AsFd,
        size: rustix::termios::Winsize,
    ) -> io::Result<(Self, std::fs::File)> {
        use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl, open};
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

        fn register(fd: OwnedFd) -> io::Result<AsyncFd<OwnedFd>> {
            unsafe { AsyncFd::register(fd) }.map_err(|error| error.into_parts().1)
        }

        let name = rustix::termios::ttyname(input, Vec::new())?;
        let proxy = name.as_c_str() == c"/dev/tty";
        #[cfg(target_os = "macos")]
        let name = if proxy {
            controlling_terminal_name()?
        } else {
            name
        };
        let terminal = open(
            name.as_c_str(),
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        if proxy {
            let session = rustix::termios::tcgetsid(input)?;
            if rustix::termios::tcgetsid(&output).ok() != Some(session)
                || rustix::termios::tcgetsid(&terminal).ok() != Some(session)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "stdin and stderr are not the same terminal",
                ));
            }
        }
        let helper = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)?;
        rustix::io::fcntl_setfd(&helper, rustix::io::FdFlags::CLOEXEC)?;
        fcntl_setfl(&helper, fcntl_getfl(&helper)? | OFlags::NONBLOCK)?;
        grantpt(&helper)?;
        unlockpt(&helper)?;
        let name = ptsname(&helper, Vec::new())?;
        let slave = open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut raw = tcgetattr(&slave)?;
        raw.make_raw();
        tcsetattr(&slave, OptionalActions::Now, &raw)?;
        rustix::termios::tcsetwinsize(&helper, size)?;
        Ok((
            Self {
                terminal: register(terminal)?,
                helper: register(helper)?,
            },
            std::fs::File::from(slave),
        ))
    }

    fn discard_startup_input(&self) -> io::Result<Option<PromptFailure>> {
        if let Some(control) = self.drain()? {
            return Ok(Some(control));
        }
        let terminal = self.terminal.get_ref();
        let line = tcgetattr(terminal)?;
        if !line.local_modes.contains(LocalModes::ICANON) {
            return Ok(None);
        }
        let mut partial = line.clone();
        partial.local_modes.remove(LocalModes::ICANON);
        partial.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 1;
        partial.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;
        tcsetattr(terminal, OptionalActions::Now, &partial)?;
        let control = self.drain();
        tcsetattr(terminal, OptionalActions::Now, &line)?;
        control
    }

    fn drain(&self) -> io::Result<Option<PromptFailure>> {
        let mut buffer = [0; 4096];
        loop {
            match rustix::io::read(self.terminal.get_ref(), &mut buffer) {
                Ok(0) => return Ok(Some(PromptFailure::Closed)),
                Ok(count) => match buffer[..count].iter().find(|&&key| key == 3 || key == 4) {
                    Some(3) => return Ok(Some(PromptFailure::Cancelled)),
                    Some(_) => return Ok(Some(PromptFailure::Closed)),
                    None => {}
                },
                Err(rustix::io::Errno::AGAIN) => return Ok(None),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn relay(
        &self,
        ready: oneshot::Sender<Option<PromptFailure>>,
        mut output: impl Write,
    ) -> io::Result<()> {
        let mut ready = Some(ready);
        let mut pending = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = self
                .helper
                .async_io(Interest::READABLE, |fd| {
                    rustix::io::read(fd, &mut buffer).map_err(io::Error::from)
                })
                .await;
            let count = match count {
                Ok(0) => return Ok(()),
                Ok(count) => count,
                Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                Err(error) => return Err(error),
            };
            pending.extend_from_slice(&buffer[..count]);
            while let Some(index) = pending
                .windows(READY.len())
                .position(|bytes| bytes == READY.as_bytes())
            {
                if let Some(ready) = ready.take() {
                    let control = self.discard_startup_input()?;
                    output.write_all(&pending[..index])?;
                    output.flush()?;
                    let _ = ready.send(control);
                } else {
                    output.write_all(&pending[..index])?;
                    output.flush()?;
                }
                pending.drain(..index + READY.len());
            }
            if ready.is_some() {
                if pending.len() > 64 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the terminal prompt frame is too long",
                    ));
                }
                continue;
            }
            let keep = (1..READY.len())
                .rev()
                .find(|&length| pending.ends_with(&READY.as_bytes()[..length]))
                .unwrap_or(0);
            let written = pending.len() - keep;
            output.write_all(&pending[..written])?;
            output.flush()?;
            pending.drain(..written);
        }
    }

    async fn forward(
        &self,
        mut rendered: oneshot::Receiver<Option<PromptFailure>>,
    ) -> Result<(), PromptFailure> {
        let mut buffer = [0; 4096];
        let mut accepting = false;
        loop {
            let count = tokio::select! {
                biased;
                ready = &mut rendered, if !accepting => {
                    match ready {
                        Err(_) => return Ok(()),
                        Ok(Some(failure)) => return Err(failure),
                        Ok(None) => {}
                    }
                    accepting = true;
                    continue;
                },
                count = self.terminal.async_io(Interest::READABLE, |fd| {
                    rustix::io::read(fd, &mut buffer).map_err(io::Error::from)
                }) => count?,
            };
            if count == 0 || buffer[..count].contains(&4) {
                return Err(PromptFailure::Closed);
            }
            if !accepting {
                if buffer[..count].contains(&3) {
                    return Err(PromptFailure::Cancelled);
                }
                continue;
            }
            if let Ok(size) = rustix::termios::tcgetwinsize(io::stderr()) {
                rustix::termios::tcsetwinsize(&self.helper, size).map_err(io::Error::from)?;
            }
            let mut bytes = &buffer[..count];
            while !bytes.is_empty() {
                let count = self
                    .helper
                    .async_io(Interest::WRITABLE, |fd| {
                        rustix::io::write(fd, bytes).map_err(io::Error::from)
                    })
                    .await;
                let count = match count {
                    Ok(count) if count > 0 => count,
                    _ => return Ok(()),
                };
                bytes = &bytes[count..];
            }
        }
    }
}

struct SavedTerminal {
    input: OwnedFd,
    original: Termios,
}

impl Drop for SavedTerminal {
    fn drop(&mut self) {
        let _ = tcsetattr(&self.input, OptionalActions::Now, &self.original);
    }
}

pub struct Terminal {
    interaction: Interaction,
    setup: bool,
    active: Mutex<Option<ActivePrompt>>,
}

pub struct Progress<'a> {
    terminal: &'a Terminal,
    bar: Option<cliclack::ProgressBar>,
}

impl Progress<'_> {
    pub fn update(&self, message: &str) {
        let message = crate::printable(message);
        match &self.bar {
            Some(bar) => bar.set_message(message),
            None => self.terminal.message(&message),
        }
    }

    pub fn finish(mut self, message: &str) {
        if let Some(bar) = self.bar.take() {
            bar.clear();
        }
        self.terminal.complete(message);
    }
}

impl Drop for Progress<'_> {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.clear();
        }
    }
}

fn nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

pub(crate) fn plain_requested() -> bool {
    std::env::var_os("KOI_PLAIN").is_some_and(|value| value == "1")
        || std::env::var_os("TERM").is_some_and(|value| value == "dumb")
        || std::env::args_os()
            .skip(1)
            .take_while(|value| value != "--")
            .any(|value| value == "--plain")
}

pub fn rich(plain: bool, width: Option<u16>, terminal_attached: bool) -> bool {
    terminal_attached && !plain && matches!(width, Some(60..))
}

pub fn clap_color() -> clap::ColorChoice {
    if nonempty("NO_COLOR") || plain_requested() {
        clap::ColorChoice::Never
    } else {
        clap::ColorChoice::Auto
    }
}

pub(crate) fn colors(plain: bool) {
    let forced =
        std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| !value.is_empty() && value != "0");
    let enabled = !plain
        && !nonempty("NO_COLOR")
        && io::stderr().is_terminal()
        && std::env::var_os("TERM").is_none_or(|value| value != "dumb")
        && (forced || std::env::var_os("CLICOLOR").is_none_or(|value| value != "0"));
    console::set_colors_enabled(enabled);
    console::set_colors_enabled_stderr(enabled);
}

impl Terminal {
    pub fn new(plain: bool, guided: bool) -> Self {
        let width = console::Term::stderr()
            .size_checked()
            .map(|(_, width)| width);
        let interaction = Self::interaction(guided, plain || plain_requested(), width);
        colors(interaction != Interaction::Rich);
        Self {
            interaction,
            setup: false,
            active: Mutex::new(None),
        }
    }

    pub fn setup(plain: bool, guided: bool) -> Self {
        Self {
            setup: true,
            ..Self::new(plain, guided)
        }
    }

    pub fn setup_guided(&self) -> bool {
        self.setup && self.guided()
    }

    fn interaction(guided: bool, plain: bool, width: Option<u16>) -> Interaction {
        match (guided, rich(plain, width, guided)) {
            (false, _) => Interaction::Direct,
            (true, true) => Interaction::Rich,
            _ => Interaction::Plain,
        }
    }

    pub fn guided(&self) -> bool {
        self.interaction != Interaction::Direct
    }

    pub fn message(&self, message: &str) {
        let message = crate::printable(message);
        if self.interaction == Interaction::Rich {
            if self.setup {
                let theme = DefaultTheme;
                let symbol = theme
                    .bar_color(&cliclack::ThemeState::Submit)
                    .apply_to("│")
                    .to_string();
                eprint!(
                    "{}",
                    theme.format_log_with_spacing(&message, &symbol, false)
                );
            } else {
                let _ = cliclack::log::info(message);
            }
        } else {
            eprintln!("{message}");
        }
    }

    pub fn complete(&self, message: &str) {
        let message = crate::printable(message);
        if self.interaction == Interaction::Rich {
            let theme = DefaultTheme;
            eprint!(
                "{}",
                theme.format_log_with_spacing(&message, &theme.submit_symbol(), false)
            );
        } else {
            eprintln!("{message}.");
        }
    }

    pub fn progress(&self, message: &str) -> Progress<'_> {
        self.start_progress(message, true)
    }

    pub fn continue_progress(&self, message: &str) -> Progress<'_> {
        self.start_progress(message, false)
    }

    fn start_progress(&self, message: &str, announce: bool) -> Progress<'_> {
        let bar = (self.interaction == Interaction::Rich).then(cliclack::spinner);
        match &bar {
            Some(bar) => bar.start(crate::printable(message)),
            None if announce => self.message(message),
            None => {}
        }
        Progress {
            terminal: self,
            bar,
        }
    }

    pub fn outro(&self, message: &str) {
        let message = message
            .trim_end()
            .lines()
            .map(crate::printable)
            .collect::<Vec<_>>()
            .join("\n");
        self.separator();
        if self.interaction == Interaction::Rich {
            let _ = cliclack::outro(message.replace('\n', "\n   "));
        } else {
            eprintln!("{message}");
        }
    }

    pub fn separator(&self) {
        if self.interaction == Interaction::Rich {
            eprintln!(
                "{}",
                DefaultTheme
                    .bar_color(&cliclack::ThemeState::Submit)
                    .apply_to("│")
            );
        } else {
            eprintln!();
        }
    }

    pub fn intro(&self) {
        let title = format!("Koidrive {}", env!("CARGO_PKG_VERSION"));
        if self.interaction == Interaction::Rich {
            let _ = cliclack::intro(title);
        } else {
            eprintln!("{title}\n");
        }
    }

    pub async fn input(&self, question: &str, secret: bool) -> Result<String, PromptFailure> {
        self.text(Prompt::Input {
            question: question.into(),
            secret,
        })
        .await
    }

    async fn text(&self, request: Prompt) -> Result<String, PromptFailure> {
        match self.prompt(request).await? {
            Answer::Text(text) => Ok(text),
            _ => Err(PromptFailure::Io(
                "the terminal returned the wrong answer type".into(),
            )),
        }
    }

    pub async fn select(&self, question: &str, items: Vec<String>) -> Result<usize, PromptFailure> {
        if items.is_empty() {
            return Err(PromptFailure::Io("there are no choices to select".into()));
        }
        match self
            .prompt(Prompt::Select {
                question: question.into(),
                items,
            })
            .await?
        {
            Answer::Selection(index) => Ok(index),
            _ => Err(PromptFailure::Io(
                "the terminal returned the wrong answer type".into(),
            )),
        }
    }

    pub async fn confirm(&self, question: &str, default: bool) -> Result<bool, PromptFailure> {
        match self
            .prompt(Prompt::Confirm {
                question: question.into(),
                default,
            })
            .await?
        {
            Answer::Confirmation(answer) => Ok(answer),
            _ => Err(PromptFailure::Io(
                "the terminal returned the wrong answer type".into(),
            )),
        }
    }

    async fn prompt(&self, request: Prompt) -> Result<Answer, PromptFailure> {
        if !self.guided() || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(PromptFailure::Io(
                "this question needs a terminal on stdin and stderr".into(),
            ));
        }
        let mut active = self.active.lock().await;
        let input = io::stdin().as_fd().try_clone_to_owned()?;
        let original = tcgetattr(&input).map_err(io::Error::from)?;
        let saved = SavedTerminal { input, original };
        let mut hidden = saved.original.clone();
        if self.interaction == Interaction::Rich {
            hidden.make_raw();
            hidden.output_modes = saved.original.output_modes;
        }
        if self.interaction == Interaction::Rich
            || matches!(&request, Prompt::Input { secret: true, .. })
        {
            hidden.local_modes.remove(LocalModes::ECHO);
        }
        tcsetattr(&saved.input, OptionalActions::Flush, &hidden).map_err(io::Error::from)?;
        let size = rustix::termios::tcgetwinsize(io::stderr()).map_err(io::Error::from)?;
        let (bridge, input) = InputBridge::new(&saved.input, io::stderr(), size)?;
        if self.setup && self.interaction == Interaction::Rich {
            eprintln!(
                "{}",
                DefaultTheme
                    .bar_color(&cliclack::ThemeState::Submit)
                    .apply_to("│")
            );
        }
        let child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("_prompt")
            .arg(if self.interaction == Interaction::Rich {
                if self.setup { "setup-rich" } else { "rich" }
            } else {
                "plain"
            })
            .arg(serde_json::to_string(&request).expect("prompt requests serialize"))
            .stdin(Stdio::from(input.try_clone()?))
            .stderr(Stdio::from(input))
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        *active = Some(ActivePrompt { child, saved });
        let result = {
            let answer = async {
                let running = active.as_mut().expect("the helper was just started");
                let mut output = Vec::new();
                let stdout = running
                    .child
                    .stdout
                    .take()
                    .expect("the helper has a stdout pipe")
                    .take(64 * 1024 + 1);
                let mut stdout = stdout;
                stdout.read_to_end(&mut output).await?;
                drop(stdout);
                if output.len() > 64 * 1024 {
                    return Err(PromptFailure::Io("the terminal answer is too long".into()));
                }
                let status = running.child.wait().await?;
                if !status.success() {
                    return Err(PromptFailure::Io(
                        "the terminal helper stopped before returning an answer".into(),
                    ));
                }
                serde_json::from_slice::<Result<Answer, PromptFailure>>(&output).map_err(|_| {
                    PromptFailure::Io("the terminal helper returned an invalid answer".into())
                })?
            };
            let (ready, rendered) = oneshot::channel();
            let completion = async {
                tokio::try_join!(answer, async {
                    bridge
                        .relay(ready, io::stderr())
                        .await
                        .map_err(PromptFailure::from)
                })
                .map(|(answer, ())| answer)
            };
            tokio::pin!(completion);
            let input = bridge.forward(rendered);
            tokio::select! {
                biased;
                result = &mut completion => result,
                result = input => match result {
                    Ok(()) => completion.await,
                    Err(error) => Err(error),
                },
            }
        };
        self.cleanup(&mut active).await;
        result
    }

    async fn cleanup(&self, active: &mut Option<ActivePrompt>) {
        if let Some(running) = active.as_mut() {
            let _ = running.child.kill().await;
            let _ = running.child.wait().await;
        }
        if let Some(running) = active.take() {
            drop(running.saved);
            if self.interaction == Interaction::Rich {
                let _ = console::Term::stderr().show_cursor();
            }
        }
    }

    pub async fn cancel(&self) {
        self.cleanup(&mut *self.active.lock().await).await;
    }

    pub async fn wait<T>(
        &self,
        message: &str,
        work: impl Future<Output = Result<T, CliError>>,
    ) -> Result<T, CliError> {
        struct Progress(Option<cliclack::ProgressBar>);
        impl Drop for Progress {
            fn drop(&mut self) {
                if let Some(progress) = &self.0 {
                    progress.clear();
                }
            }
        }
        let mut progress =
            Progress((self.interaction == Interaction::Rich).then(cliclack::spinner));
        if let Some(progress) = &progress.0 {
            progress.start(message);
        } else {
            self.message(message);
        }
        let result = work.await;
        if let Some(progress) = progress.0.take() {
            if result.is_ok() {
                progress.stop("Done");
            } else {
                progress.error("Failed");
            }
        }
        result
    }
}

pub(crate) fn read_line(mut input: impl BufRead) -> io::Result<String> {
    let mut text = String::new();
    match input.read_line(&mut text)? {
        0 => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stdin closed before an answer",
        )),
        _ => Ok(text),
    }
}

fn line(question: &str, secret: bool) -> Result<String, PromptFailure> {
    let input = io::stdin();
    let original = secret
        .then(|| tcgetattr(&input).map_err(io::Error::from))
        .transpose()?;
    if let Some(original) = &original {
        let mut hidden = original.clone();
        hidden.local_modes.remove(LocalModes::ECHO);
        tcsetattr(&input, OptionalActions::Now, &hidden).map_err(io::Error::from)?;
    }
    let result = (|| {
        io::stderr().write_all(question.as_bytes())?;
        io::stderr().write_all(READY.as_bytes())?;
        io::stderr().flush()?;
        read_line(input.lock())
    })();
    if let Some(original) = original {
        tcsetattr(&input, OptionalActions::Now, &original).map_err(io::Error::from)?;
        eprintln!();
    }
    Ok(result?.trim().to_owned())
}

fn interact(request: Prompt, rich: bool) -> Result<Answer, PromptFailure> {
    match request {
        Prompt::Input { question, secret } => {
            let question = crate::printable(&question);
            let text = if rich && secret {
                cliclack::password(question).interact()?
            } else if rich {
                cliclack::input(question).interact::<String>()?
            } else {
                line(&format!("{question}: "), secret)?
            };
            Ok(Answer::Text(text))
        }
        Prompt::Select { question, items } => {
            if items.is_empty() {
                return Err(PromptFailure::Io("there are no choices to select".into()));
            }
            let question = crate::printable(&question);
            if rich {
                let mut select = cliclack::select(question).initial_value(0);
                for (index, item) in items.iter().enumerate() {
                    select = select.item(index, crate::printable(item), "");
                }
                return Ok(Answer::Selection(select.interact()?));
            }
            eprintln!("{question}");
            for (index, item) in items.iter().enumerate() {
                eprintln!("  {}. {}", index + 1, crate::printable(item));
            }
            loop {
                if let Ok(index) =
                    line(&format!("Choose 1 to {}: ", items.len()), false)?.parse::<usize>()
                    && (1..=items.len()).contains(&index)
                {
                    return Ok(Answer::Selection(index - 1));
                }
                eprintln!("Enter one of the numbers shown above.");
            }
        }
        Prompt::Confirm { question, default } => {
            let question = crate::printable(&question);
            if rich {
                return Ok(Answer::Confirmation(
                    cliclack::confirm(question)
                        .initial_value(default)
                        .interact()?,
                ));
            }
            loop {
                let question = format!("{question} {} ", if default { "[Y/n]" } else { "[y/N]" });
                match line(&question, false)?.to_ascii_lowercase().as_str() {
                    "" => return Ok(Answer::Confirmation(default)),
                    "y" | "yes" => return Ok(Answer::Confirmation(true)),
                    "n" | "no" => return Ok(Answer::Confirmation(false)),
                    _ => eprintln!("Enter yes or no."),
                }
            }
        }
    }
}

pub fn helper(args: &[OsString]) -> Option<ExitCode> {
    if args.get(1).is_none_or(|arg| arg != "_prompt") {
        return None;
    }
    if args.len() != 4
        || args
            .get(2)
            .is_none_or(|value| value != "plain" && value != "rich" && value != "setup-rich")
    {
        return Some(ExitCode::from(2));
    }
    let Some(request) = args
        .get(3)
        .and_then(|text| text.to_str())
        .and_then(|text| serde_json::from_str(text).ok())
    else {
        return Some(ExitCode::from(2));
    };
    let rich = args.get(2).is_some_and(|value| value != "plain");
    colors(!rich);
    if rich {
        cliclack::set_theme(ReadyTheme {
            compact: args.get(2).is_some_and(|value| value == "setup-rich"),
            ..Default::default()
        });
    }
    let result = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async {
            let _interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let _terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            interact(request, rich)
        }),
        Err(error) => Err(error.into()),
    };
    let output = serde_json::to_vec(&result).expect("terminal answers serialize");
    Some(if io::stdout().write_all(&output).is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_bridge(raw: bool) -> (OwnedFd, OwnedFd, InputBridge, std::fs::File) {
        use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl, open};
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

        let terminal = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&terminal).unwrap();
        unlockpt(&terminal).unwrap();
        let name = ptsname(&terminal, Vec::new()).unwrap();
        let slave = open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY,
            Mode::empty(),
        )
        .unwrap();
        if raw {
            let mut modes = tcgetattr(&slave).unwrap();
            modes.make_raw();
            tcsetattr(&slave, OptionalActions::Now, &modes).unwrap();
        }
        let size = rustix::termios::Winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (bridge, helper) = InputBridge::new(&slave, &slave, size).unwrap();
        fcntl_setfl(&helper, fcntl_getfl(&helper).unwrap() | OFlags::NONBLOCK).unwrap();
        (terminal, slave, bridge, helper)
    }

    #[tokio::test]
    async fn input_waits_for_the_complete_rendered_frame_and_discards_startup_keys() {
        let (terminal, _slave, bridge, mut helper) = open_bridge(true);
        rustix::io::write(&terminal, b"\r").unwrap();
        let (ready, rendered) = oneshot::channel();
        let (started, accepting) = oneshot::channel();
        let mut output = Vec::new();
        let received = {
            let display = bridge.relay(ready, &mut output);
            let input = async {
                rendered.await.unwrap();
                started.send(()).unwrap();
                let (ready, rendered) = oneshot::channel();
                ready.send(None).unwrap();
                bridge.forward(rendered).await
            };
            let dialog = async {
                helper.write_all(b"Send reports? [Y/n] ").unwrap();
                helper.write_all(&READY.as_bytes()[..3]).unwrap();
                tokio::task::yield_now().await;
                let mut buffer = [0; 32];
                assert_eq!(
                    rustix::io::read(&helper, &mut buffer),
                    Err(rustix::io::Errno::AGAIN)
                );
                helper.write_all(&READY.as_bytes()[3..]).unwrap();
                accepting.await.unwrap();
                rustix::io::write(&terminal, b"n\n").unwrap();
                let mut received = Vec::new();
                loop {
                    match rustix::io::read(&helper, &mut buffer) {
                        Ok(count) => received.extend_from_slice(&buffer[..count]),
                        Err(rustix::io::Errno::AGAIN) => tokio::task::yield_now().await,
                        other => panic!("unexpected helper input: {other:?}"),
                    }
                    if received.ends_with(b"\n") {
                        break received;
                    }
                }
            };
            tokio::pin!(display, input, dialog);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::select! {
                    received = &mut dialog => received,
                    result = &mut display => panic!("the display stopped early: {result:?}"),
                    result = &mut input => panic!("input stopped early: {result:?}"),
                }
            })
            .await
            .unwrap()
        };
        assert_eq!(received, b"n\n");
        assert_eq!(output, b"Send reports? [Y/n] ");
        for key in [3_u8, 4] {
            let (_sender, rendered) = oneshot::channel();
            rustix::io::write(&terminal, &[key]).unwrap();
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(2), bridge.forward(rendered))
                    .await
                    .unwrap();
            assert!(matches!(
                (key, result),
                (3, Err(PromptFailure::Cancelled)) | (4, Err(PromptFailure::Closed))
            ));
        }
    }

    #[tokio::test]
    async fn control_keys_queued_before_the_frame_is_ready_still_end_the_prompt() {
        for key in [3_u8, 4] {
            let (terminal, _slave, bridge, mut helper) = open_bridge(true);
            rustix::io::write(&terminal, &[b'y', key]).unwrap();
            helper.write_all(b"Send reports? [Y/n] ").unwrap();
            helper.write_all(READY.as_bytes()).unwrap();
            let (ready, rendered) = oneshot::channel();
            let mut output = Vec::new();
            let result = {
                let display = bridge.relay(ready, &mut output);
                let input = async {
                    let control = rendered.await.unwrap();
                    let (ready, rendered) = oneshot::channel();
                    ready.send(control).unwrap();
                    bridge.forward(rendered).await
                };
                tokio::pin!(display, input);
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    tokio::select! {
                        result = &mut input => result,
                        result = &mut display => panic!("the display stopped early: {result:?}"),
                    }
                })
                .await
                .unwrap()
            };
            assert!(matches!(
                (key, result),
                (3, Err(PromptFailure::Cancelled)) | (4, Err(PromptFailure::Closed))
            ));
            assert_eq!(output, b"Send reports? [Y/n] ");
            let mut buffer = [0; 32];
            assert_eq!(
                rustix::io::read(&helper, &mut buffer),
                Err(rustix::io::Errno::AGAIN)
            );
        }
    }

    #[tokio::test]
    async fn a_key_typed_before_the_frame_is_ready_never_reaches_the_answer() {
        for raw in [false, true] {
            let (terminal, slave, bridge, mut helper) = open_bridge(raw);
            rustix::io::write(&terminal, b"y").unwrap();
            helper.write_all(b"Use the existing files? [y/N] ").unwrap();
            helper.write_all(READY.as_bytes()).unwrap();
            let (ready, rendered) = oneshot::channel();
            let (started, accepting) = oneshot::channel();
            let mut output = Vec::new();
            let received = {
                let display = bridge.relay(ready, &mut output);
                let input = async {
                    let control = rendered.await.unwrap();
                    started.send(()).unwrap();
                    let (ready, rendered) = oneshot::channel();
                    ready.send(control).unwrap();
                    bridge.forward(rendered).await
                };
                let dialog = async {
                    accepting.await.unwrap();
                    rustix::io::write(&terminal, b"\n").unwrap();
                    let mut buffer = [0; 32];
                    let mut received = Vec::new();
                    loop {
                        match rustix::io::read(&helper, &mut buffer) {
                            Ok(count) => received.extend_from_slice(&buffer[..count]),
                            Err(rustix::io::Errno::AGAIN) => tokio::task::yield_now().await,
                            other => panic!("unexpected helper input: {other:?}"),
                        }
                        if received.ends_with(b"\n") {
                            break received;
                        }
                    }
                };
                tokio::pin!(display, input, dialog);
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    tokio::select! {
                        received = &mut dialog => received,
                        result = &mut display => panic!("the display stopped early: {result:?}"),
                        result = &mut input => panic!("input stopped early: {result:?}"),
                    }
                })
                .await
                .unwrap()
            };
            assert_eq!(received, b"\n", "raw: {raw}");
            assert_eq!(output, b"Use the existing files? [y/N] ");
            let modes = tcgetattr(&slave).unwrap().local_modes;
            assert_eq!(modes.contains(LocalModes::ICANON), !raw);
        }
    }

    #[test]
    fn interaction_policy_keeps_direct_and_plain_modes_separate() {
        assert_eq!(
            Terminal::interaction(false, false, Some(100)),
            Interaction::Direct
        );
        assert_eq!(
            Terminal::interaction(false, true, Some(100)),
            Interaction::Direct
        );
        assert_eq!(
            Terminal::interaction(true, true, Some(100)),
            Interaction::Plain
        );
        assert_eq!(Terminal::interaction(true, false, None), Interaction::Plain);
        assert_eq!(
            Terminal::interaction(true, false, Some(59)),
            Interaction::Plain
        );
        assert_eq!(
            Terminal::interaction(true, false, Some(60)),
            Interaction::Rich
        );
    }
}
