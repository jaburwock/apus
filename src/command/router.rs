//! Resolves and dispatches commands. Resolution happens once, then each outcome's
//! audience decides which [`DispatchTarget`] method performs it.
//!
//! Each outcome takes effect before its follow-ups or any later line.

use crate::command::protocol::{CommandOutcome, DispatchReport, DispatchTarget, GenomeCommand, TableCommand};

const MAX_FOLLOW_UP_DEPTH: usize = 8;

/// File commands with a fixed owner and shared settings can reach a backgrounded tab.
fn is_broadcast(outcome: &CommandOutcome) -> bool {
    matches!(
        outcome,
        CommandOutcome::Genome(
            GenomeCommand::ActiveBam(_) | GenomeCommand::AddBam(_) | GenomeCommand::ChooseVariantBam { .. }
        ) | CommandOutcome::Table(TableCommand::ActiveVcf(_))
            | CommandOutcome::Setting(_)
    )
}

/// Resolves a command batch and applies its scalar outcomes to `target`.
///
/// Audience decides the method: shell-owned outcomes go to `apply_shell`, outcomes
/// whose recipient is not the active tab to `apply_broadcast`, everything else to
/// `apply_active`. Effects take hold before the next outcome, which lets a sequence's
/// `:focustab` redirect the steps after it. `pipefail` controls whether an applied failure stops
/// the sequence; resolution errors always apply nothing because resolution completes first.
pub fn route_command(target: &mut impl DispatchTarget, input: &str, pipefail: bool) -> DispatchReport {
    let lines = match super::parse::resolve_batch(input) {
        Ok(lines) => lines,
        Err(message) => return DispatchReport::failure(message),
    };

    let mut report = DispatchReport::default();
    for line in lines {
        if let Err(error) = route_line(target, line) {
            report.failures.push(error.message);
            if pipefail {
                break;
            }
        }
    }
    report
}

/// Applies a bar submission, which excludes any command reserved for key bindings.
pub fn route_bar_command(target: &mut impl DispatchTarget, input: &str) -> Result<(), DispatchError> {
    let lines = super::parse::resolve_bar_batch(input).map_err(|message| DispatchError {
        message,
        follow_up: false,
    })?;
    for line in lines {
        route_line(target, line)?;
    }
    Ok(())
}

fn route_line(target: &mut impl DispatchTarget, line: super::parse::ResolvedLine) -> Result<(), DispatchError> {
    route_outcome(target, line.outcome).map_err(|error| DispatchError {
        message: format!("line {} ({:?}): {}", line.number, line.text, error.message),
        ..error
    })
}

/// A failed application, distinguishing a submitted command from the follow-ups it emitted.
#[derive(Debug, PartialEq, Eq)]
pub struct DispatchError {
    pub message: String,
    pub follow_up: bool,
}

/// Applies one resolved outcome and all follow-ups in depth-first order.
pub fn route_outcome(target: &mut impl DispatchTarget, outcome: CommandOutcome) -> Result<(), DispatchError> {
    dispatch(target, outcome, 0)
}

