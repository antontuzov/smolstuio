//! The plan, as the model writes it.
//!
//! The loop opens every run with a short scaffold it inferred from the repository
//! — read these files, make the change, run the command that validates it. That
//! is what a 1.5B model can hold without spending its window restating a task,
//! and it is why the plan is not asked of the model by default. These two tools
//! exist for the runs where the model knows better: it can put its own steps in
//! place of the scaffold, and say which of them it has finished.
//!
//! A plan is **not a file**. It changes what this run holds in mind and what its
//! transcript records, and nothing in the repository, which is why both tools are
//! [`Permission::ReadOnly`] — an agent that writes its own plan must not have to
//! be approved to think. The steps go through to `agent-core`, which applies them
//! to the run's plan; from then on the model marks progress by copying a plan back
//! with a box ticked, in the same `[x]` shape the prompt shows it.
//!
//! The parser is deliberately forgiving about spelling, because it reads what a
//! small model typed after copying it out of its own prompt: a leading `[x]`, an
//! `N.` and a `- ` are all stripped, and a copied `Goal:` line becomes the goal
//! rather than a step.

use async_trait::async_trait;
use serde_json::json;

use agent_sandbox::Permission;

use crate::call::{ToolCall, ToolOutput, ToolResult};
use crate::context::Context;
use crate::schema;
use crate::tool::{Intent, Tool, ToolDefinition};

/// The longest plan a small model can hold in mind, and the longest one a prompt
/// can carry on every step without taking room from the repository.
pub const MAX_PLAN_STEPS: usize = 8;

/// A step is one action, not a paragraph. Beyond this the model is writing a
/// second task description and the plan stops being a checklist.
pub const MAX_STEP_CHARS: usize = 90;

/// Where a step of a written plan stands, in the one character the plan is
/// rendered with. `agent-core` owns what moves them while the plan is its own; a
/// model marks them by copying its plan back with the box ticked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepMark {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

impl StepMark {
    /// The character the plan is rendered with, and the one a model copies back.
    pub fn character(self) -> char {
        match self {
            Self::Pending => ' ',
            Self::Running => '>',
            Self::Done => 'x',
            Self::Failed => '!',
            Self::Skipped => '-',
        }
    }

    pub fn parse(character: char) -> Option<Self> {
        match character {
            ' ' => Some(Self::Pending),
            '>' => Some(Self::Running),
            'x' | 'X' => Some(Self::Done),
            '!' => Some(Self::Failed),
            '-' => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// One step as it was written: a sentence and, if the model said so, how far it
/// got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedStep {
    pub title: String,
    pub mark: StepMark,
}

/// A plan a call asked for, after the parser has made it safe to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDoc {
    /// `None` keeps whatever goal the run already has, which is the task in one
    /// line. A model that restates it is allowed to.
    pub goal: Option<String>,
    pub steps: Vec<PlannedStep>,
}

impl PlanDoc {
    /// The plan these lines describe, or a refusal a model can act on.
    ///
    /// Long titles are cut rather than refused, because the answer echoes the plan
    /// back and the model sees what it got; too many steps are refused, because
    /// silently dropping the last one could drop the one that says "run the tests".
    pub fn parse(goal: Option<String>, lines: &[String]) -> Result<Self, String> {
        let mut steps = Vec::new();
        let mut stated = goal.filter(|text| !text.trim().is_empty());
        for line in lines {
            let Some(step) = step_of(line) else {
                continue;
            };
            if step
                .get(..5)
                .is_some_and(|head| head.eq_ignore_ascii_case("goal:"))
            {
                if stated.is_none() {
                    stated = Some(step[5..].trim().to_owned());
                }
                continue;
            }
            if step
                .trim_end_matches(':')
                .eq_ignore_ascii_case("plan for this run")
            {
                continue;
            }
            steps.push(PlannedStep {
                title: step.chars().take(MAX_STEP_CHARS).collect(),
                mark: mark_of(line),
            });
        }
        if steps.is_empty() {
            return Err(
                "the plan has no steps in it. Name what this run will do, one short sentence \
                 apiece, as a list like [\"read src/lib.rs\", \"add the check\", \"run cargo \
                 test\"]"
                    .to_owned(),
            );
        }
        if steps.len() > MAX_PLAN_STEPS {
            return Err(format!(
                "a plan of {} steps is a task list, not something a small model keeps in mind \
                 while it works; fold it to {MAX_PLAN_STEPS} or fewer",
                steps.len()
            ));
        }
        Ok(Self {
            goal: stated.filter(|text| !text.trim().is_empty()),
            steps,
        })
    }

