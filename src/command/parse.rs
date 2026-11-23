//! Converts typed command text into resolved [`CommandOutcome`] values.
//!
//! Resolution is deliberately app-global: this module receives text and produces
//! messages, but never reads viewer state or applies an outcome. Viewer-dependent
//! validation is represented by carrying unresolved names or targets for the eventual
//! receiver to interpret.
//!
//! [`resolve_command`] handles one line, including the bar's `:`, `/`, and `c`
//! shorthands. [`resolve_batch`] resolves nonblank newline-separated lines in order
//! and wraps them in a protocol batch; it does not dispatch them.
//!
//! Coordinates are converted from display values at this boundary, while one `goto`
//! outcome preserves both genome-coordinate and VCF-row readings. Generated quoted
//! arguments must remain accepted here because key actions and startup replay return
//! through the same parser.
//!
//! The trap is adding viewer knowledge to make parsing appear more precise: that would
//! break the command/viewer_app seam and make dispatch order observable.

use crate::basepair::BasePair;
use crate::command::protocol::{
    CommandOutcome, Direction, FeatureFilterEdit, GenomeCommand, LineTrigger, ScrollStep, ShellCommand, TableCommand,
};
use crate::locus;
use crate::setting::{ReadColor, Setting, SettingKind, SettingValue};

use super::{CommandDescriptor, expand_path, find_command, unquote};

/// Resolves one submitted command line. A bare trigger yields `None`.
pub fn resolve_command(raw: &str) -> Result<CommandOutcome, String> {
    resolve_line(raw, false)
}

fn resolve_line(raw: &str, from_bar: bool) -> Result<CommandOutcome, String> {
    let line = strip_trigger(raw);
    let (base, target, payload) = split_line(&line);

    if base.is_empty() {
        return Ok(CommandOutcome::None);
    }
    match lookup(base, target, payload)? {
        Found::Command { descriptor, .. } if from_bar && !descriptor.typable => {
            Err(format!("{} is only available through key bindings", descriptor.name))
        }
        Found::Command { descriptor, arg } => (descriptor.run)(arg),
        Found::Setting { key, name, arg } => resolve_value(key, name, arg).map(CommandOutcome::Setting),
    }
}

/// One resolved line, carrying the position it came from.
///
/// The line number travels with the outcome because only this function knows which lines it
/// skipped. A consumer that re-derived that from the input would be maintaining a second copy of
/// the skip rule, and a failure would be misattributed the moment the two spellings drifted.
#[derive(Debug, PartialEq, Eq)]
pub struct ResolvedLine {
    /// 1-based, as reported to the user.
    pub number: usize,
    /// The line as written, for the failure message that names it.
    pub text: String,
    pub outcome: CommandOutcome,
}

/// Resolves a newline-separated command batch without applying any of its outcomes.
///
/// Blank lines and whole-line comments are dropped; every surviving line yields one [`ResolvedLine`],
/// including the ones that resolve to [`CommandOutcome::None`].
pub fn resolve_batch(input: &str) -> Result<Vec<ResolvedLine>, String> {
    resolve_lines(input, resolve_command)
}

pub(super) fn resolve_bar_batch(input: &str) -> Result<Vec<ResolvedLine>, String> {
    resolve_lines(input, |line| resolve_line(line, true))
}

fn resolve_lines(
    input: &str,
    resolve: impl Fn(&str) -> Result<CommandOutcome, String>,
) -> Result<Vec<ResolvedLine>, String> {
    let mut resolved = Vec::new();
    for (line_index, line) in input.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let number = line_index + 1;
        let outcome = resolve(line).map_err(|error| format!("line {number} ({line:?}): {error}"))?;
        resolved.push(ResolvedLine {
            number,
            text: line.to_string(),
            outcome,
        });
    }
    Ok(resolved)
}

pub(super) fn no_args(args: &str, outcome: CommandOutcome) -> Result<CommandOutcome, String> {
    if args.is_empty() {
        Ok(outcome)
    } else {
        Err(format!("unexpected arguments: {args:?}"))
    }
}

