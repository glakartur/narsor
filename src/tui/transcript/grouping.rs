use narsor_protocol::chat::{SubagentRef, UNNAMED_SUBAGENT};
use std::collections::HashMap;
use std::sync::Arc;

use super::entry::{
    TranscriptDetailLine, TranscriptEntry, TranscriptEntryId, TranscriptId, TranscriptPresentation,
};
use super::state::TranscriptState;
use crate::tui::form::field::clip;
use crate::tui::scroll::one_line;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ExpansionIdentity {
    conversation: TranscriptId,
    entry: TranscriptEntryId,
}

fn grouped_detail_lines<'a>(
    members: impl IntoIterator<Item = &'a TranscriptEntry>,
    omitted_line: Option<&str>,
) -> Vec<TranscriptDetailLine> {
    members
        .into_iter()
        .flat_map(|member| {
            let time = member
                .header
                .split(" · ")
                .nth(2)
                .and_then(|stamp| stamp.split_whitespace().nth(1))
                .unwrap_or("");
            member.rendered_lines.iter().filter_map(move |line| {
                let line = line.trim();
                let keep = !line.is_empty() && omitted_line != Some(line);
                keep.then(|| {
                    let attribution = if is_error_entry(member) {
                        "BŁĄD · "
                    } else {
                        ""
                    };
                    if time.is_empty() {
                        TranscriptDetailLine::new(
                            format!("{attribution}{line}"),
                            TranscriptPresentation::Technical,
                        )
                    } else {
                        TranscriptDetailLine::new(
                            format!("{time} · {attribution}{line}"),
                            TranscriptPresentation::Technical,
                        )
                    }
                })
            })
        })
        .collect()
}

fn is_error_entry(entry: &TranscriptEntry) -> bool {
    entry.presentation == TranscriptPresentation::Error
        || entry.details.as_ref().is_some_and(|details| {
            details
                .get("error.message")
                .is_some_and(|error| !error.is_null() && error != "")
        })
}

/// The entries of one subagent, in the order they were recorded.
struct SubagentThread {
    subagent: SubagentRef,
    /// Position of the first entry among all entries.
    first: usize,
    members: Vec<TranscriptEntry>,
}

/// One thread per subagent id, in the order of each subagent's first entry.
fn subagent_threads(entries: &[TranscriptEntry]) -> Vec<SubagentThread> {
    let mut threads: Vec<SubagentThread> = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        let Some(subagent) = &entry.subagent else {
            continue;
        };
        match threads
            .iter_mut()
            .find(|thread| thread.subagent.id == subagent.id)
        {
            Some(thread) => thread.members.push(entry.clone()),
            None => threads.push(SubagentThread {
                subagent: subagent.clone(),
                first: position,
                members: vec![entry.clone()],
            }),
        }
    }
    threads
}

/// The name of the subagent at `order` (from 0) as one safe line. A missing
/// name, and a name another subagent also has, get the subagent's number so
/// the groups stay apart.
fn subagent_name(order: usize, threads: &[SubagentThread]) -> String {
    let clean = |thread: &SubagentThread| one_line(&thread.subagent.name).trim().to_owned();
    let name = clean(&threads[order]);
    let unnamed = name.is_empty() || name == UNNAMED_SUBAGENT;
    let repeated = threads
        .iter()
        .filter(|thread| clean(thread) == name)
        .count()
        > 1;
    match (unnamed, repeated) {
        (true, _) => format!("{UNNAMED_SUBAGENT} {}", order + 1),
        (false, true) => format!("{name} {}", order + 1),
        (false, false) => name,
    }
}

/// `name · N wpisy · błędy: F`, on one line at `width`: the name is cut with
/// `…`, the rest never is. No run state is shown: the stream carries no
/// reliable end signal for a subagent (a background one answers its `Task`
/// call at once and keeps running).
fn subagent_header(name: &str, members: usize, failures: usize, width: usize) -> String {
    let mut tail = format!(
        " · {members} {}",
        plural(members, ["wpis", "wpisy", "wpisów"])
    );
    if failures > 0 {
        tail.push_str(&format!(" · błędy: {failures}"));
    }
    let room = width.saturating_sub(2 + tail.chars().count()).max(1);
    format!("{}{tail}", clip(name, room))
}