    /// The plan as the model is shown it every step, so the answer it gets back is
    /// in the spelling it can copy.
    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        if let Some(goal) = &self.goal {
            lines.push(format!("Goal: {goal}"));
        }
        for (index, step) in self.steps.iter().enumerate() {
            lines.push(format!(
                "[{}] {}. {}",
                step.mark.character(),
                index + 1,
                step.title
            ));
        }
        lines.join("\n")
    }
}

/// One line of a written plan: the title, with whatever furniture a model copied
/// along with it stripped off. `None` for a line that says nothing.
fn step_of(line: &str) -> Option<String> {
    let mut text = line.trim();
    // `[x] 3. the thing`, with the bracket and the number both optional.
    if let Some(end) = text.find(']') {
        if text.starts_with('[') {
            text = text[end + 1..].trim();
        }
    }
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        // Only an enumeration goes: a plan step that opens "3 ways to fix it" is a
        // sentence, not a numbered one.
        if let Some(rest) = text[digits.len()..].strip_prefix(['.', ')']) {
            if !rest.trim().is_empty() {
                text = rest.trim();
            }
        }
    }
    let text = text.trim_start_matches(['-', '*', '•']).trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// What the model marked a line as, from the bracket it copied.
fn mark_of(line: &str) -> StepMark {
    let text = line.trim_start();
    let Some(rest) = text.strip_prefix('[') else {
        return StepMark::Pending;
    };
    // A bracket is a mark only when it holds one character and closes; `[]` and
    // `[not a mark] x` are sentences that happen to start with a bracket.
    let (marked, closed) = (rest.chars().next(), rest.chars().nth(1));
    match (marked, closed) {
        (Some(character), Some(']')) => StepMark::parse(character).unwrap_or(StepMark::Pending),
        _ => StepMark::Pending,
    }
}

/// The plan this call writes, when it writes one.
///
/// `agent-core` uses it to replace the run's plan after the tool has answered;
/// `Ok(None)` for every other tool, and `Err` when the arguments will not parse,
/// which the tool has already told the model about.
pub fn authored(call: &ToolCall) -> Result<Option<PlanDoc>, String> {
    if !matches!(call.name.as_str(), "task_plan_create" | "task_plan_update") {
        return Ok(None);
    }
    let Some(lines) = call.str_list_arg("steps") else {
        return Err(format!(
            "a plan needs \"steps\": an array of short sentences, {MAX_PLAN_STEPS} of them at \
             most"
        ));
    };
    PlanDoc::parse(call.str_arg("goal"), &lines).map(Some)
}

pub struct TaskPlanCreate;

#[async_trait]
impl Tool for TaskPlanCreate {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "task_plan_create",
            "Replace the plan this run opened with: the steps you will work through, short \
             sentences, eight at most. It writes nothing and changes only what the run aims at.",
            schema::object(
                json!({
                    "steps": schema::string_array("what to do, in order; eight at most"),
                    "goal": schema::string("the one line they are for; optional"),
                }),
                &["steps"],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let _ = (call, ctx);
        Ok(Intent::default())
    }

    async fn run(&self, call: &ToolCall, _ctx: &Context) -> ToolResult {
        answered(call)
    }
}

pub struct TaskPlanUpdate;

#[async_trait]
impl Tool for TaskPlanUpdate {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "task_plan_update",
            "Rewrite the plan when the work turns out different from what was planned. Copy the \
             lines you are keeping and put an x in the brackets of the ones already done, so the \
             run keeps what happened: `[x] 1. read src/lib.rs`.",
            schema::object(
                json!({
                    "steps": schema::string_array("the whole plan now, done steps marked; eight at most"),
                    "goal": schema::string("the one line they are for; optional"),
                }),
                &["steps"],
            ),
            Permission::ReadOnly,
        )
    }

    fn intent(&self, call: &ToolCall, ctx: &Context) -> Result<Intent, String> {
        let _ = (call, ctx);
        Ok(Intent::default())
    }

    async fn run(&self, call: &ToolCall, _ctx: &Context) -> ToolResult {
        answered(call)
    }
}

