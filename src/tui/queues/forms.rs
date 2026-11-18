//! The forms of the queues view, on the form engine every form of the
//! interface uses (a chooser with one entry and its fields, confirmed before
//! it sends):
//!
//! * **create** — name, source, queue, mode, schedule and the first script
//!   (R31). The settings the CLI sets are not here (R34).
//! * **edit** — queue, mode and schedule of an observer; only what changed
//!   is sent (R35).
//! * **script** — the paste field for the next script version, when no
//!   editor is set (KTD-10). Saving never approves it (R32).
//!
//! The server checks what is sent; what it refuses is shown next to the
//! field it is about, and the form stays open with what was typed.

use narsor_protocol::controller::{
    CreateObserverRequest, ObserverMode, ObserverView, UpdateObserverRequest,
};

use super::super::chooser::{Chooser, ChooserField, ChooserItem, Submission};
use super::super::scroll::one_line;
use super::detail::schedule_value;
use super::{QueueWrite, CLI_HINT};

/// The observer's name.
pub const NAME: &str = "name";
/// The source it polls.
pub const SOURCE: &str = "source";
/// The queue it serves.
pub const QUEUE: &str = "queue";
/// Its run mode.
pub const MODE: &str = "mode";
/// Its schedule: seconds or a cron expression.
pub const SCHEDULE: &str = "schedule";
/// The script.
pub const SCRIPT: &str = "script";

/// The modes a form offers, in the order of the select.
const MODES: [&str; 2] = ["on_episodes", "always"];

const SCHEDULE_HELP: &str =
    "seconds between runs (60 or more), or a five-field cron expression evaluated in UTC";

