//! The repositories (U13, origin §3): `R` shows what the server has
//! registered, in the layout of a list and a description — the same chooser as
//! the answer panel and the action menu.
//!
//! ```text
//! │ Zadania            │ Repozytoria (2) · a dodaj · d usuń                  │
//! │ ● NAR-14  edit     │╭ Akcje ───────────╮ ╭ Opis ───────────────────────╮│
//! │                    ││ │ 1 narsor       │ │ narsor                      ││
//! │                    ││   2 docs         │ │ github · gałąź main         ││
//! │                    │╰──────────────────╯ │ git@github.com:acme/narsor  ││
//! ```
//!
//! * `a` opens a form (a chooser with one entry and its fields). The server
//!   validates what is sent exactly as `narsor repo add` does, and what it
//!   refuses (`invalid_repository`) is shown next to the field it is about;
//!   on a server that is not the local daemon only remote git URLs are
//!   accepted, and the field says so.
//! * `d` asks `y` / `n` in the window every action `!` uses, then removes.
//! * What the server registered is passed through [`one_line`] / the chooser's
//!   sanitising before it is drawn.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::api::ClientError;
use crate::controller_protocol::{CreateRepositoryRequest, RepositoryListResponse, RepositoryView};

use super::super::chooser::{self, Chooser, ChooserAction, ChooserField, ChooserItem, Submission};
use super::super::keymap::{self, Action, Context};
use super::super::scroll::one_line;
use super::super::style::{Role, Styles};
use super::actions::{chooser_context, Confirmation, Confirmed, Overlay};
use super::background::{JobKey, OpeningKind, Outcome};
use super::notify::Severity;
use super::{failure_reason, Focus, Request, TaskApp, View};

/// The key the unsent data of the form is kept under in
/// [`DetailState::set_draft`](super::DetailState::set_draft); not a task id.
pub const DRAFT_KEY: &str = "·repozytorium";

/// The label of the one entry of the form.
const ADD_LABEL: &str = "dodaj repozytorium";

const ID: &str = "id";
const REMOTE: &str = "remote";
const NAME: &str = "nazwa";
const PROVIDER: &str = "dostawca";
const BRANCH: &str = "gałąź";
const DESCRIPTION: &str = "opis";

/// What is registered, and the form that adds to it.
#[derive(Debug)]
pub struct Repositories {
    chooser: Chooser,
    repositories: Vec<RepositoryView>,
    unreadable: Vec<String>,
    adding: Option<Chooser>,
    /// Whether the server accepts a path on its own disk, not only a URL.
    local_paths: bool,
}

impl Repositories {
    /// The repositories of `listing`.
    pub fn new(listing: RepositoryListResponse, local_paths: bool) -> Self {
        let mut repositories = Self {
            chooser: Chooser::new(Vec::new()),
            repositories: Vec::new(),
            unreadable: Vec::new(),
            adding: None,
            local_paths,
        };
        repositories.set_listing(listing, None);
        repositories
    }

    /// Takes a new listing in. The highlighted repository stays, or `select`
    /// is the one to highlight.
    fn set_listing(&mut self, listing: RepositoryListResponse, select: Option<&str>) {
        let kept = select.map(str::to_string).or_else(|| {
            self.selected_repository()
                .map(|repository| repository.id.clone())
        });
        self.repositories = listing.repositories;
        self.unreadable = listing.unreadable;
        let index = kept
            .and_then(|id| {
                self.repositories
                    .iter()
                    .position(|repository| repository.id == id)
            })
            .unwrap_or(0);
        self.chooser =
            Chooser::new(self.repositories.iter().map(item_of).collect()).selecting(index);
    }

    /// The registered repositories, ordered by id.
    pub fn repositories(&self) -> &[RepositoryView] {
        &self.repositories
    }

    /// The highlighted repository.
    pub fn selected_repository(&self) -> Option<&RepositoryView> {
        self.repositories.get(self.chooser.selected())
    }

    /// The list.
    pub fn chooser(&self) -> &Chooser {
        &self.chooser
    }

    /// The form that adds one, while it is open.
    pub fn form(&self) -> Option<&Chooser> {
        self.adding.as_ref()
    }

