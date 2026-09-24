//! A plan the head agent proposes and the person shapes before anything runs.
//!
//! For work with more than one step, the head agent calls `propose_plan` instead of
//! delegating each piece. The plan appears on a board as one card per step; the person
//! edits a task, switches a role, removes a step or comments on it, then either sends the
//! comments back for a revision or runs it. Running is the harness's job, not the head
//! agent's: independent steps start in parallel, and a step that depends on another starts
//! only once that one has **landed** — so it builds on real code on the person's branch,
//! not on a branch that may yet be discarded.
//!
//! Each step is an ordinary worker: it goes through verification, the merge gate and the
//! autonomy level like any other. The board is a view onto that, lane by lane.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::roles::RoleRegistry;

/// Enough for a real piece of work; more than this is a plan that should be split.
pub const MAX_STEPS: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StepInput {
    /// Short id other steps refer to in `depends_on`, e.g. "api" or "1".
    pub id: String,
    /// A few words for the card.
    pub title: String,
    /// The role to run it.
    pub role: String,
    /// The worker's brief — self-contained, since it cannot see the conversation.
    pub task: String,
    #[serde(default)]
    pub context_files: Vec<String>,
    /// Steps that must have landed before this one starts.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// Proposed, being reviewed. Nothing has run.
    Draft,
    Running,
    /// Every step is in a final state.
    Finished,
    Discarded,
}

/// Where a step stands — one lane on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Ready to start once the plan runs.
    Planned,
    /// Held until the steps it depends on have landed.
    Waiting,
    Running,
    /// Its merge is being verified.
    Checking,
    /// Waiting for the person to merge it.
    Review,
    /// Merged — or, for a step that changes no files, done.
    Landed,
    Failed,
    /// Never started: something it depended on failed or was discarded.
    Skipped,
}

impl StepState {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Landed | Self::Failed | Self::Skipped)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    #[serde(flatten)]
    pub input: StepInput,
    pub state: StepState,
    /// The worker running it, once started.
    #[serde(default)]
    pub worker_id: Option<String>,
    /// Why it failed or was skipped, or what a no-change step reported.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub status: PlanStatus,
    pub steps: Vec<PlanStep>,
}

impl Plan {
    pub fn new(id: String, title: String, summary: String, steps: Vec<StepInput>) -> Self {
        Self {
            id,
            title,
            summary,
            status: PlanStatus::Draft,
            steps: steps
                .into_iter()
                .map(|input| PlanStep {
                    input,
                    state: StepState::Planned,
                    worker_id: None,
                    note: None,
                })
                .collect(),
        }
    }