pub(super) fn run_scroll(args: &str, direction: Direction) -> Result<CommandOutcome, String> {
    let step = match unquote(args).as_deref() {
        Some("near") => ScrollStep::Near,
        Some("near-x10") => ScrollStep::NearX10,
        Some("mid") => ScrollStep::Mid,
        Some("far") => ScrollStep::Far,
        _ => {
            return Err(format!(
                "invalid scroll step: {args:?} (expected near, near-x10, mid, or far)"
            ));
        }
    };
    Ok(CommandOutcome::Scroll { direction, step })
}

pub(super) fn run_open_line(args: &str) -> Result<CommandOutcome, String> {
    let trigger = match unquote(args).as_deref() {
        Some(":") => LineTrigger::Command,
        Some("/") => LineTrigger::Slash,
        Some("c") => LineTrigger::Contig,
        _ => return Err(format!("invalid command line trigger: {args:?} (expected :, /, or c)")),
    };
    Ok(CommandOutcome::Shell(ShellCommand::OpenLine(trigger)))
}

/// Accepts a quoted name as well as a bare one: `encode_arg` quotes every value it emits, so a
/// generated `:set activecontig` and a hand-typed one must mean the same contig.
pub(super) fn run_active_contig(args: &str) -> Result<CommandOutcome, String> {
    let name = unquote(args).unwrap_or_default();
    Ok(CommandOutcome::Genome(GenomeCommand::ActiveContig(name)))
}

pub(super) fn run_active_fasta(args: &str) -> Result<CommandOutcome, String> {
    parse_path_arg("activefasta", args).map(|path| CommandOutcome::Genome(GenomeCommand::ActiveFasta(path)))
}

pub(super) fn run_active_bam(args: &str) -> Result<CommandOutcome, String> {
    parse_path_arg("activebam", args).map(|path| CommandOutcome::Genome(GenomeCommand::ActiveBam(path)))
}

pub(super) fn run_add_bam(args: &str) -> Result<CommandOutcome, String> {
    parse_path_arg("addbam", args).map(|path| CommandOutcome::Genome(GenomeCommand::AddBam(path)))
}

pub(super) fn run_active_vcf(args: &str) -> Result<CommandOutcome, String> {
    parse_path_arg("activevcf", args).map(|path| CommandOutcome::Table(TableCommand::ActiveVcf(path)))
}

pub(super) fn run_focus_tab(args: &str) -> Result<CommandOutcome, String> {
    unquote(args)
        .filter(|name| !name.trim().is_empty())
        .map(|name| CommandOutcome::Shell(ShellCommand::FocusTab(name)))
        .ok_or_else(|| "focustab requires a tab name".to_string())
}

pub(super) fn run_feature_filter(args: &str) -> Result<CommandOutcome, String> {
    parse_feature_filter_setting(args).map(|edit| CommandOutcome::Genome(GenomeCommand::FeatureFilter(edit)))
}

pub(super) fn run_horizontal_zoom(args: &str) -> Result<CommandOutcome, String> {
    parse_zoom_setting(true, args)
}

pub(super) fn run_alignment_vertical_zoom(args: &str) -> Result<CommandOutcome, String> {
    parse_zoom_setting(false, args)
}

/// Parses a quoted, expanded path argument.
fn parse_path_arg(setting: &str, args: &str) -> Result<std::path::PathBuf, String> {
    let arg = unquote(args).ok_or_else(|| format!("{setting} requires a path"))?;
    expand_path(&arg)
}

/// A table command or a declared setting.
enum Found<'a> {
    Command {
        descriptor: &'static CommandDescriptor,
        arg: &'a str,
    },
    Setting {
        key: Setting,
        /// Name as typed, for error messages.
        name: &'a str,
        arg: &'a str,
    },
}

/// Splits a command into base, target, and payload.
fn split_line(line: &str) -> (&str, &str, &str) {
    let (base, rest) = split_first_word(line);
    if base == "set" {
        let (target, payload) = split_first_word(rest);
        (base, target, payload)
    } else {
        (base, "", rest)
    }
}