    /// Registry files the server could not read.
    pub fn unreadable(&self) -> &[String] {
        &self.unreadable
    }

    /// What was typed in the form and not sent.
    pub fn draft_text(&self) -> String {
        self.adding
            .as_ref()
            .map(Chooser::draft_text)
            .unwrap_or_default()
    }

    fn open_form(&mut self) {
        self.adding = Some(add_form(self.local_paths));
    }
}

/// How a repository reads: its id in the list, the rest in the description.
fn item_of(repository: &RepositoryView) -> ChooserItem {
    let mut lines = Vec::new();
    if repository.name != repository.id && !repository.name.trim().is_empty() {
        lines.push(repository.name.trim().to_string());
    }
    let mut facts = vec![repository.provider.clone()];
    if let Some(branch) = repository
        .default_branch
        .as_deref()
        .filter(|branch| !branch.trim().is_empty())
    {
        facts.push(format!("gałąź {branch}"));
    }
    lines.push(facts.join(" · "));
    lines.push(format!("remote: {}", repository.remote));
    if let Some(description) = repository
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        lines.push(String::new());
        lines.push(description.trim().to_string());
    }
    ChooserItem::new(repository.id.clone(), lines.join("\n"))
}

/// The form: an id and a remote are required, the rest the server fills in.
fn add_form(local_paths: bool) -> Chooser {
    let remote = if local_paths {
        "URL, który przyjmie `git clone`, albo ścieżka na dysku serwera"
    } else {
        "tylko zdalny URL git: https://…, ssh://… albo user@host:ścieżka"
    };
    Chooser::new(vec![ChooserItem::new(
        ADD_LABEL,
        "Rejestruje repozytorium, do którego workflow mogą się odwoływać po id. Serwer sprawdza \
         je tak jak `narsor repo add`.",
    )
    .with_field(
        ChooserField::text(ID)
            .required()
            .described("nazwa, którą workflow wskazuje repozytorium"),
    )
    .with_field(ChooserField::text(REMOTE).required().described(remote))
    .with_field(ChooserField::text(NAME).described("nazwa wyświetlana; domyślnie id"))
    .with_field(ChooserField::text(PROVIDER).described("github, gitlab, …; domyślnie z adresu"))
    .with_field(
        ChooserField::text(BRANCH).described(
            "gałąź, gdy workflow nie podaje `ref`; jawny `ref` workflow ma pierwszeństwo",
        ),
    )
    .with_field(
        ChooserField::text(DESCRIPTION).described("dowolna notatka"),
    )])
}

/// The request a submitted form stands for.
fn request_of(submission: &Submission) -> Option<CreateRepositoryRequest> {
    let value = |name: &str| {
        submission
            .values
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    Some(CreateRepositoryRequest {
        id: value(ID)?,
        remote: value(REMOTE)?,
        name: value(NAME),
        provider: value(PROVIDER),
        default_branch: value(BRANCH),
        description: value(DESCRIPTION),
    })
}

/// Which field of the form a refusal of the server is about: the id when the
/// server says the id is no good or taken, otherwise the remote.
fn field_of_refusal(message: &str) -> &'static str {
    let said = message.to_ascii_lowercase();
    let about_id = said.starts_with("the id")
        || said.starts_with("repository id")
        || said.contains("usable file name")
        || said.contains("already registered");
    if about_id {
        ID
    } else {
        REMOTE
    }
}

// ---------------------------------------------------------------------
// The application's part
// ---------------------------------------------------------------------

impl TaskApp {
    /// The repositories, while they are open.
    pub fn repositories(&self) -> Option<&Repositories> {
        match &self.view {
            View::Repositories(repositories) => Some(repositories),
            _ => None,
        }
    }

