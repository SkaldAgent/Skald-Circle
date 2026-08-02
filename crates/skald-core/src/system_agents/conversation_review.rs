//! The conversation review — a nightly read of what a supervised person and the
//! assistant said to each other, turned into one report.
//!
//! The first [`AgentScope::PerSubject`] agent, and the reason that scope exists.
//! Everything it reads belongs to the subject; everything it leaves behind — the
//! ephemeral session, the run row — belongs to the supervisor whose runtime it
//! borrowed; and the one thing that crosses between them is the report, in
//! `system.db`, where the people entitled to it can read it.
//!
//! ## One report per person, never one per conversation
//!
//! A day's activity is spread over however many sessions somebody happened to
//! open, and reviewing them one at a time would produce a stack of fragments
//! nobody can act on — the useful signal is often *across* conversations (the
//! same subject raised twice, in two places, hours apart). So a pass takes the
//! whole window at once: every session, in one transcript, one turn, one report.
//!
//! ## What the model is shown, and what it is not
//!
//! Only what was **said** — see [`chat_history::conversation_window`] for the
//! four exclusions and why each one exists. Tool calls and their results are not
//! filtered out so much as absent by construction: they live in a different
//! table. The consequence is real and the prompt says so plainly, because a model
//! shown a gap will otherwise narrate over it — a web search the assistant ran is
//! invisible, query included.
//!
//! ## Why the report is the turn's own answer
//!
//! There is no `save_report` tool. The final assistant message *is* the body, and
//! this module writes the row. A tool would have to be whitelisted past the
//! approval gate — an unattended pass auto-denies anything gated — and would add
//! a way for the pass to silently produce nothing at all. The cost of not having
//! one is that the model cannot set a severity; see [`REPORT_SEVERITY`].

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use sqlx::SqlitePool;
use tracing::warn;

use core_api::system_bus::{SystemEvent, SystemEventBus};
use core_api::{ConfigProperty, ConfigSet, PropertyType};

use crate::config_store::GlobalConfigManager;
use crate::db::chat_history::{self, TranscriptLine};
use crate::db::{reports, system_agent_coverage};

use super::{
    AgentOutcome, AgentRunCtx, AgentScope, SystemAgent, configured_run_context,
    enabled_from_config, enabled_property, run_ephemeral_turn, security_group_property,
};

pub const CONVERSATION_REVIEW_AGENT: &str = "conversation-review";

/// The chat `source` a pass runs under, and the `kind` of the reports it writes.
/// Same string on purpose: one name to grep for when tracing where a report came
/// from.
const REVIEW_SOURCE: &str = "conversation-review";

pub const ENABLED_KEY:        &str = "conversation_review.enabled";
pub const SECURITY_GROUP_KEY: &str = "conversation_review.security_group";
pub const RUN_AT_HOUR_KEY:    &str = "conversation_review.run_at_hour";

/// 4am: late enough that the day is over, early enough that the report is waiting
/// when somebody wakes up.
const DEFAULT_RUN_AT_HOUR: u32 = 4;

const DAY_SECS: u64 = 24 * 60 * 60;

/// How far back the very first pass for a subject looks. Their history may go
/// back months; opening with a report on all of it would be expensive, mostly
/// stale, and unlike every report after it.
const FIRST_WINDOW_HOURS: i64 = 24;

/// Caps on what one turn is shown. A day of chatter is normally far below these;
/// they exist so that an outlier costs a truncated report rather than a refused
/// request.
const MAX_MESSAGES:      i64   = 600;
const MAX_MESSAGE_CHARS: usize = 2_000;

/// What the agent answers when the window holds nothing worth writing up.
///
/// A sentinel rather than a judgement call in the parser: "did the model mean
/// there was nothing?" is not a question worth asking of prose, and getting it
/// wrong in the lenient direction files an empty report every single night.
pub const NOTHING_TO_REPORT: &str = "NOTHING_TO_REPORT";