/// Resolves a command or setting from the split fields.
fn lookup<'a>(base: &'a str, target: &'a str, payload: &'a str) -> Result<Found<'a>, String> {
    let command = |descriptor| {
        Ok(Found::Command {
            descriptor,
            arg: payload,
        })
    };
    if base != "set" {
        return command(find_command(base).ok_or_else(|| format!("unrecognised command: {base}"))?);
    }
    if target.is_empty() {
        return Err("set requires an argument".to_string());
    }
    if let Some(descriptor) = find_command(&format!("set {target}")) {
        return command(descriptor);
    }
    match Setting::from_name(target) {
        Some(key) => Ok(Found::Setting {
            key,
            name: target,
            arg: payload,
        }),
        None => Err(format!("unrecognised setting: {target}")),
    }
}

/// Expands a leading trigger, if there is one.
///
/// Triggers are the *command bar's* shorthand, not part of the command language: a line arriving
/// from a palette or config file is already bare, and must resolve the same way as the `:`-prefixed
/// spelling a user types. So an unrecognised leading character is not an error here — it is a bare
/// line, and an unknown first *word* is refused by [`lookup`] with a message that names it.
///
/// Total: every input yields a line, and never an `Err`.
fn strip_trigger(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('/') {
        return format!("goto {}", rest.trim());
    }
    if let Some(rest) = trimmed.strip_prefix(':') {
        return rest.trim().to_string();
    }
    // `c` is the one trigger that is a bare letter rather than punctuation, so it collides with any
    // command whose name starts with one — `contigselector` would read as `set activecontig
    // ontigselector`. Resolving the whole word first gives a real command right of way; only a
    // leftover like `c17` is a contig switch.
    if let Some(rest) = trimmed.strip_prefix('c')
        && find_command(split_first_word(trimmed).0).is_none()
    {
        return format!("set activecontig {}", rest.trim());
    }
    trimmed.to_string()
}

/// Splits off the first whitespace-delimited word.
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (s, ""),
    }
}

/// Parses a setting value according to its declared type.
pub(crate) fn resolve_value(key: Setting, name: &str, raw: &str) -> Result<SettingValue, String> {
    let arg = unquote(raw).ok_or_else(|| format!("{name} requires a value"))?;
    let arg = arg.as_str();
    match key.kind() {
        SettingKind::Bool => parse_bool(arg).and_then(|v| SettingValue::from_bool(key, v)),
        SettingKind::BasePair => BasePair::parse(arg).and_then(|v| SettingValue::from_basepair(key, v)),
        SettingKind::BoundedInteger { .. } => arg
            .parse()
            .ok()
            .and_then(|v| SettingValue::from_bounded_integer(key, v)),
        SettingKind::ReadColor => ReadColor::parse(arg).and_then(|v| SettingValue::from_read_color(key, v)),
    }
    .ok_or_else(|| format!("invalid value for {name}: {arg:?}"))
}

