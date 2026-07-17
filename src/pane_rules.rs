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
    /// Liveness guards (Line scope only): regexes that mark a line as
    /// ACTIVITY. A matched line is voided when any of these matches a line
    /// BELOW it, because output rendered after the match proves the match is
    /// replayed scrollback (a resumed pane re-shows old banners), not a
    /// current blocking state. Empty keeps every match, the prior behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_below: Vec<String>,
    /// Required-liveness guards (Line scope only): the mirror of
    /// `stale_below`. A matched line is voided UNLESS at least one of these
    /// matches a line BELOW it. Use for banners that are only actionable
    /// while the pane is still doing something — e.g. a 529 banner is a live
    /// block only while the running footer renders below it; a recovered
    /// pane idling at the prompt keeps the banner in its tail but no longer
    /// satisfies the guard. Empty keeps every match, the prior behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_below: Vec<String>,
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
    stale_below: Vec<Regex>,
    require_below: Vec<Regex>,
    tail_lines: usize,
    scope: RuleScope,
    strip_decoration: bool,
    pub priority: u32,
}

impl CompiledRule {
    /// A Line-scope match only counts while nothing below it looks alive:
    /// when any `stale_below` guard matches a line rendered after the
    /// matched one, the match is replayed scrollback (the session resumed
    /// and kept working), not a current blocking state. Symmetrically, when
    /// `require_below` guards are set, the match only counts while at least
    /// one line below satisfies one — proof the pane is still in the state
    /// that makes the banner actionable. A match on the very last line has
    /// nothing below it, so it cannot satisfy a `require_below` guard and is
    /// voided; the next scan sees the settled pane.
    fn match_is_current(&self, below: &[&str]) -> bool {
        let not_stale = self.stale_below.is_empty()
            || !below
                .iter()
                .any(|l| self.stale_below.iter().any(|g| g.is_match(l)));
        let required_alive = self.require_below.is_empty()
            || below
                .iter()
                .any(|l| self.require_below.iter().any(|g| g.is_match(l)));
        not_stale && required_alive
    }
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
            let mut stale_below = Vec::with_capacity(r.stale_below.len());
            for s in &r.stale_below {
                match Regex::new(s) {
                    Ok(g) => stale_below.push(g),
                    Err(e) => {
                        tracing::warn!(
                            target: "pane_rules",
                            rule = %r.name,
                            error = %e,
                            "invalid stale_below guard; rule dropped"
                        );
                        return None;
                    }
                }
            }
            let mut require_below = Vec::with_capacity(r.require_below.len());
            for s in &r.require_below {
                match Regex::new(s) {
                    Ok(g) => require_below.push(g),
                    Err(e) => {
                        tracing::warn!(
                            target: "pane_rules",
                            rule = %r.name,
                            error = %e,
                            "invalid require_below guard; rule dropped"
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
                stale_below,
                require_below,
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
            RuleScope::Line => window.iter().enumerate().any(|(idx, line)| {
                if rule.negative.iter().any(|g| g.is_match(line)) {
                    return false;
                }
                let candidate = if rule.strip_decoration {
                    normalize_line(line)
                } else {
                    line
                };
                rule.pattern.is_match(candidate) && rule.match_is_current(&window[idx + 1..])
            }),
        }
    })
}

/// Like [`classify`], but also returns a stable fingerprint of the exact text
/// that triggered the match: the normalized matched line (Line scope) or the
/// matched substring (Window scope). ANSI-stripped, whitespace-collapsed and
/// lowercased so it is stable across cosmetic pane churn — spinner frames,
/// elapsed-time counters and token tallies live on *other* lines and never
/// enter the fingerprint. The watchdog uses this to tell an UNCHANGED standing
/// gate (already surfaced → suppress the re-wake) from a NEW/CHANGED one (wake
/// immediately), so a parked Ben-gate stops re-paging the Commander. WO #139.
pub fn classify_fp<'a>(raw: &str, rules: &'a [CompiledRule]) -> Option<(&'a CompiledRule, String)> {
    let stripped = crate::tmux::utils::strip_ansi(raw);
    let lines: Vec<&str> = stripped
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();

    rules.iter().find_map(|rule| {
        let window = &lines[lines.len().saturating_sub(rule.tail_lines)..];
        match rule.scope {
            RuleScope::Window => {
                let text = window.join("\n");
                if rule.negative.iter().any(|g| g.is_match(&text)) {
                    return None;
                }
                rule.pattern
                    .find(&text)
                    .map(|m| (rule, gate_fingerprint(rule, m.as_str())))
            }
            RuleScope::Line => window.iter().enumerate().find_map(|(idx, line)| {
                if rule.negative.iter().any(|g| g.is_match(line)) {
                    return None;
                }
                let candidate = if rule.strip_decoration {
                    normalize_line(line)
                } else {
                    line
                };
                (rule.pattern.is_match(candidate) && rule.match_is_current(&window[idx + 1..]))
                    .then(|| (rule, gate_fingerprint(rule, candidate)))
            }),
        }
    })
}

