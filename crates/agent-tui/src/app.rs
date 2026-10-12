//! The screen's own loop: keys in, commands out, one run on another task.
//!
//! This module is the only one in the crate that touches a real terminal, and the
//! only one that starts a run. It is deliberately thin: the events become a
//! [`Screen`] through [`crate::state`], the [`Screen`] becomes a picture through
//! [`crate::render`], and what a key means is decided by [`press`], which takes no
//! terminal at all and is therefore tested without one.

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, oneshot};

use agent_core::{Agent, AgentError, AgentEvent, ApprovalDecision, Budget, Run, StoredSession};
use smollm_core::chat::CancelToken;

use crate::hub::Msg;
use crate::render;
use crate::state::{Header, Kind, Phase, Screen};
use crate::theme::{Palette, Theme};

/// The screen the app draws into, once the terminal has been taken over.
type Surface = ratatui::Terminal<CrosstermBackend<Stdout>>;

/// How often the screen wakes up to move its own clock.
///
/// A tick rather than a redraw-on-event rate: a run against a local model spends
/// most of its time waiting, and the one number a person watches while it does is
/// the elapsed seconds in the status bar.
const BEAT: Duration = Duration::from_millis(60);

/// What a key means, once [`press`] has decided.
///
/// The effect is returned rather than carried out because the things it asks for
/// belong to different owners: the run lives on the worker's task, the palette
/// lives in the draw loop, and the answer channels live in the app's hands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Do nothing at all.
    None,
    /// Start a task on the loop.
    Start(String),
    /// Put the last recorded run's files back.
    Rollback,
    /// Say this on screen, in the transcript.
    Notice(String),
    /// Switch palette.
    Theme(Palette),
    /// Empty the panes, keeping what the screen said about itself.
    Clear,
    /// Stop the run that is going.
    Stop,
    /// Answer the question the loop is waiting on.
    Answer(ApprovalDecision),
    /// Leave the screen.
    Quit,
}

/// What the screen did and what it cost, once the person has left it.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// How many tasks were run.
    pub turns: usize,
    /// How many of them ended in something other than success.
    pub failures: usize,
    /// The line to print after the terminal has gone back: where the last record
    /// is, and what the last run said.
    pub message: String,
}

/// Why the screen could not be shown.
#[derive(Debug)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Self(format!("the terminal could not be set up: {source}"))
    }
}

