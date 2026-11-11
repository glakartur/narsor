//! The task's ticket and title, as the templates of its steps render them.

use super::*;

/// The ticket and title `{{ task.ticket }}` and `{{ task.title }}` render.
///
/// The operator's values come from the sealed launch spec. The title the
/// conductor generates arrives later, so it sits behind a lock; the
/// operator's title always wins.
#[derive(Default)]
pub(crate) struct TaskLabels {
    pub(crate) ticket: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) generated_title: Mutex<Option<String>>,
}

impl TaskLabels {
    pub(crate) fn effective_title(&self) -> Option<String> {
        let generated = self
            .generated_title
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        effective_title(self.title.as_deref(), generated.as_deref())
    }
}
