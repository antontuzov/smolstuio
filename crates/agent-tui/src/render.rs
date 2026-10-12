//! The one place that knows what a rectangle is.
//!
//! [`draw`] takes the model from [`crate::state`] and puts it on a frame. It holds
//! no state of its own: nothing here decides what a run did, only where the rows
//! that say so land. That split is what makes a render test meaningful — the test
//! builds a `Screen` from events, draws it into a buffer of known size, and reads
//! back the characters, so the assertion is about the screen a person would see
//! rather than about a struct they cannot look at.
//!
//! Two decisions here are worth their weight in scrollback:
//!
//! - Panes show their **newest** rows. A transcript that starts at the top of a
//!   long run is one whose last line is off screen, and the last line is the one
//!   that says what happened. The plan is the exception: it is read from the top.
//! - The newest row must be **whole**. So a pane is fitted by visual rows after
//!   wrapping, not by source lines: a paragraph that needs three rows and has two
//!   is dropped rather than cut in half at the bottom of the screen.
//!
//! ratatui 0.29 is the version this workspace can build on its 1.77 floor, and it
//! is written against that line's shape of the API: a `Frame` that is not generic
//! over a backend (so the same `draw` serves a real terminal and a test buffer),
//! and a layout that hands back a list of rectangles rather than an array.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line as FrameLine, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{Header, Kind, Line, Phase, Screen};
use crate::theme::Theme;

/// The two fixed rows: the plan's box in the right column, the log's in the left.
const PLAN_ROWS: u16 = 9;
const LOG_ROWS: u16 = 7;

/// Which end of a pane a person needs to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Newest,
    Oldest,
}

use Edge::{Newest, Oldest};

/// One row to be laid out: its text, what it means, and the mark in front of it.
pub struct Row<'a> {
    pub text: &'a str,
    pub style: Style,
    pub prefix: &'static str,
}

/// Draw the whole screen into `frame`.
pub fn draw(frame: &mut Frame, screen: &Screen, theme: &Theme) {
    let asking = screen.pending.is_some();
    // Four rows, not three, when a question is up: the box has two borders and two
    // lines of content, and the second line is the one that says what `[a]` does.
    // A strip that clips it leaves a person approving on a guess.
    let strips = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(if asking { 4 } else { 0 }),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());
    let (bar, body, question, composer, hints) =
        (strips[0], strips[1], strips[2], strips[3], strips[4]);

    let halves = Layout::horizontal([Constraint::Ratio(3, 5), Constraint::Ratio(2, 5)]).split(body);
    let (left, right) = (halves[0], halves[1]);
    let columns = Layout::vertical([Constraint::Min(4), Constraint::Length(LOG_ROWS)]).split(left);
    let (transcript, log) = (columns[0], columns[1]);
    let columns =
        Layout::vertical([Constraint::Length(PLAN_ROWS), Constraint::Min(4)]).split(right);
    let (plan, detail) = (columns[0], columns[1]);

    let (title, rows) = screen.detail();
    let rows = if asking {
        plain_rows(&rows, theme)
    } else {
        diff_rows(&rows, theme)
    };
    let plan_rows = screen.plan_lines();

    draw_pane(
        frame,
        transcript,
        &transcript_rows(&screen.transcript, theme),
        "transcript",
        theme,
        Newest,
    );
    draw_pane(
        frame,
        log,
        &log_rows(&screen.log, theme),
        "tool log",
        theme,
        Newest,
    );
    draw_pane(
        frame,
        plan,
        &plan_rows_at(&plan_rows, theme),
        "plan",
        theme,
        Oldest,
    );
    draw_pane(frame, detail, &rows, title, theme, Newest);

    draw_line(frame, bar, &status_spans(screen, theme));
    if asking {
        question_bar(frame, screen, theme, question);
    }
    draw_line(frame, composer, &composer_line(screen, theme));
    let hint = match screen.phase {
        Phase::Running => "/help · /theme <name> · /rollback · /clear · /quit — Esc stops the run",
        Phase::Idle => "/help · /theme <name> · /rollback · /clear · /quit — q quits",
    };
    draw_line(
        frame,
        hints,
        &FrameLine::from(Span::styled(hint, theme.muted)),
    );
}