impl Error {
    fn said(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Whether a full-screen run makes sense here.
///
/// A pipe and a CI job have no screen to draw on; `smoll chat` uses this to say so
/// with an exit code rather than to write escape sequences into someone's file.
pub fn has_a_screen() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

/// Take over the terminal, run the screen, and hand the terminal back.
///
/// `agent` arrives already carrying the [`Hub`] as its observer and approver — the
/// caller wires that, because the caller is also the thing that has to build the
/// provider. `feed` is the hub's receiver, taken before the hub was shared.
/// `first` is the task to start with, so `smoll chat "…"` is the same screen with
/// one keypress saved.
pub async fn run(
    agent: Agent,
    mut feed: mpsc::UnboundedReceiver<Msg>,
    cwd: PathBuf,
    header: Header,
    first: Option<String>,
) -> Result<Report, Error> {
    if !has_a_screen() {
        return Err(Error::said(
            "there is no terminal to draw on here; `smoll task …` gives the same run as printed \
             output, and `smoll task --json` gives it as one document.",
        ));
    }
    let mut guard = TerminalGuard::enter()?;
    let (tx, mut keys) = mpsc::unbounded_channel::<Key>();
    let reader = tokio::task::spawn_blocking(move || read_keys(tx));

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Done>();
    let worker = tokio::spawn(work(agent, cmd_rx, done_tx, cwd));

    let mut app = App::new(header);
    if let Some(task) = first {
        app.submit(&cmd_tx, task);
    }

    let outcome = loop {
        if app.quit {
            break Ok(app.report());
        }
        tokio::select! {
            msg = feed.recv() => match msg {
                Some(msg) => app.receive(msg),
                // The hub went with the run: nobody is left to report anything.
                None => break Ok(app.report()),
            },
            done = done_rx.recv() => match done {
                Some(done) => app.finish(done),
                None => break Ok(app.report()),
            },
            key = keys.recv() => match key {
                Some(Key::Event(event)) => app.press_key(&cmd_tx, event),
                Some(Key::Tick) => app.screen.tick(),
                None => break Ok(app.report()),
            },
        }
        if let Err(error) = app.draw(guard.surface()) {
            break Err(error);
        }
    };

    // Whatever happened, the questions the loop was waiting on get answered and the
    // run is told to stop. Silence is not consent on the way out either.
    app.shutdown();
    drop(cmd_tx);
    drop(guard);
    // The receiver goes before the waiter: the reader thread keeps sending itself a
    // tick every beat, so it only ends once there is nothing to send to. Awaiting it
    // while `keys` is still alive would wait forever.
    drop(keys);
    let _ = reader.await;
    worker.abort();
    outcome
}

/// The screen, the palette it is drawn in, and the handles a key needs to pull.
struct App {
    screen: Screen,
    theme: Theme,
    /// The answer channels the loop is blocked on, oldest first. One run asks one
    /// question at a time, so this is a queue of one in practice; it is a queue so
    /// that a key can answer every open channel rather than lose one.
    open: Vec<oneshot::Sender<ApprovalDecision>>,
    cancel: Option<CancelToken>,
    quit: bool,
    failures: usize,
    message: String,
}

impl App {
    fn new(header: Header) -> Self {
        let palette = header.palette;
        Self {
            screen: Screen::new(header),
            theme: Theme::of(palette),
            open: Vec::new(),
            cancel: None,
            quit: false,
            failures: 0,
            message: String::new(),
        }
    }

    fn draw(&self, surface: &mut Surface) -> Result<(), Error> {
        surface
            .draw(|frame| render::draw(frame, &self.screen, &self.theme))
            .map_err(Error::from)
            .map(|_| ())
    }

    fn receive(&mut self, msg: Msg) {
        match msg {
            Msg::Event(event) => {
                if matches!(event, AgentEvent::Finished { .. }) {
                    self.cancel = None;
                }
                self.screen.note(&event);
            }
            Msg::Approval { request, reply } => {
                // A question about a run that has already been stopped from the
                // screen is answered before it is shown. The loop can still be
                // inside a model call when Esc is pressed, so a question can
                // arrive after the stop; holding the screen for an answer to a
                // run that is on its way out would leave a person pressing keys
                // with nothing left to press them for.
                if self
                    .cancel
                    .as_ref()
                    .is_some_and(|cancel| cancel.is_cancelled())
                {
                    let _ = reply.send(ApprovalDecision::Rejected {
                        reason: "the run was stopped from the screen before this question was \
                                asked"
                            .to_owned(),
                    });
                    self.screen.push(
                        Kind::Muted,
                        format!(
                            "refused {} because the run was already stopped",
                            request.summary
                        ),
                    );
                } else {
                    self.open.push(reply);
                    self.screen.ask(request);
                }
            }
        }
    }

    fn finish(&mut self, done: Done) {
        match done {
            Done::Turn { message, failed } => {
                self.message = message.clone();
                if failed {
                    self.failures += 1;
                }
                self.cancel = None;
                self.screen.turn_done(message, failed);
            }
            Done::Notice { message, failed } => self
                .screen
                .push(if failed { Kind::Trouble } else { Kind::Muted }, message),
        }
    }

    /// One key, one meaning, one action.
    fn press_key(&mut self, tx: &mpsc::UnboundedSender<Cmd>, event: Event) {
        let key = match event {
            Event::Key(key) => key,
            // Resize is taken care of by the next draw, which reads the terminal's
            // own size; mouse and focus events change nothing here.
            _ => return,
        };
        let effect = press(&mut self.screen, &key);
        self.apply(tx, effect);
    }

    fn apply(&mut self, tx: &mpsc::UnboundedSender<Cmd>, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Start(task) => self.submit(tx, task),
            Effect::Rollback => {
                let _ = tx.send(Cmd::Rollback);
            }
            Effect::Notice(message) => self.screen.push(Kind::Muted, message),
            Effect::Theme(palette) => {
                self.screen.palette = palette;
                self.theme = Theme::of(palette);
            }
            Effect::Clear => self.screen.clear(),
            Effect::Stop => match &self.cancel {
                Some(cancel) => {
                    cancel.cancel();
                    self.screen.push(
                        Kind::Warning,
                        "stopping: the run ends at its next check of the token",
                    );
                }
                // Esc with nothing to stop has already been answered with a notice
                // by `press`; this is the case where the run ended in between.
                None => self.screen.push(
                    Kind::Muted,
                    "nothing is running, so there is nothing to stop",
                ),
            },
            Effect::Answer(decision) => {
                let open = std::mem::take(&mut self.open);
                for reply in open {
                    // A closed channel means the loop already gave up on the
                    // question, which is the same refusal this answered with.
                    let _ = reply.send(decision.clone());
                }
            }
            Effect::Quit => self.quit = true,
        }
    }

    fn submit(&mut self, tx: &mpsc::UnboundedSender<Cmd>, task: String) {
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.screen.running();
        let _ = tx.send(Cmd::Task { task, cancel });
    }

    fn report(&self) -> Report {
        Report {
            turns: self.screen.status.turns,
            failures: self.failures,
            message: if self.message.is_empty() {
                match self.screen.status.turns {
                    0 => "no task was run".to_owned(),
                    turns => format!("{turns} task(s) run; records in .smoll/sessions"),
                }
            } else {
                self.message.clone()
            },
        }
    }

    /// The last thing the screen does with the handles it holds.
    fn shutdown(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        let open = std::mem::take(&mut self.open);
        for reply in open {
            let _ = reply.send(ApprovalDecision::Rejected {
                reason: "the screen closed while the question was on it".to_owned(),
            });
        }
    }
}

/// What a key means, decided without a terminal, a run, or a channel in sight.
///
/// Every branch of this is a test, because the difference between "Esc means no"
/// and "Esc means consent" is decided here and nowhere else in the crate.
pub fn press(screen: &mut Screen, key: &KeyEvent) -> Effect {
    let modifiers = key.modifiers;
    if modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Effect::Quit,
            KeyCode::Char('u') => {
                screen.input.clear();
                Effect::None
            }
            _ => Effect::None,
        };
    }

    // A question the loop is waiting on owns the keyboard: y and a answer it, n and
    // Esc refuse it, and nothing else is taken. Typing a task while a run is
    // waiting to write a file would be a strange thing to do with the only keys
    // that have an answer in them.
    if screen.pending.is_some() {
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => ApprovalDecision::Approved,
            KeyCode::Char('a') | KeyCode::Char('A') => ApprovalDecision::ApprovedForRun,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ApprovalDecision::Rejected {
                reason: match key.code {
                    KeyCode::Esc => "the question was dismissed with Esc, which is a no".to_owned(),
                    _ => "the person reading it said no".to_owned(),
                },
            },
            _ => return Effect::None,
        };
        screen.answered(&decision);
        return Effect::Answer(decision);
    }

    match key.code {
        KeyCode::Esc if screen.phase == Phase::Running => Effect::Stop,
        KeyCode::Esc => Effect::Notice(
            "nothing is running and no question is open; Enter starts a task and q quits"
                .to_owned(),
        ),
        KeyCode::Char('q') if screen.phase == Phase::Idle && screen.input.is_empty() => {
            Effect::Quit
        }
        KeyCode::Char('q') if screen.phase == Phase::Running => Effect::Notice(
            "a run is going: Esc stops it, and q quits when the screen is idle".to_owned(),
        ),
        KeyCode::Enter => submit(screen),
        KeyCode::Backspace => {
            screen.input.pop();
            Effect::None
        }
        KeyCode::Char(character) if !modifiers.contains(KeyModifiers::ALT) => {
            screen.input.push(character);
            Effect::None
        }
        _ => Effect::None,
    }
}