    /// One line per step, for the head agent when the plan ends.
    pub fn outcome(&self) -> String {
        self.steps
            .iter()
            .map(|step| {
                let state = serde_json::to_value(step.state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                match &step.note {
                    Some(note) => {
                        format!("{} ({}): {state} — {note}", step.input.title, step.input.id)
                    }
                    None => format!("{} ({}): {state}", step.input.title, step.input.id),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// The rules a plan must follow before it is shown or run: something to do, not too
/// much, unique ids, real roles, and dependencies that point backwards to steps that
/// exist — so it can always finish.
pub fn validate(steps: &[StepInput], registry: &RoleRegistry) -> Result<()> {
    if steps.is_empty() {
        bail!("a plan needs at least one step");
    }
    if steps.len() > MAX_STEPS {
        bail!("a plan has at most {MAX_STEPS} steps; split the work into more than one plan");
    }
    let mut seen = HashSet::new();
    for step in steps {
        let id = step.id.trim();
        if id.is_empty() || id.len() > 40 {
            bail!("every step needs a short id");
        }
        if !seen.insert(id) {
            bail!("two steps are both called `{id}`");
        }
        if step.task.trim().is_empty() {
            bail!("step `{id}` has no task");
        }
        if !registry.roles.contains_key(&step.role) {
            bail!(
                "step `{id}` names role `{}`, which is not in the fleet ({})",
                step.role,
                registry.role_names().join(", ")
            );
        }
        for dep in &step.depends_on {
            if dep == id {
                bail!("step `{id}` depends on itself");
            }
            if !steps.iter().any(|other| other.id == *dep) {
                bail!("step `{id}` depends on `{dep}`, which is not in the plan");
            }
        }
    }
    if let Some(cycle) = find_cycle(steps) {
        bail!("steps depend on each other in a circle, through `{cycle}`");
    }
    Ok(())
}

fn find_cycle(steps: &[StepInput]) -> Option<String> {
    let deps: HashMap<&str, &[String]> = steps
        .iter()
        .map(|s| (s.id.as_str(), s.depends_on.as_slice()))
        .collect();
    // 0 = unvisited, 1 = on the current path, 2 = done.
    let mut mark: HashMap<&str, u8> = HashMap::new();
    fn visit<'a>(
        id: &'a str,
        deps: &HashMap<&'a str, &'a [String]>,
        mark: &mut HashMap<&'a str, u8>,
    ) -> Option<String> {
        match mark.get(id) {
            Some(1) => return Some(id.to_string()),
            Some(2) => return None,
            _ => {}
        }
        mark.insert(id, 1);
        for dep in deps.get(id).copied().unwrap_or_default() {
            if let Some(found) = visit(dep.as_str(), deps, mark) {
                return Some(found);
            }
        }
        mark.insert(id, 2);
        None
    }
    steps
        .iter()
        .find_map(|s| visit(s.id.as_str(), &deps, &mut mark))
}

/// What the runner should do with a step that has not started, given its dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Start,
    Wait,
    Skip(String),
}

pub fn readiness(plan: &Plan, step: &PlanStep) -> Readiness {
    let mut waiting = false;
    for dep in &step.input.depends_on {
        let Some(other) = plan.steps.iter().find(|s| s.input.id == *dep) else {
            continue;
        };
        match other.state {
            StepState::Landed => {}
            StepState::Failed | StepState::Skipped => {
                return Readiness::Skip(format!("`{dep}` did not land"));
            }
            _ => waiting = true,
        }
    }
    if waiting {
        Readiness::Wait
    } else {
        Readiness::Start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> RoleRegistry {
        RoleRegistry::from_toml(
            "[roles.builder]\nprovider = \"mock\"\nisolation = \"worktree\"\n\
             [roles.tester]\nprovider = \"mock\"\nisolation = \"readonly\"\n",
        )
        .unwrap()
    }

    fn step(id: &str, deps: &[&str]) -> StepInput {
        StepInput {
            id: id.into(),
            title: format!("step {id}"),
            role: "builder".into(),
            task: "do it".into(),
            context_files: Vec::new(),
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
        }
    }

    #[test]
    fn a_sound_plan_passes() {
        validate(
            &[step("a", &[]), step("b", &["a"]), step("c", &["a", "b"])],
            &registry(),
        )
        .unwrap();
    }

    #[test]
    fn plans_that_could_never_finish_are_refused() {
        let reg = registry();
        let cases: Vec<(Vec<StepInput>, &str)> = vec![
            (vec![], "at least one step"),
            (vec![step("a", &[]), step("a", &[])], "both called"),
            (vec![step("a", &["missing"])], "not in the plan"),
            (vec![step("a", &["a"])], "depends on itself"),
            (vec![step("a", &["b"]), step("b", &["a"])], "circle"),
            (
                (0..=MAX_STEPS).map(|i| step(&i.to_string(), &[])).collect(),
                "at most",
            ),
        ];
        for (steps, expected) in cases {
            let err = validate(&steps, &reg).unwrap_err().to_string();
            assert!(err.contains(expected), "{err}");
        }
        let mut unknown = step("a", &[]);
        unknown.role = "wizard".into();
        assert!(validate(&[unknown], &reg)
            .unwrap_err()
            .to_string()
            .contains("not in the fleet"));
    }

    #[test]
    fn a_step_waits_for_its_dependencies_to_land_and_is_skipped_if_one_does_not() {
        let mut plan = Plan::new(
            "p".into(),
            "t".into(),
            "s".into(),
            vec![step("a", &[]), step("b", &["a"])],
        );
        assert_eq!(readiness(&plan, &plan.steps[0]), Readiness::Start);
        assert_eq!(readiness(&plan, &plan.steps[1]), Readiness::Wait);

        // Done is not enough — it has to have landed.
        plan.steps[0].state = StepState::Review;
        assert_eq!(readiness(&plan, &plan.steps[1]), Readiness::Wait);
        plan.steps[0].state = StepState::Landed;
        assert_eq!(readiness(&plan, &plan.steps[1]), Readiness::Start);
        plan.steps[0].state = StepState::Failed;
        assert!(matches!(
            readiness(&plan, &plan.steps[1]),
            Readiness::Skip(_)
        ));
    }

    #[test]
    fn a_step_serializes_flat_for_the_board() {
        let plan = Plan::new("p".into(), "t".into(), "s".into(), vec![step("a", &[])]);
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(json["steps"][0]["id"], "a");
        assert_eq!(json["steps"][0]["state"], "planned");
        assert_eq!(json["status"], "draft");
    }
}
