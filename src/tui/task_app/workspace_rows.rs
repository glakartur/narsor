//! The workspace rows of the task header: one row for each workspace, its
//! columns aligned between the rows and cut to the width of the terminal.
//!
//! ```text
//! code: orders-api · ⎇ feat/nar-12-orders → main · +120 −34 · #42 ↗
//! docs: orders-docs · ⎇ main
//! ```
//!
//! A separator ` · ` stands before the branch, the changes and the pull
//! request, only where the row has text for that column. A column that is empty
//! in one row and not in another is kept as blanks, so the columns line up.
//!
//! A narrow terminal takes from the right of the rows what matters least: the
//! base branch goes first, then the changes, then the branch is cut, then the
//! repository. The name of the workspace and the pull request are never cut.

use narsor_protocol::display::WorkspaceHeader;

use super::detail::{cells, fit_cells};

const GAP: &str = " ";
/// Stands before the branch name.
const BRANCH_MARK: &str = "⎇ ";
/// Stands before the branch, the changes and the pull request.
const SEPARATOR: &str = " · ";
/// The smallest a cut branch or repository gets: one character and `…`.
const MIN_CUT_CELLS: usize = 2;

/// What a piece of a row is, for the colour it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Part {
    Name,
    Gap,
    Repo,
    Branch,
    /// `+N`.
    Added,
    /// `−M`.
    Removed,
    /// A pull request with an address to follow.
    PrLink,
    PrText,
    /// ` · ` between the columns.
    Separator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Piece {
    pub text: String,
    pub part: Part,
    /// Where a [`Part::PrLink`] points.
    pub url: Option<String>,
    /// Stands against the piece before it, without the gap between columns.
    pub joined: bool,
}

/// The rows of `workspaces`, each one a list of pieces ending where the text
/// does; the pieces include the gaps between the columns.
pub(super) fn rows(workspaces: &[WorkspaceHeader], width: usize) -> Vec<Vec<Piece>> {
    let entries: Vec<Entry> = workspaces.iter().map(Entry::of).collect();
    let columns = Columns::fitted(&entries, width);
    entries.iter().map(|entry| columns.row(entry)).collect()
}

struct Entry {
    name: String,
    repo: String,
    branch: Option<String>,
    base_branch: Option<String>,
    /// `(+N, −M)`, when the workspace reports either.
    changes: Option<(String, String)>,
    pr: Option<Piece>,
}

impl Entry {
    fn of(workspace: &WorkspaceHeader) -> Self {
        let known = |text: &Option<String>| {
            text.as_deref()
                .map(super::super::scroll::one_line)
                .filter(|text| !text.trim().is_empty())
        };
        let (branch, base_branch) = (known(&workspace.branch), known(&workspace.base_branch));
        let label = workspace.pr_label();
        let pr = match (&label.number, &label.url) {
            (number, Some(url)) => Some(Piece {
                text: format!("{} ↗", number.as_deref().unwrap_or("PR")),
                part: Part::PrLink,
                url: Some(url.clone()),
                joined: false,
            }),
            (Some(number), None) => Some(Piece {
                text: number.clone(),
                part: Part::PrText,
                url: None,
                joined: false,
            }),
            (None, None) => None,
        };
        let changes = match (workspace.added, workspace.removed) {
            (None, None) => None,
            (added, removed) => Some((
                format!("+{}", added.unwrap_or(0)),
                format!("−{}", removed.unwrap_or(0)),
            )),
        };
        Self {
            name: format!("{}:", super::super::scroll::one_line(&workspace.name)),
            repo: super::super::scroll::one_line(&workspace.repo),
            branch,
            base_branch,
            changes,
            pr,
        }
    }

    /// The changes as they are shown: `+N −M`.
    fn changes_text(&self) -> String {
        self.changes
            .as_ref()
            .map_or_else(String::new, |(added, removed)| format!("{added} {removed}"))
    }

    /// `branch → base`, or the one name when the work is not on a branch of
    /// its own yet; without the base when `with_base` is not set.
    fn branches(&self, with_base: bool) -> String {
        match (&self.branch, &self.base_branch) {
            (Some(branch), Some(base)) if with_base && branch != base => {
                format!("{branch} → {base}")
            }
            (Some(branch), _) => branch.clone(),
            (None, Some(base)) => base.clone(),
            (None, None) => String::new(),
        }
    }
}

/// The widths of the columns and which of the optional ones are shown.
struct Columns {
    name: usize,
    repo: usize,
    branches: usize,
    with_base: bool,
    changes: usize,
    pr: usize,
}