/// Enter, which either runs a command or starts a task.
fn submit(screen: &mut Screen) -> Effect {
    let text = screen.input.trim().to_owned();
    screen.input.clear();
    if text.is_empty() {
        return Effect::None;
    }
    if screen.phase == Phase::Running {
        return Effect::Notice(
            "a run is going, and a second task would be a second run at the same files; stop it \
             with Esc first"
                .to_owned(),
        );
    }
    let Some(command) = text.strip_prefix('/') else {
        return Effect::Start(text);
    };
    let (name, argument) = match command.split_once(' ') {
        Some((name, argument)) => (name, argument.trim()),
        None => (command, ""),
    };
    match name {
        "help" => Effect::Notice(
            "/theme [name] lists or sets the palette · /rollback puts the last run's files back \
             · /clear empties the panes · /tools prices the tool list · /quit leaves the screen\n\
             Enter starts a task · y / a / n answer a question · Esc stops a run or refuses a \
             question · q quits when idle · Ctrl+C leaves at any time"
                .to_owned(),
        ),
        "quit" | "exit" => Effect::Quit,
        "clear" => Effect::Clear,
        "rollback" => Effect::Rollback,
        "tools" => Effect::Notice(format!(
            "{} tools offered, costing about {} tokens of every step's prompt; `smoll tools` \
             prices each one and `[tools.<name>] enabled = false` is what switches one off",
            screen.header.tools, screen.header.tool_tokens
        )),
        "theme" => {
            if argument.is_empty() {
                return Effect::Notice(format!(
                    "themes: {} — this screen is drawing in {}",
                    Palette::names().join(", "),
                    screen.palette.name()
                ));
            }
            match Palette::parse(argument) {
                Some(palette) if palette == screen.palette => {
                    Effect::Notice(format!("{} is already the palette", palette.name()))
                }
                Some(palette) => Effect::Theme(palette),
                None => Effect::Notice(format!(
                    "there is no theme {argument:?}; this screen has {}",
                    Palette::names().join(", ")
                )),
            }
        }
        other => Effect::Notice(format!(
            "there is no /{other} command; /help lists the ones there are"
        )),
    }
}