/// How many wrapped rows a row takes in `width` columns, prefix included.
fn cost(row: &Row, width: usize) -> usize {
    rows_for(row.text, width.saturating_sub(row.prefix.chars().count()))
}

/// How many wrapped rows a piece of text takes in `width` columns.
///
/// Greedy, like the widget that will lay it out: this count decides which rows the
/// pane can afford, and under-counting would cut the newest line off the bottom.
pub fn rows_for(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut rows = 1usize;
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let len = word.chars().count();
        if used == 0 {
            used = len;
        } else if used + 1 + len <= width {
            used += 1 + len;
        } else {
            rows += 1;
            used = len;
        }
        // A word longer than the row breaks mid-word, which is what wrapping does.
        while used > width {
            rows += 1;
            used = used.saturating_sub(width);
        }
    }
    rows
}

/// The rows that fit, whole, taken from the end asked for.
pub fn fit(rows: &[Row<'_>], width: usize, height: usize, edge: Edge) -> Vec<FrameLine<'static>> {
    let order: Vec<usize> = match edge {
        Newest => (0..rows.len()).rev().collect(),
        Oldest => (0..rows.len()).collect(),
    };
    let mut taken: Vec<&Row> = Vec::new();
    let mut used = 0usize;
    for index in order {
        let row = &rows[index];
        let need = cost(row, width);
        if used + need > height && !taken.is_empty() {
            break;
        }
        used += need;
        taken.push(row);
        if used >= height {
            break;
        }
    }
    if edge == Newest {
        taken.reverse();
    }
    taken
        .into_iter()
        .map(|row| {
            FrameLine::from(vec![
                Span::styled(row.prefix, row.style),
                Span::styled(row.text.to_owned(), row.style),
            ])
        })
        .collect()
}

/// Draw one bordered pane, fitted to what the terminal can show.
///
/// A pane too narrow or too short for a border is not drawn at all: ratatui will
/// paint one row of a box over the top of someone's transcript, and a screen
/// resized to a sliver is better off showing the strips that still mean something.
pub fn draw_pane(
    frame: &mut Frame,
    area: Rect,
    rows: &[Row<'_>],
    title: &str,
    theme: &Theme,
    edge: Edge,
) {
    if area.width < 6 || area.height < 3 {
        return;
    }
    let width = (area.width - 2) as usize;
    let height = (area.height - 2) as usize;
    frame.render_widget(
        Paragraph::new(fit(rows, width, height, edge))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.border)
                    .title(Span::styled(format!(" {title} "), theme.accent)),
            ),
        area,
    );
}

/// Draw one row across a whole strip: the status bar, the composer, the key hints.
/// No border, because these are chrome rather than content.
pub fn draw_line(frame: &mut Frame, area: Rect, line: &FrameLine) {
    if area.height < 1 || area.width < 1 {
        return;
    }
    frame.render_widget(Paragraph::new(vec![line.clone()]), area);
}

fn transcript_rows<'a>(lines: &'a [Line], theme: &Theme) -> Vec<Row<'a>> {
    lines
        .iter()
        .map(|line| Row {
            text: &line.text,
            style: style_for(line.kind, theme),
            prefix: line.prefix(),
        })
        .collect()
}

/// The log's rows carry their own aligned column, so they get no voice prefix.
fn log_rows<'a>(lines: &'a [Line], theme: &Theme) -> Vec<Row<'a>> {
    lines
        .iter()
        .map(|line| Row {
            text: &line.text,
            style: style_for(line.kind, theme),
            prefix: "",
        })
        .collect()
}