/// Every report this agent files carries the same severity.
///
/// Not laziness — a consequence of the report being the turn's own answer: there
/// is no structured channel for the model to grade its own finding on, and
/// inferring one from prose would be a guess presented as a fact. `notice` is the
/// honest middle: this was worth writing down, and a human decides how much it
/// matters. A grade would come from giving the agent a structured hand-off, which
/// is a change to make deliberately rather than by parsing.
const REPORT_SEVERITY: &str = reports::SEVERITY_NOTICE;

pub fn config_set() -> ConfigSet {
    ConfigSet {
        name:        "Conversation review".into(),
        description: "A daily read of the conversations of the people someone supervises. For one \
                      subject at a time it reads everything they and the assistant said to each \
                      other since the previous review, and writes a single report about the whole \
                      stretch — not one per conversation. The report is stored for the people who \
                      supervise that person; the subject does not see it. Tool calls are not \
                      included, so what a connector did on their behalf is outside what it can \
                      see. Nobody is reviewed unless a supervision link says so."
            .into(),
        properties:  vec![
            enabled_property(
                ENABLED_KEY,
                "Enable the conversation review for the whole instance. When disabled, nobody is \
                 reviewed, whatever the supervision links say.",
            ),
            security_group_property(SECURITY_GROUP_KEY),
            ConfigProperty {
                key:           RUN_AT_HOUR_KEY.into(),
                name:          "Run at (hour)".into(),
                description:   "Hour of the day, 0–23 in this machine's local time, after which \
                                the review runs. It runs once per day per person; if the machine \
                                was off at that hour, the next start catches up and the report \
                                covers the whole stretch that was missed."
                    .into(),
                property_type: PropertyType::Int,
                default_value: Some(DEFAULT_RUN_AT_HOUR.to_string()),
            },
        ],
        owner:       Some(CONVERSATION_REVIEW_AGENT.into()),
    }
}

pub struct ConversationReviewAgent {
    config_store:  Arc<GlobalConfigManager>,
    /// `system.db` — the supervision edges, the coverage watermarks and the
    /// reports all live here.
    registry_pool: Arc<SqlitePool>,
    system_bus:    Arc<SystemEventBus>,
}

impl ConversationReviewAgent {
    pub fn new(
        config_store:  Arc<GlobalConfigManager>,
        registry_pool: Arc<SqlitePool>,
        system_bus:    Arc<SystemEventBus>,
    ) -> Arc<Self> {
        Arc::new(Self { config_store, registry_pool, system_bus })
    }

    async fn run_at_hour(&self) -> u32 {
        match self.config_store.get(RUN_AT_HOUR_KEY).await {
            Ok(Some(v)) => v.trim().parse::<u32>().ok().filter(|h| *h <= 23).unwrap_or(DEFAULT_RUN_AT_HOUR),
            _           => DEFAULT_RUN_AT_HOUR,
        }
    }

    /// The window this pass would cover, or `None` when the subject is not due.
    ///
    /// Both halves of scheduling live here, together, because they are one
    /// question: *is there a stretch of time we have not looked at yet, ending
    /// after today's hour?* Splitting them across `is_due` and `has_work` was what
    /// made the first sketch wrong — the attempt marker moves before the work, so
    /// by the time the agent ran, the window it was meant to cover had already
    /// been marked as covered.
    async fn window_for(
        &self,
        subject: &str,
        now:     DateTime<Utc>,
    ) -> Result<Option<(String, String)>> {
        let covered = system_agent_coverage::covered_through(
            &self.registry_pool,
            CONVERSATION_REVIEW_AGENT,
            subject,
        )
        .await?;

        let start = covered.unwrap_or_else(|| {
            system_agent_coverage::stamp(now - Duration::hours(FIRST_WINDOW_HOURS))
        });

        // Due when the covered stretch stops before the most recent occurrence of
        // the configured hour. That single comparison is what makes the schedule
        // survive downtime: a machine off for three days simply finds a watermark
        // three days old, and covers all of it in one pass.
        let boundary = system_agent_coverage::stamp(
            most_recent_occurrence(&Local, self.run_at_hour().await, now),
        );
        if start >= boundary {
            return Ok(None);
        }

        Ok(Some((start, system_agent_coverage::stamp(now))))
    }
}

