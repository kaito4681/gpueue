//! The `status` query language (the same syntax as pueue 4.0):
//!
//! ```text
//! [columns=COL(,COL)*]? [FILTER]* [order_by COL [asc|desc]]? [(first|last) N]?
//! ```
//!
//! Supported like pueue:
//! - columns: `id status command label path enqueue_at dependencies start end`, plus
//!   `priority` and the puzuz columns `owner` and `gpu`.
//! - filters: `status (=|!=) queued|stashed|paused|running|success|failed`, plus the puzuz
//!   states `checkpointed`, `checkpointing`, `restoring`, `restore_queued`, `terminating` and
//!   `done`; `label`/`command` with `=`, `!=` and `%=` (contains), whose value is the rest of the
//!   query (as in pueue); `start`/`end`/`enqueue_at` with `=`, `!=`, `<` and `>` against
//!   `YYYY-MM-DD`, `HH:mm[:ss]` or `YYYY-MM-DD[ |T]HH:mm[:ss]`.
//! - `order_by` any column, `first N`/`last N`.
//!
//! Deviations: keywords are matched by hand instead of a PEG grammar, so whitespace inside
//! dates is only allowed between the date and the time; `!=` against a date means "not on that
//! day" (pueue's implementation never matches).

use std::cmp::Ordering;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta};
use puzuz_core::model::{JobResult, JobState};
use puzuz_protocol::api::JobView;

use crate::clock::Clock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Id,
    Status,
    Priority,
    EnqueueAt,
    Dependencies,
    Label,
    Owner,
    Gpu,
    Command,
    Path,
    Start,
    End,
}