/// What the task-owning side can be asked to do.
enum Cmd {
    Task { task: String, cancel: CancelToken },
    Rollback,
}

/// What it says back. A run's events come through the hub; this is what only the
/// worker knows: where the record went, and what a rollback actually put back.
enum Done {
    Turn { message: String, failed: bool },
    Notice { message: String, failed: bool },
}

/// The other task: one agent, one run at a time, one record per run.
///
/// The agent is held here because a run borrows it and the next task in the same
/// chat needs the same limits, registry and policy. The record is written whether
/// or not the run finished — a run stopped by its step budget may already have
/// written files, and this file is the only thing that knows what they contained.
async fn work(
    mut agent: Agent,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    done: mpsc::UnboundedSender<Done>,
    cwd: PathBuf,
) {
    let mut last: Option<StoredSession> = None;
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            Cmd::Task { task, cancel } => {
                // A fresh handle per task: a token once flipped stays flipped, and
                // the next task is not the one that was stopped.
                agent = agent.with_cancel(cancel);
                let Run { session, error } = agent.run(&task).await;
                let stored = StoredSession::new(session, agent.context().snapshots());
                let record = stored.write(&cwd);
                let failed = error.is_some()
                    || stored
                        .session
                        .finished()
                        .map_or(true, |outcome| !outcome.succeeded());
                let message = turn_message(&error, &record, &stored);
                let _ = done.send(Done::Turn { message, failed });
                last = Some(stored);
            }
            Cmd::Rollback => {
                let (message, failed) = match last.as_mut() {
                    None => (
                        "no run has been made in this screen yet, so there is nothing to put back"
                            .to_owned(),
                        true,
                    ),
                    Some(stored) if stored.journal.is_empty() => (
                        format!(
                            "run {} wrote no files, so there is nothing to put back",
                            stored.session.id
                        ),
                        false,
                    ),
                    Some(stored) => restored(stored, &cwd),
                };
                let _ = done.send(Done::Notice { message, failed });
            }
        }
    }
}

/// One rollback, in the words the screen shows.
fn restored(stored: &mut StoredSession, cwd: &Path) -> (String, bool) {
    let taken = stored.restore(cwd);
    let files: Vec<PathBuf> = taken
        .iter()
        .flat_map(|entry| entry.paths.iter())
        .cloned()
        .collect();
    let left_alone = taken
        .iter()
        .flat_map(|entry| entry.left_alone.iter())
        .count();
    stored.session.record(AgentEvent::RolledBack {
        files: files.clone(),
    });
    stored.stored_at_ms = agent_core::now_ms();
    let unwritten = match stored.write(cwd) {
        Ok(_) => None,
        Err(source) => Some(format!("the record could not be rewritten: {source}")),
    };
    let left = if left_alone == 0 {
        String::new()
    } else {
        format!(", {left_alone} left alone because they changed after the run")
    };
    let message = match unwritten {
        None => format!(
            "put back {} file(s) from run {}{left}",
            files.len(),
            stored.session.id
        ),
        Some(source) => format!(
            "put back {} file(s) from run {}{left} — and {source}",
            files.len(),
            stored.session.id
        ),
    };
    (message, false)
}

