//! The screen as a terminal receives it: `draw` into a backend buffer of a known
//! size, then that buffer read back cell by cell.
//!
//! The unit tests inside the crate check the model (`state`) and the layout maths
//! (`render`). This file checks what those two only imply — that a frame of the
//! real renderer puts the right characters in the right cells, that the pane
//! titles and the status bar are where a person looks for them, and that a theme
//! changes the colours without changing a word. It is also the only screen test
//! CI can run: a test backend is a terminal that needs no terminal.

use agent_core::{AgentEvent, ApprovalRequest, Outcome, Permission, Plan};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use agent_tui::{draw, Header, Kind, Palette, Screen, Theme};

/// A terminal large enough for every pane, which is the size the layout assumes;
/// the sizes below it are tested on purpose.
const COLS: u16 = 110;
const ROWS: u16 = 32;

fn screen() -> Screen {
    Screen::new(Header {
        agent: "smoll test".to_owned(),
        provider: "scripted".to_owned(),
        branch: "tui".to_owned(),
        mode: "approve-edits".to_owned(),
        tools: 24,
        tool_tokens: 2641,
        caution: Some("no model ran: this provider is scripted".to_owned()),
        ..Default::default()
    })
}

/// One `smoll chat` frame, drawn and handed to the backend as a buffer.
fn drawn(screen: &Screen, palette: Palette, cols: u16, rows: u16) -> Buffer {
    let theme = Theme::of(palette);
    let mut terminal = Terminal::new(TestBackend::new(cols, rows)).expect("a test terminal");
    terminal
        .draw(|frame| draw(frame, screen, &theme))
        .expect("a frame draws");
    terminal.backend().buffer().clone()
}

/// The characters the backend was given, one `String` per row, with the blanks a
/// widget left to the right of its text dropped.
fn shown(buffer: &Buffer) -> Vec<String> {
    let cols = buffer.area().width.max(1) as usize;
    buffer
        .content()
        .chunks(cols)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn text(screen: &Screen) -> String {
    shown(&drawn(screen, Palette::Dark, COLS, ROWS)).join("\n")
}

fn colours(buffer: &Buffer) -> Vec<(Color, Color, Modifier)> {
    buffer
        .content()
        .iter()
        .map(|cell| (cell.fg, cell.bg, cell.modifier))
        .collect()
}

#[test]
fn the_status_bar_is_the_first_row_and_names_what_a_run_is_judged_on() {
    let rows = shown(&drawn(&screen(), Palette::Dark, COLS, ROWS));
    let bar = &rows[0];
    for part in [
        "smoll test",
        "provider scripted",
        "branch tui",
        "mode approve-edits",
        "ctx~0/4096",
        "tool idle",
        "elapsed 0s",
        "turns 0",
    ] {
        assert!(bar.contains(part), "{part} is missing from the bar: {bar}");
    }
}

#[test]
fn every_pane_is_titled_where_a_person_can_read_it() {
    let body = text(&screen());
    for title in ["transcript", "tool log", "plan"] {
        assert!(body.contains(title), "no {title} pane on screen:\n{body}");
    }
    assert!(
        body.contains("no model ran"),
        "the caution about a scripted provider belongs on the screen, not in a README:\n{body}"
    );
    assert!(
        body.contains("q quits"),
        "nothing says how to leave:\n{body}"
    );
}

#[test]
fn a_question_shows_what_it_asks_and_every_answer_it_takes() {
    let mut screen = screen();
    screen.note(&AgentEvent::Started {
        task: "add a note to NOTES.md".to_owned(),
        at_ms: 1_000,
    });
    screen.ask(ApprovalRequest::new(
        Permission::Write,
        "write_file asks: change NOTES.md",
        "NOTES.md",
    ));
    let body = text(&screen);
    assert!(
        body.contains("write_file asks: change NOTES.md"),
        "the question itself is not on screen:\n{body}"
    );
    assert!(
        body.contains("approval asked"),
        "the box is unlabelled:\n{body}"
    );
    // The point of drawing the three answers rather than a bare prompt is `[a]`:
    // a person who does not know it exists approves the same file twice.
    for answer in ["[y] yes", "[a] yes for the rest of this run", "[n] no"] {
        assert!(body.contains(answer), "{answer} is clipped:\n{body}");
    }
}

#[test]
fn a_finished_run_leaves_its_own_counts_on_the_bar_and_its_words_in_the_transcript() {
    let mut screen = screen();
    screen.note(&AgentEvent::Started {
        task: "rename the helper".to_owned(),
        at_ms: 1_000,
    });
    screen.note(&AgentEvent::ContextGathered {
        files: vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()],
        tokens: 1_200,
    });
    screen.note(&AgentEvent::Finished {
        outcome: Outcome::Completed {
            summary: "renamed in two files".to_owned(),
            steps_taken: 3,
        },
        at_ms: 7_000,
    });
    let rows = shown(&drawn(&screen, Palette::Dark, COLS, ROWS));
    let bar = &rows[0];
    for part in ["ctx~1200/4096", "elapsed 6s", "turns 1"] {
        assert!(
            bar.contains(part),
            "{part} is missing now the run is done: {bar}"
        );
    }
    let body = rows.join("\n");
    assert!(
        body.contains("renamed in two files"),
        "the outcome line is off screen:\n{body}"
    );
    assert!(
        body.contains("q quits"),
        "the bar never went back to idle:\n{body}"
    );
}