/// Parses common boolean spellings.
fn parse_bool(arg: &str) -> Option<bool> {
    match arg.to_ascii_lowercase().as_str() {
        "true" | "t" | "on" | "1" => Some(true),
        "false" | "f" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Resolves a goto, converting display coordinates to internal coordinates.
///
/// Unquoted whole, before either reading splits on the axis separator: the VCF jump emits its
/// position through [`crate::command::encode_arg`], which quotes unconditionally, so the generated
/// and typed spellings have to arrive at the same outcome.
pub(super) fn run_goto(args: &str) -> Result<CommandOutcome, String> {
    let args = unquote(args).unwrap_or_default();
    let Some((horizontal, vertical)) = parse_goto(&args) else {
        return Err(format!("invalid value for goto: {args:?}"));
    };
    Ok(CommandOutcome::Goto {
        horizontal,
        vertical,
        line: parse_line(&args),
    })
}

/// Resolves a VCF locus lookup. The two arguments are intentionally parsed as
/// one pair: a missing or extra token must not become a silently different
/// command, and generated arguments are quoted by [`crate::command::encode_arg`].
pub(super) fn run_goto_locus(args: &str) -> Result<CommandOutcome, String> {
    let arguments = split_arguments(args)?;
    if arguments.len() != 2 {
        return Err(format!(
            "gotolocus requires exactly two arguments, got {}",
            arguments.len()
        ));
    }

    let contig = unquote(&arguments[0])
        .ok_or_else(|| "gotolocus requires a non-empty contig".to_string())?
        .to_string();
    let display_position = unquote(&arguments[1])
        .and_then(|value| BasePair::parse(&value))
        .ok_or_else(|| format!("gotolocus requires a valid 1-based position: {:?}", arguments[1]))?;

    Ok(CommandOutcome::Table(TableCommand::GotoLocus {
        contig,
        position: BasePair::from_display(display_position.as_u64()),
    }))
}

/// Splits exactly the shell's simple quoted/bare argument grammar. Quoted tokens use the same
/// backslash escapes as [`crate::command::unquote`], so an escaped quote does not end the token.
fn split_arguments(input: &str) -> Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut rest = input.trim();
    while !rest.is_empty() {
        if let Some(after_quote) = rest.strip_prefix('"') {
            let mut escaped = false;
            let end = after_quote
                .char_indices()
                .find_map(|(offset, character)| {
                    if escaped {
                        escaped = false;
                        return None;
                    }
                    if character == '\\' {
                        escaped = true;
                        return None;
                    }
                    (character == '"').then_some(offset)
                })
                .ok_or_else(|| "unterminated quoted command argument".to_string())?;
            let token_end = end + 2;
            let token = &rest[..token_end];
            let after = &rest[token_end..];
            if after.chars().next().is_some_and(|character| !character.is_whitespace()) {
                return Err("quoted command arguments must be separated by whitespace".to_string());
            }
            arguments.push(token.to_string());
            rest = after.trim_start();
        } else {
            let (token, after) = match rest.split_once(char::is_whitespace) {
                Some((token, after)) => (token, after),
                None => (rest, ""),
            };
            arguments.push(token.to_string());
            rest = after.trim_start();
        }
    }
    Ok(arguments)
}

/// Reads the argument as a row number, or `None` if it is two-axis genome grammar.
///
/// Shares the `k`/`M` vocabulary of the bp axes: `10k` names a row as usefully as it names a
/// position, and a table long enough to want the shorthand is exactly where it earns its place.
///
/// A comma is still refused, but for a different reason than the suffixes once were — it is
/// [`parse_goto`]'s axis separator, so `100,50` already means "horizontal 100, vertical 50" and
/// cannot also mean the row 10050. That collision is why the comma-grouped number the title bar
/// displays is not readable back here.
///
/// The comma test is deliberate rather than redundant: [`BasePair::parse`] happens to reject a
/// comma today, so leaving this to it would make the axis-separator rule an accident of the number
/// parser. Anyone teaching that parser digit grouping would silently hand `1,234` two meanings.
fn parse_line(arg: &str) -> Option<u64> {
    let arg = arg.trim();
    if arg.is_empty() || arg.contains(',') {
        return None;
    }
    BasePair::parse(arg).map(|parsed| parsed.as_u64())
}

/// Parses horizontal and optional vertical goto values.
fn parse_goto(arg: &str) -> Option<(Option<BasePair>, Option<BasePair>)> {
    let parse = |s: &str| -> Option<Option<BasePair>> {
        let s = s.trim();
        if s.is_empty() {
            Some(None)
        } else {
            BasePair::parse(s).map(|typed| Some(BasePair::from_display(typed.as_u64())))
        }
    };
    let (horizontal, vertical) = match arg.trim().split_once(',') {
        Some((h, v)) => (parse(h)?, parse(v)?),
        None => (parse(arg)?, None),
    };
    (horizontal.is_some() || vertical.is_some()).then_some((horizontal, vertical))
}

/// Resolves a load path and its cache path.
fn resolve_load(args: &str, build_missing: bool) -> Result<CommandOutcome, String> {
    let path = unquote(args).ok_or_else(|| "load requires a path".to_string())?;
    let gtf_path = expand_path(&path)?;
    let cache_path = locus::cache_path_for(&gtf_path);
    Ok(CommandOutcome::Genome(GenomeCommand::Load {
        gtf_path,
        cache_path,
        build_missing,
    }))
}

/// Resolves `:load`, which requires an existing index cache.
pub(super) fn run_load(args: &str) -> Result<CommandOutcome, String> {
    resolve_load(args, false)
}

/// Resolves `:load_index`, which builds the cache when it is absent.
pub(super) fn run_load_index(args: &str) -> Result<CommandOutcome, String> {
    resolve_load(args, true)
}

/// Resolves one zoom axis.
fn parse_zoom_setting(is_horizontal: bool, args: &str) -> Result<CommandOutcome, String> {
    let name = if is_horizontal {
        "horizontalzoom"
    } else {
        "alignmentverticalzoom"
    };
    let args = args.trim();
    if args.is_empty() {
        return Err(format!("{name} requires a value (close or mid)"));
    }
    let to_mid = match unquote(args).unwrap_or_default().to_ascii_lowercase().as_str() {
        "mid" => true,
        "close" => false,
        _ => return Err(format!("invalid value for {name}: {args:?} (expected close or mid)")),
    };
    Ok(CommandOutcome::Genome(GenomeCommand::Zoom {
        horizontal_to_mid: is_horizontal.then_some(to_mid),
        alignment_v_to_mid: (!is_horizontal).then_some(to_mid),
    }))
}

/// Parses an include, exclude, or clear feature-filter edit.
fn parse_feature_filter_setting(rest: &str) -> Result<FeatureFilterEdit, String> {
    let (raw_word, arg) = split_first_word(rest);
    let word = unquote(raw_word).unwrap_or_default();
    match word.as_str() {
        "" => Err("featurefilter requires include/exclude/clear".to_string()),
        "clear" => Ok(FeatureFilterEdit::Clear),
        "include" | "exclude" => {
            if arg.is_empty() {
                return Err(format!("featurefilter {word} requires a value"));
            }
            // A quoted verb marks the emitted spelling. The original grammar strips the
            // optional quotes around a list and treats its backslashes as literal text.
            let value = if raw_word.starts_with('"') {
                unquote(arg).unwrap_or_default()
            } else {
                let arg = arg.strip_prefix('"').unwrap_or(arg);
                arg.strip_suffix('"').unwrap_or(arg).to_string()
            };
            let substrings: Vec<String> = value.split('|').map(str::to_string).collect();
            Ok(if word == "include" {
                FeatureFilterEdit::Include(substrings)
            } else {
                FeatureFilterEdit::Exclude(substrings)
            })
        }
        _ => Err(format!(
            "invalid featurefilter action: {word:?} (expected include, exclude, or clear)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated and typed spellings of a contig must resolve identically.
    ///
    /// `encode_arg` quotes unconditionally, so the VCF jump emits a quoted contig while a user types
    /// a bare one. If this resolver kept the quotes, the name would never match a reference contig
    /// and the jump would silently land nowhere.
    #[test]
    fn a_quoted_contig_resolves_to_the_same_name_as_a_bare_one() {
        let encoded = crate::command::encode_arg("chr1").expect("a plain contig must encode");

        assert_eq!(
            resolve_command(&format!(":set activecontig {encoded}")).expect("the generated form must resolve"),
            resolve_command(":set activecontig chr1").expect("the typed form must resolve"),
        );
    }

    #[test]
    fn generated_and_typed_gotolocus_spellings_resolve_identically() {
        let generated = format!(
            ":gotolocus {} {}",
            crate::command::encode_arg("chr1").expect("the contig must encode"),
            crate::command::encode_arg("174").expect("the position must encode"),
        );
        let typed = ":gotolocus chr1 174";

        assert_eq!(
            resolve_command(&generated).expect("the generated form must resolve"),
            resolve_command(typed).expect("the typed form must resolve"),
        );
        assert_eq!(
            resolve_command(&generated),
            Ok(CommandOutcome::Table(TableCommand::GotoLocus {
                contig: "chr1".to_string(),
                position: BasePair(173),
            }))
        );
    }

    #[test]
    fn gotolocus_requires_exactly_two_valid_arguments() {
        for line in [":gotolocus chr1", ":gotolocus chr1 10 extra", ":gotolocus chr1 nope"] {
            assert!(resolve_command(line).is_err(), "{line:?} must be rejected");
        }
        assert!(resolve_command(":gotolocus \"chr1\" \"10\"").is_ok());
    }

    /// The two commands differ in exactly this flag, and share their path resolution.
    #[test]
    fn load_and_load_index_differ_only_in_whether_they_may_build() {
        let outcome = |line: &str| resolve_command(line).expect("both commands must resolve");

        let (plain, indexing) = (
            outcome(":load \"testdata/gtf/gtf_subset.gtf.gz\""),
            outcome(":load_index \"testdata/gtf/gtf_subset.gtf.gz\""),
        );

        match (plain, indexing) {
            (
                CommandOutcome::Genome(GenomeCommand::Load {
                    gtf_path: a,
                    cache_path: cache_a,
                    build_missing: false,
                }),
                CommandOutcome::Genome(GenomeCommand::Load {
                    gtf_path: b,
                    cache_path: cache_b,
                    build_missing: true,
                }),
            ) => {
                assert_eq!(a, b, "both must resolve the same GTF path");
                assert_eq!(cache_a, cache_b, "both must resolve the same cache path");
            }
            other => panic!("expected a build_missing false/true pair, got {other:?}"),
        }
    }

    /// Each surviving line keeps the number it had in the input, so a later failure can name it
    /// without anyone re-deriving which lines were dropped.
    #[test]
    fn a_batch_resolves_each_nonblank_line_in_order_keeping_its_line_number() {
        let resolved = resolve_batch("\n:goto 10\r\n : \n:goto 20\n").expect("the batch must resolve");

        assert_eq!(
            resolved
                .iter()
                .map(|line| (line.number, line.outcome.clone()))
                .collect::<Vec<_>>(),
            vec![
                (2, resolve_command(":goto 10").unwrap()),
                (3, CommandOutcome::None),
                (4, resolve_command(":goto 20").unwrap()),
            ]
        );
    }

    #[test]
    fn a_batch_parse_error_names_its_line() {
        let error = resolve_batch(":goto 10\n:notacommand").unwrap_err();

        assert!(error.contains("line 2"));
        assert!(error.contains(":notacommand"));
    }

    /// A trigger is the bar's shorthand, so dropping it must not change what a line means — this is
    /// what lets a palette file hold bare `set palette …` lines. Checked over the whole registry
    /// rather than a sampled few, since a new command joining it is the case worth catching.
    ///
    /// Arguments come from the descriptor's own name, so each command is exercised with a payload
    /// it can actually resolve; commands that reject every argument shape are compared on their
    /// error instead, which is equally a resolution both spellings must agree on.
    #[test]
    fn every_command_resolves_the_same_bare_as_it_does_with_a_trigger() {
        for descriptor in super::super::registry() {
            for argument in ["", "1", "chr1 100"] {
                let bare = format!("{} {argument}", descriptor.name);
                assert_eq!(
                    resolve_command(&bare),
                    resolve_command(&format!(":{bare}")),
                    "`{bare}` must mean the same with and without its trigger"
                );
            }
        }
    }

    /// `:goto` is reachable from a generated line and `encode_arg` quotes unconditionally, so
    /// generated and typed spellings must resolve to the same jump.
    #[test]
    fn generated_and_typed_goto_spellings_resolve_identically() {
        for value in ["174", "10k", "1.5M", "100,50"] {
            let encoded = crate::command::encode_arg(value).expect("a goto argument must encode");
            assert_eq!(
                resolve_command(&format!(":goto {encoded}")),
                resolve_command(&format!(":goto {value}")),
                "{value:?} must mean the same quoted as bare"
            );
        }
    }

    /// `c` is a bare-letter trigger, so it shadows any command starting with one. `contigselector`
    /// is the collision that exists today: without the guard it resolves to a contig switch named
    /// `ontigselector`.
    #[test]
    fn a_command_starting_with_c_wins_over_the_contig_trigger() {
        assert_eq!(
            resolve_command("contigselector"),
            Ok(CommandOutcome::Genome(GenomeCommand::OpenContigSelector))
        );
        // The trigger still works for what it is for: a contig name is not a command.
        assert_eq!(
            resolve_command("c17"),
            Ok(CommandOutcome::Genome(GenomeCommand::ActiveContig("17".to_string())))
        );
    }

    /// Bare lines are what a palette or config file holds, and a file is not bound by code review,
    /// so resolution must be total over arbitrary text: every string either resolves or is refused,
    /// never a panic.
    #[test]
    fn arbitrary_bare_lines_never_panic() {
        // A small xorshift keeps this deterministic and dependency-free.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet: Vec<char> = "csetpal goin:/\"|()0123456789_-.\t\u{e9}".chars().collect();
        for _ in 0..20_000 {
            let len = (next() % 32) as usize;
            let line: String = (0..len).map(|_| alphabet[(next() as usize) % alphabet.len()]).collect();
            let _ = resolve_command(&line);
        }
    }

    #[test]
    fn a_pipe_stays_inside_a_feature_filter_argument() {
        let resolved = resolve_batch(":set featurefilter include A|B").expect("the filter must resolve");

        assert_eq!(
            resolved.into_iter().map(|line| line.outcome).collect::<Vec<_>>(),
            vec![CommandOutcome::Genome(GenomeCommand::FeatureFilter(
                FeatureFilterEdit::Include(vec!["A".to_string(), "B".to_string()])
            ))]
        );
    }

    /// Two near-identical names are only usable if `:help` tells them apart.
    #[test]
    fn help_distinguishes_the_two_load_commands() {
        let sections = super::super::help_sections(&[]);
        let describe = |name: &str| {
            sections
                .iter()
                .flat_map(|section| section.entries.iter())
                .find(|entry| entry.key == name)
                .map(|entry| entry.description)
                .unwrap_or_else(|| panic!("{name} must appear in :help"))
        };

        assert_ne!(
            describe("load"),
            describe("load_index"),
            "the two loads need descriptions a reader can choose between"
        );
    }

    /// The load-only failure names a command line, so that line must actually resolve — otherwise
    /// the advice is unpasteable and nothing would notice.
    #[test]
    fn the_line_named_by_a_failed_load_resolves_as_an_indexing_load() {
        // A path with a space, since quoting is what the advice has to get right.
        let gtf_path = std::path::Path::new("testdata/gtf subset.gtf.gz");
        // The real message, from the constructor `start_load` uses — not a restatement of it.
        let advice = locus::index_command_for(gtf_path).expect("the advice must be constructible");

        let outcome = resolve_command(&advice).expect("the advised line must be a valid command");

        match outcome {
            CommandOutcome::Genome(GenomeCommand::Load {
                gtf_path: resolved,
                build_missing: true,
                ..
            }) => assert!(
                resolved.ends_with("gtf subset.gtf.gz"),
                "the advice must round-trip the whole path, got {resolved:?}"
            ),
            other => panic!("pasting the advice back must build the index, got {other:?}"),
        }
    }

    #[test]
    fn a_load_advice_path_with_a_newline_stays_one_command() {
        let path = std::path::Path::new("gtf\nsubset.gtf.gz");

        let advice = locus::index_command_for(path).expect("escaped path advice must be constructible");
        match resolve_command(&advice).expect("escaped path advice must resolve") {
            CommandOutcome::Genome(GenomeCommand::Load {
                gtf_path,
                build_missing: true,
                ..
            }) => assert_eq!(gtf_path, path),
            other => panic!("escaped path advice must remain a load command, got {other:?}"),
        }
    }

    /// The command reads one input as both a 1-based genome coordinate and a 1-based row.
    /// Expected bp coordinates use the display arithmetic directly, independently of the
    /// production conversion helpers.
    #[test]
    fn goto_coordinates_and_rows_follow_display_input() {
        for (arg, display_value) in [
            ("1", 1u64),
            ("0", 0),
            ("500", 500),
            (" 1000 ", 1000),
            ("10k", 10_000),
            ("10K", 10_000),
            ("1.5M", 1_500_000),
            ("2m", 2_000_000),
        ] {
            let expected_internal = BasePair(display_value.saturating_sub(1));
            assert_eq!(parse_goto(arg), Some((Some(expected_internal), None)), "bp axis: {arg}");
            assert_eq!(parse_line(arg), Some(display_value), "row axis: {arg}");
            assert_eq!(
                run_goto(arg),
                Ok(CommandOutcome::Goto {
                    horizontal: Some(expected_internal),
                    vertical: None,
                    line: Some(display_value),
                }),
                "both readings: {arg}"
            );
        }

        for (arg, horizontal, vertical) in [
            ("100,50", Some(BasePair(99)), Some(BasePair(49))),
            ("100,", Some(BasePair(99)), None),
            (",50", None, Some(BasePair(49))),
        ] {
            assert_eq!(parse_goto(arg), Some((horizontal, vertical)), "axes: {arg}");
            assert_eq!(parse_line(arg), None, "comma separates axes, not row digits: {arg}");
        }

        for arg in ["", ",", "IL9R", "1,nope"] {
            assert_eq!(parse_goto(arg), None, "invalid or empty axes: {arg:?}");
            assert!(run_goto(arg).is_err(), "invalid goto must be rejected: {arg:?}");
        }
        for arg in ["", "100,50", "1,234", "100,", ",50", "IL9R", "-5"] {
            assert_eq!(parse_line(arg), None, "not a single row: {arg:?}");
        }
    }

    #[test]
    fn setting_names_and_aliases_resolve_typed_values() {
        for spec in Setting::specs() {
            let typed_values: Vec<SettingValue> = match spec.kind {
                SettingKind::Bool => [true, false]
                    .into_iter()
                    .map(|value| SettingValue::from_bool(spec.key, value).unwrap())
                    .collect(),
                SettingKind::BasePair => {
                    let value = BasePair(23);
                    vec![SettingValue::from_basepair(spec.key, value).unwrap()]
                }
                SettingKind::BoundedInteger { min, max } => [min, max]
                    .into_iter()
                    .map(|value| SettingValue::from_bounded_integer(spec.key, value).unwrap())
                    .collect(),
                SettingKind::ReadColor => ReadColor::ALL
                    .into_iter()
                    .map(|(_, value)| SettingValue::from_read_color(spec.key, value).unwrap())
                    .collect(),
            };
            let values: Vec<_> = typed_values
                .into_iter()
                .map(|value| (value.command_arg(), value))
                .collect();

            for name in std::iter::once(spec.name).chain(spec.aliases.iter().copied()) {
                for (arg, expected) in &values {
                    assert_eq!(
                        resolve_command(&format!(":set {name} {arg}")),
                        Ok(CommandOutcome::Setting(expected.clone())),
                        "{name} must resolve its declared typed value {arg}"
                    );
                }
            }

            if spec.kind == SettingKind::Bool {
                for (tokens, value) in [
                    (&["true", "t", "on", "1"][..], true),
                    (&["false", "f", "off", "0"][..], false),
                ] {
                    let expected = SettingValue::from_bool(spec.key, value).unwrap();
                    for token in tokens {
                        for spelling in [token.to_string(), token.to_ascii_uppercase()] {
                            assert_eq!(
                                resolve_command(&format!(":set {} {spelling}", spec.name)),
                                Ok(CommandOutcome::Setting(expected.clone())),
                                "boolean spelling {spelling:?} for {}",
                                spec.name
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn setting_requires_a_declared_name_and_value() {
        for line in [":set ruler", ":set movemin"] {
            assert!(resolve_command(line).is_err(), "{line:?} must require a value");
        }
        for line in [":set noruler", ":set ruler!", ":set ruler yes"] {
            assert!(resolve_command(line).is_err(), "{line:?} must be rejected");
        }
        assert!(resolve_command(":set ruler f").is_ok());
        assert!(resolve_command(":set movemin 10").is_ok());
        for name in ["readcolor", "readcolour"] {
            assert!(
                resolve_command(&format!(":set {name} sideways")).is_err(),
                "unknown read-colour value must be rejected through {name}"
            );
        }
    }

    #[test]
    fn local_area_scale_accepts_its_bounds_and_rejects_every_other_shape() {
        let SettingKind::BoundedInteger { min, max } = Setting::Localareascale.kind() else {
            panic!("localareascale must remain bounded");
        };

        for value in [min, max] {
            assert_eq!(
                resolve_command(&format!(":set localareascale {value}")),
                Ok(CommandOutcome::Setting(SettingValue::Localareascale(value)))
            );
        }

        for value in [min.checked_sub(1), max.checked_add(1)].into_iter().flatten() {
            assert!(resolve_command(&format!(":set localareascale {value}")).is_err());
        }
        for value in ["-1", "wide"] {
            assert!(
                resolve_command(&format!(":set localareascale {value}")).is_err(),
                "invalid value shape {value:?} must be rejected"
            );
        }
    }
}