/// Both tools do the same thing: hold the plan up so the model reads back what the
/// run will now work to. Applying it to the run is the loop's business.
fn answered(call: &ToolCall) -> ToolResult {
    let failed = |message: String| {
        ToolResult::failed(
            call.clone(),
            Permission::ReadOnly,
            message,
            ToolOutput::empty(),
        )
    };
    let doc = match authored(call) {
        Ok(Some(doc)) => doc,
        Ok(None) => return failed(format!("{} is not a tool that writes plans", call.name)),
        Err(problem) => return failed(problem),
    };
    ToolResult::completed(
        call.clone(),
        Permission::ReadOnly,
        ToolOutput::new(format!(
            "this is the plan the run works to now; nothing was written\n{}",
            doc.render()
        )),
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall::new(name, arguments)
    }

    fn context() -> Context {
        Context::new(".")
    }

    #[tokio::test]
    async fn a_written_plan_is_held_up_to_the_model_that_asked_for_it() {
        let result = TaskPlanCreate
            .run(
                &call(
                    "task_plan_create",
                    json!({"steps": ["read src/lib.rs", "add the check", "run cargo test"]}),
                ),
                &context(),
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        let text = result.output.text;
        assert!(text.contains("[ ] 1. read src/lib.rs"), "{text}");
        assert!(text.contains("[ ] 3. run cargo test"), "{text}");
        assert!(text.contains("nothing was written"), "{text}");
    }

    #[tokio::test]
    async fn a_goal_is_kept_and_a_blank_one_is_not_announced() {
        let with = TaskPlanCreate
            .run(
                &call(
                    "task_plan_create",
                    json!({"goal": "make the map cheaper", "steps": ["read map.rs"]}),
                ),
                &context(),
            )
            .await;
        assert!(
            with.output.text.contains("Goal: make the map cheaper"),
            "{}",
            with.output.text
        );
        let without = TaskPlanCreate
            .run(
                &call(
                    "task_plan_create",
                    json!({"steps": ["read map.rs"], "goal": "  "}),
                ),
                &context(),
            )
            .await;
        assert!(
            !without.output.text.contains("Goal:"),
            "{}",
            without.output.text
        );
    }

    #[test]
    fn a_plan_of_nothing_is_refused_in_words_that_teach_the_shape() {
        for lines in [vec![], vec!["   ".to_owned(), "".to_owned()]] {
            let problem = PlanDoc::parse(None, &lines).expect_err("no steps is no plan");
            assert!(problem.contains("no steps"), "{problem}");
            assert!(
                problem.contains("read src/lib.rs"),
                "it shows the shape: {problem}"
            );
        }
        let problem = authored(&call("task_plan_create", json!({"goal": "x"})))
            .expect_err("steps is required");
        assert!(problem.contains("array of short sentences"), "{problem}");
    }

    #[test]
    fn a_plan_too_long_to_hold_is_refused_rather_than_cut() {
        let lines: Vec<String> = (0..=MAX_PLAN_STEPS).map(|n| format!("step {n}")).collect();
        let problem = PlanDoc::parse(None, &lines).expect_err("nine steps is a list");
        assert!(
            problem.contains(&MAX_PLAN_STEPS.to_string()),
            "the cap is named, so the model knows what to fold to: {problem}"
        );
        let eight: Vec<String> = (0..MAX_PLAN_STEPS).map(|n| format!("step {n}")).collect();
        assert_eq!(
            PlanDoc::parse(None, &eight).expect("eight fit").steps.len(),
            MAX_PLAN_STEPS
        );
    }

    /// The point of forgiving spelling: a model copies the block its prompt
    /// showed it, furniture and all.
    #[test]
    fn a_copied_plan_still_reads_as_a_plan() {
        let copied = vec![
            "Goal: rename the helper".to_owned(),
            "[x] 1. read src/session.rs".to_owned(),
            "[ ] 2. change the name everywhere".to_owned(),
            "[>] 3. run cargo test".to_owned(),
            "  - just a sentence  ".to_owned(),
            "readme.md (3) needs the old name".to_owned(),
            "1) numbered without a bracket".to_owned(),
            "3 ways to fix the map".to_owned(),
        ];
        let doc = PlanDoc::parse(None, &copied).expect("a plan");
        assert_eq!(doc.goal.as_deref(), Some("rename the helper"));
        assert_eq!(
            doc.steps
                .iter()
                .map(|step| step.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "read src/session.rs",
                "change the name everywhere",
                "run cargo test",
                "just a sentence",
                "readme.md (3) needs the old name",
                "numbered without a bracket",
                "3 ways to fix the map",
            ]
        );
        let marks: Vec<StepMark> = doc.steps.iter().map(|step| step.mark).collect();
        assert_eq!(
            marks,
            vec![
                StepMark::Done,
                StepMark::Pending,
                StepMark::Running,
                StepMark::Pending,
                StepMark::Pending,
                StepMark::Pending,
                StepMark::Pending,
            ]
        );
    }

    #[test]
    fn a_line_that_starts_with_a_bracket_is_the_only_one_treated_as_marked() {
        assert_eq!(mark_of("[x] done thing"), StepMark::Done);
        assert_eq!(mark_of("  [x] indented"), StepMark::Done);
        assert_eq!(mark_of("(x) done thing"), StepMark::Pending);
        assert_eq!(mark_of("no bracket here"), StepMark::Pending);
        assert_eq!(mark_of("[] empty bracket"), StepMark::Pending);
        assert_eq!(mark_of("[not a mark] x"), StepMark::Pending);
        assert_eq!(StepMark::parse('X'), Some(StepMark::Done));
        assert_eq!(StepMark::parse('?'), None);
    }

    #[test]
    fn the_copied_heading_is_not_a_step() {
        let doc = PlanDoc::parse(
            None,
            &[
                "Plan for this run:".to_owned(),
                "[ ] 1. read the file".to_owned(),
            ],
        )
        .expect("a plan");
        assert_eq!(doc.steps.len(), 1);
        assert_eq!(doc.steps[0].title, "read the file");
        assert_eq!(doc.goal, None);
    }

    #[test]
    fn a_long_step_is_cut_to_one_action_and_the_cut_is_visible() {
        let long = "w".repeat(MAX_STEP_CHARS + 40);
        let doc = PlanDoc::parse(None, &[long]).expect("one step is still a plan");
        assert_eq!(doc.steps[0].title.chars().count(), MAX_STEP_CHARS);
    }

    #[tokio::test]
    async fn an_update_reads_the_same_plan_back_with_its_ticks_kept() {
        let result = TaskPlanUpdate
            .run(
                &call(
                    "task_plan_update",
                    json!({"steps": ["[x] read src/lib.rs", "write the test", "[!] run cargo test"]}),
                ),
                &context(),
            )
            .await;
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.output.text.contains("[x] 1. read src/lib.rs"),
            "{}",
            result.output.text
        );
        assert!(
            result.output.text.contains("[!] 3. run cargo test"),
            "a failed step stays failed: {}",
            result.output.text
        );
    }

    #[test]
    fn a_plan_tool_touches_nothing_and_needs_no_path() {
        let ctx = Context::new(".");
        let intent = TaskPlanCreate
            .intent(
                &call("task_plan_create", json!({"steps": ["read x"]})),
                &ctx,
            )
            .expect("a plan has no side effect to declare");
        assert_eq!(intent, Intent::default());
        assert_eq!(
            intent.permission(Permission::ReadOnly),
            Permission::ReadOnly
        );
        assert_eq!(intent.subject(), None);
        assert_eq!(
            authored(&call("read_file", json!({"path": "x"}))).expect("not a plan call"),
            None
        );
    }

    #[tokio::test]
    async fn a_call_with_no_arguments_fails_as_an_observation_not_a_crash() {
        let created = TaskPlanCreate
            .run(&call("task_plan_create", json!({})), &context())
            .await;
        let updated = TaskPlanUpdate
            .run(
                &call("task_plan_update", json!({"steps": ["   "]})),
                &context(),
            )
            .await;
        assert!(!created.is_success(), "{:?}", created.summary());
        assert!(
            created.summary().contains("array"),
            "{:?}",
            created.summary()
        );
        assert!(
            updated.summary().contains("no steps"),
            "{}",
            updated.summary()
        );
    }

    /// A model that writes a plan should not have to be approved to think: the
    /// class is the one the policy never asks about.
    #[test]
    fn writing_a_plan_is_a_read_as_far_as_the_policy_is_concerned() {
        assert_eq!(TaskPlanCreate.definition().permission, Permission::ReadOnly);
        assert_eq!(TaskPlanUpdate.definition().permission, Permission::ReadOnly);
    }
}
