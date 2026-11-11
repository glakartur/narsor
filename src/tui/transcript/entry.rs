use narsor_protocol::chat::SubagentRef;
use serde_json::Value;
use std::sync::Arc;

/// Stable identity of a conversation, normally an attempt or chat id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TranscriptId(String);

impl From<&str> for TranscriptId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for TranscriptId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// Stable identity supplied by a conversation adapter for one entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TranscriptEntryId(String);

impl TranscriptEntryId {
    /// The id as text, for an adapter that has to derive another from it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for TranscriptEntryId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for TranscriptEntryId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// The reading representation selected by the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// The semantic conversation projection.
    Rendered,
    /// Every retained source event, with its observed time.
    Chronology,
    Raw,
}

/// Semantic treatment chosen by a source adapter.  This is presentation data,
/// not terminal control text, so untrusted conversation content never carries
/// its own style instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptPresentation {
    Neutral,
    Operator,
    Agent,
    Conductor,
    System,
    Technical,
    Error,
}

impl TranscriptPresentation {
    /// The role named in an utterance header.
    pub fn role_label(self) -> &'static str {
        match self {
            Self::Operator => "USER",
            Self::Agent => "AGENT",
            Self::Conductor => "KONDUKTOR",
            Self::System => "SYSTEM",
            Self::Neutral | Self::Technical | Self::Error => "TECHNICZNE",
        }
    }
}

/// The header of one utterance: its role, who spoke and the full local date
/// and time.
pub fn utterance_header(
    presentation: TranscriptPresentation,
    speaker: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        "{} · {speaker} · {}",
        presentation.role_label(),
        at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S")
    )
}

/// One expanded transcript line and its semantic presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptDetailLine {
    pub text: String,
    pub presentation: TranscriptPresentation,
}

impl TranscriptDetailLine {
    pub fn new(text: impl Into<String>, presentation: TranscriptPresentation) -> Self {
        Self {
            text: text.into(),
            presentation,
        }
    }
}

/// One source-neutral chronological entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub id: TranscriptEntryId,
    /// A role-bearing header, rendered separately from the neutral body.
    pub header: String,
    /// Semantic lines supplied by the adapter. They are logical, not wrapped
    /// terminal rows.
    pub rendered_lines: Vec<String>,
    /// Role/type treatment shared by every renderer in rendered mode.
    pub presentation: TranscriptPresentation,
    /// Additional lines included only when expanded in rendered mode.
    pub detail_lines: Vec<TranscriptDetailLine>,
    /// Whether the adapter marked this entry as a workflow choice. Renderers
    /// highlight its card; the flag never derives from the entry's text.
    pub workflow_choice: bool,
    /// Whether the header (and the first line of a workflow choice) is one
    /// line by nature: clipped with `…` to the width instead of wrapped.
    pub clip_head: bool,
    /// Normalized adapter fields, formatted only while this entry is expanded.
    pub details: Option<Value>,
    /// Exact retained source bytes. This is never replaced by decoded text.
    /// Shared, not copied, by the entries of one source line and by every
    /// projection of the transcript.
    pub raw: Arc<[u8]>,
    /// Whether [`Self::raw`] is the same source line as the entry before
    /// it: one source line can yield several entries, and the global raw
    /// view shows that line once.
    pub(super) shares_previous_raw: bool,
    /// Source-neutral runner state metadata used to recognize a safe, contiguous
    /// summary card. Adapters decide which harness events qualify.
    pub(super) grouping: Option<TranscriptGrouping>,
    /// The subagent this entry came from; none for the main agent. Entries
    /// of one subagent are shown as one group, whatever stands between them.
    pub subagent: Option<SubagentRef>,
    /// The header of a subagent group: a header line only, with a fold
    /// marker, no body.
    pub(super) group_header: bool,
    /// Source identities represented by this item. A normal card has one;
    /// a Conversation summary card has every source event it collapsed.
    pub(super) source_ids: Vec<TranscriptEntryId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptGrouping {
    pub(super) session_id: String,
    pub(super) family: String,
    pub(super) name: String,
    pub(super) phase: Option<String>,
    pub(super) correlation_id: Option<String>,
    /// Unlike hook families, an operation is one start/result pair. Its
    /// result may arrive after unrelated entries, so it is projected by its
    /// correlation id rather than by contiguous source position.
    pub(super) operation: bool,
}