#[async_trait]
impl SystemAgent for ConversationReviewAgent {
    fn id(&self) -> &'static str { CONVERSATION_REVIEW_AGENT }

    fn scope(&self) -> AgentScope { AgentScope::PerSubject }

    fn config_set(&self) -> ConfigSet { config_set() }

    fn interval_key(&self) -> &'static str { RUN_AT_HOUR_KEY }

    async fn is_enabled(&self) -> bool {
        enabled_from_config(&self.config_store, ENABLED_KEY).await
    }

    /// Daily. Only feeds the scheduler's sleep computation — the actual cadence is
    /// the hour-of-day check in [`Self::window_for`], and the tick is clamped well
    /// below a day regardless.
    async fn interval_secs(&self) -> u64 { DAY_SECS }

    async fn has_work(&self, ctx: &AgentRunCtx<'_>) -> Result<bool> {
        let Some(subject) = ctx.subject else {
            warn!(agent = CONVERSATION_REVIEW_AGENT, "no subject on the run context; skipping");
            return Ok(false);
        };

        let Some((since, until)) = self.window_for(subject.user_id, Utc::now()).await? else {
            return Ok(false);
        };

        // Due, but the stretch may still be empty — somebody who did not open the
        // assistant yesterday should collect no run row and no report.
        let n = chat_history::conversation_window_count(subject.pool, &since, &until).await?;
        Ok(n > 0)
    }