    /// The read `R` started is back: opens the repositories over it.
    pub(super) fn on_repositories_read(
        &mut self,
        listing: Result<RepositoryListResponse, ClientError>,
    ) {
        match listing {
            Ok(listing) => self.open_repositories(listing),
            Err(error) => {
                self.note_transport(&error);
                self.notify(
                    Severity::Error,
                    format!(
                        "repozytoria: nie udało się wczytać listy ({})",
                        failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Opens the repositories over what was read.
    pub fn open_repositories(&mut self, listing: RepositoryListResponse) {
        let local = self
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.local_repository_paths);
        if let View::Repositories(repositories) = &mut self.view {
            repositories.set_listing(listing, None);
            return;
        }
        self.enter_view(
            View::Repositories(Box::new(Repositories::new(listing, local))),
            Focus::Details,
        );
    }

    /// The context whose keys apply while the repositories are open.
    pub(super) fn repositories_context(&self) -> Option<Context> {
        match &self.view {
            View::Repositories(repositories) => Some(match &repositories.adding {
                Some(form) => chooser_context(form),
                None => Context::Repositories,
            }),
            _ => None,
        }
    }

    /// A key while the repositories are open.
    pub(super) fn on_repositories_key(&mut self, key: KeyEvent) -> Request {
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.focus = Focus::List;
            return Request::None;
        }
        let View::Repositories(repositories) = &mut self.view else {
            return Request::None;
        };
        let request = match repositories.adding.as_mut() {
            Some(form) => {
                if !form.is_typing() {
                    match keymap::lookup(Context::Panel, &key) {
                        Some(Action::Help) => {
                            self.help_open = true;
                            return Request::None;
                        }
                        Some(Action::Quit) => {
                            self.on_quit();
                            return Request::None;
                        }
                        _ => {}
                    }
                }
                match form.on_key(key) {
                    ChooserAction::None => Request::None,
                    ChooserAction::Cancel => {
                        // What was typed is not kept: the form is cheap to fill.
                        repositories.adding = None;
                        Request::None
                    }
                    ChooserAction::Blocked(reason) => {
                        self.notify(Severity::Warning, one_line(&reason));
                        Request::None
                    }
                    ChooserAction::Submit(submission) => self.submit_repository(&submission),
                }
            }
            None => self.on_repositories_list_key(key),
        };
        self.sync_repository_draft();
        request
    }

    /// A paste while the view has the keys: into the field of the form being
    /// filled, if there is one; the list takes none.
    pub(super) fn on_repositories_paste(&mut self, pasted: &str) {
        let View::Repositories(repositories) = &mut self.view else {
            return;
        };
        if repositories
            .adding
            .as_mut()
            .is_some_and(|form| form.paste(pasted))
        {
            self.sync_repository_draft();
        }
    }

    fn on_repositories_list_key(&mut self, key: KeyEvent) -> Request {
        let Some(action) = keymap::lookup(Context::Repositories, &key) else {
            return Request::None;
        };
        let View::Repositories(repositories) = &mut self.view else {
            return Request::None;
        };
        match action {
            Action::Help => self.help_open = true,
            Action::Quit => self.on_quit(),
            Action::Back => self.close_view(),
            Action::AddRepository => repositories.open_form(),
            Action::RemoveRepository => self.ask_remove_repository(),
            Action::ChooseHarness => return Request::OpenHarness,
            Action::Down | Action::Up | Action::Pick | Action::PageDown | Action::PageUp => {
                // The chooser reads the same keys of the panel's context.
                repositories.chooser.on_key(key);
            }
            _ => {}
        }
        Request::None
    }

    fn sync_repository_draft(&mut self) {
        let text = self
            .repositories()
            .map(Repositories::draft_text)
            .unwrap_or_default();
        self.detail.set_draft(DRAFT_KEY, text);
    }

    /// `d`: asks before removing the highlighted repository.
    fn ask_remove_repository(&mut self) {
        let Some(repository) = self
            .repositories()
            .and_then(Repositories::selected_repository)
            .cloned()
        else {
            self.notify(Severity::Warning, "usuń: nie ma zaznaczonego repozytorium");
            return;
        };
        let id = repository.id.clone();
        self.overlay = Some(Overlay::Confirm(Confirmation::new(
            id.clone(),
            "Usuń repozytorium !",
            vec![
                format!("Usuwa repozytorium {} z rejestru serwera.", one_line(&id)),
                format!("remote: {}", one_line(&repository.remote)),
                "Workflow, które się do niego odwołują, przestaną je znajdować.".to_string(),
            ],
            Confirmed::RemoveRepository(id),
        )));
    }

    /// `Enter` in the form: the request, once the connection allows it.
    fn submit_repository(&mut self, submission: &Submission) -> Request {
        let Some(request) = request_of(submission) else {
            return Request::None;
        };
        if !self.mutation_allowed() {
            return Request::None;
        }
        Request::AddRepository(Box::new(request))
    }

    /// Registers the repository. The server's verdict on what was typed stays
    /// next to the field it is about, and the form stays open.
    pub(super) async fn add_repository(&mut self, request: &CreateRepositoryRequest) {
        match self.client.control_add_repository(request).await {
            Ok(added) => {
                if let View::Repositories(repositories) = &mut self.view {
                    repositories.adding = None;
                }
                self.reload_repositories(Some(added.id.clone()));
                self.sync_repository_draft();
                self.notify(
                    Severity::Success,
                    format!("dodano repozytorium {}", one_line(&added.id)),
                );
            }
            Err(error) => {
                self.note_transport(&error);
                let message = match &error {
                    ClientError::Request { message, .. } | ClientError::Refused { message, .. } => {
                        Some(one_line(message))
                    }
                    _ => None,
                };
                let shown = match (&mut self.view, &message) {
                    (View::Repositories(repositories), Some(message)) => {
                        repositories.adding.as_mut().is_some_and(|form| {
                            form.set_field_error(0, field_of_refusal(message), message.clone())
                        })
                    }
                    _ => false,
                };
                if !shown {
                    self.notify(
                        Severity::Error,
                        format!("dodanie repozytorium: {}", failure_reason(&error)),
                    );
                }
            }
        }
    }

    /// Removes the repository `id` from the registry.
    pub(super) async fn remove_repository(&mut self, id: &str) {
        match self.client.control_remove_repository(id).await {
            Ok(removed) => {
                self.reload_repositories(None);
                self.notify(
                    Severity::Success,
                    format!("usunięto repozytorium {}", one_line(&removed.id)),
                );
            }
            Err(ClientError::NotFound) => {
                self.reload_repositories(None);
                self.notify(
                    Severity::Warning,
                    format!("repozytorium {} już nie istnieje", one_line(id)),
                );
            }
            Err(error) => {
                self.note_transport(&error);
                self.notify(
                    Severity::Error,
                    format!(
                        "usunięcie repozytorium {}: {}",
                        one_line(id),
                        failure_reason(&error)
                    ),
                );
            }
        }
    }

    /// Starts the read that brings the open list up to date after a write;
    /// the row `select` is the one to stand on when it is back.
    pub(super) fn reload_repositories(&mut self, select: Option<String>) {
        let client = self.client.clone();
        self.background
            .start(JobKey::Reload(OpeningKind::Repositories), async move {
                let listing = client.control_repositories().await;
                Outcome::new(move |app: &mut TaskApp| {
                    let Ok(listing) = listing else {
                        return;
                    };
                    if let View::Repositories(repositories) = &mut app.view {
                        repositories.set_listing(listing, select.as_deref());
                    }
                })
            });
    }
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// Draws the repositories into `area`: a frame titled with how many there
/// are, lit while `focused`, and in it the list and the description, or the
/// form that adds one.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    repositories: &mut Repositories,
    styles: &Styles,
    focused: bool,
) {
    let mut title = format!(" Repozytoria ({})", repositories.repositories.len());
    if repositories.adding.is_some() {
        title.push_str(" · nowe repozytorium");
    } else if repositories.repositories.is_empty() {
        title.push_str(" · brak zarejestrowanych — a dodaje");
    }
    title.push(' ');
    let inner = styles.render_panel(frame, area, title, focused);

    let failures: Vec<Line> = repositories
        .unreadable
        .iter()
        .take(2)
        .map(|failure| {
            Line::styled(
                format!("✗ nie wczytano: {}", one_line(failure)),
                styles.of(Role::Problem),
            )
        })
        .collect();
    let header_rows = u16::try_from(failures.len()).unwrap_or(0);
    let [header, body] = Layout::vertical([
        Constraint::Length(header_rows.min(inner.height)),
        Constraint::Min(3),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(failures), header);
    match repositories.adding.as_mut() {
        Some(form) => chooser::render(frame, body, form, styles, focused),
        None => chooser::render(frame, body, &mut repositories.chooser, styles, focused),
    }
}