impl Columns {
    fn fitted(entries: &[Entry], width: usize) -> Self {
        let widest = |text: &dyn Fn(&Entry) -> String| {
            entries
                .iter()
                .map(|entry| cells(&text(entry)))
                .max()
                .unwrap_or(0)
        };
        let mut columns = Self {
            name: widest(&|entry| entry.name.clone()),
            repo: widest(&|entry| entry.repo.clone()),
            branches: widest(&|entry| entry.branches(true)),
            with_base: true,
            changes: widest(&|entry| entry.changes_text()),
            pr: widest(&|entry| {
                entry
                    .pr
                    .as_ref()
                    .map_or_else(String::new, |pr| pr.text.clone())
            }),
        };
        if columns.overflow(width) > 0 {
            columns.with_base = false;
            columns.branches = widest(&|entry| entry.branches(false));
        }
        if columns.overflow(width) > 0 {
            columns.changes = 0;
        }
        let over = columns.overflow(width);
        columns.branches = shrunk(columns.branches, over);
        let over = columns.overflow(width);
        columns.repo = shrunk(columns.repo, over);
        columns
    }

    fn overflow(&self, width: usize) -> usize {
        let mark = if self.branches > 0 {
            cells(BRANCH_MARK)
        } else {
            0
        };
        let columns = [
            self.name,
            self.repo,
            self.branches + mark,
            self.changes,
            self.pr,
        ];
        let separators: usize = [
            (self.repo, cells(GAP)),
            (self.branches + mark, cells(SEPARATOR)),
            (self.changes, cells(SEPARATOR)),
            (self.pr, cells(SEPARATOR)),
        ]
        .iter()
        .filter(|(column, _)| *column > 0)
        .map(|(_, separator)| separator)
        .sum();
        let total = columns.iter().sum::<usize>() + separators;
        total.saturating_sub(width)
    }

    fn row(&self, entry: &Entry) -> Vec<Piece> {
        let pr = match &entry.pr {
            Some(pr) => vec![pr.clone()],
            None => vec![self.piece("", self.pr, Part::PrText)],
        };
        // Each column after the name: the separator before it, its pieces,
        // its width with the mark, and whether this row has text in it.
        let columns = [
            (GAP, Part::Gap, vec![self.piece(&entry.repo, self.repo, Part::Repo)], self.repo),
            (
                SEPARATOR,
                Part::Separator,
                vec![self.branch_piece(entry)],
                self.branches + cells(BRANCH_MARK) * usize::from(self.branches > 0),
            ),
            (SEPARATOR, Part::Separator, self.change_pieces(entry), self.changes),
            (SEPARATOR, Part::Separator, pr, self.pr),
        ];
        let has_text = |pieces: &[Piece]| pieces.iter().any(|piece| !piece.text.trim().is_empty());
        let last = columns
            .iter()
            .rposition(|(_, _, pieces, width)| *width > 0 && has_text(pieces));
        let mut row = vec![self.piece(&entry.name, self.name, Part::Name)];
        let plain = |text: String, part| Piece {
            text,
            part,
            url: None,
            joined: false,
        };
        for (index, (separator, part, pieces, width)) in columns.into_iter().enumerate() {
            if width == 0 {
                continue;
            }
            if last.is_none_or(|last| index > last) {
                break;
            }
            if has_text(&pieces) {
                row.push(plain(separator.to_string(), part));
                row.extend(pieces);
            } else {
                row.push(plain(" ".repeat(cells(separator) + width), Part::Gap));
            }
        }
        // The text of the row ends where its last column does.
        if let Some(last) = row.last_mut() {
            if last.part == Part::Gap && last.text.trim().is_empty() {
                row.pop();
            }
        }
        row.retain(|piece| !piece.text.is_empty());
        row
    }

    /// The branch column: the mark and the name, or as many blanks.
    fn branch_piece(&self, entry: &Entry) -> Piece {
        if self.branches == 0 {
            return self.piece("", 0, Part::Branch);
        }
        let text = entry.branches(self.with_base);
        let mark = if text.is_empty() {
            " ".repeat(cells(BRANCH_MARK))
        } else {
            BRANCH_MARK.to_string()
        };
        let mut piece = self.piece(&text, self.branches, Part::Branch);
        piece.text = format!("{mark}{}", piece.text);
        piece
    }

    /// `+N`, a space and `−M`, the pair padded to the width of the column;
    /// nothing when the column was dropped.
    fn change_pieces(&self, entry: &Entry) -> Vec<Piece> {
        if self.changes == 0 {
            return Vec::new();
        }
        let Some((added, removed)) = &entry.changes else {
            return vec![self.piece("", self.changes, Part::Added)];
        };
        let padding = " ".repeat(self.changes.saturating_sub(cells(&entry.changes_text())));
        let plain = |text: String, part| Piece {
            text,
            part,
            url: None,
            joined: false,
        };
        vec![
            plain(added.clone(), Part::Added),
            Piece {
                joined: true,
                ..plain(" ".to_string(), Part::Gap)
            },
            Piece {
                joined: true,
                ..plain(format!("{removed}{padding}"), Part::Removed)
            },
        ]
    }

    fn piece(&self, text: &str, width: usize, part: Part) -> Piece {
        let cut = fit_cells(text, width);
        let padding = " ".repeat(width.saturating_sub(cells(&cut)));
        Piece {
            text: format!("{cut}{padding}"),
            part,
            url: None,
            joined: false,
        }
    }
}

fn shrunk(width: usize, over: usize) -> usize {
    if over == 0 {
        return width;
    }
    width.saturating_sub(over).max(MIN_CUT_CELLS.min(width))
}