/// A field of a form and why its value is refused.
pub type FieldError = (&'static str, String);

/// A schedule as the requests carry it: the seconds or the cron expression.
type Schedule = (Option<u64>, Option<String>);

/// Which form it is.
#[derive(Debug, Clone, PartialEq)]
pub enum FormKind {
    /// A new observer.
    Create,
    /// The queue, mode and schedule of an observer.
    Edit {
        /// The observer as it was read.
        original: Box<ObserverView>,
    },
    /// The next script version, pasted.
    Script {
        /// The observer, by name.
        observer: String,
    },
}

/// One open form.
#[derive(Debug, Clone)]
pub struct ObserverForm {
    kind: FormKind,
    chooser: Chooser,
}

impl ObserverForm {
    /// The create form. `queue` and `mode` are the open queue's, when the
    /// form was opened from one.
    pub fn create(queue: Option<&str>, mode: Option<&str>) -> Self {
        let mut queue_field = ChooserField::text(QUEUE).described(
            "the queue it serves; its name when left blank. A gate's `watch.queue` names it",
        );
        if let Some(queue) = queue {
            queue_field = queue_field.with_value(queue);
        }
        let item = ChooserItem::new(
            "create observer",
            format!(
                "Creates a draft observer. Nothing runs until a script version is approved: the \
                 first script is saved, not approved.\n\n{CLI_HINT}"
            ),
        )
        .with_field(
            ChooserField::text(NAME)
                .required()
                .described("unique name of the observer"),
        )
        .with_field(
            ChooserField::text(SOURCE)
                .required()
                .described("what it polls: gitlab, jira, a host name, …"),
        )
        .with_field(queue_field)
        .with_field(mode_field(mode))
        .with_field(
            ChooserField::text(SCHEDULE)
                .required()
                .described(SCHEDULE_HELP),
        )
        .with_field(
            ChooserField::multiline(SCRIPT)
                .described("the first script version (paste it here); saving never approves it"),
        );
        Self::open(FormKind::Create, item)
    }

    /// The edit form of `observer`, filled with what it has now.
    pub fn edit(observer: &ObserverView) -> Self {
        let item = ChooserItem::new(
            format!("edit {}", one_line(&observer.name)),
            format!(
                "Changes the queue, the mode and the schedule. Every observer of a queue has \
                 the same mode.\n\n{CLI_HINT}"
            ),
        )
        .with_field(
            ChooserField::text(QUEUE)
                .required()
                .with_value(observer.queue.clone())
                .described("the queue it serves"),
        )
        .with_field(mode_field(Some(&observer.mode)))
        .with_field(
            ChooserField::text(SCHEDULE)
                .required()
                .with_value(schedule_value(observer))
                .described(SCHEDULE_HELP),
        );
        Self::open(
            FormKind::Edit {
                original: Box::new(observer.clone()),
            },
            item,
        )
    }

    /// The paste field of the next script version of `observer`, filled
    /// with `text`, the newest version's.
    pub fn script(observer: &str, next_version: u32, text: &str) -> Self {
        let item = ChooserItem::new(
            format!("save script of {}", one_line(observer)),
            format!(
                "Saves version {next_version}. Saving never approves it: v shows it and approves \
                 it. Set $EDITOR to edit the script in your editor."
            ),
        )
        .with_field(
            ChooserField::multiline(SCRIPT)
                .required()
                .with_value(text)
                .described("the whole script; paste it, Ctrl+J is a new line"),
        );
        Self::open(
            FormKind::Script {
                observer: observer.to_string(),
            },
            item,
        )
    }

    fn open(kind: FormKind, item: ChooserItem) -> Self {
        let mut chooser = Chooser::new(vec![item]).confirming_submit();
        chooser.enter_fields();
        Self { kind, chooser }
    }

    /// Which form it is.
    pub fn kind(&self) -> &FormKind {
        &self.kind
    }

    /// The fields.
    pub fn chooser(&self) -> &Chooser {
        &self.chooser
    }

    /// The fields, to type in them.
    pub fn chooser_mut(&mut self) -> &mut Chooser {
        &mut self.chooser
    }

    /// The title of the form's frame.
    pub fn title(&self) -> String {
        match &self.kind {
            FormKind::Create => " New observer ".to_string(),
            FormKind::Edit { original } => format!(" Edit observer {} ", one_line(&original.name)),
            FormKind::Script { observer } => format!(" Script of {} ", one_line(observer)),
        }
    }

    /// The write a submitted form stands for, or the field that is wrong
    /// and why.
    pub fn write_of(&self, submission: &Submission) -> Result<QueueWrite, FieldError> {
        let value = |name: &str| {
            submission
                .values
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        match &self.kind {
            FormKind::Create => {
                let (interval_secs, cron) = schedule_of(&value(SCHEDULE).unwrap_or_default())?;
                let request = CreateObserverRequest {
                    name: value(NAME).unwrap_or_default(),
                    source: value(SOURCE).unwrap_or_default(),
                    condition: None,
                    queue: value(QUEUE),
                    mode: value(MODE).as_deref().and_then(mode_of),
                    interval_secs,
                    cron,
                    effect: None,
                    secrets: Vec::new(),
                    task_limit_per_run: None,
                    import_backlog: false,
                };
                let script = submission
                    .values
                    .iter()
                    .find(|(field, _)| field == SCRIPT)
                    .map(|(_, script)| script.clone())
                    .filter(|script| !script.trim().is_empty());
                Ok(QueueWrite::Create {
                    request: Box::new(request),
                    script,
                })
            }
            FormKind::Edit { original } => {
                let mut request = UpdateObserverRequest::default();
                if let Some(queue) = value(QUEUE).filter(|queue| *queue != original.queue) {
                    request.queue = Some(queue);
                }
                if let Some(mode) = value(MODE).filter(|mode| *mode != original.mode) {
                    request.mode = mode_of(&mode);
                }
                let schedule = value(SCHEDULE).unwrap_or_default();
                if schedule != schedule_value(original) {
                    let (interval_secs, cron) = schedule_of(&schedule)?;
                    request.interval_secs = interval_secs;
                    request.cron = cron;
                }
                if request.is_empty() {
                    return Err((
                        QUEUE,
                        "nothing changed: change the queue, the mode or the schedule".to_string(),
                    ));
                }
                Ok(QueueWrite::Update {
                    observer: original.name.clone(),
                    request,
                })
            }
            FormKind::Script { observer } => {
                let script = submission
                    .values
                    .iter()
                    .find(|(field, _)| field == SCRIPT)
                    .map(|(_, script)| script.clone())
                    .filter(|script| !script.trim().is_empty())
                    .ok_or((SCRIPT, "the script is empty".to_string()))?;
                Ok(QueueWrite::SaveScript {
                    observer: observer.clone(),
                    script,
                })
            }
        }
    }

    /// Which field of the form a refusal of the server is about: by its
    /// code first, then by what its message names; the first field when it
    /// names none.
    pub fn field_of_refusal(&self, code: &str, message: &str) -> &'static str {
        let fields: &[&'static str] = match self.kind {
            FormKind::Create => &[NAME, SOURCE, QUEUE, MODE, SCHEDULE, SCRIPT],
            FormKind::Edit { .. } => &[QUEUE, MODE, SCHEDULE],
            FormKind::Script { .. } => &[SCRIPT],
        };
        let said = message.to_ascii_lowercase();
        let named = match code {
            "observer_name_taken" => Some(NAME),
            "queue_mode_mismatch" => Some(MODE),
            _ => None,
        }
        .or_else(|| {
            let words: [(&'static str, &[&str]); 6] = [
                (SCHEDULE, &["cron", "interval", "schedule"]),
                (MODE, &["mode", "on_episodes", "always"]),
                (QUEUE, &["queue"]),
                (SOURCE, &["source"]),
                (SCRIPT, &["script"]),
                (NAME, &["name"]),
            ];
            words
                .into_iter()
                .find(|(_, words)| words.iter().any(|word| said.contains(word)))
                .map(|(field, _)| field)
        });
        named
            .filter(|field| fields.contains(field))
            .unwrap_or(fields[0])
    }

    /// Puts the server's refusal next to the field it is about. Returns
    /// whether it is shown.
    pub fn show_refusal(&mut self, code: &str, message: &str) -> bool {
        let field = self.field_of_refusal(code, message);
        self.chooser.set_field_error(0, field, one_line(message))
    }

    /// Puts a reason of the form itself next to `field`.
    pub fn show_error(&mut self, field: &str, message: &str) -> bool {
        self.chooser.set_field_error(0, field, message.to_string())
    }
}

/// The mode select, on `mode` when it is one of the modes.
fn mode_field(mode: Option<&str>) -> ChooserField {
    let mut options: Vec<String> = MODES.iter().map(|mode| mode.to_string()).collect();
    if let Some(index) = mode.and_then(|mode| options.iter().position(|option| option == mode)) {
        options.rotate_left(index);
    }
    ChooserField::select(MODE, options).described(
        "on_episodes: runs for the entries of its queue only; always: runs on every tick, \
         with no entry",
    )
}

fn mode_of(value: &str) -> Option<ObserverMode> {
    match value {
        "on_episodes" => Some(ObserverMode::OnEpisodes),
        "always" => Some(ObserverMode::Always),
        _ => None,
    }
}

/// The schedule typed: seconds, or a cron expression the server checks.
fn schedule_of(text: &str) -> Result<Schedule, FieldError> {
    let text = text.trim();
    if text.is_empty() {
        return Err((SCHEDULE, "a schedule is required".to_string()));
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        let seconds = text
            .parse::<u64>()
            .map_err(|_| (SCHEDULE, "too many seconds".to_string()))?;
        return Ok((Some(seconds), None));
    }
    Ok((None, Some(text.to_string())))
}