/// Uncoloured rows: what a question is about, shown while it waits.
fn plain_rows<'a>(lines: &'a [String], theme: &Theme) -> Vec<Row<'a>> {
    lines
        .iter()
        .map(|text| Row {
            text: text.as_str(),
            style: theme.text,
            prefix: "",
        })
        .collect()
}

fn plan_rows_at<'a>(lines: &'a [String], theme: &Theme) -> Vec<Row<'a>> {
    lines
        .iter()
        .map(|text| Row {
            text: text.as_str(),
            style: plan_style(text, theme),
            prefix: "",
        })
        .collect()
}

/// The diff is the one pane coloured by what its rows do to a file.
fn diff_rows<'a>(lines: &'a [String], theme: &Theme) -> Vec<Row<'a>> {
    lines
        .iter()
        .map(|text| Row {
            text: text.as_str(),
            style: diff_style(text, theme),
            prefix: "",
        })
        .collect()
}

fn style_for(kind: Kind, theme: &Theme) -> Style {
    match kind {
        Kind::Task => theme.heading,
        Kind::Agent | Kind::Tool => theme.text,
        Kind::Trouble => theme.failed,
        Kind::Warning => theme.warning,
        Kind::Heading => theme.heading,
        Kind::Muted => theme.muted,
    }
}

/// A diff row by the character that starts it, because that character is the
/// whole of what it means.
fn diff_style(text: &str, theme: &Theme) -> Style {
    match text.chars().next() {
        Some('+') => theme.ok,
        Some('-') => theme.failed,
        Some('@') => theme.accent,
        _ => theme.muted,
    }
}

/// A plan row by its checkbox: the mark is the state, and the state is the point.
fn plan_style(text: &str, theme: &Theme) -> Style {
    if text.starts_with("Goal:") {
        return theme.heading;
    }
    match text.chars().nth(1) {
        Some('x') => theme.ok,
        Some('>') => theme.accent,
        Some('!') => theme.failed,
        Some('-') => theme.muted,
        _ => theme.text,
    }
}

/// The seven things a person watches while a run is going, in the order they get
/// asked about: who answers, which tree, what may be written, what is happening
/// now, how full the window is, how long it has taken.
pub fn status_spans(screen: &Screen, theme: &Theme) -> FrameLine<'static> {
    let status = &screen.status;
    let header = &screen.header;
    let tool = match &status.current_tool {
        Some(name) => name.clone(),
        None if status.started_at_ms.is_some() => "thinking".to_owned(),
        None => "idle".to_owned(),
    };
    let mut spans = vec![
        Span::styled(format!(" {} ", header.agent), theme.heading),
        Span::styled("provider ", theme.accent),
        Span::styled(header.provider.clone(), theme.text),
        Span::styled(" branch ", theme.accent),
        Span::styled(header.branch.clone(), theme.text),
        Span::styled(" mode ", theme.accent),
        Span::styled(header.mode.clone(), theme.text),
        Span::styled(" ctx~", theme.accent),
        Span::styled(
            format!("{}/{}", status.context_tokens, header.window),
            context_style(header, status.context_tokens, theme),
        ),
        Span::styled(" tool ", theme.accent),
        Span::styled(tool, theme.text),
        Span::styled(" elapsed ", theme.accent),
        Span::styled(format!("{}s", status.elapsed_seconds()), theme.text),
        Span::styled(" turns ", theme.accent),
        Span::styled(status.turns.to_string(), theme.text),
    ];
    if status.redactions > 0 {
        spans.push(Span::styled(
            format!(" {} secret(s) masked", status.redactions),
            theme.warning,
        ));
    }
    FrameLine::from(spans)
}

/// The window is the number a run dies on, so it is the one worth colouring: the
/// loop warns a model at ninety percent full, and the bar says the same thing.
fn context_style(header: &Header, used: usize, theme: &Theme) -> Style {
    if header.window > 0 && used * 10 >= header.window * 9 {
        theme.warning
    } else {
        theme.text
    }
}