/// Collapse a matched fragment to a churn-stable key: trim, collapse internal
/// whitespace runs to single spaces, and lowercase.
fn fingerprint(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Route the fingerprint by rule kind. `action` gates re-word their prose every
/// tick ("#18 remains parked …" → "#18 needs only your authorization …"), so a
/// raw full-line fingerprint sees a NEW gate each cycle and re-wakes the
/// Commander even though the gate is unchanged (WO d6bcae49, for-AVHR
/// 0a3ac9fc). For action gates the STABLE identity is the set of ticket/probe/WO
/// refs it cites (#N), not the churning prose — so key on that. All other kinds
/// keep the full-line fingerprint. Combined with the per-session map key in the
/// watchdog, this makes an unchanged-but-reworded held gate stay suppressed for
/// its TTL while a genuinely different ref set still wakes immediately.
fn gate_fingerprint(rule: &CompiledRule, candidate: &str) -> String {
    if rule.kind == "action" {
        action_gate_fingerprint(candidate)
    } else {
        fingerprint(candidate)
    }
}

/// Extract the gate-identity fingerprint from an ACTION REQUIRED line: the
/// sorted, de-duplicated set of `#<digits>` refs it cites, order-independent
/// ("#18 and #24" == "#24 then #18"). Pure byte scan — no per-call regex. If the
/// line cites no `#N` ref, fall back to the full-line prose fingerprint so two
/// unrelated id-less gates stay distinct rather than over-collapsing.
pub(crate) fn action_gate_fingerprint(candidate: &str) -> String {
    let bytes = candidate.as_bytes();
    let mut refs: Vec<u64> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#' {
            let mut j = i + 1;
            let mut n: u64 = 0;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                n = n
                    .saturating_mul(10)
                    .saturating_add((bytes[j] - b'0') as u64);
                j += 1;
            }
            if j > i + 1 {
                refs.push(n);
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if refs.is_empty() {
        return fingerprint(candidate);
    }
    refs.sort_unstable();
    refs.dedup();
    let joined = refs
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("action-gate:{joined}")
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
            // scrollback prose that merely mentions a limit from firing. An
            // optional error prefix covers a capacity-capped /compact, which
            // renders the same cap sentence behind "Error during compaction:"
            // (WO #362: five Fable sessions failed /compact silently).
            pattern: r"(?i)^(?:(?:api )?error(?: during compaction)?\W{0,10})?(?:claude usage limit reached|usage limit reached|session limit reached|5-hour limit reached|weekly limit reached|stop and wait for limit|switch to usage credits|switch to team plan|run /usage-credits|switch models with /model|(?:you'?re |you are )?out of usage credits|your limit will reset|(?:you'?ve|you have) (?:hit|reached) your .*limit)".into(),
            // The transient server-side 429 banner and the Fable promo blurb
            // both talk about usage limits without the account being capped.
            negative: vec![
                r"(?i)not your usage limit".into(),
                r"(?i)up to 50% of".into(),
                // A quote character inside the leading decoration is template
                // text quoting a banner (a WO or a session building this
                // detector), never a real CLI banner. Checked on the RAW line;
                // normalize would strip the quote and false-fire the anchor
                // (same convention as the action-required quoted guard).
                r#"^[^\p{L}\p{N}]*['"`\x{2018}\x{2019}\x{201C}\x{201D}]"#.into(),
            ],
            // Liveness: a cap banner is authoritative only while it is the
            // last substantive thing in the pane. A tool-call bullet, a
            // tool-result elbow, or a running spinner rendered below it means
            // the session resumed and is serving again, so the banner is
            // replayed scrollback from before a restart and must not revoke
            // the account's headroom (the forit-main and xce-main false
            // revokes of 2026-07-15).
            stale_below: vec![
                r"^\s*[⏺●]".into(),
                r"^\s*⎿".into(),
                r"(?i)\besc to interrupt\b".into(),
            ],
            require_below: Vec::new(),
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
            stale_below: Vec::new(),
            require_below: Vec::new(),
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
            // Liveness both ways. stale_below: an assistant/tool bullet below
            // the banner means the retry succeeded and the session kept
            // working — replayed scrollback. require_below: a 529 is a live
            // block only while the CLI is still in its retry loop, which
            // renders the running footer ("esc to interrupt") below the
            // banner. A recovered pane idling at the ready prompt keeps the
            // banner in its 8-line tail forever but has no running footer, so
            // it must not fire (the overloaded-then-idle false-fire that kept
            // an URGENT Overloaded badge fresh past its TTL, 2026-07-16).
            // Note the cap rule's guards do NOT transfer here: a live retry
            // has a `⎿ Tip:` elbow and the running footer BELOW the banner,
            // so cap-style stale guards would void exactly the live case.
            stale_below: vec![r"^\s*[⏺●]".into()],
            require_below: vec![r"(?i)\besc to interrupt\b".into()],
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
                // WO d6bcae49: void a QUOTED-template match. Stop-hook /
                // commander boilerplate quotes the phrase to describe the
                // format (`ACTION REQUIRED:` / 'ACTION REQUIRED:'). normalize
                // strips the leading backtick/quote so the quotation false-fires
                // `^ACTION REQUIRED`. The negative guard sees the RAW line, so
                // when a backtick or straight/smart quote is the leading
                // decoration wrapping the phrase, it is template text, not a
                // live gate. Bullet/blockquote decoration (-, >) is NOT a quote
                // and still fires; a backtick elsewhere in a real payload
                // ("run `git push`") is not at line-start and still fires.
                r#"^\s*[`'"\x{2018}\x{2019}\x{201C}\x{201D}]\s*ACTION REQUIRED"#.into(),
            ],
            stale_below: Vec::new(),
            require_below: Vec::new(),
            tail_lines: 15,
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 3,
            enabled: true,
        },
    ]
}