    async fn run(&self, ctx: &AgentRunCtx<'_>) -> Result<AgentOutcome> {
        let subject = ctx.subject.ok_or_else(|| anyhow::anyhow!("no subject on the run context"))?;
        let now     = Utc::now();

        let Some((since, until)) = self.window_for(subject.user_id, now).await? else {
            // `has_work` said yes a moment ago; only a concurrent pass could land
            // here, and the scheduler is single-instance. Treat it as a no-op
            // rather than an error.
            return Ok(AgentOutcome {
                session_id: None,
                stats:      serde_json::json!({ "skipped": "not due" }),
            });
        };

        let total = chat_history::conversation_window_count(subject.pool, &since, &until).await?;
        let lines = chat_history::conversation_window(subject.pool, &since, &until, MAX_MESSAGES).await?;
        let dropped = (total - lines.len() as i64).max(0) as usize;
        let sessions = distinct_sessions(&lines);

        let transcript = build_transcript(subject.username, &lines, dropped);
        let prompt = build_prompt(subject.username, &since, &until, &transcript);

        // The security group is the **acting** user's business: the pass runs in
        // the supervisor's runtime, on their permissions, and reconciling against
        // the subject's role would hand a restricted account's tool set to the
        // person reviewing it.
        let rc = configured_run_context(
            &self.config_store,
            &self.registry_pool,
            SECURITY_GROUP_KEY,
            ctx.user_id,
        )
        .await;

        // Who the report is about, in the system prompt rather than the trigger
        // message: an age, a name and a sex change what counts as worth reporting
        // — the same sentence reads differently from a nine-year-old and from a
        // seventeen-year-old — so the model must have it before it reads a word of
        // the transcript. It cannot come from `__USER_PROFILE__`, which resolves
        // the session owner, and the session belongs to the supervisor.
        let mut substitutions = std::collections::HashMap::new();
        substitutions.insert(
            "SUBJECT_PROFILE".to_string(),
            crate::loop_adapters::system::render_user_profile_section(
                &self.registry_pool,
                subject.user_id,
            )
            .await
            .unwrap_or_else(|e| {
                warn!(user = %subject.user_id, error = %e, "conversation-review: no subject profile");
                "unknown".to_string()
            }),
        );

        let (session_id, _) = run_ephemeral_turn(
            CONVERSATION_REVIEW_AGENT,
            REVIEW_SOURCE,
            &prompt,
            rc.as_ref(),
            "Conversation review",
            substitutions,
            ctx,
        )
        .await?;

        // The turn ran in the supervisor's runtime, so its answer is in their file.
        let answer = chat_history::last_assistant_for_session(ctx.pool, session_id)
            .await?
            .unwrap_or_default();

        let report_id = match parse_report(&answer) {
            None => None,
            Some(ParsedReport { title, summary, body }) => {
                let title = title.unwrap_or_else(|| {
                    format!("Conversation review — {} — {}", subject.username, &until[..10])
                });
                let id = reports::create(&self.registry_pool, &reports::NewReport {
                    kind:             CONVERSATION_REVIEW_AGENT,
                    title:            &title,
                    summary:          summary.as_deref(),
                    body:             &body,
                    severity:         REPORT_SEVERITY,
                    subject_user_id:  Some(subject.user_id),
                    audience:         reports::AUDIENCE_SUPERVISORS,
                    period_start:     Some(&since),
                    period_end:       Some(&until),
                    produced_by:      CONVERSATION_REVIEW_AGENT,
                    producer_user_id: Some(ctx.user_id),
                    run_id:           ctx.run_id,
                    metadata:         Some(&serde_json::json!({
                        "messages_examined": lines.len(),
                        "messages_dropped":  dropped,
                        "sessions":          sessions,
                    }).to_string()),
                })
                .await?;

                // Announced, not delivered. Who should hear about a new report —
                // the supervisors, a badge, a future digest — is not this agent's
                // business, and wiring it here would make every new recipient a
                // change to the reviewer.
                let _ = self.system_bus.send(SystemEvent::ReportCreated {
                    report_id:       id,
                    kind:            CONVERSATION_REVIEW_AGENT.to_string(),
                    subject_user_id: Some(subject.user_id.to_string()),
                });
                Some(id)
            }
        };

        // Only now, and only here: the watermark moves because the stretch was
        // actually looked at. A pass that failed above never reaches this line, so
        // the same window is offered again next time — a duplicate report being a
        // nuisance and a missed window being a blind spot.
        system_agent_coverage::advance(
            &self.registry_pool,
            CONVERSATION_REVIEW_AGENT,
            subject.user_id,
            &until,
        )
        .await?;

        Ok(AgentOutcome {
            session_id: Some(session_id),
            stats:      serde_json::json!({
                "subject":           subject.user_id,
                "window_start":      since,
                "window_end":        until,
                "messages_examined": lines.len(),
                "messages_dropped":  dropped,
                "sessions":          sessions,
                "report_id":         report_id,
            }),
        })
    }
}

// ── Transcript ────────────────────────────────────────────────────────────────

fn distinct_sessions(lines: &[TranscriptLine]) -> usize {
    let mut seen: Vec<i64> = Vec::new();
    for l in lines {
        if !seen.contains(&l.session_id) {
            seen.push(l.session_id);
        }
    }
    seen.len()
}

/// Render the window as a readable transcript, grouped by conversation.
///
/// **Prose, not JSON**, and the choice is about what the model does with it: a
/// dialogue read as a dialogue is what these models are best at, JSON spends
/// tokens on syntax, and — the deciding argument — nothing machine-readable comes
/// back this way. The structured artefact is the report, on the other end.
///
/// Grouped by session rather than strictly chronological because the question
/// "what was this conversation about" is answered by contiguity; sessions are
/// ordered by when each one was first spoken in, so the day still reads forwards.
fn build_transcript(subject_label: &str, lines: &[TranscriptLine], dropped: usize) -> String {
    if lines.is_empty() {
        return "(no messages in this window)".to_string();
    }

    let mut out = String::new();
    if dropped > 0 {
        out.push_str(&format!(
            "> Note: {dropped} older message(s) in this window were left out to fit. What follows \
             is the most recent part of the stretch.\n\n",
        ));
    }

    let mut order: Vec<i64> = Vec::new();
    for l in lines {
        if !order.contains(&l.session_id) {
            order.push(l.session_id);
        }
    }

    for session_id in order {
        let head = lines.iter().find(|l| l.session_id == session_id).expect("session came from lines");
        let title = head.session_title.as_deref().filter(|t| !t.is_empty()).unwrap_or("untitled");
        out.push_str(&format!(
            "\n## Conversation {session_id} — \"{title}\" (via {}, assistant: {})\n\n",
            head.source, head.agent_id,
        ));

        for line in lines.iter().filter(|l| l.session_id == session_id) {
            let who = if line.role == "user" { subject_label } else { "assistant" };
            out.push_str(&format!(
                "[{}] {who}: {}\n\n",
                line.created_at,
                truncate(&line.content, MAX_MESSAGE_CHARS),
            ));
        }
    }

    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [truncated]")
}