fn composer_line(screen: &Screen, theme: &Theme) -> FrameLine<'static> {
    match screen.phase {
        Phase::Running => FrameLine::from(Span::styled(
            " a run is going — Esc stops it; a question is answered with y, a or n",
            theme.muted,
        )),
        Phase::Idle if screen.pending.is_some() => {
            FrameLine::from(Span::styled(" answer above: y / a / n", theme.asking))
        }
        Phase::Idle => FrameLine::from(vec![
            Span::styled("› ", theme.accent),
            Span::styled(screen.input.clone(), theme.text),
            Span::styled("▏", theme.accent),
        ]),
    }
}

/// The question itself, in its own strip, with the answers written next to it.
///
/// A strip rather than a centred dialog, because the detail a person needs in
/// order to answer is in the pane beside it: a box over the diff would hide the
/// thing being approved.
fn question_bar(frame: &mut Frame, screen: &Screen, theme: &Theme, area: Rect) {
    let Some(request) = &screen.pending else {
        return;
    };
    let lines = vec![
        FrameLine::from(Span::styled(request.summary.clone(), theme.heading)),
        FrameLine::from(vec![
            Span::styled("[y] yes  ", theme.ok),
            Span::styled("[a] yes for the rest of this run  ", theme.accent),
            Span::styled("[n] no  [Esc] no", theme.failed),
        ]),
    ];
    if area.height < 4 || area.width < 8 {
        // A terminal too small for the box still has to say that the run is
        // waiting, and what it is waiting for. The answer keys are on the
        // composer line below, so only the question's own text is dropped.
        draw_line(frame, area, &lines[0]);
        return;
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.asking)
                .title(Span::styled(" approval asked ", theme.asking)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::text::Text;

    use super::*;

    fn row(text: &str) -> Row<'_> {
        Row {
            text,
            style: Style::default(),
            prefix: "",
        }
    }

    /// The characters a framed line will put on the terminal.
    fn shown(lines: &[FrameLine]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_short_text_wraps_the_way_a_paragraph_does() {
        assert_eq!(rows_for("", 10), 1);
        assert_eq!(rows_for("one two three", 10), 2);
        assert_eq!(rows_for("one two three four", 10), 2);
        let long = "a".repeat(25);
        assert_eq!(rows_for(&long, 10), 3, "a long word breaks mid-word");
        assert_eq!(rows_for("hello", 0), 1, "a zero-width pane is one row tall");
    }

    #[test]
    fn the_wrap_count_never_under_promise_the_room_a_row_needs() {
        // The case a word wrap turns against a counter: five five-letter words in
        // ten columns pack into three rows only if you count them optimistically.
        assert_eq!(rows_for("aaaaa bbbbb ccccc ddddd eeeee", 10), 5);
    }

    #[test]
    fn a_pane_takes_its_newest_rows_whole_and_its_oldest_from_the_top() {
        let rows = vec![row("first"), row("second"), row("third"), row("fourth")];
        let newest = shown(&fit(&rows, 20, 2, Newest));
        assert!(newest.starts_with("third"), "{newest}");
        assert!(newest.ends_with("fourth"), "{newest}");
        let oldest = shown(&fit(&rows, 20, 2, Oldest));
        assert!(oldest.starts_with("first"), "{oldest}");
        assert!(oldest.ends_with("second"), "{oldest}");
    }

    #[test]
    fn one_row_is_always_shown_even_when_it_cannot_fit() {
        let long = "a".repeat(200);
        let rows = vec![row(long.as_str())];
        assert_eq!(
            fit(&rows, 10, 1, Newest).len(),
            1,
            "an empty pane tells a person nothing"
        );
        assert_eq!(fit(&[], 10, 5, Newest).len(), 0);
    }

    #[test]
    fn a_wrapping_row_pays_for_the_mark_in_front_of_it() {
        let text = "one two three four five six";
        let wrapped = Row {
            text,
            style: Style::default(),
            prefix: "› ",
        };
        let plain = Row {
            text,
            style: Style::default(),
            prefix: "",
        };
        assert_eq!(cost(&plain, 20), 2);
        assert!(
            cost(&wrapped, 12) >= cost(&plain, 12),
            "a prefix is not free"
        );
    }

    #[test]
    fn the_plan_is_coloured_by_its_marks_and_the_diff_by_its_edges() {
        let theme = Theme::default();
        assert_eq!(plan_style("Goal: fix it", &theme).fg, theme.heading.fg);
        assert_eq!(
            plan_style("[x] 1. read", &theme).fg,
            theme.ok.fg,
            "a done step reads as done"
        );
        assert_eq!(plan_style("[ ] 2. write", &theme).fg, theme.text.fg);
        assert_eq!(plan_style("[!] 2. write", &theme).fg, theme.failed.fg);
        assert_eq!(diff_style("+added", &theme).fg, theme.ok.fg);
        assert_eq!(diff_style("-gone", &theme).fg, theme.failed.fg);
        assert_eq!(diff_style("@@ -1 +1 @@", &theme).fg, theme.accent.fg);
        assert_eq!(diff_style("context", &theme).fg, theme.muted.fg);
    }

    #[test]
    fn the_status_bar_shows_seven_things_and_says_which_run_it_is() {
        let mut screen = Screen::new(Header {
            agent: "smoll 0.1.0".to_owned(),
            provider: "openai-compatible (llama3.2)".to_owned(),
            branch: "master".to_owned(),
            mode: "approve-edits".to_owned(),
            window: 4096,
            ..Default::default()
        });
        screen.status.current_tool = Some("fs_read_file".to_owned());
        screen.status.context_tokens = 200;
        screen.status.elapsed_ms = 4_000;
        screen.status.turns = 2;
        let text = shown(&[status_spans(&screen, &Theme::default())]);
        for wanted in [
            "smoll 0.1.0",
            "openai-compatible (llama3.2)",
            "master",
            "approve-edits",
            "200/4096",
            "fs_read_file",
            "4s",
            "2",
        ] {
            assert!(text.contains(wanted), "{wanted} missing from {text}");
        }
    }

    #[test]
    fn an_idle_bar_says_idle_and_a_running_one_says_what_it_is_doing() {
        let idle = shown(&[status_spans(
            &Screen::new(Header::default()),
            &Theme::default(),
        )]);
        assert!(idle.contains("idle"), "{idle}");

        let mut screen = Screen::new(Header::default());
        screen.status.started_at_ms = Some(1);
        let running = shown(&[status_spans(&screen, &Theme::default())]);
        assert!(running.contains("thinking"), "{running}");
    }

    #[test]
    fn a_nearly_full_window_is_a_caution_and_an_empty_one_is_not() {
        let theme = Theme::default();
        let header = Header {
            window: 1000,
            ..Default::default()
        };
        assert_eq!(context_style(&header, 899, &theme).fg, theme.text.fg);
        assert_eq!(
            context_style(&header, 900, &theme).fg,
            theme.warning.fg,
            "the loop warns at ninety percent, so the bar does too"
        );
        assert_eq!(
            context_style(&Header::default(), 1, &theme).fg,
            theme.text.fg
        );
    }

    #[test]
    fn the_text_a_test_reads_back_is_the_text_a_terminal_gets() {
        // Guards this file's own helper: a framed line's spans are the whole of what
        // lands in a buffer, so `shown` must not be quietly dropping one.
        let line = FrameLine::from(vec![Span::raw("ab"), Span::raw("cd")]);
        assert_eq!(shown(std::slice::from_ref(&line)), "abcd");
        assert_eq!(Text::from(vec![line]).lines.len(), 1);
    }
}