/// The line a turn ends on: where its record is, and what to do about the two
/// stops a person can act on.
fn turn_message(
    error: &Option<AgentError>,
    record: &Result<PathBuf, AgentError>,
    stored: &StoredSession,
) -> String {
    let path = match record {
        Ok(path) => format!("record {}", path.display()),
        Err(source) => format!("the record could not be written: {source}"),
    };
    let outcome = stored
        .session
        .finished()
        .map(|outcome| outcome.summary())
        .unwrap_or_else(|| "no outcome was recorded".to_owned());
    match error {
        None => format!("{outcome} · {path}"),
        Some(error) => {
            let hint = match error {
                AgentError::BudgetExhausted(Budget::Tokens { .. }) => {
                    " — the tool list is the part a person can trade away: /tools prices it, and \
                     `[tools.<name>] enabled = false` in smoll.toml buys room"
                        .to_owned()
                }
                AgentError::Cancelled => " (stopped from the screen)".to_owned(),
                _ => String::new(),
            };
            format!("stopped: {error}{hint} · {path}")
        }
    }
}

/// A key, or the beat that keeps the clock moving.
enum Key {
    Event(Event),
    Tick,
}

/// The keyboard, on a thread of its own.
///
/// `crossterm::event::read` blocks, and a screen that waits for a key cannot draw
/// while it waits. So the keys come over a channel and the poll timeout is the
/// tick: one loop, no timer task, and the thread ends by itself when the screen
/// stops reading.
fn read_keys(tx: mpsc::UnboundedSender<Key>) {
    loop {
        match event::poll(BEAT) {
            Ok(true) => match event::read() {
                Ok(event) => {
                    if tx.send(Key::Event(event)).is_err() {
                        return;
                    }
                }
                // A read error means the terminal is gone rather than that a key
                // was pressed wrongly; the screen simply stops hearing about keys.
                Err(_) => return,
            },
            Ok(false) => {
                if tx.send(Key::Tick).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Set once the terminal has been taken over, so a panic hook knows there is
/// something to put back.
static TAKEN: AtomicBool = AtomicBool::new(false);

/// The terminal, and the promise that it comes back.
///
/// Held in a guard rather than set up and torn down inline because there are three
/// ways out of the screen — a clean quit, a draw that failed, and a panic in a
/// widget — and a shell left in raw mode is a worse outcome than any run this
/// screen could show.
struct TerminalGuard {
    surface: Surface,
}

impl TerminalGuard {
    fn enter() -> Result<Self, Error> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        TAKEN.store(true, Ordering::SeqCst);
        guard_against_a_panic();
        Ok(Self {
            surface: ratatui::Terminal::new(CrosstermBackend::new(stdout))?,
        })
    }

    fn surface(&mut self) -> &mut Surface {
        &mut self.surface
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Give up raw mode and the alternate screen, once, however it is reached.
///
/// Every call is idempotent by intent: turning raw mode off when it is already off
/// and leaving a screen the process never entered are both things the terminal
/// agrees to do without complaint, and this runs from a `Drop`, from an error path
/// and from a panic hook.
fn restore() {
    if !TAKEN.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, Show);
}

/// Put the terminal back before a panic message reaches the user.
///
/// Without this, a panic in a widget leaves a screen full of escape codes and a
/// shell that will not echo, and the person reading it has no way to know the run
/// is over.
fn guard_against_a_panic() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use agent_core::{ApprovalRequest, Permission};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)
    }

    fn typed(text: &str) -> Vec<KeyEvent> {
        text.chars().map(|c| key(KeyCode::Char(c))).collect()
    }

    fn question() -> ApprovalRequest {
        ApprovalRequest::new(Permission::Write, "write NOTES.md", "+a line")
    }

    /// One screen with one question open, which is the state every approval key
    /// is judged in.
    fn asking() -> Screen {
        let mut screen = Screen::new(Header::default());
        screen.ask(question());
        screen
    }

    #[test]
    fn y_says_yes_and_closes_the_question() {
        let mut screen = asking();
        assert_eq!(
            press(&mut screen, &key(KeyCode::Char('y'))),
            Effect::Answer(ApprovalDecision::Approved)
        );
        assert!(
            screen.pending.is_none(),
            "the screen stops asking once it has answered"
        );
    }

    #[test]
    fn a_says_yes_for_the_rest_of_the_run() {
        let mut screen = asking();
        assert_eq!(
            press(&mut screen, &key(KeyCode::Char('a'))),
            Effect::Answer(ApprovalDecision::ApprovedForRun)
        );
    }

    #[test]
    fn n_and_esc_are_both_no_and_say_so_differently() {
        for (code, wanted) in [
            (KeyCode::Char('n'), "said no"),
            (KeyCode::Esc, "dismissed with Esc"),
        ] {
            let mut screen = asking();
            let Effect::Answer(ApprovalDecision::Rejected { reason }) =
                press(&mut screen, &key(code))
            else {
                panic!("{code:?} on a question is a refusal, nothing else");
            };
            assert!(reason.contains(wanted), "{reason}");
            assert!(screen.pending.is_none());
        }
    }

    #[test]
    fn a_question_owns_the_keyboard_so_typing_a_task_is_not_a_task() {
        // The alternative is a person typing into a screen whose run is waiting to
        // write a file, and a task that starts on top of it.
        let mut screen = asking();
        for event in typed("rm -rf /") {
            assert_eq!(press(&mut screen, &event), Effect::None);
        }
        assert_eq!(screen.input, "");
        assert!(
            screen.pending.is_some(),
            "the question is still the question"
        );
    }

    #[test]
    fn ctrl_c_leaves_even_with_a_question_open() {
        // Quitting is not consent: `App::shutdown` answers the open channel no, and
        // this test is the half that says the key is taken at all.
        let mut screen = asking();
        assert_eq!(press(&mut screen, &ctrl('c')), Effect::Quit);
        assert!(screen.pending.is_some(), "quitting does not answer it");
    }

    #[test]
    fn q_quits_only_when_the_screen_is_idle_and_empty() {
        let mut screen = Screen::new(Header::default());
        assert_eq!(press(&mut screen, &key(KeyCode::Char('q'))), Effect::Quit);

        let mut screen = Screen::new(Header::default());
        screen.running();
        let Effect::Notice(message) = press(&mut screen, &key(KeyCode::Char('q'))) else {
            panic!("q in the middle of a run must not leave the record unwritten");
        };
        assert!(message.contains("Esc stops it"), "{message}");

        let mut screen = Screen::new(Header::default());
        for event in typed("nope") {
            press(&mut screen, &event);
        }
        assert_eq!(
            press(&mut screen, &key(KeyCode::Char('q'))),
            Effect::None,
            "q is a letter while a task is being typed"
        );
        assert_eq!(screen.input, "nopeq");
    }

    #[test]
    fn enter_starts_a_task_or_runs_a_command() {
        let cases: Vec<(&str, Effect)> = vec![
            (
                "rename the helper",
                Effect::Start("rename the helper".to_owned()),
            ),
            (
                "/theme monokai",
                Effect::Theme(Palette::parse("monokai").expect("a known theme")),
            ),
            ("/clear", Effect::Clear),
            ("/rollback", Effect::Rollback),
            ("/quit", Effect::Quit),
        ];
        for (text, wanted) in cases {
            let mut screen = Screen::new(Header::default());
            for event in typed(text) {
                press(&mut screen, &event);
            }
            assert_eq!(press(&mut screen, &key(KeyCode::Enter)), wanted, "{text}");
            assert_eq!(screen.input, "", "{text} was taken out of the composer");
        }
    }

    #[test]
    fn a_command_that_does_not_exist_is_said_so_rather_than_run_as_a_task() {
        // Sending it to the model would be worse than refusing it: the task would
        // become "/mode approve-commands", and a small model would try to do it.
        for text in ["/mode autonomous-safe", "/nope"] {
            let mut screen = Screen::new(Header::default());
            for event in typed(text) {
                press(&mut screen, &event);
            }
            let Effect::Notice(message) = press(&mut screen, &key(KeyCode::Enter)) else {
                panic!("{text} is not a task");
            };
            assert!(message.contains("there is no /"), "{message}");
        }
    }

    #[test]
    fn a_second_task_while_one_is_running_is_refused() {
        let mut screen = Screen::new(Header::default());
        screen.running();
        for event in typed("and another thing") {
            press(&mut screen, &event);
        }
        let Effect::Notice(message) = press(&mut screen, &key(KeyCode::Enter)) else {
            panic!("two runs at one workspace is how files get lost");
        };
        assert!(message.contains("a run is going"), "{message}");
    }

    #[test]
    fn esc_with_a_question_open_is_not_also_a_stop() {
        // The pending branch is taken first, so the run keeps going and only the
        // one proposal is refused. A key that did both would stop a run its person
        // meant to carry on with.
        let mut screen = asking();
        assert!(matches!(
            press(&mut screen, &key(KeyCode::Esc)),
            Effect::Answer(ApprovalDecision::Rejected { .. })
        ));
        assert_eq!(
            screen.phase,
            Phase::Idle,
            "the phase is set by running(), not by this"
        );
    }

    #[tokio::test]
    async fn a_question_arriving_after_a_stop_is_answered_before_it_is_shown() {
        let mut app = App::new(Header::default());
        let cancel = CancelToken::new();
        app.cancel = Some(cancel.clone());
        app.screen.running();
        cancel.cancel();
        let (reply, answer) = oneshot::channel();
        app.receive(Msg::Approval {
            request: question(),
            reply,
        });
        match answer.await.expect("the screen answered it") {
            ApprovalDecision::Rejected { reason } => {
                assert!(reason.contains("stopped from the screen"), "{reason}");
            }
            other => panic!("a run already told to stop does not get a yes, got {other:?}"),
        }
        assert!(
            app.screen.pending.is_none(),
            "the question was never put on screen to be waited on"
        );
        assert!(app.open.is_empty());
    }

    #[tokio::test]
    async fn a_question_on_a_running_run_is_shown_and_its_key_answers_it() {
        let mut app = App::new(Header::default());
        app.screen.running();
        let (reply, answer) = oneshot::channel();
        app.receive(Msg::Approval {
            request: question(),
            reply,
        });
        assert!(app.screen.pending.is_some(), "the screen is showing it");
        let (tx, mut cmds) = mpsc::unbounded_channel();
        app.press_key(&tx, Event::Key(key(KeyCode::Char('y'))));
        assert_eq!(answer.await.expect("answered"), ApprovalDecision::Approved);
        assert!(
            cmds.try_recv().is_err(),
            "answering a question sends no command"
        );
    }

    #[tokio::test]
    async fn leaving_the_screen_refuses_every_question_it_still_holds() {
        let mut app = App::new(Header::default());
        let (reply, answer) = oneshot::channel();
        app.receive(Msg::Approval {
            request: question(),
            reply,
        });
        app.shutdown();
        match answer.await.expect("shut down with it") {
            ApprovalDecision::Rejected { reason } => {
                assert!(reason.contains("screen closed"), "{reason}");
            }
            other => panic!("an unanswered question is not a yes, got {other:?}"),
        }
    }

    #[test]
    fn stopping_a_run_says_which_token_it_told_to_stop() {
        let mut app = App::new(Header::default());
        let cancel = CancelToken::new();
        app.cancel = Some(cancel.clone());
        app.screen.running();
        let (tx, mut cmds) = mpsc::unbounded_channel();
        app.press_key(&tx, Event::Key(key(KeyCode::Esc)));
        assert!(cancel.is_cancelled());
        assert!(cmds.try_recv().is_err(), "Esc sends no command");
        let last = app.screen.transcript.last().expect("the notice");
        assert!(last.text.contains("next check of the token"), "{last:?}");
    }

    #[test]
    fn ctrl_u_clears_the_line_being_typed() {
        let mut screen = Screen::new(Header::default());
        for event in typed("a long wrong task") {
            press(&mut screen, &event);
        }
        assert_eq!(press(&mut screen, &ctrl('u')), Effect::None);
        assert_eq!(screen.input, "");
    }
}
