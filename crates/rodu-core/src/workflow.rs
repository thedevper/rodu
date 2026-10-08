use crate::error::{Result, RoduError};
use crate::model::{Category, Link, LinkKind, Rule, State, Transition, Workflow};

/// Workflow for software teams: review needs a linked pull request, work needs an owner.
pub fn dev_workflow() -> Workflow {
    let state = |name: &str, category| State { name: name.into(), category };
    let rule = |from: &str, to: &str, rules| Transition { from: from.into(), to: to.into(), rules };
    Workflow {
        states: vec![
            state("Backlog", Category::Backlog),
            state("Todo", Category::Backlog),
            state("In Progress", Category::Active),
            state("In Review", Category::Review),
            state("Done", Category::Done),
            state("Canceled", Category::Done),
        ],
        initial: "Backlog".into(),
        transitions: vec![
            rule("*", "Backlog", vec![]),
            rule("*", "Todo", vec![]),
            rule("*", "In Progress", vec![Rule::RequireAssignee]),
            rule(
                "In Progress",
                "In Review",
                vec![Rule::RequireAssignee, Rule::RequireLink { link: LinkKind::ImplementsPr }],
            ),
            rule("In Progress", "Done", vec![]),
            rule("In Review", "Done", vec![]),
            rule("*", "Canceled", vec![]),
        ],
    }
}

fn same(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

pub fn find_state<'a>(workflow: &'a Workflow, name: &str) -> Option<&'a State> {
    workflow.states.iter().find(|s| same(&s.name, name))
}

/// Fails when the workflow refers to states it does not define.
pub fn validate_workflow(workflow: &Workflow) -> Result<()> {
    let mut names: Vec<String> = workflow.states.iter().map(|s| s.name.to_lowercase()).collect();
    names.sort();
    names.dedup();
    if names.len() != workflow.states.len() {
        return Err(RoduError::invalid("Workflow state names must be unique"));
    }
    if find_state(workflow, &workflow.initial).is_none() {
        return Err(RoduError::invalid(format!(
            "Initial state \"{}\" is not a workflow state",
            workflow.initial
        )));
    }
    for t in &workflow.transitions {
        for end in [&t.from, &t.to] {
            if end != "*" && find_state(workflow, end).is_none() {
                return Err(RoduError::invalid(format!(
                    "Transition refers to unknown state \"{end}\""
                )));
            }
        }
    }
    Ok(())
}

fn allowed_targets(workflow: &Workflow, from: &str) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    for t in &workflow.transitions {
        if (t.from == "*" || same(&t.from, from))
            && !same(&t.to, from)
            && !targets.iter().any(|x| same(x, &t.to))
        {
            targets.push(t.to.clone());
        }
    }
    targets
}

fn rule_hint(rule: &Rule) -> String {
    match rule {
        Rule::RequireAssignee => r#"assign it first (update_item with {"assignee": "me"})"#.into(),
        Rule::RequireEstimate => {
            r#"estimate it first (update_item with {"estimate": <points>})"#.into()
        }
        Rule::RequireLink { link: LinkKind::ImplementsPr } => {
            r#"link the pull request first (link with {"kind": "implements_pr", "target": "<PR URL>"})"#
                .into()
        }
        Rule::RequireLink { link } => format!("add a \"{link}\" link first"),
    }
}

/// The parts of an item that transitions look at.
pub struct TransitionSubject<'a> {
    pub key: &'a str,
    pub status: &'a str,
    pub assignee_id: Option<&'a str>,
    pub estimate: Option<f64>,
}

fn rule_holds(rule: &Rule, item: &TransitionSubject<'_>, links: &[Link]) -> bool {
    match rule {
        Rule::RequireAssignee => item.assignee_id.is_some(),
        Rule::RequireEstimate => item.estimate.is_some(),
        Rule::RequireLink { link } => links.iter().any(|l| l.kind == *link),
    }
}

