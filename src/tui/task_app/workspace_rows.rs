//! The workspace rows of the task header: one row for each workspace, its
//! columns aligned between the rows and cut to the width of the terminal.
//!
//! ```text
//! code  orders-api   feat/nar-12-orders → main  +120 −34  PR #42 ↗
//! docs  orders-docs  main                                 —
//! ```
//!
//! A narrow terminal takes from the right of the rows what matters least: the
//! base branch goes first, then the changes, then the branch is cut, then the
//! repository. The name of the workspace and the pull request are never cut.

use narsor_protocol::display::WorkspaceHeader;

use super::detail::{cells, fit_cells};

const GAP: &str = "  ";
const NO_PR: &str = "—";
/// The smallest a cut branch or repository gets: one character and `…`.
const MIN_CUT_CELLS: usize = 2;

/// What a piece of a row is, for the colour it gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Part {
    Name,
    Gap,
    Repo,
    Branch,
    Changes,
    /// A pull request with an address to follow.
    PrLink,
    PrText,
    NoPr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Piece {
    pub text: String,
    pub part: Part,
    /// Where a [`Part::PrLink`] points.
    pub url: Option<String>,
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
    changes: String,
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
        let branch_known = branch.is_some() || base_branch.is_some();
        let pr = match (&label.number, &label.url) {
            (_, Some(url)) => Some(Piece {
                text: format!("{} ↗", label.text()),
                part: Part::PrLink,
                url: Some(url.clone()),
            }),
            (Some(_), None) => Some(Piece {
                text: label.text(),
                part: Part::PrText,
                url: None,
            }),
            (None, None) if branch_known => Some(Piece {
                text: NO_PR.to_string(),
                part: Part::NoPr,
                url: None,
            }),
            (None, None) => None,
        };
        let changes = match (workspace.added, workspace.removed) {
            (None, None) => String::new(),
            (added, removed) => format!("+{} −{}", added.unwrap_or(0), removed.unwrap_or(0)),
        };
        Self {
            name: super::super::scroll::one_line(&workspace.name),
            repo: super::super::scroll::one_line(&workspace.repo),
            branch,
            base_branch,
            changes,
            pr,
        }
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
            changes: widest(&|entry| entry.changes.clone()),
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
        let columns = [self.name, self.repo, self.branches, self.changes, self.pr];
        let shown = columns.iter().filter(|column| **column > 0).count();
        let total = columns.iter().sum::<usize>() + cells(GAP) * shown.saturating_sub(1);
        total.saturating_sub(width)
    }

    fn row(&self, entry: &Entry) -> Vec<Piece> {
        let mut pieces = vec![
            self.piece(&entry.name, self.name, Part::Name),
            self.piece(&entry.repo, self.repo, Part::Repo),
            self.piece(&entry.branches(self.with_base), self.branches, Part::Branch),
            self.piece(&entry.changes, self.changes, Part::Changes),
        ];
        pieces.push(match &entry.pr {
            Some(pr) => pr.clone(),
            None => self.piece("", self.pr, Part::PrText),
        });
        let mut row = Vec::new();
        for piece in pieces.into_iter().filter(|piece| !piece.text.is_empty()) {
            if !row.is_empty() {
                row.push(Piece {
                    text: GAP.to_string(),
                    part: Part::Gap,
                    url: None,
                });
            }
            row.push(piece);
        }
        row
    }

    fn piece(&self, text: &str, width: usize, part: Part) -> Piece {
        let cut = fit_cells(text, width);
        let padding = " ".repeat(width.saturating_sub(cells(&cut)));
        Piece {
            text: format!("{cut}{padding}"),
            part,
            url: None,
        }
    }
}

fn shrunk(width: usize, over: usize) -> usize {
    if over == 0 {
        return width;
    }
    width.saturating_sub(over).max(MIN_CUT_CELLS.min(width))
}