impl TranscriptEntry {
    pub fn new(
        id: TranscriptEntryId,
        rendered_lines: Vec<String>,
        raw: impl Into<Arc<[u8]>>,
    ) -> Self {
        let source_ids = vec![id.clone()];
        Self {
            id,
            header: String::new(),
            rendered_lines,
            presentation: TranscriptPresentation::Neutral,
            detail_lines: Vec::new(),
            workflow_choice: false,
            clip_head: false,
            details: None,
            raw: raw.into(),
            shares_previous_raw: false,
            grouping: None,
            subagent: None,
            group_header: false,
            source_ids,
        }
    }

    /// Marks this entry as coming from `subagent` (none: the main agent).
    pub fn with_subagent(mut self, subagent: Option<SubagentRef>) -> Self {
        self.subagent = subagent;
        self
    }

    /// Marks [`Self::raw`] as the source line of the entry before this one,
    /// so the global raw view folds this entry into it.
    pub fn sharing_previous_raw(mut self) -> Self {
        self.shares_previous_raw = true;
        self
    }

    pub fn with_header(mut self, header: impl Into<String>) -> Self {
        self.header = header.into();
        self
    }

    pub fn with_detail_lines(self, detail_lines: Vec<String>) -> Self {
        self.with_details(
            detail_lines
                .into_iter()
                .map(|text| TranscriptDetailLine::new(text, TranscriptPresentation::Technical))
                .collect(),
        )
    }

    pub fn with_details(mut self, detail_lines: Vec<TranscriptDetailLine>) -> Self {
        self.detail_lines = detail_lines;
        self
    }

    pub fn with_presentation(mut self, presentation: TranscriptPresentation) -> Self {
        self.presentation = presentation;
        self
    }

    /// Makes the header of this entry, and the first line of a workflow
    /// choice, clip to the width instead of wrapping.
    pub fn with_clipped_head(mut self) -> Self {
        self.clip_head = true;
        self
    }

    /// Marks this entry as a workflow choice, highlighted in rendered views.
    pub fn as_workflow_choice(mut self) -> Self {
        self.workflow_choice = true;
        self
    }

    /// Attaches the complete adapter-normalized operation tuple used by the
    /// Conversation projection.  The tuple deliberately contains no source
    /// JSON: presentation never reparses raw records to infer correlation.
    pub fn with_operation_grouping(
        mut self,
        session_id: Option<&str>,
        name: Option<&str>,
        phase: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Self {
        let Some(session_id) = session_id else {
            return self;
        };
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return self;
        };
        self.grouping = Some(TranscriptGrouping {
            session_id: session_id.to_owned(),
            family: "Hooki".to_owned(),
            name: name.to_owned(),
            phase: phase.map(str::to_owned),
            correlation_id: correlation_id.map(str::to_owned),
            operation: false,
        });
        self
    }

    /// Names the technical family shown by a contiguous summary card.
    pub fn with_group_family(mut self, family: impl Into<String>) -> Self {
        if let Some(grouping) = &mut self.grouping {
            grouping.family = family.into();
        }
        self
    }

    /// Attaches one correlated tool or skill operation.  This is deliberately
    /// distinct from a hook family: multiple operations can interleave and
    /// must still remain independently expandable and diagnosable.
    pub fn with_correlated_operation(
        mut self,
        session_id: Option<&str>,
        name: Option<&str>,
        phase: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Self {
        let Some(session_id) = session_id else {
            return self;
        };
        let Some(name) = name.filter(|name| !name.is_empty()) else {
            return self;
        };
        let Some(correlation_id) = correlation_id.filter(|id| !id.is_empty()) else {
            return self;
        };
        self.grouping = Some(TranscriptGrouping {
            session_id: session_id.to_owned(),
            family: "Narzędzie".to_owned(),
            name: name.to_owned(),
            phase: phase.map(str::to_owned),
            correlation_id: Some(correlation_id.to_owned()),
            operation: true,
        });
        self
    }

    /// Prefixes all stable source identities when composing several attempts.
    pub fn prefix_source_ids(&mut self, prefix: &str) {
        self.id = TranscriptEntryId::from(format!("{prefix}/{}", self.id.as_str()));
        self.source_ids = self
            .source_ids
            .iter()
            .map(|id| TranscriptEntryId::from(format!("{prefix}/{}", id.as_str())))
            .collect();
    }
}
