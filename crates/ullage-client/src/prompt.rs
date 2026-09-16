//! Terminal interaction for the commands that have to talk to a person.
//!
//! Everything a `Prompt` writes goes to standard error and only ever reaches an
//! attached terminal, so it never lands in piped or captured output. That is why
//! prompts may show values that [`crate::RunOutput`] would redact.

use std::io::{IsTerminal as _, Write as _};

/// Result of a question that must not echo the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretInput {
    /// The person typed a value.
    Answer(String),
    /// Standard input ended.
    Eof,
    /// The terminal could not turn echo off, so the secret was not read.
    EchoUnavailable,
}

/// A question and answer channel with the person running the command.
pub trait Prompt {
    /// Writes one instruction line for the user.
    fn tell(&mut self, line: &str);

    /// Asks for one line of input. `None` means the input ended.
    fn ask(&mut self, question: &str) -> Option<String>;

    /// Asks for one line of input without echoing it.
    fn ask_secret(&mut self, question: &str) -> SecretInput {
        match self.ask(question) {
            Some(answer) => SecretInput::Answer(answer),
            None => SecretInput::Eof,
        }
    }

    /// Whether both the question and the answer channel are a terminal.
    fn is_interactive(&self) -> bool;
}

/// The real terminal: questions on standard error, answers from standard input.
pub struct TerminalPrompt {
    interactive: bool,
}

impl Default for TerminalPrompt {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalPrompt {
    pub fn new() -> Self {
        Self {
            interactive: std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        }
    }

    fn write(&mut self, text: &str) {
        let mut stderr = std::io::stderr();
        let _ = stderr.write_all(text.as_bytes());
        let _ = stderr.flush();
    }

    fn read_line(&mut self) -> Option<String> {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
        }
    }
}

impl Prompt for TerminalPrompt {
    fn tell(&mut self, line: &str) {
        self.write(&format!("{line}\n"));
    }

    fn ask(&mut self, question: &str) -> Option<String> {
        self.write(&format!("{question}: "));
        self.read_line()
    }

    fn ask_secret(&mut self, question: &str) -> SecretInput {
        let Some(guard) = EchoGuard::disable() else {
            return SecretInput::EchoUnavailable;
        };
        self.write(&format!("{question}: "));
        let answer = self.read_line();
        drop(guard);
        self.write("\n");
        match answer {
            Some(answer) => SecretInput::Answer(answer),
            None => SecretInput::Eof,
        }
    }

    fn is_interactive(&self) -> bool {
        self.interactive
    }
}

/// Turns terminal echo off for as long as it is alive. Restores the previous
/// mode on drop, including when the read fails.
struct EchoGuard {
    #[cfg(unix)]
    previous: Option<libc::termios>,
    #[cfg(windows)]
    previous: Option<u32>,
    #[cfg(not(any(unix, windows)))]
    previous: Option<()>,
}

#[cfg(unix)]
impl EchoGuard {
    fn disable() -> Option<Self> {
        // SAFETY: `tcgetattr` only writes through the pointer we hand it, and
        // `termios` is a plain C struct that is valid fully zeroed.
        let mut current: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd 0 is standard input and `current` is a live local.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut current) } != 0 {
            return None;
        }
        let previous = current;
        current.c_lflag &= !libc::ECHO;
        // SAFETY: same preconditions as the `tcgetattr` call above.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &current) } != 0 {
            return None;
        }
        Some(Self {
            previous: Some(previous),
        })
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            // SAFETY: `previous` is the mode we read from this same descriptor.
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &previous) };
        }
    }
}

#[cfg(windows)]
impl EchoGuard {
    fn disable() -> Option<Self> {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        };

        // SAFETY: `GetStdHandle` takes a constant and returns a borrowed handle.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() {
            return None;
        }
        let mut mode = 0u32;
        // SAFETY: `handle` is a live console handle and `mode` is a live local.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return None;
        }
        // SAFETY: same preconditions as the `GetConsoleMode` call above.
        if unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) } == 0 {
            return None;
        }
        Some(Self {
            previous: Some(mode),
        })
    }
}

#[cfg(windows)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};

        if let Some(previous) = self.previous.take() {
            // SAFETY: `previous` is the mode we read from this same handle.
            unsafe {
                let handle = GetStdHandle(STD_INPUT_HANDLE);
                if !handle.is_null() {
                    SetConsoleMode(handle, previous);
                }
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
impl EchoGuard {
    fn disable() -> Option<Self> {
        None
    }
}

#[cfg(not(any(unix, windows)))]
impl Drop for EchoGuard {
    fn drop(&mut self) {}
}

/// A scripted prompt for tests: answers come from a queue, questions and
/// instructions are recorded.
#[derive(Debug, Default)]
pub struct ScriptedPrompt {
    answers: std::collections::VecDeque<String>,
    transcript: Vec<String>,
    secrets_asked: usize,
    echo_unavailable: bool,
}

impl ScriptedPrompt {
    pub fn new<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            answers: answers.into_iter().map(Into::into).collect(),
            transcript: Vec::new(),
            secrets_asked: 0,
            echo_unavailable: false,
        }
    }

    /// Pretend the terminal cannot hide secret input.
    pub fn refuse_secret_echo(&mut self) {
        self.echo_unavailable = true;
    }

    /// Every line told and every question asked, in order.
    pub fn transcript(&self) -> &[String] {
        &self.transcript
    }

    /// Whether the transcript contains a line with this substring.
    pub fn said(&self, needle: &str) -> bool {
        self.transcript.iter().any(|line| line.contains(needle))
    }

    /// How many answers were never consumed.
    pub fn remaining(&self) -> usize {
        self.answers.len()
    }

    /// How many questions were asked without echo.
    pub fn secrets_asked(&self) -> usize {
        self.secrets_asked
    }
}

impl Prompt for ScriptedPrompt {
    fn tell(&mut self, line: &str) {
        self.transcript.push(line.to_owned());
    }

    fn ask(&mut self, question: &str) -> Option<String> {
        self.transcript.push(question.to_owned());
        self.answers.pop_front()
    }

    fn ask_secret(&mut self, question: &str) -> SecretInput {
        self.secrets_asked += 1;
        if self.echo_unavailable {
            self.transcript.push(question.to_owned());
            return SecretInput::EchoUnavailable;
        }
        match self.ask(question) {
            Some(answer) => SecretInput::Answer(answer),
            None => SecretInput::Eof,
        }
    }

    fn is_interactive(&self) -> bool {
        true
    }
}