/// Checks that `item` may move to `to` and returns the target state. Rules are enforced here, in
/// the domain, so a person, the CLI and an agent all hit the same guardrails.
pub fn check_transition<'w>(
    workflow: &'w Workflow,
    item: &TransitionSubject<'_>,
    to: &str,
    links: &[Link],
) -> Result<&'w State> {
    let Some(target) = find_state(workflow, to) else {
        let names: Vec<&str> = workflow.states.iter().map(|s| s.name.as_str()).collect();
        return Err(RoduError::invalid(format!("Unknown status \"{to}\""))
            .with_hint(format!("Valid statuses: {}", names.join(", "))));
    };
    if same(item.status, &target.name) {
        return Err(RoduError::invalid(format!("{} is already in \"{}\"", item.key, target.name)));
    }
    let transitions: Vec<&Transition> = workflow
        .transitions
        .iter()
        .filter(|t| (t.from == "*" || same(&t.from, item.status)) && same(&t.to, &target.name))
        .collect();
    if transitions.is_empty() {
        let targets = allowed_targets(workflow, item.status);
        let error = RoduError::rule_violation(format!(
            "{} cannot move from \"{}\" to \"{}\"",
            item.key, item.status, target.name
        ));
        return Err(if targets.is_empty() {
            error
        } else {
            error.with_hint(format!(
                "From \"{}\" it can move to: {}",
                item.status,
                targets.join(", ")
            ))
        });
    }
    // Several transitions may match ("*" and an exact one): any one whose rules all hold is enough.
    let failures: Vec<Vec<&Rule>> = transitions
        .iter()
        .map(|t| t.rules.iter().filter(|r| !rule_holds(r, item, links)).collect())
        .collect();
    if failures.iter().any(Vec::is_empty) {
        return Ok(target);
    }
    let fewest = failures.iter().min_by_key(|f| f.len()).expect("at least one transition");
    let hints: Vec<String> = fewest.iter().map(|r| rule_hint(r)).collect();
    Err(RoduError::rule_violation(format!("{} cannot move to \"{}\" yet", item.key, target.name))
        .with_hint(hints.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn subject<'a>(status: &'a str, assignee: Option<&'a str>) -> TransitionSubject<'a> {
        TransitionSubject { key: "DEMO-1", status, assignee_id: assignee, estimate: None }
    }

    fn pr_link() -> Link {
        Link {
            id: "l1".into(),
            from_item_id: "i1".into(),
            kind: LinkKind::ImplementsPr,
            target: "https://example.com/pr/1".into(),
            created_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn the_dev_workflow_is_valid() {
        validate_workflow(&dev_workflow()).unwrap();
    }

    #[test]
    fn requires_an_assignee_to_start_work() {
        let wf = dev_workflow();
        let err = check_transition(&wf, &subject("Backlog", None), "in progress", &[]).unwrap_err();
        assert_eq!(err.code, ErrorCode::RuleViolation);
        assert!(err.hint.unwrap().contains("assign it first"));
        let ok =
            check_transition(&wf, &subject("Backlog", Some("u1")), "in progress", &[]).unwrap();
        assert_eq!(ok.name, "In Progress");
    }

    #[test]
    fn review_needs_a_pull_request_link() {
        let wf = dev_workflow();
        let item = subject("In Progress", Some("u1"));
        let err = check_transition(&wf, &item, "In Review", &[]).unwrap_err();
        assert!(err.hint.unwrap().contains("implements_pr"));
        assert!(check_transition(&wf, &item, "In Review", &[pr_link()]).is_ok());
    }

    #[test]
    fn explains_where_an_item_can_go() {
        let wf = dev_workflow();
        let err = check_transition(&wf, &subject("Backlog", None), "In Review", &[]).unwrap_err();
        assert_eq!(err.code, ErrorCode::RuleViolation);
        assert!(err.hint.unwrap().starts_with("From \"Backlog\" it can move to: Todo"));
    }

    #[test]
    fn rejects_unknown_and_unchanged_statuses() {
        let wf = dev_workflow();
        assert_eq!(
            check_transition(&wf, &subject("Backlog", None), "Nope", &[]).unwrap_err().code,
            ErrorCode::Invalid
        );
        assert!(
            check_transition(&wf, &subject("Backlog", None), "backlog", &[])
                .unwrap_err()
                .message
                .contains("already in")
        );
    }

    #[test]
    fn rejects_workflows_that_name_missing_states() {
        let mut wf = dev_workflow();
        wf.transitions.push(Transition {
            from: "Nowhere".into(),
            to: "Done".into(),
            rules: vec![],
        });
        assert!(validate_workflow(&wf).is_err());
    }
}