/// The trigger message. Thin on purpose: *how* to review is the agent's
/// `AGENT.md`, and a second copy of it here would be one to keep in step.
fn build_prompt(subject_label: &str, since: &str, until: &str, transcript: &str) -> String {
    format!(
        "[REVIEW] Scheduled review of {subject_label}'s conversations\n\
         Window: {since} → {until} (UTC)\n\n\
         Below is everything {subject_label} and the assistant said to each other in that window, \
         grouped by conversation. Tool calls and their results are not included.\n\n\
         Read it, and write the report. If there is nothing worth reporting, answer with \
         `{NOTHING_TO_REPORT}` and nothing else.\n\n\
         ---\n\n{transcript}"
    )
}

// ── The answer ────────────────────────────────────────────────────────────────

struct ParsedReport {
    title:   Option<String>,
    summary: Option<String>,
    body:    String,
}

/// Turn the turn's answer into a report, or `None` for "nothing to report".
///
/// Deliberately shallow, and it only works because the report's shape is fixed
/// by the prompt: a leading heading, then one summary paragraph, then sections.
/// So the heading becomes the title — a document's first heading *is* its title —
/// and the opening paragraph becomes the summary, whole rather than by its first
/// line, because a paragraph written to be the summary is exactly what
/// `reports.summary` is for. Anything more would be parsing prose, which is how a
/// report ends up filed under half a sentence.
fn parse_report(answer: &str) -> Option<ParsedReport> {
    let answer = answer.trim();
    if answer.is_empty() {
        return None;
    }
    // Lenient on the sentinel: a model that adds a sentence after it still means
    // the same thing, and the alternative is filing that sentence as a report.
    if answer.lines().next().is_some_and(|l| l.trim().starts_with(NOTHING_TO_REPORT)) {
        return None;
    }

    let mut lines = answer.lines().peekable();
    let mut title = None;
    if let Some(first) = lines.peek() {
        if let Some(heading) = first.trim().strip_prefix("# ") {
            let heading = heading.trim();
            if !heading.is_empty() {
                title = Some(heading.to_string());
                lines.next();
            }
        }
    }

    let body: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    let body = if body.is_empty() { answer.to_string() } else { body };

    Some(ParsedReport { title, summary: leading_paragraph(&body), body })
}

/// The first paragraph of prose: everything from the first ordinary line up to
/// the blank line that ends it, flattened onto one line.
///
/// Headings and rules are skipped on the way in, so a body that opens with a
/// `## Summary` heading yields the paragraph under it rather than the word
/// "Summary".
fn leading_paragraph(body: &str) -> Option<String> {
    let mut para: Vec<&str> = Vec::new();
    for line in body.lines().map(str::trim) {
        let skippable = line.is_empty() || line.starts_with('#') || line.starts_with("---");
        match (skippable, para.is_empty()) {
            (true, true)  => continue,      // still looking for the paragraph
            (true, false) => break,         // it just ended
            (false, _)    => para.push(line),
        }
    }
    (!para.is_empty()).then(|| truncate(&para.join(" "), 400))
}

