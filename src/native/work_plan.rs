use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, fmt::Write as _};

const MAX_STEPS: usize = 24;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    #[default]
    Pending,
    InProgress,
    Completed,
    Blocked,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => tr!("Ожидает", "Pending"),
            Self::InProgress => tr!("В работе", "In progress"),
            Self::Completed => tr!("Готово", "Completed"),
            Self::Blocked => tr!("Заблокировано", "Blocked"),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Step {
    id: String,
    title: String,
    acceptance: String,
    status: Status,
    evidence: String,
    recheck: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct WorkPlan {
    goal: String,
    constraints: Vec<String>,
    steps: Vec<Step>,
    approved: bool,
}

impl WorkPlan {
    pub fn is_empty(&self) -> bool {
        self.goal.is_empty()
    }

    pub fn approve(&mut self) {
        self.approved = !self.is_empty();
    }

    pub fn cancel_draft(&mut self) {
        if !self.approved {
            *self = Self::default();
        }
    }

    pub fn recheck(&mut self) {
        for step in &mut self.steps {
            step.recheck = true;
            if step.status == Status::Completed {
                step.status = Status::Pending;
                step.evidence.clear();
            }
        }
    }

    pub fn apply(&mut self, action: &Value, planning: bool) -> Result<String> {
        match action["operation"].as_str() {
            Some("define") => {
                ensure!(
                    !self.approved || planning,
                    "{}",
                    tr!(
                        "Обнови статусы утверждённого плана или открой /plan для его пересмотра.",
                        "Update the approved plan's steps or use /plan to revise it."
                    )
                );
                let goal = field(action, "goal", 2_000)?;
                let constraints: Vec<String> = action
                    .get("constraints")
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?
                    .unwrap_or_default();
                ensure!(
                    constraints.len() <= 12 && constraints.iter().all(|value| value.len() <= 500),
                    "Plan constraints exceed their limit"
                );
                let steps: Vec<Step> = serde_json::from_value(action["steps"].clone())?;
                ensure!(
                    (1..=MAX_STEPS).contains(&steps.len()),
                    "Supply 1–{MAX_STEPS} plan steps"
                );
                let mut ids = HashSet::new();
                for step in &steps {
                    ensure!(
                        (1..=24).contains(&step.id.len())
                            && step
                                .id
                                .bytes()
                                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                            && ids.insert(&step.id),
                        "Step IDs must be unique short alphanumeric labels"
                    );
                    ensure!(
                        !step.title.trim().is_empty()
                            && step.title.len() <= 500
                            && !step.acceptance.trim().is_empty()
                            && step.acceptance.len() <= 1_000,
                        "Each step needs a title and an observable acceptance criterion"
                    );
                    ensure!(
                        step.status == Status::Pending && step.evidence.is_empty() && !step.recheck,
                        "New steps start pending"
                    );
                }
                *self = Self {
                    goal,
                    constraints,
                    steps,
                    approved: false,
                };
            }
            Some("update") => {
                ensure!(
                    !planning,
                    "Update execution status after entering coding mode"
                );
                let id = field(action, "id", 24)?;
                let status: Status = serde_json::from_value(action["status"].clone())?;
                let evidence = action["evidence"].as_str().unwrap_or("").trim();
                ensure!(evidence.len() <= 1_000, "Step result is too long");
                ensure!(
                    !matches!(status, Status::Completed | Status::Blocked) || !evidence.is_empty(),
                    "Completed and blocked steps need an observed result or a blocker"
                );
                let step = self
                    .steps
                    .iter_mut()
                    .find(|step| step.id == id)
                    .context("Unknown plan step ID")?;
                step.status = status;
                step.evidence = evidence.to_owned();
                step.recheck = false;
            }
            None if action["text"].is_string() && !self.approved => {
                let text = field(action, "text", 2_000)?;
                *self = Self {
                    goal: text,
                    ..Self::default()
                };
            }
            None if action["operation"].is_null() && action["text"].is_string() => {
                return Ok(self.render());
            }
            _ => anyhow::bail!("Use plan with operation=define or operation=update"),
        }
        Ok(self.render())
    }

    pub fn render(&self) -> String {
        if self.is_empty() {
            return tr!("В этом чате пока нет плана.", "This chat has no plan yet.").into();
        }
        let mut text = trf!("Цель · {}", "Goal · {}", self.goal);
        for constraint in &self.constraints {
            let _ = write!(text, "\n  · {constraint}");
        }
        for step in &self.steps {
            let _ = write!(
                text,
                "\n\n{} · {} · {}",
                step.id,
                step.status.label(),
                step.title
            );
            let _ = write!(text, "\n{} · {}", tr!("Проверка", "Check"), step.acceptance);
            if !step.evidence.is_empty() {
                let _ = write!(text, "\n{}", step.evidence);
            }
            if step.recheck {
                text.push_str(tr!(
                    "\nПроверь после смены папки.",
                    "\nRecheck after the folder change."
                ));
            }
        }
        text
    }

    pub fn prompt(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        serde_json::to_string(self).unwrap_or_default()
    }
}

fn field(value: &Value, key: &str, limit: usize) -> Result<String> {
    let text = value[key].as_str().unwrap_or("").trim();
    ensure!(
        (1..=limit).contains(&text.len()),
        "{key} must contain 1–{limit} bytes"
    );
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_and_project_change_preserve_the_goal_and_require_fresh_evidence() -> Result<()> {
        let mut plan = WorkPlan::default();
        let draft = json!({"operation":"define","goal":"Ship a site","steps":[
            {"id":"verify","title":"Run the tests","acceptance":"Tests pass"}
        ]});
        plan.apply(&draft, true)?;
        plan.approve();
        assert!(plan.apply(&draft, false).is_err());
        assert!(
            plan.apply(
                &json!({"operation":"update","id":"verify","status":"completed"}),
                false
            )
            .is_err()
        );
        plan.apply(&json!({"operation":"update","id":"verify","status":"completed","evidence":"3 tests passed"}), false)?;
        let mut plan: WorkPlan = serde_json::from_str(&plan.prompt())?;
        assert_eq!(plan.steps[0].status, Status::Completed);
        plan.recheck();
        assert_eq!(plan.goal, "Ship a site");
        assert_eq!(plan.steps[0].status, Status::Pending);
        assert!(plan.steps[0].evidence.is_empty());
        Ok(())
    }
}