/// The Polish form of a noun after `count`: one, a few, many.
fn plural(count: usize, forms: [&'static str; 3]) -> &'static str {
    match (count, count % 10, count % 100) {
        (1, _, _) => forms[0],
        (_, 2..=4, tens) if !(12..=14).contains(&tens) => forms[1],
        _ => forms[2],
    }
}

impl TranscriptState {
    /// The main thread folded as before, with each subagent's entries
    /// gathered into one group at the place of the subagent's first entry.
    /// The main thread is folded without the subagent entries, so a tool
    /// call and its result stay one operation card around a subagent's run.
    pub(super) fn fold_subagent_threads(&self) -> Vec<TranscriptEntry> {
        let main: Vec<TranscriptEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.subagent.is_none())
            .cloned()
            .collect();
        let threads = subagent_threads(&self.entries);
        let position: HashMap<&TranscriptEntryId, usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (&entry.id, index))
            .collect();
        let mut projected = Vec::new();
        let mut waiting = threads.iter().enumerate().peekable();
        for entry in self.fold_contiguous_groups(&main) {
            let at = entry
                .source_ids
                .iter()
                .filter_map(|id| position.get(id))
                .min()
                .copied()
                .unwrap_or(0);
            while let Some((order, thread)) = waiting.next_if(|(_, thread)| thread.first < at) {
                projected.extend(self.subagent_block(order, thread, &threads));
            }
            projected.push(entry);
        }
        for (order, thread) in waiting {
            projected.extend(self.subagent_block(order, thread, &threads));
        }
        projected
    }

    /// The group header of one subagent and, while it is open, its entries
    /// folded like the main thread's, so each stays expandable by itself.
    fn subagent_block(
        &self,
        order: usize,
        thread: &SubagentThread,
        threads: &[SubagentThread],
    ) -> Vec<TranscriptEntry> {
        let id = TranscriptEntryId::from(format!("subagent/{}", thread.subagent.id));
        let open = self.expanded.contains(&self.expansion_identity(&id));
        let failures = thread
            .members
            .iter()
            .filter(|member| is_error_entry(member))
            .count();
        let header = subagent_header(
            &subagent_name(order, threads),
            thread.members.len(),
            failures,
            self.width,
        );
        let raw: Arc<[u8]> = thread
            .members
            .iter()
            .enumerate()
            .flat_map(|(index, member)| {
                (index > 0)
                    .then_some(b'\n')
                    .into_iter()
                    .chain(member.raw.iter().copied())
            })
            .collect();
        let mut group = TranscriptEntry::new(id, Vec::new(), raw)
            .with_header(header)
            .with_presentation(TranscriptPresentation::Technical)
            .with_clipped_head();
        group.group_header = true;
        if !open {
            group.source_ids = std::iter::once(group.id.clone())
                .chain(
                    thread
                        .members
                        .iter()
                        .flat_map(|member| member.source_ids.clone()),
                )
                .collect();
            return vec![group];
        }
        let mut block = vec![group];
        block.extend(self.fold_contiguous_groups(&thread.members));
        block
    }

    /// Folds contiguous technical events and correlated tool operations of
    /// `entries` into summary and operation cards.
    pub(super) fn fold_contiguous_groups(
        &self,
        entries: &[TranscriptEntry],
    ) -> Vec<TranscriptEntry> {
        let mut projected = Vec::new();
        let mut cursor = 0;
        while cursor < entries.len() {
            let entry = &entries[cursor];
            let Some(grouping) = entry.grouping.as_ref() else {
                projected.push(entry.clone());
                cursor += 1;
                continue;
            };
            // A runner state card is only safe when the adapter supplied an
            // operation identity. Similar-looking records without one stay
            // individually reachable rather than being guessed into a group.
            if grouping.correlation_id.is_none() {
                projected.push(entry.clone());
                cursor += 1;
                continue;
            }
            if grouping.operation {
                let correlation_id = grouping.correlation_id.as_deref().expect("checked above");
                let same_operation = |candidate: &TranscriptEntry| {
                    candidate
                        .grouping
                        .as_ref()
                        .is_some_and(|candidate_grouping| {
                            candidate_grouping.operation
                                && candidate_grouping.session_id == grouping.session_id
                                && candidate_grouping.correlation_id.as_deref()
                                    == Some(correlation_id)
                        })
                };
                if grouping.phase.as_deref() != Some("started") {
                    let started_before = entries[..cursor].iter().any(|candidate| {
                        same_operation(candidate)
                            && candidate.grouping.as_ref().is_some_and(|metadata| {
                                metadata.phase.as_deref() == Some("started")
                            })
                    });
                    if started_before {
                        let start = entries[..cursor]
                            .iter()
                            .rposition(|candidate| {
                                same_operation(candidate)
                                    && candidate.grouping.as_ref().is_some_and(|metadata| {
                                        metadata.phase.as_deref() == Some("started")
                                    })
                            })
                            .expect("started operation exists");
                        let group_identity = self.expansion_identity(&entries[start].id);
                        if self
                            .revealed_members
                            .get(&group_identity)
                            .is_none_or(|snapshot| snapshot.contains(&entry.id))
                        {
                            cursor += 1;
                            continue;
                        }
                    }
                    projected.push(entry.clone());
                    cursor += 1;
                    continue;
                }
                let mut members = vec![entry];
                if let Some(completed) = entries[cursor + 1..].iter().find(|candidate| {
                    same_operation(candidate)
                        && candidate
                            .grouping
                            .as_ref()
                            .is_some_and(|metadata| metadata.phase.as_deref() == Some("completed"))
                }) {
                    members.push(completed);
                }
                let group_identity = self.expansion_identity(&entry.id);
                let included: Vec<_> = self
                    .revealed_members
                    .get(&group_identity)
                    .map(|snapshot| {
                        members
                            .iter()
                            .copied()
                            .filter(|member| snapshot.contains(&member.id))
                            .collect()
                    })
                    .unwrap_or(members);
                let mut operation = included[0].clone();
                let completed = included.iter().any(|member| {
                    member
                        .grouping
                        .as_ref()
                        .is_some_and(|metadata| metadata.phase.as_deref() == Some("completed"))
                });
                let failed = included.iter().any(|member| is_error_entry(member));
                let mut summary = format!(
                    "Narzędzie · {} · {}",
                    grouping.name,
                    if completed { "zakończone" } else { "w toku" }
                );
                if failed {
                    summary.push_str(" · błąd");
                }
                operation.rendered_lines = vec![summary];
                operation.detail_lines = grouped_detail_lines(included.iter().copied(), None);
                operation.raw = included
                    .iter()
                    .enumerate()
                    .flat_map(|(index, member)| {
                        (index > 0)
                            .then_some(b'\n')
                            .into_iter()
                            .chain(member.raw.iter().copied())
                    })
                    .collect();
                operation.source_ids = included
                    .iter()
                    .flat_map(|member| member.source_ids.clone())
                    .collect();
                operation.grouping = None;
                projected.push(operation);
                cursor += 1;
                continue;
            }
            let mut end = cursor;
            while end < entries.len() {
                let candidate = &entries[end];
                let Some(candidate_grouping) = candidate.grouping.as_ref() else {
                    break;
                };
                if candidate_grouping.session_id != grouping.session_id
                    || candidate_grouping.name != grouping.name
                {
                    break;
                }
                end += 1;
            }
            if end - cursor < 2 {
                projected.push(entry.clone());
                cursor += 1;
                continue;
            }
            let members = &entries[cursor..end];
            let group_identity = self.expansion_identity(&entry.id);
            let included: Vec<_> = self
                .revealed_members
                .get(&group_identity)
                .map(|snapshot| {
                    members
                        .iter()
                        .filter(|member| snapshot.contains(&member.id))
                        .collect()
                })
                .unwrap_or_else(|| members.iter().collect());
            if included.is_empty() {
                cursor = end;
                continue;
            }
            let mut group = (*included[0]).clone();
            let has_error = included.iter().any(|member| is_error_entry(member));
            let mut operations: std::collections::BTreeMap<&str, bool> =
                std::collections::BTreeMap::new();
            for member in &included {
                let Some(metadata) = member.grouping.as_ref() else {
                    continue;
                };
                let Some(id) = metadata.correlation_id.as_deref() else {
                    continue;
                };
                match metadata.phase.as_deref() {
                    Some("started") => {
                        operations.entry(id).or_insert(false);
                    }
                    Some("completed") => {
                        if let Some(done) = operations.get_mut(id) {
                            *done = true;
                        }
                    }
                    _ => {}
                }
            }
            let completed = operations.values().filter(|done| **done).count();
            let in_progress = operations.values().filter(|done| !**done).count();
            let mut summary = format!("{} · {}", grouping.family, grouping.name);
            if !operations.is_empty() {
                if completed > 0 {
                    summary.push_str(&format!(" · {completed} zakończone"));
                }
                if in_progress > 0 {
                    summary.push_str(&format!(" · {in_progress} w toku"));
                }
            } else {
                summary.push_str(&format!(" · {} wpisy", included.len()));
            }
            if has_error {
                summary.push_str(" · błąd");
            }
            group.rendered_lines = vec![summary.clone()];
            group.detail_lines = grouped_detail_lines(included.iter().copied(), Some(&summary));
            group.raw = included
                .iter()
                .enumerate()
                .flat_map(|(index, member)| {
                    (index > 0)
                        .then_some(b'\n')
                        .into_iter()
                        .chain(member.raw.iter().copied())
                })
                .collect();
            group.source_ids = included
                .iter()
                .flat_map(|member| member.source_ids.clone())
                .collect();
            group.grouping = None;
            projected.push(group);
            // A frozen reveal must not absorb later records, but it must not
            // make them disappear either. Keep newly appended members as
            // independent cards until the original group is collapsed.
            if let Some(snapshot) = self.revealed_members.get(&group_identity) {
                projected.extend(
                    members
                        .iter()
                        .filter(|member| !snapshot.contains(&member.id))
                        .cloned(),
                );
            }
            cursor = end;
        }
        projected
    }

    pub(super) fn expansion_identity(&self, entry: &TranscriptEntryId) -> ExpansionIdentity {
        ExpansionIdentity {
            conversation: self.conversation.clone(),
            entry: entry.clone(),
        }
    }

    pub(super) fn remember_revealed_members(
        &mut self,
        entry: &TranscriptEntryId,
        _identity: &ExpansionIdentity,
    ) {
        let Some(group_identity) = self.group_identity_for(entry) else {
            return;
        };
        let members = self.group_members_for(&group_identity.entry);
        if members.len() > 1
            || self
                .entries
                .iter()
                .find(|source| source.id == group_identity.entry)
                .and_then(|source| source.grouping.as_ref())
                .is_some_and(|grouping| grouping.operation)
        {
            self.revealed_members
                .insert(group_identity, members.into_iter().collect());
        }
    }

    pub(super) fn forget_revealed_members_if_hidden(&mut self, identity: &ExpansionIdentity) {
        let Some(group_identity) = self.group_identity_for(&identity.entry) else {
            return;
        };
        let still_revealed = self
            .group_members_for(&group_identity.entry)
            .iter()
            .any(|member| {
                let member_identity = self.expansion_identity(member);
                self.expanded.contains(&member_identity)
                    || self.raw_entries.contains(&member_identity)
            });
        if !still_revealed {
            self.revealed_members.remove(&group_identity);
        }
    }

    /// The stable source which owns a rendered group.  A hook family owns a
    /// contiguous run, while an operation owns the source that started its
    /// correlation id even if its completion is later in the transcript.
    pub(super) fn group_identity_for(
        &self,
        entry: &TranscriptEntryId,
    ) -> Option<ExpansionIdentity> {
        let index = self.entries.iter().position(|source| source.id == *entry)?;
        let grouping = self.entries[index].grouping.as_ref()?;
        grouping.correlation_id.as_ref()?;
        let primary = if grouping.operation {
            self.entries
                .iter()
                .position(|candidate| {
                    candidate.grouping.as_ref().is_some_and(|metadata| {
                        metadata.operation
                            && metadata.session_id == grouping.session_id
                            && metadata.correlation_id == grouping.correlation_id
                            && metadata.phase.as_deref() == Some("started")
                    })
                })
                .unwrap_or(index)
        } else {
            (0..=index)
                .rev()
                .take_while(|&candidate| {
                    self.entries[candidate]
                        .grouping
                        .as_ref()
                        .is_some_and(|metadata| {
                            !metadata.operation
                                && metadata.session_id == grouping.session_id
                                && metadata.name == grouping.name
                        })
                })
                .last()
                .unwrap_or(index)
        };
        Some(self.expansion_identity(&self.entries[primary].id))
    }

    fn group_members_for(&self, primary: &TranscriptEntryId) -> Vec<TranscriptEntryId> {
        let Some(index) = self.entries.iter().position(|source| source.id == *primary) else {
            return Vec::new();
        };
        let Some(grouping) = self.entries[index].grouping.as_ref() else {
            return vec![primary.clone()];
        };
        if grouping.operation {
            return self
                .entries
                .iter()
                .filter(|candidate| {
                    candidate.grouping.as_ref().is_some_and(|metadata| {
                        metadata.operation
                            && metadata.session_id == grouping.session_id
                            && metadata.correlation_id == grouping.correlation_id
                    })
                })
                .map(|candidate| candidate.id.clone())
                .collect();
        }
        self.entries[index..]
            .iter()
            .take_while(|candidate| {
                candidate.grouping.as_ref().is_some_and(|metadata| {
                    !metadata.operation
                        && metadata.session_id == grouping.session_id
                        && metadata.name == grouping.name
                })
            })
            .map(|candidate| candidate.id.clone())
            .collect()
    }
}
