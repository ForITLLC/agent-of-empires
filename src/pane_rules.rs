//! Configurable pane-text rules: regex patterns that classify a captured
//! tmux pane tail into named event kinds.
//!
//! This is the detection engine behind the daemon's pane watchdog
//! (`server::pane_watchdog`). Rules are data, not code: each rule is a
//! compiled regex plus matching options, declared in config as
//! `[[watchdog.rules]]` entries and merged with the built-in defaults
//! ([`default_rules`]). Adding a new banner to watch for is a config edit,
//! not a source change.
//!
//! ```toml
//! [[watchdog.rules]]
//! name = "my-banner"
//! kind = "cap"
//! pattern = '(?i)^some banner text'
//! tail_lines = 10
//! ```
//!
//! Everything is fail-open: an invalid pattern logs and drops that rule at
//! compile time; the rest keep working.

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Where a rule's pattern is applied within the pane tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleScope {
    /// Match each non-empty line of the tail window individually.
    #[default]
    Line,
    /// Match once against the tail window joined with newlines (for
    /// multi-line prompts like device-code sign-in blocks).
    Window,
}

fn default_tail_lines() -> usize {
    15
}

fn default_true() -> bool {
    true
}

/// One declarative pane-text rule, as written in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRuleConfig {
    /// Unique-ish label used in logs and escalation reasons.
    pub name: String,
    /// Event kind emitted on match. The watchdog maps kinds to actions;
    /// see `server::pane_watchdog` for the kinds it recognizes.
    pub kind: String,
    /// Regex tried against each line ([`RuleScope::Line`]) or the joined
    /// tail window ([`RuleScope::Window`]). Case-sensitive unless the
    /// pattern opts into `(?i)`.
    pub pattern: String,
    /// Negative-guard regexes: text they match can never fire this rule
    /// (applied to the raw line or window before the pattern).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub negative: Vec<String>,
    /// How many non-empty lines from the live edge of the pane are in scope.
    /// Small windows keep stale scrollback (a finished sign-in flow, a
    /// recovered error) from firing.
    #[serde(default = "default_tail_lines")]
    pub tail_lines: usize,
    #[serde(default)]
    pub scope: RuleScope,
    /// Strip leading decoration (selector glyphs, bullets) and a `1.` or `2)`
    /// option enumerator from each line before matching, so `^`-anchored
    /// patterns survive TUI chrome. Line scope only.
    #[serde(default = "default_true")]
    pub strip_decoration: bool,
    /// Evaluation order: lower fires first when several rules match.
    #[serde(default)]
    pub priority: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Watchdog section of the user config (`[watchdog]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Disable the pane watchdog entirely.
    #[serde(default)]
    pub disabled: bool,
    /// Scan interval in seconds (minimum 10; default 180).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    /// When true, `rules` fully replaces the built-in default rules instead
    /// of extending them.
    #[serde(default)]
    pub replace_default_rules: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PaneRuleConfig>,
}

impl WatchdogConfig {
    /// The rule set this config asks for: defaults extended by (or replaced
    /// with) the user's `[[watchdog.rules]]` entries.
    pub fn effective_rules(&self) -> Vec<PaneRuleConfig> {
        if self.replace_default_rules {
            self.rules.clone()
        } else {
            let mut rules = default_rules();
            rules.extend(self.rules.iter().cloned());
            rules
        }
    }
}

/// A rule with its regexes compiled, ready to run against pane text.
#[derive(Debug)]
pub struct CompiledRule {
    pub name: String,
    pub kind: String,
    pattern: Regex,
    negative: Vec<Regex>,
    tail_lines: usize,
    scope: RuleScope,
    strip_decoration: bool,
    pub priority: u32,
}

/// Compile a rule list, dropping disabled entries and (with a warning) any
/// rule whose pattern or guards fail to parse. The result is sorted by
/// `priority` (stable, so config order breaks ties).
pub fn compile(rules: &[PaneRuleConfig]) -> Vec<CompiledRule> {
    let mut compiled: Vec<CompiledRule> = rules
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| {
            let pattern = match Regex::new(&r.pattern) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target: "pane_rules",
                        rule = %r.name,
                        error = %e,
                        "invalid rule pattern; rule dropped"
                    );
                    return None;
                }
            };
            let mut negative = Vec::with_capacity(r.negative.len());
            for n in &r.negative {
                match Regex::new(n) {
                    Ok(g) => negative.push(g),
                    Err(e) => {
                        tracing::warn!(
                            target: "pane_rules",
                            rule = %r.name,
                            error = %e,
                            "invalid negative guard; rule dropped"
                        );
                        return None;
                    }
                }
            }
            Some(CompiledRule {
                name: r.name.clone(),
                kind: r.kind.clone(),
                pattern,
                negative,
                tail_lines: r.tail_lines.max(1),
                scope: r.scope,
                strip_decoration: r.strip_decoration,
                priority: r.priority,
            })
        })
        .collect();
    compiled.sort_by_key(|r| r.priority);
    compiled
}

/// Strip leading non-alphanumeric decoration, then a numeric option
/// enumerator (`1.` or `2)` only, so `5-hour limit reached` survives).
/// Case is preserved; patterns opt into `(?i)` themselves.
fn normalize_line(line: &str) -> &str {
    let trimmed = line.trim_start_matches(|c: char| !c.is_alphanumeric());
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = &trimmed[digits..];
    if digits > 0 && (rest.starts_with('.') || rest.starts_with(')')) {
        rest[1..].trim_start()
    } else {
        trimmed
    }
}

/// Run the compiled rules against a raw `capture-pane` tail. Returns the
/// first matching rule in priority order, or None for a healthy pane.
pub fn classify<'a>(raw: &str, rules: &'a [CompiledRule]) -> Option<&'a CompiledRule> {
    let stripped = crate::tmux::utils::strip_ansi(raw);
    let lines: Vec<&str> = stripped
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();

    rules.iter().find(|rule| {
        let window = &lines[lines.len().saturating_sub(rule.tail_lines)..];
        match rule.scope {
            RuleScope::Window => {
                let text = window.join("\n");
                if rule.negative.iter().any(|g| g.is_match(&text)) {
                    return false;
                }
                rule.pattern.is_match(&text)
            }
            RuleScope::Line => window.iter().any(|line| {
                if rule.negative.iter().any(|g| g.is_match(line)) {
                    return false;
                }
                let candidate = if rule.strip_decoration {
                    normalize_line(line)
                } else {
                    line
                };
                rule.pattern.is_match(candidate)
            }),
        }
    })
}

/// The built-in rule set: the operator-blocking states the watchdog shipped
/// with, expressed as data. Config `[[watchdog.rules]]` entries extend these
/// (or replace them via `replace_default_rules`).
pub fn default_rules() -> Vec<PaneRuleConfig> {
    vec![
        PaneRuleConfig {
            name: "usage-cap".into(),
            kind: "cap".into(),
            // Line-anchored banner prefixes plus the generalized personal-cap
            // sentence ("You've hit/reached your <X> limit", any <X>), so new
            // model names never need a rule edit. Prefix anchoring keeps
            // scrollback prose that merely mentions a limit from firing.
            pattern: r"(?i)^(?:claude usage limit reached|usage limit reached|session limit reached|5-hour limit reached|weekly limit reached|stop and wait for limit|switch to usage credits|switch to team plan|(?:you'?re |you are )?out of usage credits|your limit will reset|(?:you'?ve|you have) (?:hit|reached) your .*limit)".into(),
            // The transient server-side 429 banner and the Fable promo blurb
            // both talk about usage limits without the account being capped.
            negative: vec![
                r"(?i)not your usage limit".into(),
                r"(?i)up to 50% of".into(),
            ],
            tail_lines: 30,
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 0,
            enabled: true,
        },
        PaneRuleConfig {
            name: "device-code".into(),
            kind: "auth".into(),
            // Window scope: the URL, code, and instruction lines arrive as a
            // multi-line block. Only counts while still at the live edge of
            // the pane; a devicelogin URL buried under later output is a
            // finished flow.
            pattern: r"(?i)microsoft\.com/devicelogin|/login/device|first copy your one-time code|to sign in, use a web browser|enter the code[\s\S]*to authenticate|to authenticate[\s\S]*enter the code".into(),
            negative: Vec::new(),
            tail_lines: 8,
            scope: RuleScope::Window,
            strip_decoration: false,
            priority: 1,
            enabled: true,
        },
        PaneRuleConfig {
            name: "server-overload".into(),
            kind: "overload".into(),
            // The JSON error type, or a word-bounded 529 next to an "API
            // Error" banner or within 40 chars of "overload". Bare numbers
            // and plain prose about overload never fire. Live-edge only.
            pattern: r"(?i)overloaded_error|api error\W{0,8}529\b|\b529\b.{0,40}overload|overload.{0,40}\b529\b".into(),
            negative: Vec::new(),
            tail_lines: 8,
            scope: RuleScope::Line,
            strip_decoration: false,
            priority: 2,
            enabled: true,
        },
        PaneRuleConfig {
            name: "action-required".into(),
            kind: "action".into(),
            // Case-sensitive by design: a worker's gate line is upper-case;
            // "no action required here" prose must not fire.
            pattern: r"^ACTION REQUIRED".into(),
            // A NEGATED payload voids the gate: "ACTION REQUIRED: none — cert
            // registered ..." is a self-cleared recap, not a live gate (the
            // for-Migrator 93e985ee false wake, WO e2846188). Suppress when a
            // negation token immediately follows the phrase (after optional
            // :/dash/whitespace). Checked against the raw line, so it is left
            // un-anchored to tolerate leading decoration.
            negative: vec![
                r"(?i)ACTION REQUIRED[:\s—–-]*(?:none|nothing|n/?a|cleared)\b".into(),
            ],
            tail_lines: 15,
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 3,
            enabled: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, pattern: &str) -> PaneRuleConfig {
        PaneRuleConfig {
            name: name.into(),
            kind: "cap".into(),
            pattern: pattern.into(),
            negative: Vec::new(),
            tail_lines: default_tail_lines(),
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 0,
            enabled: true,
        }
    }

    #[test]
    fn toml_rule_parses_with_defaults() {
        let cfg: WatchdogConfig = toml::from_str(
            r#"
            [[rules]]
            name = "my-banner"
            kind = "cap"
            pattern = '(?i)^some banner'
            "#,
        )
        .unwrap();
        let r = &cfg.rules[0];
        assert_eq!(r.name, "my-banner");
        assert_eq!(r.kind, "cap");
        assert_eq!(r.tail_lines, 15);
        assert_eq!(r.scope, RuleScope::Line);
        assert!(r.strip_decoration);
        assert!(r.enabled);
        assert!(r.negative.is_empty());
        assert!(!cfg.replace_default_rules);
        assert!(!cfg.disabled);
    }

    #[test]
    fn compile_drops_invalid_regex_keeps_valid() {
        let rules = vec![rule("bad", "(unclosed"), rule("good", "^fine$")];
        let compiled = compile(&rules);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].name, "good");
    }

    #[test]
    fn compile_drops_disabled_and_sorts_by_priority() {
        let mut a = rule("second", "a");
        a.priority = 5;
        let mut b = rule("first", "b");
        b.priority = 1;
        let mut c = rule("off", "c");
        c.enabled = false;
        let compiled = compile(&[a, b, c]);
        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled[0].name, "first");
        assert_eq!(compiled[1].name, "second");
    }

    #[test]
    fn custom_window_rule_fires_across_lines() {
        let mut r = rule("multi", r"(?is)first half[\s\S]*second half");
        r.scope = RuleScope::Window;
        r.tail_lines = 5;
        let compiled = compile(&[r]);
        let pane = "first half of the prompt\nsome middle text\nsecond half here\n";
        assert_eq!(
            classify(pane, &compiled).map(|m| m.name.as_str()),
            Some("multi")
        );
    }

    #[test]
    fn negative_guard_suppresses_match() {
        let mut r = rule("guarded", r"(?i)^usage limit reached");
        r.negative = vec![r"(?i)not your usage limit".into()];
        let compiled = compile(&[r]);
        assert!(classify("Usage limit reached ∙ resets 3pm\n", &compiled).is_some());
        assert!(classify("usage limit reached (not your usage limit)\n", &compiled).is_none());
    }

    #[test]
    fn strip_decoration_handles_selector_and_enumerator() {
        let compiled = compile(&[rule("opt", r"(?i)^stop and wait")]);
        assert!(classify(" ❯ 1. Stop and wait for limit reset\n", &compiled).is_some());
        // 5-hour style: leading digits without a dot or paren are not an
        // enumerator and must survive normalization.
        let compiled = compile(&[rule("hour", r"(?i)^5-hour limit")]);
        assert!(classify("5-hour limit reached\n", &compiled).is_some());
    }

    #[test]
    fn tail_lines_bounds_the_window() {
        let mut r = rule("edge", r"(?i)^stale banner");
        r.tail_lines = 3;
        let compiled = compile(&[r]);
        let mut pane = String::from("stale banner\n");
        for i in 0..10 {
            pane.push_str(&format!("later line {i}\n"));
        }
        assert!(classify(&pane, &compiled).is_none());
    }

    #[test]
    fn default_rules_all_compile() {
        let compiled = compile(&default_rules());
        assert_eq!(compiled.len(), 4);
        assert_eq!(compiled[0].kind, "cap");
        assert_eq!(compiled[1].kind, "auth");
        assert_eq!(compiled[2].kind, "overload");
        assert_eq!(compiled[3].kind, "action");
    }

    #[test]
    fn action_required_negated_payload_does_not_fire() {
        // WO Commander e2846188 (extends #179/#192/#230): for-Migrator 93e985ee
        // got a false action-required wake whose SOLE matching line was a
        // self-cleared recap: "ACTION REQUIRED: none — cert registered ...".
        // A negation token immediately following the phrase voids the gate.
        let compiled = compile(&default_rules());
        assert!(
            classify(
                "ACTION REQUIRED: none — cert registered, deploy verified, stop\n",
                &compiled,
            )
            .is_none(),
            "the for-Migrator self-cleared recap must produce zero gate signal",
        );
        for line in [
            "ACTION REQUIRED: none\n",
            "ACTION REQUIRED — n/a\n",
            "ACTION REQUIRED: na\n",
            "ACTION REQUIRED: cleared\n",
            "ACTION REQUIRED: nothing pending\n",
        ] {
            assert!(
                classify(line, &compiled).is_none(),
                "negated payload must not fire: {line:?}",
            );
        }
        // A genuinely actionable gate STILL fires.
        assert_eq!(
            classify("ACTION REQUIRED: reply send to approve the merge\n", &compiled)
                .map(|m| m.name.as_str()),
            Some("action-required"),
        );
    }

    #[test]
    fn effective_rules_extend_by_default() {
        let cfg: WatchdogConfig = toml::from_str(
            r#"
            [[rules]]
            name = "extra"
            kind = "cap"
            pattern = "^extra banner"
            "#,
        )
        .unwrap();
        let rules = cfg.effective_rules();
        assert_eq!(rules.len(), default_rules().len() + 1);
        assert_eq!(rules.last().unwrap().name, "extra");
    }

    #[test]
    fn effective_rules_replace_when_asked() {
        let cfg: WatchdogConfig = toml::from_str(
            r#"
            replace_default_rules = true
            [[rules]]
            name = "only"
            kind = "cap"
            pattern = "^only banner"
            "#,
        )
        .unwrap();
        let rules = cfg.effective_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "only");
    }
}
