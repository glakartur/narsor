//! The form of the decision on a permission the agent asked for.

#[cfg(test)]
mod tests;

use super::input::*;
use crate::tui::chooser::{ChooserField, ChooserItem, Submission};
use narsor_protocol::api::*;
use serde_json::Value;

/// The field the operator types a Bash pattern into.
pub const PATTERN_FIELD: &str = "Wzorzec polecenia";

impl InputPanel {
    pub(in crate::tui::task_app::panel) fn build_permission_info(
        &mut self,
        pending: &PendingHumanInputView,
    ) {
        let denials = pending
            .permission
            .as_ref()
            .map(|permission| permission.denials.as_slice())
            .unwrap_or_default();
        let resumable = pending
            .agent_session_id
            .as_deref()
            .is_some_and(|id| !id.is_empty());
        let mut exact: Vec<Option<Vec<String>>> = Vec::new();
        let mut group: Vec<Option<Vec<String>>> = Vec::new();
        for denial in denials {
            let command = denial_command(denial);
            self.context.push(ContextLine::problem(match command {
                Some(command) => format!("{}: {command}", denial.tool_name),
                None => format!("{}: {}", denial.tool_name, denial.input),
            }));
            self.context
                .push(ContextLine::dim(format!("katalog: {}", denial.cwd)));
            let rules = |scope| grant_rules(denial, scope);
            let (denial_exact, denial_group) =
                (rules(BashGrantScope::Exact), rules(BashGrantScope::Group));
            match (&denial_exact, why_not_grantable(denial, resumable)) {
                (Some(rules), None) => self.context.push(ContextLine::dim(format!(
                    "reguły zgody: {}",
                    rules.join(", ")
                ))),
                (_, Some(why)) => {
                    self.context
                        .push(ContextLine::plain(format!("zgoda niedostępna: {why}")));
                    if let Some(reason) = denial.reason.as_deref() {
                        self.context.push(ContextLine::dim(reason));
                    }
                }
                (None, None) => {}
            }
            exact.push(denial_exact.filter(|_| resumable));
            group.push(denial_group.filter(|_| resumable));
        }

        let many = denials.len() > 1;
        let all = |grants: &[Option<Vec<String>>]| -> Option<Vec<String>> {
            let mut rules: Vec<String> = Vec::new();
            for grant in grants {
                for rule in grant.as_ref()? {
                    if !rules.contains(rule) {
                        rules.push(rule.clone());
                    }
                }
            }
            (!rules.is_empty()).then_some(rules)
        };
        // A command no rule can pin down could still be approved in simpler
        // parts: the engine asks the agent for them along with the refusal.
        let unpinnable = denials.iter().any(|denial| {
            denial.tool_name == "Bash"
                && is_approvable_permission_reason(denial.reason_type.as_deref())
                && grant_rules(denial, BashGrantScope::Exact).is_none()
        });
        self.push(
            ChooserItem::new(
                if many { "Odmów wszystkim" } else { "Odmów" },
                if unpinnable {
                    "Agent wznowi tę sesję i dostanie prośbę, aby polecenia bez reguły \
                     zgody uruchomił jako proste polecenia, na które można dać zgodę."
                } else {
                    "Agent wznowi tę sesję z informacją, że te operacje są zablokowane."
                },
            )
            .with_field(ChooserField::multiline(MESSAGE_FIELD)),
            Plan::Permission(PermissionDecisionDto {
                denial_index: None,
                decision: PermissionDecisionKindDto::Deny,
                scope: None,
                pattern: None,
                message: None,
            }),
        );
        if let Some(rules) = all(&exact) {
            self.push(
                ChooserItem::new(
                    if many {
                        "Zezwól dokładnie na te polecenia"
                    } else {
                        "Zezwól dokładnie na to polecenie"
                    },
                    format!(
                        "Agent wznowi sesję i może je uruchomić. Reguły tylko dla bieżącego \
                         tasku: {}",
                        rules.join(", ")
                    ),
                ),
                Plan::Permission(PermissionDecisionDto {
                    denial_index: None,
                    decision: PermissionDecisionKindDto::Allow,
                    scope: Some(PermissionGrantScopeDto::ExactOperation),
                    pattern: None,
                    message: None,
                }),
            );
        } else if many {
            // Not every denial can be granted: offer the ones that can.
            for (index, rules) in exact.iter().enumerate() {
                let (Some(rules), Some(command)) = (rules, denial_command(&denials[index])) else {
                    continue;
                };
                self.push(
                    ChooserItem::new(
                        format!("Zezwól dokładnie: {command}"),
                        format!("Reguły tylko dla bieżącego tasku: {}", rules.join(", ")),
                    ),
                    Plan::Permission(PermissionDecisionDto {
                        denial_index: Some(index),
                        decision: PermissionDecisionKindDto::Allow,
                        scope: Some(PermissionGrantScopeDto::ExactOperation),
                        pattern: None,
                        message: None,
                    }),
                );
            }
        }
        if let Some(rules) = all(&group).filter(|rules| Some(rules) != all(&exact).as_ref()) {
            self.push(
                ChooserItem::new(
                    format!("Zezwól na grupę: {}", rules.join(", ")),
                    "Zgoda obejmie te grupy poleceń do końca bieżącego tasku.",
                ),
                Plan::Permission(PermissionDecisionDto {
                    denial_index: None,
                    decision: PermissionDecisionKindDto::Allow,
                    scope: Some(PermissionGrantScopeDto::CommandGroup),
                    pattern: None,
                    message: None,
                }),
            );
        }
        // The operator's own rule, for what the rules above do not cover:
        // a program outside the group list, or `*` for every command.
        let patternable = !denials.is_empty()
            && denials.iter().all(|denial| {
                denial.tool_name == "Bash"
                    && is_approvable_permission_reason(denial.reason_type.as_deref())
            });
        if resumable && patternable {
            let first = denial_command(&denials[0]).unwrap_or_default();
            self.push(
                ChooserItem::new(
                    "Zezwól według wzorca…",
                    "Wpisz całe polecenie albo jego początek z * na końcu, np. npm run *. \
                     Zgoda obejmie pasujące polecenia Bash do końca bieżącego tasku.",
                )
                .with_field(
                    ChooserField::text(PATTERN_FIELD)
                        .required()
                        .with_value(first)
                        .described(
                            "Polecenie złożone (&&, ;, |, pętla) Claude Code sprawdza \
                             po częściach: wtedy pomoże tylko *. Sam * zezwala na KAŻDE \
                             polecenie Bash.",
                        ),
                ),
                Plan::Permission(PermissionDecisionDto {
                    denial_index: None,
                    decision: PermissionDecisionKindDto::Allow,
                    scope: Some(PermissionGrantScopeDto::Pattern),
                    pattern: None,
                    message: None,
                }),
            );
        }
    }