/// The most recent moment at which the local clock read `hour:00`, at or before
/// `now`.
///
/// Generic over the timezone so it can be tested without depending on where the
/// machine is. Resolution goes through the timezone rather than arithmetic on
/// UTC, so an hour that a DST jump skipped is handled instead of silently landing
/// an hour out: today's candidate and yesterday's are both resolved, and the
/// latest one that exists and has already passed wins.
fn most_recent_occurrence<Tz: TimeZone>(tz: &Tz, hour: u32, now: DateTime<Utc>) -> DateTime<Utc> {
    let local_now = now.with_timezone(tz);
    let today     = local_now.date_naive();

    let mut best: Option<DateTime<Utc>> = None;
    for back in 0..=1 {
        let Some(day) = today.checked_sub_days(chrono::Days::new(back)) else { continue };
        let Some(naive) = day.and_hms_opt(hour.min(23), 0, 0) else { continue };
        // `.earliest()` is `None` inside a DST gap — that wall-clock time did not
        // happen on that day, so there is nothing to pick.
        let Some(candidate) = tz.from_local_datetime(&naive).earliest() else { continue };
        let candidate = candidate.with_timezone(&Utc);
        if candidate <= now && best.is_none_or(|b| candidate > b) {
            best = Some(candidate);
        }
    }

    // Neither candidate resolved (a DST gap on both days, which no real zone does):
    // fall back to a full day back, which is never later than the true answer.
    best.unwrap_or(now - Duration::days(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(session_id: i64, title: &str, role: &str, content: &str, at: &str) -> TranscriptLine {
        TranscriptLine {
            session_id,
            session_title: Some(title.to_string()),
            source:        "web".into(),
            agent_id:      "kid".into(),
            role:          role.into(),
            content:       content.into(),
            created_at:    at.into(),
        }
    }

    #[test]
    fn the_transcript_groups_by_conversation_and_names_the_person() {
        let lines = vec![
            line(12, "Homework", "user",      "help me with history", "2026-07-28 21:04:00"),
            line(12, "Homework", "assistant", "sure",                 "2026-07-28 21:04:30"),
            line(15, "",         "user",      "are you awake",        "2026-07-29 02:31:00"),
            line(12, "Homework", "user",      "one more thing",       "2026-07-29 07:00:00"),
        ];

        let t = build_transcript("luca", &lines, 0);

        // Two conversations, in the order they were first spoken in.
        assert_eq!(t.matches("## Conversation").count(), 2);
        assert!(t.find("Conversation 12").unwrap() < t.find("Conversation 15").unwrap());
        // A session with no title still reads as something.
        assert!(t.contains("\"untitled\""));
        // The person is named; the machine is not named after them.
        assert!(t.contains("luca: help me with history"));
        assert!(t.contains("assistant: sure"));
        // Later messages of an earlier conversation stay with it.
        let block12 = &t[t.find("Conversation 12").unwrap()..t.find("Conversation 15").unwrap()];
        assert!(block12.contains("one more thing"));
        // Timestamps survive: "at 2am" is half the finding.
        assert!(t.contains("[2026-07-29 02:31:00]"));
    }

    #[test]
    fn dropped_messages_are_declared_not_hidden() {
        let lines = vec![line(1, "t", "user", "hi", "2026-07-28 21:04:00")];
        let t = build_transcript("luca", &lines, 42);
        assert!(t.contains("42 older message(s)"), "a truncated window must say so");

        assert!(build_transcript("luca", &[], 0).contains("no messages"));
    }

    #[test]
    fn long_messages_are_truncated_with_a_marker() {
        let long = "x".repeat(MAX_MESSAGE_CHARS + 500);
        let t = build_transcript("luca", &[line(1, "t", "user", &long, "2026-07-28 21:04:00")], 0);
        assert!(t.contains("[truncated]"));
        assert!(t.len() < long.len() + 500);
    }

    #[test]
    fn the_sentinel_files_nothing() {
        assert!(parse_report(NOTHING_TO_REPORT).is_none());
        assert!(parse_report("  NOTHING_TO_REPORT  \n").is_none());
        assert!(parse_report("NOTHING_TO_REPORT — quiet day").is_none(),
                "a model that explains itself still means nothing to report");
        assert!(parse_report("").is_none());
        assert!(parse_report("   \n  ").is_none());
    }

    /// The shape the prompt asks for: heading, summary paragraph, then sections.
    #[test]
    fn the_report_shape_maps_onto_the_row() {
        let answer = "# Late-night messages\n\
                      \n\
                      Three conversations after midnight, all about the same worry.\n\
                      Nothing was said that needs acting on tonight.\n\
                      \n\
                      ## What happened\n\
                      \n\
                      Detail follows.\n\
                      \n\
                      ## Worth knowing\n\
                      \n\
                      More detail.";
        let parsed = parse_report(answer).expect("this is a report");

        assert_eq!(parsed.title.as_deref(), Some("Late-night messages"));
        assert!(!parsed.body.starts_with('#'), "the title is not repeated in the body");
        assert!(parsed.body.contains("## What happened"), "the sections stay in the body");
        // The whole opening paragraph, on one line — not just its first sentence.
        assert_eq!(
            parsed.summary.as_deref(),
            Some("Three conversations after midnight, all about the same worry. \
                  Nothing was said that needs acting on tonight."),
        );
    }

    #[test]
    fn a_summary_under_its_own_heading_is_still_found() {
        let parsed = parse_report("# Title\n\n## Summary\n\nThe paragraph that matters.\n\n## Detail\n\nx")
            .expect("this is a report");
        assert_eq!(parsed.summary.as_deref(), Some("The paragraph that matters."),
                   "a heading must not be mistaken for the paragraph it introduces");
    }

    #[test]
    fn a_report_without_a_heading_keeps_its_whole_body() {
        let parsed = parse_report("Nothing structural, just prose.\n\nMore prose.")
            .expect("this is a report");
        assert!(parsed.title.is_none(), "the caller supplies a title when the model gives none");
        assert!(parsed.body.starts_with("Nothing structural"));
        assert_eq!(parsed.summary.as_deref(), Some("Nothing structural, just prose."));

        // A `#` that is not a heading (no space) is body, not a title.
        let parsed = parse_report("#hashtag not a heading").expect("this is a report");
        assert!(parsed.title.is_none());
        assert_eq!(parsed.body, "#hashtag not a heading");
    }

    #[test]
    fn the_daily_boundary_is_the_most_recent_occurrence_of_the_hour() {
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);

        // Later the same day: today's 04:00.
        assert_eq!(
            most_recent_occurrence(&Utc, 4, at("2026-07-29T09:00:00Z")),
            at("2026-07-29T04:00:00Z"),
        );
        // Before it: yesterday's.
        assert_eq!(
            most_recent_occurrence(&Utc, 4, at("2026-07-29T02:00:00Z")),
            at("2026-07-28T04:00:00Z"),
        );
        // Exactly on the hour counts as passed, so the pass fires at 04:00 sharp.
        assert_eq!(
            most_recent_occurrence(&Utc, 4, at("2026-07-29T04:00:00Z")),
            at("2026-07-29T04:00:00Z"),
        );
        // Midnight is an hour like any other.
        assert_eq!(
            most_recent_occurrence(&Utc, 0, at("2026-07-29T00:30:00Z")),
            at("2026-07-29T00:00:00Z"),
        );
        // A machine that was off for days still gets one boundary, not none: what
        // makes the missed window recoverable is that the watermark is older than
        // this, not that the boundary moved.
        assert_eq!(
            most_recent_occurrence(&Utc, 4, at("2026-08-02T05:00:00Z")),
            at("2026-08-02T04:00:00Z"),
        );
    }

    #[test]
    fn the_prompt_states_the_window_and_the_tool_blind_spot() {
        let p = build_prompt("luca", "2026-07-28 04:00:00", "2026-07-29 04:00:00", "…");
        assert!(p.contains("luca"));
        assert!(p.contains("2026-07-28 04:00:00"));
        assert!(p.contains("Tool calls and their results are not included"));
        assert!(p.contains(NOTHING_TO_REPORT));
    }
}