impl Column {
    const KEYWORDS: [(&'static str, Column); 12] = [
        // Longer keywords first where one is a prefix of another.
        ("enqueue_at", Column::EnqueueAt),
        ("dependencies", Column::Dependencies),
        ("priority", Column::Priority),
        ("command", Column::Command),
        ("status", Column::Status),
        ("label", Column::Label),
        ("owner", Column::Owner),
        ("start", Column::Start),
        ("path", Column::Path),
        ("gpu", Column::Gpu),
        ("end", Column::End),
        ("id", Column::Id),
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    Queued,
    Stashed,
    Paused,
    Running,
    Success,
    Failed,
    Done,
    Checkpointing,
    Checkpointed,
    RestoreQueued,
    Restoring,
    Terminating,
}

impl StatusFilter {
    const KEYWORDS: [(&'static str, StatusFilter); 12] = [
        ("queued", StatusFilter::Queued),
        ("stashed", StatusFilter::Stashed),
        ("paused", StatusFilter::Paused),
        ("running", StatusFilter::Running),
        ("success", StatusFilter::Success),
        ("failed", StatusFilter::Failed),
        ("done", StatusFilter::Done),
        ("checkpointing", StatusFilter::Checkpointing),
        ("checkpointed", StatusFilter::Checkpointed),
        ("restore_queued", StatusFilter::RestoreQueued),
        ("restoring", StatusFilter::Restoring),
        ("terminating", StatusFilter::Terminating),
    ];

    fn matches(self, state: &JobState) -> bool {
        match self {
            StatusFilter::Queued => matches!(state, JobState::Queued { .. }),
            StatusFilter::Stashed => matches!(state, JobState::Stashed { .. }),
            StatusFilter::Paused => matches!(state, JobState::Paused),
            StatusFilter::Running => matches!(state, JobState::Running),
            StatusFilter::Success => matches!(
                state,
                JobState::Done {
                    result: JobResult::Success
                }
            ),
            StatusFilter::Failed => {
                matches!(state, JobState::Done { result } if *result != JobResult::Success)
            }
            StatusFilter::Done => matches!(state, JobState::Done { .. }),
            StatusFilter::Checkpointing => matches!(state, JobState::Checkpointing),
            StatusFilter::Checkpointed => matches!(state, JobState::Checkpointed),
            StatusFilter::RestoreQueued => matches!(state, JobState::RestoreQueued { .. }),
            StatusFilter::Restoring => matches!(state, JobState::Restoring),
            StatusFilter::Terminating => matches!(state, JobState::Terminating { .. }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Neq,
    Lt,
    Gt,
    Contains,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeOperand {
    DateTime(NaiveDateTime),
    Date(NaiveDate),
    Time(NaiveTime),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Filter {
    Status(Op, StatusFilter),
    Label(Op, String),
    Command(Op, String),
    Time(Column, Op, TimeOperand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limit {
    First,
    Last,
}

/// A parsed query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Explicitly selected columns; empty means the default columns.
    pub columns: Vec<Column>,
    filters: Vec<Filter>,
    /// The column and whether the order is descending.
    order_by: Option<(Column, bool)>,
    limit: Option<(Limit, usize)>,
}

impl Query {
    /// Parses the `status` query arguments (joined with spaces, as pueue does).
    pub fn parse(input: &str) -> Result<Self, String> {
        Parser { input, pos: 0 }.query()
    }

    /// Filters, orders and limits `jobs`.
    pub fn apply(&self, jobs: Vec<JobView>, clock: &Clock) -> Vec<JobView> {
        let mut jobs: Vec<JobView> = jobs
            .into_iter()
            .filter(|job| self.filters.iter().all(|f| filter_matches(f, job, clock)))
            .collect();

        if let Some((column, descending)) = self.order_by {
            jobs.sort_by(|a, b| compare(column, a, b));
            if descending {
                jobs.reverse();
            }
        }

        if let Some((limit, count)) = self.limit
            && count != 0
            && jobs.len() > count
        {
            jobs = match limit {
                Limit::First => jobs[..count].to_vec(),
                Limit::Last => jobs[jobs.len() - count..].to_vec(),
            };
        }
        jobs
    }
}

fn filter_matches(filter: &Filter, job: &JobView, clock: &Clock) -> bool {
    match filter {
        Filter::Status(op, status) => status.matches(&job.state) == (*op == Op::Eq),
        Filter::Label(op, value) => match &job.label {
            Some(label) => text_matches(*op, label, value),
            None => *op == Op::Neq,
        },
        Filter::Command(op, value) => match &job.details {
            Some(details) => text_matches(*op, &details.command, value),
            // The command of other users' jobs is unknown.
            None => false,
        },
        Filter::Time(column, op, operand) => {
            let Some(field) = time_field(*column, job) else {
                return false;
            };
            let field = clock.at(field);
            time_matches(*op, field, *operand, clock)
        }
    }
}

fn text_matches(op: Op, text: &str, value: &str) -> bool {
    match op {
        Op::Eq => text == value,
        Op::Neq => text != value,
        Op::Contains => text.contains(value),
        Op::Lt | Op::Gt => false,
    }
}

fn time_field(column: Column, job: &JobView) -> Option<i64> {
    match column {
        Column::Start => job.started_at,
        Column::End => job.finished_at,
        Column::EnqueueAt => match job.state {
            JobState::Stashed { enqueue_at } => enqueue_at,
            _ => None,
        },
        _ => None,
    }
}

fn time_matches(op: Op, field: DateTime<FixedOffset>, operand: TimeOperand, clock: &Clock) -> bool {
    let point = match operand {
        TimeOperand::DateTime(naive) => clock.localize(naive),
        TimeOperand::Time(time) => clock.localize(clock.today().and_time(time)),
        TimeOperand::Date(date) => {
            // A date stands for the whole day.
            let (Some(start), Some(end)) = (
                clock.start_of_day(date),
                date.checked_add_signed(TimeDelta::days(1))
                    .and_then(|next| clock.start_of_day(next)),
            ) else {
                return false;
            };
            let on_day = field >= start && field < end;
            return match op {
                Op::Eq => on_day,
                Op::Neq => !on_day,
                Op::Lt => field < start,
                Op::Gt => field >= end,
                Op::Contains => false,
            };
        }
    };
    let Some(point) = point else {
        return false;
    };
    match op {
        Op::Eq => field == point,
        Op::Neq => field != point,
        Op::Lt => field < point,
        Op::Gt => field > point,
        Op::Contains => false,
    }
}

/// Sort rank of a state for `order_by status` (pueue's order, with the
/// puzuz states slotted in).
fn status_rank(state: &JobState) -> u8 {
    match state {
        JobState::Stashed { .. } => 0,
        JobState::Queued { .. } | JobState::RestoreQueued { .. } => 2,
        JobState::Paused | JobState::Checkpointed => 3,
        JobState::Running
        | JobState::Checkpointing
        | JobState::Restoring
        | JobState::Terminating { .. } => 4,
        JobState::Done {
            result: JobResult::Success,
        } => 6,
        JobState::Done { .. } => 5,
    }
}

fn compare(column: Column, a: &JobView, b: &JobView) -> Ordering {
    let command = |job: &JobView| job.details.as_ref().map(|d| d.command.clone());
    let path = |job: &JobView| job.details.as_ref().map(|d| d.cwd.clone());
    let enqueue = |job: &JobView| match job.state {
        JobState::Queued { enqueued_at } | JobState::RestoreQueued { enqueued_at } => {
            Some(enqueued_at)
        }
        JobState::Stashed { enqueue_at } => enqueue_at,
        _ => None,
    };
    match column {
        Column::Id => a.id.cmp(&b.id),
        Column::Status => status_rank(&a.state).cmp(&status_rank(&b.state)),
        Column::Priority => a.priority.cmp(&b.priority),
        // Jobs without a date sort last, as in pueue.
        Column::EnqueueAt => match (enqueue(a), enqueue(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
        Column::Dependencies => a.dependencies.cmp(&b.dependencies),
        Column::Label => a.label.cmp(&b.label),
        Column::Owner => a.owner.cmp(&b.owner),
        Column::Gpu => a.gpus.cmp(&b.gpus),
        Column::Command => command(a).cmp(&command(b)),
        Column::Path => path(a).cmp(&path(b)),
        Column::Start => a.started_at.cmp(&b.started_at),
        Column::End => a.finished_at.cmp(&b.finished_at),
    }
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn query(mut self) -> Result<Query, String> {
        let mut query = Query::default();

        self.skip_ws();
        if self.eat_keyword("columns") {
            self.expect_op(&[Op::Eq])?;
            loop {
                query.columns.push(self.column()?);
                self.skip_ws();
                if !self.eat(",") {
                    break;
                }
            }
        }

        while let Some(filter) = self.filter()? {
            query.filters.push(filter);
        }

        self.skip_ws();
        if self.eat_keyword("order_by") {
            let column = self.column()?;
            self.skip_ws();
            let descending = if self.eat_keyword("desc") {
                true
            } else {
                self.eat_keyword("asc");
                false
            };
            query.order_by = Some((column, descending));
        }

        self.skip_ws();
        let limit = if self.eat_keyword("first") {
            Some(Limit::First)
        } else if self.eat_keyword("last") {
            Some(Limit::Last)
        } else {
            None
        };
        if let Some(limit) = limit {
            self.skip_ws();
            let digits = self.take_while(|c| c.is_ascii_digit());
            let count = if digits.is_empty() {
                0
            } else {
                digits
                    .parse()
                    .map_err(|_| format!("invalid limit `{digits}`"))?
            };
            query.limit = Some((limit, count));
        }

        self.skip_ws();
        if self.pos < self.input.len() {
            return Err(format!(
                "failed to parse query: unexpected `{}`",
                &self.input[self.pos..]
            ));
        }
        Ok(query)
    }

    /// Parses one filter, or returns `None` (without consuming input) if the
    /// next word does not start a filter.
    fn filter(&mut self) -> Result<Option<Filter>, String> {
        self.skip_ws();
        let start = self.pos;
        let column = Column::KEYWORDS
            .iter()
            .find(|(keyword, _)| self.eat_keyword(keyword))
            .map(|(_, column)| *column);
        let Some(column) = column else {
            return Ok(None);
        };
        let filter = match column {
            Column::Status => {
                let op = self.expect_op(&[Op::Eq, Op::Neq])?;
                self.skip_ws();
                let word = self.take_while(|c| c.is_ascii_alphanumeric() || c == '_');
                let status = StatusFilter::KEYWORDS
                    .iter()
                    .find(|(keyword, _)| keyword.eq_ignore_ascii_case(word))
                    .map(|(_, status)| *status)
                    .ok_or_else(|| format!("unknown status `{word}` in query"))?;
                Filter::Status(op, status)
            }
            Column::Label | Column::Command => {
                let op = self.expect_op(&[Op::Eq, Op::Neq, Op::Contains])?;
                self.skip_ws();
                // As in pueue, the value is the rest of the query.
                let value = self.input[self.pos..].trim_end().to_owned();
                self.pos = self.input.len();
                if column == Column::Label {
                    Filter::Label(op, value)
                } else {
                    Filter::Command(op, value)
                }
            }
            Column::Start | Column::End | Column::EnqueueAt => {
                let op = self.expect_op(&[Op::Eq, Op::Neq, Op::Lt, Op::Gt])?;
                self.skip_ws();
                Filter::Time(column, op, self.time_operand()?)
            }
            // Not a filter column: maybe `order_by`'s argument or garbage;
            // let the caller decide.
            _ => {
                self.pos = start;
                return Ok(None);
            }
        };
        Ok(Some(filter))
    }

    fn time_operand(&mut self) -> Result<TimeOperand, String> {
        let rest = &self.input[self.pos..];
        let token_len = rest.find(' ').unwrap_or(rest.len());
        let first = &rest[..token_len];

        if let Ok(time) = parse_time(first) {
            self.pos += token_len;
            return Ok(TimeOperand::Time(time));
        }

        // `YYYY-MM-DD`, optionally followed by a time after ` `, `T` or
        // nothing.
        if first.len() >= 10
            && let Ok(date) = NaiveDate::parse_from_str(&first[..10], "%Y-%m-%d")
        {
            let after_date = &first[10..];
            let time_text = after_date.strip_prefix('T').unwrap_or(after_date);
            if !time_text.is_empty() {
                let time = parse_time(time_text)
                    .map_err(|_| format!("invalid time `{time_text}` in query"))?;
                self.pos += token_len;
                return Ok(TimeOperand::DateTime(date.and_time(time)));
            }
            self.pos += token_len;
            // A time may follow as a separate word.
            let save = self.pos;
            self.skip_ws();
            let rest = &self.input[self.pos..];
            let next_len = rest.find(' ').unwrap_or(rest.len());
            if next_len > 0
                && let Ok(time) = parse_time(&rest[..next_len])
            {
                self.pos += next_len;
                return Ok(TimeOperand::DateTime(date.and_time(time)));
            }
            self.pos = save;
            return Ok(TimeOperand::Date(date));
        }

        Err(format!(
            "expected `YYYY-MM-DD`, `HH:mm[:ss]` or `YYYY-MM-DD HH:mm[:ss]`, found `{first}`"
        ))
    }

    fn column(&mut self) -> Result<Column, String> {
        self.skip_ws();
        Column::KEYWORDS
            .iter()
            .find(|(keyword, _)| self.eat_keyword(keyword))
            .map(|(_, column)| *column)
            .ok_or_else(|| {
                let rest = &self.input[self.pos..];
                let word = rest.split([' ', ',']).next().unwrap_or(rest);
                format!("unknown column `{word}` in query")
            })
    }

    fn expect_op(&mut self, allowed: &[Op]) -> Result<Op, String> {
        self.skip_ws();
        let ops = [
            ("!=", Op::Neq),
            ("%=", Op::Contains),
            ("=", Op::Eq),
            ("<", Op::Lt),
            (">", Op::Gt),
        ];
        for (text, op) in ops {
            if self.input[self.pos..].starts_with(text) {
                if !allowed.contains(&op) {
                    return Err(format!("operator `{text}` is not supported here"));
                }
                self.pos += text.len();
                return Ok(op);
            }
        }
        Err(format!(
            "expected an operator at `{}`",
            &self.input[self.pos..]
        ))
    }

    fn skip_ws(&mut self) {
        while self.input[self.pos..].starts_with(' ') {
            self.pos += 1;
        }
    }

    fn eat(&mut self, text: &str) -> bool {
        if self.input[self.pos..].starts_with(text) {
            self.pos += text.len();
            true
        } else {
            false
        }
    }

    /// Consumes `keyword` case-insensitively.
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        let rest = &self.input[self.pos..];
        if rest.len() >= keyword.len()
            && rest.is_char_boundary(keyword.len())
            && rest[..keyword.len()].eq_ignore_ascii_case(keyword)
        {
            self.pos += keyword.len();
            true
        } else {
            false
        }
    }

    fn take_while(&mut self, pred: impl Fn(char) -> bool) -> &str {
        let rest = &self.input[self.pos..];
        let len = rest.find(|c: char| !pred(c)).unwrap_or(rest.len());
        self.pos += len;
        &rest[..len]
    }
}

fn parse_time(text: &str) -> Result<NaiveTime, chrono::ParseError> {
    NaiveTime::parse_from_str(text, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(text, "%H:%M"))
}

#[cfg(test)]
mod tests;