/// True when a session's `extra_args` pins it to the Fable model
/// (`--model fable`, `--model=fable`, or a full `claude-fable-*` id). The Fable
/// model-drift detector ([`fable_drift_rules`]) is gated on this: condition (a)
/// — a Sonnet/Opus SUBAGENT — is normal on a non-Fable session and must never
/// fire there; only a Fable-pinned session dispatching off-Fable work is drift.
/// WO d6bcae49 (subagent-vector + silent-Fable-limit).
pub fn is_fable_pinned(extra_args: &str) -> bool {
    let lower = extra_args.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(idx) = rest.find("--model") {
        let after = &rest[idx + "--model".len()..];
        // Tolerate both `--model fable` and `--model=fable`.
        let after = after.strip_prefix('=').unwrap_or(after);
        let val = after
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '"' || c == '\'');
        if val == "fable" || val.starts_with("claude-fable") || val.starts_with("fable-") {
            return true;
        }
        rest = &rest[idx + "--model".len()..];
    }
    false
}

/// Extract the model a session's `extra_args` pins it to (`--model X` or
/// `--model=X`, quotes stripped). The LAST occurrence wins, matching CLI
/// override semantics. `None` when no `--model` carries a usable value, so a
/// bare trailing flag or a value that is itself another flag never reports a
/// pin. Surfaced through the sessions API (WO#414 capacity work): relocation
/// must preserve the pin, so the pin has to be visible.
pub fn model_pin(extra_args: &str) -> Option<String> {
    let mut pin = None;
    let mut rest = extra_args;
    while let Some(idx) = rest.find("--model") {
        let after = &rest[idx + "--model".len()..];
        rest = after;
        // Accept only `--model=X` or whitespace-separated `--model X`; a run-on
        // token like `--modelfoo` is a different flag.
        let after = match after.strip_prefix('=') {
            Some(a) => a,
            None => {
                if !after.starts_with(|c: char| c.is_whitespace()) {
                    continue;
                }
                after
            }
        };
        let val = after
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == '"' || c == '\'');
        if !val.is_empty() && !val.starts_with('-') {
            pin = Some(val.to_string());
        }
    }
    pin
}