fn dispatch(target: &mut impl DispatchTarget, outcome: CommandOutcome, depth: usize) -> Result<(), DispatchError> {
    if outcome == CommandOutcome::None {
        return Ok(());
    }
    if depth > MAX_FOLLOW_UP_DEPTH {
        return Err(DispatchError {
            message: format!("command follow-up depth exceeded ({MAX_FOLLOW_UP_DEPTH})"),
            follow_up: true,
        });
    }
    let applied = match outcome {
        CommandOutcome::Shell(command) => target.apply_shell(command),
        outcome if is_broadcast(&outcome) => target.apply_broadcast(outcome),
        outcome => target.apply_active(outcome),
    };
    let outcomes = applied.into_follow_ups().map_err(|message| DispatchError {
        message,
        follow_up: depth > 0,
    })?;
    for follow_up in outcomes {
        dispatch(target, follow_up, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::command::protocol::{Applied, GenomeCommand, HelpContent, ListMove, ShellCommand};

    /// Which port method received an outcome, so a test can assert audience rather
    /// than infer it from a viewer's state.
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Shell(ShellCommand),
        Active(CommandOutcome),
        Broadcast(CommandOutcome),
    }

    #[derive(Default)]
    struct Recorder {
        calls: Vec<Call>,
        response: Option<Applied>,
        script: VecDeque<Applied>,
    }

    impl Recorder {
        fn answering(applied: Applied) -> Self {
            Self {
                response: Some(applied),
                ..Self::default()
            }
        }

        fn answer(&mut self) -> Applied {
            self.script
                .pop_front()
                .or_else(|| self.response.clone())
                .unwrap_or(Applied::Ok)
        }
    }

    impl DispatchTarget for Recorder {
        fn apply_shell(&mut self, command: ShellCommand) -> Applied {
            self.calls.push(Call::Shell(command));
            self.answer()
        }

        fn apply_active(&mut self, outcome: CommandOutcome) -> Applied {
            self.calls.push(Call::Active(outcome));
            self.answer()
        }

        fn apply_broadcast(&mut self, outcome: CommandOutcome) -> Applied {
            self.calls.push(Call::Broadcast(outcome));
            self.answer()
        }
    }

    #[test]
    fn a_shell_outcome_reaches_only_the_shell() {
        let mut target = Recorder::default();

        assert!(route_command(&mut target, ":focustab VCFViewer", true).is_ok());
        assert_eq!(
            target.calls,
            vec![Call::Shell(ShellCommand::FocusTab("VCFViewer".to_string()))]
        );
    }

    #[test]
    fn a_broadcast_outcome_reaches_only_the_broadcast_port() {
        let mut target = Recorder::default();

        assert!(route_command(&mut target, ":set activevcf testdata/vcf/sample.vcf", true).is_ok());
        assert_eq!(
            target.calls,
            vec![Call::Broadcast(
                TableCommand::ActiveVcf("testdata/vcf/sample.vcf".into()).into()
            )]
        );
    }

    #[test]
    fn a_setting_reaches_only_the_broadcast_port() {
        let mut target = Recorder::default();

        assert!(route_command(&mut target, ":set ruler false", true).is_ok());
        assert_eq!(
            target.calls,
            vec![Call::Broadcast(CommandOutcome::Setting(
                crate::setting::SettingValue::Ruler(false)
            ))]
        );
    }

    #[test]
    fn an_ordinary_outcome_reaches_the_active_viewer() {
        let mut target = Recorder::default();

        assert!(route_command(&mut target, ":goto 1k", true).is_ok());
        assert!(matches!(
            target.calls.as_slice(),
            [Call::Active(CommandOutcome::Goto { .. })]
        ));
    }

    #[test]
    fn a_rejection_keeps_the_receivers_message() {
        let mut target = Recorder::answering(Applied::Err("owned rejection".to_string()));

        let failures = route_command(&mut target, ":goto 1k", true).failures;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("line 1 (\":goto 1k\")"));
        assert!(failures[0].ends_with("owned rejection"));
    }

    #[test]
    fn a_not_applicable_answer_becomes_an_error_for_targeted_dispatch() {
        let mut target = Recorder::answering(Applied::NotApplicable);

        let failures = route_command(&mut target, ":goto 1k", true).failures;
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn a_syntax_error_in_any_line_applies_nothing() {
        let mut target = Recorder::default();

        let report = route_command(&mut target, ":goto 10\n:notacommand\n:goto 20", true);

        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].contains("line 2"));
        assert!(target.calls.is_empty());
    }

    #[test]
    fn blank_lines_crlf_and_bare_colons_are_ignored() {
        let mut target = Recorder::default();

        let report = route_command(&mut target, "\r\n : \r\n:goto 1\r\n\n", true);

        assert!(report.is_ok());
        assert_eq!(target.calls.len(), 1);
    }

    /// A failure names the line it came from, however many lines before it were skipped.
    ///
    /// Skipped lines are the hazard: blank ones, whitespace-only ones, and bare colons all vanish
    /// between the input and the dispatched outcomes, so a reported number that counted only the
    /// surviving lines would drift further with each one. The whitespace-only cases are the ones
    /// that bite — a line that is skipped by one rule and kept by another shifts every later
    /// attribution by one, while a wholly empty line is dropped by any spelling of the rule.
    ///
    /// The expectation is derived from the input — the 1-based index of the line whose text is the
    /// failing command — rather than pinned, so the case list can grow without hand-recounting.
    #[test]
    fn a_failure_names_its_line_however_many_were_skipped_before_it() {
        for input in [
            ":goto 9",
            "\n:goto 9",
            "   \n:goto 9",
            ": \n:goto 9",
            "\r\n \t \r\n\n:goto 9",
            ":\n\n   \n\r\n:goto 9",
        ] {
            let expected = input
                .lines()
                .position(|line| line.trim() == ":goto 9")
                .expect("every case must contain the failing command")
                + 1;
            let mut target = Recorder::answering(Applied::Err("rejected".to_string()));

            let report = route_command(&mut target, input, true);

            assert_eq!(
                report.failures,
                vec![format!("line {expected} (\":goto 9\"): rejected")],
                "{input:?} must attribute the failure to line {expected}"
            );
        }
    }

    #[test]
    fn pipefail_stops_after_the_first_failure() {
        let mut target = Recorder::answering(Applied::Err("rejected".to_string()));

        let report = route_command(&mut target, ":goto 1\n:goto 2", true);

        assert_eq!(report.failures, vec!["line 1 (\":goto 1\"): rejected".to_string()]);
        assert_eq!(target.calls.len(), 1);
    }

    #[test]
    fn without_pipefail_all_failures_are_retained() {
        let mut target = Recorder::answering(Applied::Err("rejected".to_string()));

        let report = route_command(&mut target, ":goto 1\n:goto 2", false);

        assert_eq!(
            report.failures,
            vec![
                "line 1 (\":goto 1\"): rejected".to_string(),
                "line 2 (\":goto 2\"): rejected".to_string(),
            ]
        );
        assert_eq!(target.calls.len(), 2);
    }

    #[test]
    fn follow_ups_run_before_the_next_line() {
        use ShellCommand::*;

        let focus = FocusTab("VCFViewer".into());
        let follow_up: CommandOutcome = TableCommand::GotoLocus {
            contig: "chr2".into(),
            position: crate::basepair::BasePair(37),
        }
        .into();
        let mut target = Recorder {
            script: [
                Applied::FollowUps(vec![focus.clone().into(), OpenPalettePane.into()]),
                Applied::FollowUps(vec![follow_up.clone()]),
            ]
            .into(),
            ..Recorder::default()
        };

        let report = route_command(&mut target, ":help\n:prevfeature", true);

        assert!(report.is_ok(), "{:?}", report.failures);
        assert_eq!(
            target.calls,
            [
                Call::Shell(OpenHelp(HelpContent::Settings)),
                Call::Shell(focus),
                Call::Active(follow_up),
                Call::Shell(OpenPalettePane),
                Call::Active(GenomeCommand::JumpToFeature { forward: false }.into()),
            ]
        );
    }

    #[test]
    fn a_failed_follow_up_stops_its_chain() {
        use ShellCommand::*;

        let focus = FocusTab("VCFViewer".into());
        let mut target = Recorder {
            script: [
                Applied::FollowUps(vec![focus.clone().into(), OpenPalettePane.into()]),
                Applied::FollowUps(vec![CommandOutcome::JumpToPeer, Help(ListMove::Close).into()]),
                Applied::Err("failed follow-up".to_string()),
            ]
            .into(),
            ..Recorder::default()
        };

        let report = route_command(&mut target, ":help\n:prevfeature", false);

        assert_eq!(report.failures, ["line 1 (\":help\"): failed follow-up"]);
        assert_eq!(
            target.calls,
            [
                Call::Shell(OpenHelp(HelpContent::Settings)),
                Call::Shell(focus),
                Call::Active(CommandOutcome::JumpToPeer),
                Call::Active(GenomeCommand::JumpToFeature { forward: false }.into()),
            ]
        );
    }

    #[test]
    fn follow_up_depth_is_capped() {
        let mut target = Recorder::answering(Applied::FollowUps(vec![CommandOutcome::JumpToPeer]));

        let result = route_outcome(&mut target, CommandOutcome::JumpToPeer);

        let error = result.expect_err("the cycle must be rejected");
        assert!(error.follow_up);
        assert!(error.message.contains("follow-up depth"));
        assert_eq!(target.calls.len(), MAX_FOLLOW_UP_DEPTH + 1);
    }

    #[derive(Default)]
    struct FocusRecorder {
        active: usize,
        active_calls: Vec<(usize, CommandOutcome)>,
    }

    impl DispatchTarget for FocusRecorder {
        fn apply_shell(&mut self, command: ShellCommand) -> Applied {
            if matches!(command, ShellCommand::FocusTab(_)) {
                self.active = 1;
            }
            Applied::Ok
        }

        fn apply_active(&mut self, outcome: CommandOutcome) -> Applied {
            self.active_calls.push((self.active, outcome));
            Applied::Ok
        }

        fn apply_broadcast(&mut self, _outcome: CommandOutcome) -> Applied {
            Applied::Ok
        }
    }

    #[test]
    fn focus_redirects_the_following_active_outcome() {
        let mut target = FocusRecorder::default();

        let report = route_command(&mut target, ":focustab VCFViewer\n:goto 10", true);

        assert!(report.is_ok());
        assert_eq!(target.active_calls.len(), 1);
        assert_eq!(target.active_calls[0].0, 1);
    }
}