#[test]
fn only_the_newest_rows_of_a_long_run_reach_the_screen() {
    let mut screen = screen();
    for index in 0..120 {
        screen.push(Kind::Task, format!("line {index} of a long run"));
    }
    let body = text(&screen);
    assert!(
        body.contains("line 119 of a long run"),
        "the newest row is the one that says what just happened:\n{body}"
    );
    assert!(
        !body.contains("line 0 of a long run"),
        "the pane scrolled to its oldest row instead:\n{body}"
    );
}

#[test]
fn the_plan_keeps_its_rows_from_the_top_while_the_run_keeps_going() {
    let mut screen = screen();
    screen.note(&AgentEvent::PlanSet {
        plan: Plan::new(
            "rename the helper",
            (1..=6).map(|step| format!("step {step}")).collect(),
        ),
    });
    for index in 0..120 {
        screen.push(Kind::Task, format!("line {index} of a long run"));
    }
    let body = text(&screen);
    // The plan box has a fixed height, so a long run cannot push the last step off
    // the bottom of it the way it pushes the oldest transcript line off the screen.
    for step in ["step 1", "step 4", "step 6"] {
        assert!(
            body.contains(step),
            "{step} fell out of the plan pane:\n{body}"
        );
    }
    assert!(body.contains("line 119 of a long run"), "{body}");
}

#[test]
fn a_theme_changes_the_colours_and_not_one_character_of_the_words() {
    let mut screen = screen();
    screen.note(&AgentEvent::Started {
        task: "rename the helper".to_owned(),
        at_ms: 1_000,
    });
    screen.note(&AgentEvent::ToolRequested {
        name: "fs_read_file".to_owned(),
        arguments: serde_json::json!({"path": "src/lib.rs"}),
    });
    screen.ask(ApprovalRequest::new(
        Permission::Write,
        "write_file asks: change NOTES.md",
        "NOTES.md",
    ));

    let dark = drawn(&screen, Palette::Dark, COLS, ROWS);
    let monokai = drawn(&screen, Palette::Monokai, COLS, ROWS);
    assert_eq!(
        shown(&dark),
        shown(&monokai),
        "a palette must not decide what the screen says"
    );
    assert_ne!(
        colours(&dark),
        colours(&monokai),
        "/theme would otherwise be a key that changes nothing"
    );
}

#[test]
fn a_terminal_too_small_for_every_pane_still_draws_and_still_says_who_answers() {
    let idle = screen();
    let mut asking = screen();
    asking.ask(ApprovalRequest::new(
        Permission::Execute,
        "shell asks: run cargo test --all",
        "cargo test --all",
    ));
    for (cols, rows) in [(20, 8), (40, 12), (60, 16)] {
        for screen in [&idle, &asking] {
            let body = shown(&drawn(screen, Palette::Dark, cols, rows)).join("\n");
            assert!(
                body.contains("smoll test"),
                "at {cols}x{rows} the bar lost the one thing it must always say:\n{body}"
            );
        }
    }
}