/// The Fable model-drift rule battery (kind `"fable"`). Run as a SEPARATE pass,
/// only for Fable-pinned sessions (see [`is_fable_pinned`]), so the general
/// engine stays model-agnostic. Three vectors, all tightened so that a Fable
/// session merely *building a Claude app* (writing `model="claude-opus-4-8"`) or
/// carrying injected skill prose ("default to using Opus") never fires — an
/// ACTION verb, a subagent/Task token, or a named Fable-limit is required:
///   1. `fable-limit-paraphrase` — the account hit a Fable cap on a background /
///      subagent path and the exact credit-out banner (the general `cap` rule)
///      never rendered; "Fable" named next to a limit/credit/quota word.
///   2. `fable-downgrade-verb` — an explicit relaunch / fall-back / drop-to /
///      switch-to onto a non-Fable model ("relaunched on Sonnet", "dropped to
///      sonnet").
///   3. `fable-subagent-model` — a subagent / Task tool named adjacent to a
///      non-Fable model ("Sonnet subagents", "Task(build) running on
///      claude-sonnet-5") — condition (a), the subagent vector.
///
/// On a hit the watchdog PAGES the Commander (never auto-swaps the model), with
/// a content-gated fingerprint dampener so a standing drift does not re-flood.
pub fn fable_drift_rules() -> Vec<PaneRuleConfig> {
    vec![
        PaneRuleConfig {
            name: "fable-limit-paraphrase".into(),
            kind: "fable".into(),
            // "Fable" named within 40 chars of a limit/credit/quota word, in
            // either order. This is the SILENT-limit case the general `cap`
            // rule misses because the paraphrase does not match a cap banner.
            pattern: r"(?i)(?:fable\b[^\n]{0,40}\b(?:usage limit|usage credit|out of (?:usage )?credits?|credit limit|credits? (?:remaining|left|exhausted|depleted|ran out)|quota|rate.?limit(?:ed)?|limit (?:reached|hit|exceeded))|\b(?:usage limit|out of (?:usage )?credits?|limit (?:reached|hit|exceeded))[^\n]{0,40}\bfable\b)".into(),
            // The Fable rollout promo names Fable next to "weekly usage limit".
            negative: vec![
                r"(?i)up to 50% of".into(),
                r"(?i)try claude fable".into(),
            ],
            stale_below: Vec::new(),
            require_below: Vec::new(),
            tail_lines: 25,
            scope: RuleScope::Window,
            strip_decoration: false,
            priority: 0,
            enabled: true,
        },
        PaneRuleConfig {
            name: "fable-downgrade-verb".into(),
            kind: "fable".into(),
            // A strong runtime-downgrade verb within 25 chars of a non-Fable
            // model. Strong verbs only — "default to", "use", "using" are
            // excluded because they dominate app-building prose / injected skill
            // context ("default to using Opus", "unless the user says 'use
            // sonnet'").
            pattern: r"(?i)\b(?:re-?launch(?:ed|ing)?|restart(?:ed|ing)?|fell back|fall(?:ing)? back|fallback|drop(?:ped|ping)? (?:down |back )?to|switch(?:ed|ing)? (?:to|onto)|revert(?:ed|ing)? to|downgrad(?:ed|e|ing)? to|bumped? down to|kicked (?:it )?(?:down|over) to|moved? (?:down |back )?to)\b[^\n]{0,25}\b(?:claude-)?(?:sonnet|opus)\b".into(),
            negative: Vec::new(),
            stale_below: Vec::new(),
            require_below: Vec::new(),
            tail_lines: 25,
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 1,
            enabled: true,
        },
        PaneRuleConfig {
            name: "fable-subagent-model".into(),
            kind: "fable".into(),
            // A subagent / Task tool named adjacent to a non-Fable model, either
            // order. A subagent with NO model named (a normal Fable subagent) or
            // a model literal with NO subagent (app code) never fires.
            pattern: r"(?i)(?:\b(?:claude-)?(?:sonnet|opus)\b[^\n]{0,20}\bsub-?agents?\b|\bsub-?agents?\b[^\n]{0,25}\b(?:claude-)?(?:sonnet|opus)\b|task\([^\n]{0,60}\b(?:claude-)?(?:sonnet|opus)\b)".into(),
            negative: Vec::new(),
            stale_below: Vec::new(),
            require_below: Vec::new(),
            tail_lines: 25,
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 2,
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
            stale_below: Vec::new(),
            require_below: Vec::new(),
            tail_lines: default_tail_lines(),
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 0,
            enabled: true,
        }
    }

    // ---- Fable model-drift detection (WO d6bcae49) ---------------------------

    #[test]
    fn is_fable_pinned_recognizes_fable_model_flag() {
        // Positive: --model fable / --model=fable / full claude-fable id.
        assert!(is_fable_pinned("--model fable"));
        assert!(is_fable_pinned("--model=fable"));
        assert!(is_fable_pinned("--model claude-fable-5"));
        assert!(is_fable_pinned(
            "--dangerously-skip-permissions --model fable --resume abc123"
        ));
        assert!(is_fable_pinned("--model=\"claude-fable-5\""));
        // Loops past a non-fable --model to a later fable one.
        assert!(is_fable_pinned("--model opus --model fable"));
    }

    #[test]
    fn is_fable_pinned_rejects_non_fable() {
        assert!(!is_fable_pinned(""));
        assert!(!is_fable_pinned("--resume xyz"));
        assert!(!is_fable_pinned("--model sonnet"));
        assert!(!is_fable_pinned("--model claude-opus-4-8"));
        assert!(!is_fable_pinned("--model=claude-sonnet-5 --resume q"));
        // A session merely mentioning fable in a non-model arg must NOT pin.
        assert!(!is_fable_pinned("--resume fable-notes-session"));
    }

    // ---- Model-pin extraction (WO#414 capacity API) ---------------------------

    #[test]
    fn model_pin_extracts_flag_value() {
        assert_eq!(model_pin("--model fable"), Some("fable".to_string()));
        assert_eq!(
            model_pin("--model=claude-opus-4-8"),
            Some("claude-opus-4-8".to_string())
        );
        assert_eq!(
            model_pin("--model \"claude-fable-5\""),
            Some("claude-fable-5".to_string())
        );
        assert_eq!(model_pin("--model='sonnet'"), Some("sonnet".to_string()));
        assert_eq!(
            model_pin("--dangerously-skip-permissions --model fable --resume abc123"),
            Some("fable".to_string())
        );
    }

    #[test]
    fn model_pin_last_flag_wins() {
        assert_eq!(
            model_pin("--model opus --model fable"),
            Some("fable".to_string())
        );
    }

    #[test]
    fn model_pin_none_when_absent_or_malformed() {
        assert_eq!(model_pin(""), None);
        assert_eq!(model_pin("--resume xyz"), None);
        assert_eq!(model_pin("--model"), None);
        assert_eq!(model_pin("--model --resume x"), None);
        assert_eq!(model_pin("--modelfoo bar"), None);
    }

    #[test]
    fn fable_drift_fires_on_silent_limit_and_downgrade_and_subagent() {
        let compiled = compile(&fable_drift_rules());
        // (b) silent Fable-limit paraphrase — the exact cap banner never rendered.
        assert!(
            classify(
                "You've hit your Fable usage limit for the week.\n",
                &compiled
            )
            .is_some(),
            "fable usage-limit paraphrase must fire"
        );
        assert!(
            classify("Error: out of usage credits for fable.\n", &compiled).is_some(),
            "out-of-credits (fable) must fire"
        );
        // (b) explicit runtime downgrade onto a non-Fable model.
        assert!(
            classify("Fable capped — relaunched on Sonnet.\n", &compiled).is_some(),
            "relaunch-on-sonnet must fire"
        );
        assert!(
            classify("dropped to sonnet for the rest of the run\n", &compiled).is_some(),
            "dropped-to-sonnet must fire"
        );
        // (a) subagent vector — non-Fable model named at a subagent/Task.
        assert!(
            classify(
                "dispatching its whole build on Sonnet subagents now\n",
                &compiled
            )
            .is_some(),
            "sonnet-subagents must fire"
        );
        assert!(
            classify(
                "⏺ Task(build the API) running on claude-sonnet-5\n",
                &compiled
            )
            .is_some(),
            "Task-on-sonnet must fire"
        );
        assert!(
            classify("spawned an Opus subagent to do the migration\n", &compiled).is_some(),
            "opus-subagent must fire (condition (a) covers Sonnet OR Opus)"
        );
    }

    #[test]
    fn fable_drift_ignores_clean_pane_and_app_code() {
        let compiled = compile(&fable_drift_rules());
        // Clean working spinner — no drift.
        assert!(classify("✻ Working… (12s · ↑ 1.2k tokens)\n", &compiled).is_none());
        // A Fable session BUILDING a Claude app writes model literals — must NOT fire.
        assert!(classify("        model=\"claude-opus-4-8\",\n", &compiled).is_none());
        assert!(
            classify(
                "default to using claude-opus-4-8 for the API calls\n",
                &compiled
            )
            .is_none(),
            "injected skill prose 'using opus' must NOT fire"
        );
        assert!(
            classify(
                "unless the user says 'use sonnet' or 'use haiku'\n",
                &compiled
            )
            .is_none(),
            "'use sonnet' imperative must NOT fire"
        );
        // A subagent with NO model named is a NORMAL Fable subagent — must NOT fire.
        assert!(classify("Use parallel subagents aggressively.\n", &compiled).is_none());
        assert!(classify("⏺ Task(build the classifier)\n", &compiled).is_none());
        // A Fable session literally coding sonnet into an app (no subagent/verb) — no fire.
        assert!(
            classify(
                "        model = \"claude-sonnet-5\"  # classifier\n",
                &compiled
            )
            .is_none(),
            "sonnet model literal without subagent/downgrade-verb must NOT fire"
        );
        // The Fable rollout promo names Fable next to 'usage limit' — negative guard.
        assert!(
            classify(
                "Try Claude Fable 5 — up to 50% of your weekly usage limit.\n",
                &compiled
            )
            .is_none(),
            "fable promo line must NOT fire"
        );
    }

    #[test]
    fn fable_drift_fingerprint_stable_across_spinner_churn() {
        let compiled = compile(&fable_drift_rules());
        let a = "You've hit your Fable usage limit.\n✻ Working… (3s)\n";
        let b = "You've hit your Fable usage limit.\n✻ Working… (49s)\n";
        let (_, fp_a) = classify_fp(a, &compiled).expect("a matches");
        let (_, fp_b) = classify_fp(b, &compiled).expect("b matches");
        assert_eq!(
            fp_a, fp_b,
            "fingerprint must be stable across cosmetic churn"
        );
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
    fn stale_below_voids_match_with_activity_below() {
        let mut r = rule("cap-live", r"(?i)^usage limit reached");
        r.stale_below = vec![r"^\s*[⏺●]".into()];
        let compiled = compile(&[r]);
        // Banner at the live edge: current, fires.
        assert!(classify(
            "earlier output\nUsage limit reached ∙ resets 3pm\n",
            &compiled
        )
        .is_some());
        // Activity rendered BELOW the banner: replayed scrollback, voided.
        let replayed = "Usage limit reached ∙ resets 3pm\n⏺ Bash(cargo test)\n";
        assert!(classify(replayed, &compiled).is_none());
        assert!(classify_fp(replayed, &compiled).is_none());
    }

    #[test]
    fn stale_below_ignores_activity_above_the_match() {
        let mut r = rule("cap-live", r"(?i)^usage limit reached");
        r.stale_below = vec![r"^\s*[⏺●]".into()];
        let compiled = compile(&[r]);
        // Activity ABOVE the banner is history, not liveness evidence.
        assert!(classify("⏺ Bash(cargo test)\nUsage limit reached\n", &compiled).is_some());
    }

    #[test]
    fn default_usage_cap_voided_by_replayed_scrollback() {
        let compiled = compile(&default_rules());
        let replayed = "You've reached your usage limit\n⏺ Read(src/main.rs)\n  ⎿ Read 40 lines\n";
        assert!(
            classify(replayed, &compiled).is_none(),
            "cap banner with tool activity below must not revoke headroom"
        );
        assert!(
            classify("You've reached your usage limit ∙ resets 3pm\n", &compiled).is_some(),
            "cap banner at the live edge must still fire"
        );
    }

    #[test]
    fn default_overload_recovered_then_idle_is_none() {
        // A 529 that the CLI finished past (turn ended, pane idle at the
        // ready prompt, footer has no "esc to interrupt") is history, not a
        // live block. The per-Website false-fire of 2026-07: the banner sat
        // in the 8-line tail of an idle pane and re-fired every tick.
        let compiled = compile(&default_rules());
        let idle = "⏺ API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment.\n\
                    ✻ Crunched for 3m 22s\n\
                    ──────\n\
                    ❯\n\
                    ──────\n\
                    ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n";
        assert!(
            classify(idle, &compiled).is_none(),
            "recovered 529 above an idle prompt must not fire"
        );
    }

    #[test]
    fn default_overload_live_retry_fires() {
        // The CLI mid-retry: spinner banner at the live edge, running footer
        // ("esc to interrupt") below. This is the real blocked state.
        let compiled = compile(&default_rules());
        let live = "✻ 529 Overloaded · Retrying in 2s · attempt 10/10\n\
                    ⎿  Tip: Use /btw to ask a quick side question\n\
                    ──────\n\
                    ❯\n\
                    ──────\n\
                    ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← for agents\n";
        assert_eq!(
            classify(live, &compiled).map(|m| m.name.as_str()),
            Some("server-overload"),
            "live 529 retry with a running footer must fire"
        );
    }

    #[test]
    fn default_overload_resumed_activity_is_none() {
        // Retry succeeded and the session kept working: assistant/tool
        // bullets below the banner prove it is replayed scrollback even
        // though the running footer is present.
        let compiled = compile(&default_rules());
        let resumed = "⏺ API Error: 529 Overloaded. This is a server-side issue.\n\
                       ⏺ Bash(cargo test)\n\
                       ⎿  running 5 tests\n\
                       ✻ Crunching… (esc to interrupt)\n";
        assert!(
            classify(resumed, &compiled).is_none(),
            "529 with resumed activity below must not fire"
        );
    }

    #[test]
    fn toml_stale_below_parses_and_defaults_empty() {
        let cfg: WatchdogConfig = toml::from_str(
            r#"
            [[rules]]
            name = "bare"
            kind = "cap"
            pattern = "^x"

            [[rules]]
            name = "guarded"
            kind = "cap"
            pattern = "^y"
            stale_below = ["^z"]
            "#,
        )
        .expect("parses");
        assert!(cfg.rules[0].stale_below.is_empty());
        assert_eq!(cfg.rules[1].stale_below, vec!["^z".to_string()]);
    }

    #[test]
    fn invalid_stale_below_guard_drops_rule() {
        let mut r = rule("bad-guard", r"^x");
        r.stale_below = vec![r"(unclosed".into()];
        assert!(compile(&[r]).is_empty());
    }

    #[test]
    fn require_below_voids_match_without_required_line() {
        let mut r = rule("overload-live", r"(?i)^api error.*529");
        r.require_below = vec![r"(?i)\besc to interrupt\b".into()];
        let compiled = compile(&[r]);
        // Running footer below the banner: required liveness present, fires.
        let live = "API Error: 529 Overloaded\n  esc to interrupt\n";
        assert!(classify(live, &compiled).is_some());
        assert!(classify_fp(live, &compiled).is_some());
        // Idle prompt below, no footer: guard unsatisfied, voided.
        let idle = "API Error: 529 Overloaded\n❯\n";
        assert!(classify(idle, &compiled).is_none());
        assert!(classify_fp(idle, &compiled).is_none());
        // Banner as the very last line: nothing below can satisfy the guard.
        assert!(classify("API Error: 529 Overloaded\n", &compiled).is_none());
    }

    #[test]
    fn require_below_ignores_required_line_above_the_match() {
        let mut r = rule("overload-live", r"(?i)^api error.*529");
        r.require_below = vec![r"(?i)\besc to interrupt\b".into()];
        let compiled = compile(&[r]);
        // The footer ABOVE the banner is history, not liveness evidence.
        assert!(classify("  esc to interrupt\nAPI Error: 529 Overloaded\n", &compiled).is_none());
    }

    #[test]
    fn toml_require_below_parses_and_defaults_empty() {
        let cfg: WatchdogConfig = toml::from_str(
            r#"
            [[rules]]
            name = "bare"
            kind = "overload"
            pattern = "^x"

            [[rules]]
            name = "guarded"
            kind = "overload"
            pattern = "^y"
            require_below = ["^z"]
            "#,
        )
        .expect("parses");
        assert!(cfg.rules[0].require_below.is_empty());
        assert_eq!(cfg.rules[1].require_below, vec!["^z".to_string()]);
    }

    #[test]
    fn invalid_require_below_guard_drops_rule() {
        let mut r = rule("bad-guard", r"^x");
        r.require_below = vec![r"(unclosed".into()];
        assert!(compile(&[r]).is_empty());
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
            classify(
                "ACTION REQUIRED: reply send to approve the merge\n",
                &compiled
            )
            .map(|m| m.name.as_str()),
            Some("action-required"),
        );
    }

    #[test]
    fn action_required_quoted_template_text_does_not_fire() {
        // WO Commander d6bcae49: the gate-watchdog fired on its OWN pane. The
        // sole matching line was stop-hook / commander BOILERPLATE that QUOTES
        // the phrase to describe the format, e.g. the monitoring-loop rule
        // "your correct final line is `ACTION REQUIRED:` / a heartbeat note".
        // normalize_line() strips the leading backtick/quote, so the quoted
        // template becomes `^ACTION REQUIRED` and false-fires. A real emitted
        // gate is never wrapped in a backtick/quote at line start; that wrapper
        // is the template signature. Guard runs on the RAW line so the wrapper
        // is still visible.
        let compiled = compile(&default_rules());
        for line in [
            // backtick-wrapped (global CLAUDE.md monitoring rule + this session)
            "`ACTION REQUIRED:` — prefix a line only when Ben must personally act\n",
            "   `ACTION REQUIRED:` / a heartbeat note with the task still OPEN\n",
            // single-quote-wrapped (claude-commander-session-start-hook.py:166)
            "'ACTION REQUIRED:' ONLY when Ben must personally act\n",
            // double-quote-wrapped
            "\"ACTION REQUIRED:\" is the Moshi-notify sentinel\n",
            // smart-quote-wrapped (voice/markdown renderers emit these)
            "\u{2018}ACTION REQUIRED:\u{2019} template, not a live gate\n",
            "\u{201c}ACTION REQUIRED\u{201d} prefix a line\n",
        ] {
            assert!(
                classify(line, &compiled).is_none(),
                "quoted template text must not fire: {line:?}",
            );
        }
        // A real gate whose PAYLOAD happens to contain a backtick still fires
        // (the wrapper guard keys on the phrase itself being quoted, not on a
        // backtick anywhere in the payload).
        assert_eq!(
            classify(
                "ACTION REQUIRED: run `git push` to land the approved merge\n",
                &compiled
            )
            .map(|m| m.name.as_str()),
            Some("action-required"),
            "a real gate with a backtick in its payload must still fire",
        );
        // A bullet/blockquote-decorated real gate (non-quote leading decoration)
        // still fires — only quote/backtick wrapping is the template signature.
        for line in [
            "- ACTION REQUIRED: approve the send\n",
            "> ACTION REQUIRED: approve the send\n",
        ] {
            assert_eq!(
                classify(line, &compiled).map(|m| m.name.as_str()),
                Some("action-required"),
                "bullet/blockquote-decorated real gate must still fire: {line:?}",
            );
        }
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

    // ── WO #139: content fingerprint for the re-fire dampener ────────────

    #[test]
    fn classify_fp_is_stable_across_cosmetic_pane_churn() {
        // Two captures of the SAME open ACTION REQUIRED gate, differing only
        // in the spinner frame + elapsed/token counters on the surrounding
        // chrome lines. The fingerprint must be identical so a standing,
        // unchanged gate does not re-wake the Commander every tick.
        let compiled = compile(&default_rules());
        let pane_a = "✻ Working… (12s · ↑ 1.2k tokens)\n\
                      ACTION REQUIRED: reply send to release the outward email\n\
                      > \n";
        let pane_b = "✢ Working… (47s · ↑ 3.9k tokens)\n\
                      ACTION REQUIRED: reply send to release the outward email\n\
                      > \n";
        let (_, fp_a) = classify_fp(pane_a, &compiled).expect("gate a matches");
        let (_, fp_b) = classify_fp(pane_b, &compiled).expect("gate b matches");
        assert_eq!(
            fp_a, fp_b,
            "unchanged gate must fingerprint identically despite spinner/counter churn"
        );
    }

    #[test]
    fn classify_fp_changes_when_gate_payload_changes() {
        // A genuinely new/different gate on the same session must yield a
        // different fingerprint so it wakes immediately.
        let compiled = compile(&default_rules());
        let (_, fp1) = classify_fp(
            "ACTION REQUIRED: reply send to release the outward email\n",
            &compiled,
        )
        .expect("gate 1");
        let (_, fp2) = classify_fp(
            "ACTION REQUIRED: approve the Ramp virtual-card charge\n",
            &compiled,
        )
        .expect("gate 2");
        assert_ne!(
            fp1, fp2,
            "a changed gate must fingerprint differently so it re-wakes"
        );
    }

    #[test]
    fn classify_fp_agrees_with_classify_on_the_winning_rule() {
        let compiled = compile(&default_rules());
        let pane = "ACTION REQUIRED: reply send to approve the merge\n";
        let rule = classify(pane, &compiled).expect("classify matches");
        let (fp_rule, _) = classify_fp(pane, &compiled).expect("classify_fp matches");
        assert_eq!(
            rule.name, fp_rule.name,
            "both must pick the same winning rule"
        );
    }

    // ── WO d6bcae49: reworded held gate defeats the content dampener ──────
    // The for-AVHR (0a3ac9fc) #18 cutover gate stayed parked-on-Ben and
    // unchanged, yet the worker reworded its ACTION REQUIRED line each cycle
    // ("#18 remains parked on your authorization" vs "#18 needs only your
    // authorization"). The old full-line fingerprint flipped on every reword,
    // so the dampener saw a NEW gate and re-woke the Commander 4+ times in
    // ~30 min. An ACTION REQUIRED line is identified by the ticket/probe/WO
    // refs it cites (#N), not by the churning prose around them.

    #[test]
    fn action_gate_fingerprint_is_stable_across_reworded_held_gate() {
        let compiled = compile(&default_rules());
        let pane_a = "ACTION REQUIRED (Ben): #18 remains parked on your authorization \
                      to execute the cutover\n> \n";
        let pane_b = "ACTION REQUIRED (Ben): #18 needs only your authorization — nothing \
                      else blocks the cutover\n> \n";
        let (_, fp_a) = classify_fp(pane_a, &compiled).expect("gate a matches");
        let (_, fp_b) = classify_fp(pane_b, &compiled).expect("gate b matches");
        assert_eq!(
            fp_a, fp_b,
            "the SAME #18 gate reworded must fingerprint identically so a parked \
             gate stops re-paging the Commander for its TTL"
        );
    }

    #[test]
    fn action_gate_fingerprint_differs_for_a_different_ticket() {
        // A genuinely different gate (#24 vs #18) MUST still wake immediately.
        let compiled = compile(&default_rules());
        let (_, fp18) =
            classify_fp("ACTION REQUIRED (Ben): #18 parked on you\n", &compiled).expect("gate #18");
        let (_, fp24) =
            classify_fp("ACTION REQUIRED (Ben): #24 parked on you\n", &compiled).expect("gate #24");
        assert_ne!(
            fp18, fp24,
            "different ticket refs are different gates and must re-wake"
        );
    }

    #[test]
    fn action_gate_fingerprint_keys_on_ref_set_not_ref_order() {
        // Same gate citing the same two refs, listed in either order or with
        // reworded prose, is one fingerprint.
        let compiled = compile(&default_rules());
        let (_, fp1) = classify_fp(
            "ACTION REQUIRED: #18 and #24 both await your ok\n",
            &compiled,
        )
        .expect("gate 1");
        let (_, fp2) = classify_fp(
            "ACTION REQUIRED: #24 then #18 still need sign-off\n",
            &compiled,
        )
        .expect("gate 2");
        assert_eq!(fp1, fp2, "ref SET, not order or prose, identifies the gate");
    }

    #[test]
    fn action_gate_without_ref_falls_back_to_full_line_fingerprint() {
        // No #N cited → keep the prose fingerprint (unchanged behavior), so an
        // id-less gate is not over-collapsed with an unrelated id-less gate.
        let compiled = compile(&default_rules());
        let (_, fp1) = classify_fp(
            "ACTION REQUIRED: reply send to release the outward email\n",
            &compiled,
        )
        .expect("gate 1");
        let (_, fp2) = classify_fp(
            "ACTION REQUIRED: approve the Ramp virtual-card charge\n",
            &compiled,
        )
        .expect("gate 2");
        assert_ne!(
            fp1, fp2,
            "distinct id-less gates must stay distinct (full-line fallback)"
        );
    }
}