    /// The pattern typed for a `pattern` grant, refused when no rule can
    /// carry it; `None` for every other decision.
    pub(in crate::tui::task_app::panel) fn permission_pattern(
        decision: &PermissionDecisionDto,
        submission: &Submission,
    ) -> Result<Option<String>, String> {
        if decision.scope != Some(PermissionGrantScopeDto::Pattern) {
            return Ok(None);
        }
        let pattern = Self::value_of(submission, PATTERN_FIELD)
            .unwrap_or_default()
            .trim();
        if bash_pattern_rule(pattern).is_none() {
            return Err(
                "Wzorzec nie może być pusty ani zawierać nawiasów lub znaków sterujących."
                    .to_string(),
            );
        }
        Ok(Some(pattern.to_string()))
    }
}

/// The allow rules that would grant `denial` at `scope`, the same the
/// engine grants ([`narsor_protocol::api::bash_grant_rules`]).
fn grant_rules(denial: &PermissionDenialView, scope: BashGrantScope) -> Option<Vec<String>> {
    if denial.tool_name != "Bash" || !is_approvable_permission_reason(denial.reason_type.as_deref())
    {
        return None;
    }
    bash_grant_rules(denial_command(denial)?, scope)
}

/// The trimmed command of a Bash denial; `None` when the input has none.
fn denial_command(denial: &PermissionDenialView) -> Option<&str> {
    denial
        .input
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
}

/// Why `denial` cannot be granted, in a few words; `None` when it can.
fn why_not_grantable(denial: &PermissionDenialView, resumable: bool) -> Option<String> {
    if !is_approvable_permission_reason(denial.reason_type.as_deref()) {
        return Some(format!(
            "blokada polityki Claude Code ({})",
            denial.reason_type.as_deref().unwrap_or("bez powodu")
        ));
    }
    if denial.tool_name != "Bash" {
        return Some(format!(
            "zgodę można dać tylko na polecenia Bash, nie na {}",
            denial.tool_name
        ));
    }
    if grant_rules(denial, BashGrantScope::Exact).is_none() {
        return Some(
            "polecenie ma konstrukcję, której reguła nie opisze dokładnie \
             ($(…), zmienna, glob, przekierowanie do pliku, nawiasy)"
                .to_string(),
        );
    }
    (!resumable).then(|| "sesji agenta nie da się wznowić".to_string())
}
