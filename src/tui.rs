#[cfg(test)]
use crate::config::Config;
use crate::mutation;
use crate::store::{Revision, Store};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SLOGAN_STEP: Duration = Duration::from_secs(2);
const STARTUP_REVEAL: Duration = Duration::from_secs(6);
const WAVE_DURATION: Duration = Duration::from_millis(1_800);
const WAVE_FRAME: Duration = Duration::from_millis(60);
const WAVE_INTERVAL: Duration = Duration::from_secs(60);
const RAINBOW_BAND_WIDTH: usize = 14;
const PREFERRED_SLOGAN_GAP: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainTab {
    Timeline,
    ReposFiles,
    Search,
}

impl MainTab {
    fn next(self) -> Self {
        match self {
            Self::Timeline => Self::ReposFiles,
            Self::ReposFiles => Self::Search,
            Self::Search => Self::Timeline,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Timeline => Self::Search,
            Self::ReposFiles => Self::Timeline,
            Self::Search => Self::ReposFiles,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Create,
    Edit,
    Access,
}

impl EventKind {
    fn label(self) -> &'static str {
        match self {
            Self::Create => "CREATE",
            Self::Edit => "EDIT",
            Self::Access => "ACCESS",
        }
    }

    fn is_change(self) -> bool {
        self != Self::Access
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    pub revision: Revision,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedTarget {
    None,
    Timeline { path: PathBuf, revision: u64 },
    ReposFiles { path: PathBuf, revision: u64 },
    Search { path: PathBuf, revision: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffContext {
    pub target: SelectedTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedHistoryItem {
    pub revision: Revision,
    pub kind: EventKind,
}

impl Default for DiffContext {
    fn default() -> Self {
        Self {
            target: SelectedTarget::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeaderSnapshot {
    slogan: &'static str,
    wave_active: bool,
    wave_elapsed: Option<Duration>,
}

#[derive(Debug, Default, Clone, Copy)]
struct HeaderAnimationState;

impl HeaderAnimationState {
    fn snapshot(self, elapsed: Duration) -> HeaderSnapshot {
        let slogan = if elapsed < SLOGAN_STEP {
            ""
        } else if elapsed < SLOGAN_STEP * 2 {
            "BACKUPS"
        } else if elapsed < STARTUP_REVEAL {
            "BACKUPS SAVE"
        } else {
            "BACKUPS SAVE LIVES"
        };
        let wave_elapsed = if elapsed >= STARTUP_REVEAL && elapsed < STARTUP_REVEAL + WAVE_DURATION
        {
            Some(elapsed - STARTUP_REVEAL)
        } else if elapsed >= STARTUP_REVEAL + WAVE_DURATION + WAVE_INTERVAL {
            let since_recurring = elapsed - (STARTUP_REVEAL + WAVE_DURATION + WAVE_INTERVAL);
            let cycle = WAVE_INTERVAL + WAVE_DURATION;
            let within =
                Duration::from_millis((since_recurring.as_millis() % cycle.as_millis()) as u64);
            (within < WAVE_DURATION).then_some(within)
        } else {
            None
        };
        HeaderSnapshot {
            slogan,
            wave_active: wave_elapsed.is_some(),
            wave_elapsed,
        }
    }

    fn next_delay(self, elapsed: Duration) -> Duration {
        let snapshot = self.snapshot(elapsed);
        if snapshot.wave_active {
            return WAVE_FRAME;
        }
        if elapsed < SLOGAN_STEP {
            return SLOGAN_STEP - elapsed;
        }
        if elapsed < SLOGAN_STEP * 2 {
            return SLOGAN_STEP * 2 - elapsed;
        }
        if elapsed < STARTUP_REVEAL {
            return STARTUP_REVEAL - elapsed;
        }
        if elapsed < STARTUP_REVEAL + WAVE_DURATION + WAVE_INTERVAL {
            return STARTUP_REVEAL + WAVE_DURATION + WAVE_INTERVAL - elapsed;
        }
        let since_recurring = elapsed - (STARTUP_REVEAL + WAVE_DURATION + WAVE_INTERVAL);
        let cycle = WAVE_INTERVAL + WAVE_DURATION;
        let within =
            Duration::from_millis((since_recurring.as_millis() % cycle.as_millis()) as u64);
        if within < WAVE_DURATION {
            WAVE_FRAME
        } else {
            cycle - within
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Files,
    History,
    Diff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Main,
    Help,
    SearchInput,
    SearchResults,
    Inspect,
    TagInput,
    ConfirmRestore,
    Activity,
    Filters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterField {
    Path,
    Tag,
    Actor,
    Editor,
    Date,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilters {
    pub path: String,
    pub tag: String,
    pub actor: String,
    pub editor: String,
    pub date: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub path: PathBuf,
    pub revision: u64,
    pub matches: usize,
    pub epoch: u64,
    pub editor: String,
    pub actor: String,
    pub tag: Option<String>,
    pub preview: String,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub path: PathBuf,
    pub label: String,
    pub directory: bool,
    pub depth: usize,
}

pub struct App {
    pub main_tab: MainTab,
    pub files: Vec<PathBuf>,
    pub revisions: Vec<Revision>,
    pub file_index: usize,
    pub tree: Vec<TreeNode>,
    pub tree_index: usize,
    pub expanded: BTreeSet<PathBuf>,
    pub revision_index: usize,
    pub panel: Panel,
    pub mode: Mode,
    pub marked_revision: Option<u64>,
    pub compare_current: bool,
    pub diff: String,
    pub diff_scroll: u16,
    pub inspect: String,
    pub inspect_scroll: u16,
    pub input: String,
    pub search_results: Vec<SearchResult>,
    pub search_index: usize,
    pub activity: Vec<Revision>,
    pub activity_index: usize,
    pub status: String,
    pub quit: bool,
    pub restore_rendered: bool,
    pub filters: SearchFilters,
    pub filter_field: FilterField,
    pub back_mode: Mode,
    pub filter_origin: Mode,
    pub tree_offset: usize,
    pub history_offset: usize,
    pub search_offset: usize,
    pub activity_offset: usize,
    pub timeline: Vec<TimelineEvent>,
    pub timeline_index: usize,
    pub timeline_changes_only: bool,
    pub diff_context: DiffContext,
    pub selected_item: Option<SelectedHistoryItem>,
    header_started: Instant,
}

impl App {
    pub fn load(store: &Store) -> io::Result<Self> {
        let all = store.revisions()?;
        let mut files: Vec<_> = all.iter().map(|r| r.access.path.clone()).collect();
        files.sort();
        files.dedup();
        let expanded = files
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect();
        let mut app = Self {
            main_tab: MainTab::Timeline,
            files,
            revisions: Vec::new(),
            file_index: 0,
            tree: Vec::new(),
            tree_index: 0,
            expanded,
            revision_index: 0,
            panel: Panel::Files,
            mode: Mode::Main,
            marked_revision: None,
            compare_current: true,
            diff: String::new(),
            diff_scroll: 0,
            inspect: String::new(),
            inspect_scroll: 0,
            input: String::new(),
            search_results: Vec::new(),
            search_index: 0,
            activity: Vec::new(),
            activity_index: 0,
            status: "Ready".into(),
            quit: false,
            restore_rendered: true,
            filters: SearchFilters::default(),
            filter_field: FilterField::Path,
            back_mode: Mode::Main,
            filter_origin: Mode::Main,
            tree_offset: 0,
            history_offset: 0,
            search_offset: 0,
            activity_offset: 0,
            timeline: Vec::new(),
            timeline_index: 0,
            timeline_changes_only: false,
            diff_context: DiffContext::default(),
            selected_item: None,
            header_started: Instant::now(),
        };
        app.refresh(store)?;
        Ok(app)
    }

    pub fn selected_file(&self) -> Option<&Path> {
        self.files.get(self.file_index).map(PathBuf::as_path)
    }
    pub fn selected_revision(&self) -> Option<&Revision> {
        self.revisions.get(self.revision_index)
    }
    pub fn selected_tree(&self) -> Option<&TreeNode> {
        self.tree.get(self.tree_index)
    }

    fn selected_context_revision(&self) -> Option<&Revision> {
        self.selected_item.as_ref().map(|item| &item.revision)
    }

    pub fn visible_timeline(&self) -> Vec<&TimelineEvent> {
        self.timeline
            .iter()
            .filter(|event| !self.timeline_changes_only || event.kind.is_change())
            .collect()
    }

    pub fn next_tab(&mut self, store: &Store) -> io::Result<()> {
        self.main_tab = self.main_tab.next();
        self.activate_tab(store)
    }

    pub fn previous_tab(&mut self, store: &Store) -> io::Result<()> {
        self.main_tab = self.main_tab.previous();
        self.activate_tab(store)
    }

    fn activate_tab(&mut self, store: &Store) -> io::Result<()> {
        self.mode = if self.main_tab == MainTab::Search {
            Mode::SearchInput
        } else {
            Mode::Main
        };
        match self.main_tab {
            MainTab::Timeline => self.select_timeline(store),
            MainTab::ReposFiles => self.update_diff(store),
            MainTab::Search => self.select_search_result(store),
        }
    }

    pub fn select_timeline(&mut self, store: &Store) -> io::Result<()> {
        let selected = self
            .visible_timeline()
            .get(self.timeline_index)
            .map(|event| (event.revision.clone(), event.kind));
        let Some((revision, kind)) = selected else {
            self.diff = "No event selected".into();
            self.diff_context = DiffContext::default();
            return Ok(());
        };
        self.diff_context.target = SelectedTarget::Timeline {
            path: revision.access.path.clone(),
            revision: revision.number,
        };
        self.selected_item = Some(SelectedHistoryItem {
            revision: revision.clone(),
            kind,
        });
        self.load_event_diff(store, &revision, kind)
    }

    pub fn select_search_result(&mut self, store: &Store) -> io::Result<()> {
        let Some(hit) = self.search_results.get(self.search_index).cloned() else {
            self.diff = if self.input.is_empty() {
                "Enter a search query".into()
            } else {
                "No search results".into()
            };
            self.diff_context = DiffContext::default();
            return Ok(());
        };
        let revision = store
            .revisions()?
            .into_iter()
            .find(|revision| revision.number == hit.revision && revision.access.path == hit.path);
        let Some(revision) = revision else {
            self.diff = "Selected search result is unavailable".into();
            return Ok(());
        };
        let kind = if revision.diff.is_none() {
            EventKind::Access
        } else if store.backup(&revision)?.is_empty() {
            EventKind::Create
        } else {
            EventKind::Edit
        };
        self.diff_context.target = SelectedTarget::Search {
            path: revision.access.path.clone(),
            revision: revision.number,
        };
        self.selected_item = Some(SelectedHistoryItem {
            revision: revision.clone(),
            kind,
        });
        self.load_event_diff(store, &revision, kind)
    }

    fn load_event_diff(
        &mut self,
        store: &Store,
        revision: &Revision,
        kind: EventKind,
    ) -> io::Result<()> {
        self.diff_scroll = 0;
        if kind == EventKind::Access {
            self.diff = "No content change — file accessed only".into();
            return Ok(());
        }
        self.diff = unified_diff(
            &store.backup(revision)?,
            &store.render(revision)?,
            &format!("before rev {}", revision.number),
            format!("rev {}", revision.number),
        )?;
        Ok(())
    }

    pub fn refresh(&mut self, store: &Store) -> io::Result<()> {
        let selected_path = self.selected_file().map(Path::to_path_buf);
        let selected_number = self.selected_revision().map(|r| r.number);
        let all = store.revisions()?;
        self.files = all.iter().map(|r| r.access.path.clone()).collect();
        self.files.sort();
        self.files.dedup();
        if let Some(path) = selected_path {
            self.file_index = self.files.iter().position(|p| *p == path).unwrap_or(0);
        }
        self.activity = all.clone();
        self.activity.sort_by(|a, b| {
            b.access
                .epoch
                .cmp(&a.access.epoch)
                .then(b.number.cmp(&a.number))
        });
        self.timeline = all
            .iter()
            .cloned()
            .map(|revision| {
                let kind = if revision.diff.is_none() {
                    EventKind::Access
                } else if store.backup(&revision)?.is_empty() {
                    EventKind::Create
                } else {
                    EventKind::Edit
                };
                Ok(TimelineEvent { revision, kind })
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.timeline.sort_by(|a, b| {
            b.revision
                .access
                .epoch
                .cmp(&a.revision.access.epoch)
                .then(b.revision.number.cmp(&a.revision.number))
        });
        self.timeline_index = self
            .timeline_index
            .min(self.visible_timeline().len().saturating_sub(1));
        self.rebuild_tree();
        if let Some(path) = self.files.get(self.file_index) {
            self.tree_index = self
                .tree
                .iter()
                .position(|node| !node.directory && node.path == *path)
                .unwrap_or(0);
            keep_visible(self.tree_index, &mut self.tree_offset, 8);
        }
        self.revisions = self
            .selected_file()
            .map(|path| {
                all.iter()
                    .filter(|r| r.access.path == path)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.revisions
            .sort_by_key(|revision| std::cmp::Reverse(revision.number));
        if let Some(number) = selected_number {
            self.revision_index = self
                .revisions
                .iter()
                .position(|r| r.number == number)
                .unwrap_or(0);
        }
        keep_visible(self.revision_index, &mut self.history_offset, 8);
        match self.main_tab {
            MainTab::Timeline => self.select_timeline(store),
            MainTab::ReposFiles => self.update_diff(store),
            MainTab::Search => self.select_search_result(store),
        }
    }

    pub fn move_down(&mut self, store: &Store) -> io::Result<()> {
        match self.mode {
            Mode::SearchResults => {
                self.search_index = next(self.search_index, self.search_results.len());
                self.keep_search_visible();
                self.select_search_result(store)?;
            }
            Mode::Activity => {
                self.activity_index = next(self.activity_index, self.activity.len());
                keep_visible(self.activity_index, &mut self.activity_offset, 8);
            }
            Mode::Inspect => self.inspect_scroll = self.inspect_scroll.saturating_add(1),
            Mode::Main if self.main_tab == MainTab::Timeline => {
                self.timeline_index = next(self.timeline_index, self.visible_timeline().len());
                self.select_timeline(store)?;
            }
            Mode::Main => match self.panel {
                Panel::Files => {
                    self.tree_index = next(self.tree_index, self.tree.len());
                    keep_visible(self.tree_index, &mut self.tree_offset, 8);
                    self.select_tree_file(store)?;
                }
                Panel::History => {
                    self.revision_index = next(self.revision_index, self.revisions.len());
                    keep_visible(self.revision_index, &mut self.history_offset, 8);
                    self.update_diff(store)?;
                }
                Panel::Diff => self.diff_scroll = self.diff_scroll.saturating_add(1),
            },
            _ => {}
        }
        Ok(())
    }
    pub fn move_up(&mut self, store: &Store) -> io::Result<()> {
        match self.mode {
            Mode::SearchResults => {
                self.search_index = self.search_index.saturating_sub(1);
                self.keep_search_visible();
                self.select_search_result(store)?;
            }
            Mode::Activity => {
                self.activity_index = self.activity_index.saturating_sub(1);
                keep_visible(self.activity_index, &mut self.activity_offset, 8);
            }
            Mode::Inspect => self.inspect_scroll = self.inspect_scroll.saturating_sub(1),
            Mode::Main if self.main_tab == MainTab::Timeline => {
                self.timeline_index = self.timeline_index.saturating_sub(1);
                self.select_timeline(store)?;
            }
            Mode::Main => match self.panel {
                Panel::Files => {
                    self.tree_index = self.tree_index.saturating_sub(1);
                    keep_visible(self.tree_index, &mut self.tree_offset, 8);
                    self.select_tree_file(store)?;
                }
                Panel::History => {
                    self.revision_index = self.revision_index.saturating_sub(1);
                    keep_visible(self.revision_index, &mut self.history_offset, 8);
                    self.update_diff(store)?;
                }
                Panel::Diff => self.diff_scroll = self.diff_scroll.saturating_sub(1),
            },
            _ => {}
        }
        Ok(())
    }
    pub fn update_diff(&mut self, store: &Store) -> io::Result<()> {
        self.diff_scroll = 0;
        let Some(right) = self.selected_revision().cloned() else {
            self.diff.clear();
            self.diff_context = DiffContext::default();
            return Ok(());
        };
        let left_number = self.marked_revision.unwrap_or(right.number);
        let Some(left) = self
            .revisions
            .iter()
            .find(|r| r.number == left_number)
            .cloned()
        else {
            self.diff.clear();
            return Ok(());
        };
        let left_bytes = store.render(&left)?;
        let right_bytes = if self.compare_current {
            fs::read(&right.access.path).unwrap_or_default()
        } else {
            store.render(&right)?
        };
        let left_bytes = if self.compare_current && left_bytes == right_bytes && left.diff.is_some()
        {
            store.backup(&left)?
        } else {
            left_bytes
        };
        self.diff = unified_diff(
            &left_bytes,
            &right_bytes,
            &format!("rev {left_number}"),
            if self.compare_current {
                "current".into()
            } else {
                format!("rev {}", right.number)
            },
        )?;
        self.diff_context.target = SelectedTarget::ReposFiles {
            path: right.access.path.clone(),
            revision: right.number,
        };
        let kind = if right.diff.is_none() {
            EventKind::Access
        } else if store.backup(&right)?.is_empty() {
            EventKind::Create
        } else {
            EventKind::Edit
        };
        self.selected_item = Some(SelectedHistoryItem {
            revision: right,
            kind,
        });
        Ok(())
    }
    pub fn mark_diff(&mut self, store: &Store) -> io::Result<()> {
        if let Some(n) = self.selected_revision().map(|r| r.number) {
            if self.marked_revision.is_some() {
                self.compare_current = false;
            } else {
                self.marked_revision = Some(n);
            }
            self.update_diff(store)?;
        }
        Ok(())
    }
    pub fn compare_to_current(&mut self, store: &Store) -> io::Result<()> {
        self.compare_current = true;
        self.update_diff(store)
    }
    pub fn inspect_selected(&mut self, store: &Store) -> io::Result<()> {
        if let Some(revision) = self.selected_context_revision().cloned() {
            self.inspect = String::from_utf8_lossy(&store.render(&revision)?).into_owned();
            self.back_mode = self.mode.clone();
            self.inspect_scroll = 0;
            self.mode = Mode::Inspect;
        }
        Ok(())
    }
    pub fn search(&mut self, store: &Store) -> io::Result<()> {
        let needle = self.input.clone();
        self.search_results.clear();
        if needle.is_empty() {
            self.status = "Search cannot be empty".into();
            return Ok(());
        }
        for r in store.revisions()? {
            if !self.filters.matches(&r) {
                continue;
            }
            let rendered = String::from_utf8_lossy(&store.render(&r)?).into_owned();
            let content_count = rendered.matches(&needle).count();
            let tag_match = r.tag.as_deref().is_some_and(|tag| tag.contains(&needle));
            let path_text = r.access.path.display().to_string();
            let path_match = contains_fold(&path_text, &needle);
            let editor_match = contains_fold(&r.access.editor, &needle);
            let actor_match = contains_fold(&r.actor, &needle);
            let count = content_count
                + usize::from(tag_match)
                + usize::from(path_match)
                + usize::from(editor_match)
                + usize::from(actor_match);
            if count > 0 {
                let content_preview = rendered
                    .lines()
                    .find(|line| line.contains(&needle))
                    .map(|line| line.replace(&needle, &format!("⟦{needle}⟧")))
                    .unwrap_or_default();
                let preview = if !content_preview.is_empty() {
                    content_preview
                } else if tag_match {
                    format!(
                        "tag: {}",
                        r.tag
                            .as_deref()
                            .unwrap_or_default()
                            .replace(&needle, &format!("⟦{needle}⟧"))
                    )
                } else if path_match {
                    format!("filename: {path_text}")
                } else if editor_match {
                    format!("editor: {}", r.access.editor)
                } else {
                    format!("actor: {}", r.actor)
                };
                self.search_results.push(SearchResult {
                    path: r.access.path.clone(),
                    revision: r.number,
                    matches: count,
                    epoch: r.access.epoch,
                    editor: r.access.editor.clone(),
                    actor: r.actor.clone(),
                    tag: r.tag.clone(),
                    preview,
                    kind: if r.diff.is_none() {
                        EventKind::Access
                    } else if store.backup(&r)?.is_empty() {
                        EventKind::Create
                    } else {
                        EventKind::Edit
                    },
                });
            }
        }
        self.search_results.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then(b.epoch.cmp(&a.epoch))
                .then(b.revision.cmp(&a.revision))
        });
        self.search_index = 0;
        self.search_offset = 0;
        self.main_tab = MainTab::Search;
        self.mode = Mode::SearchResults;
        self.select_search_result(store)?;
        Ok(())
    }

    fn update_incremental_search(&mut self, store: &Store) -> io::Result<()> {
        if self.input.is_empty() {
            self.search_results.clear();
            self.search_index = 0;
            self.search_offset = 0;
            self.diff = "Enter a search query".into();
            self.diff_context = DiffContext::default();
            self.selected_item = None;
        } else {
            self.search(store)?;
        }
        self.mode = Mode::SearchInput;
        Ok(())
    }
    pub fn clear_search(&mut self) {
        self.input.clear();
        self.search_results.clear();
        self.search_index = 0;
        self.search_offset = 0;
    }
    fn keep_search_visible(&mut self) {
        let line = search_result_line(&self.search_results, self.search_index);
        keep_visible(line, &mut self.search_offset, 8);
    }
    pub fn jump_search(&mut self, store: &Store) -> io::Result<()> {
        if let Some(hit) = self.search_results.get(self.search_index).cloned() {
            self.file_index = self.files.iter().position(|p| *p == hit.path).unwrap_or(0);
            self.refresh(store)?;
            self.revision_index = self
                .revisions
                .iter()
                .position(|r| r.number == hit.revision)
                .unwrap_or(0);
            self.inspect_selected(store)?;
        }
        Ok(())
    }
    pub fn jump_activity(&mut self, store: &Store) -> io::Result<()> {
        if let Some(hit) = self.activity.get(self.activity_index).cloned() {
            self.file_index = self
                .files
                .iter()
                .position(|p| *p == hit.access.path)
                .unwrap_or(0);
            self.refresh(store)?;
            self.revision_index = self
                .revisions
                .iter()
                .position(|r| r.number == hit.number)
                .unwrap_or(0);
            self.mode = Mode::Main;
            self.panel = Panel::History;
            self.update_diff(store)?;
        }
        Ok(())
    }
    pub fn filter_activity(&mut self, store: &Store) -> io::Result<()> {
        self.activity = store
            .revisions()?
            .into_iter()
            .filter(|revision| self.filters.matches(revision))
            .collect();
        self.activity.sort_by(|a, b| {
            b.access
                .epoch
                .cmp(&a.access.epoch)
                .then(b.number.cmp(&a.number))
        });
        self.activity_index = 0;
        self.activity_offset = 0;
        self.mode = Mode::Activity;
        Ok(())
    }
    pub fn tag(&mut self, store: &Store) -> io::Result<()> {
        let text = self.input.trim();
        if text.is_empty() {
            self.status = "Tag cannot be empty".into();
            return Ok(());
        }
        if let Some(r) = self.selected_revision() {
            mutation::write_tag(store, r, text)?;
        }
        self.input.clear();
        self.mode = Mode::Main;
        self.status = "Tag saved".into();
        self.refresh(store)
    }
    fn rebuild_tree(&mut self) {
        let today = now_epoch() / 86_400;
        let mut changed_today: BTreeMap<PathBuf, usize> = BTreeMap::new();
        for revision in &self.activity {
            if revision.access.epoch / 86_400 == today {
                *changed_today
                    .entry(revision.access.path.clone())
                    .or_default() += 1;
            }
        }
        let mut groups: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for file in &self.files {
            groups
                .entry(file.parent().unwrap_or(Path::new("/")).to_path_buf())
                .or_default()
                .push(file.clone());
        }
        self.tree.clear();
        for (parent, files) in groups {
            let label = parent
                .strip_prefix(std::env::current_dir().unwrap_or_default())
                .unwrap_or(&parent)
                .display()
                .to_string();
            self.tree.push(TreeNode {
                path: parent.clone(),
                label: if label.is_empty() {
                    "./".into()
                } else {
                    format!("{label}/")
                },
                directory: true,
                depth: 0,
            });
            if self.expanded.contains(&parent) {
                for file in files {
                    let count = changed_today.get(&file).copied().unwrap_or(0);
                    let name = file.file_name().unwrap_or_default().to_string_lossy();
                    self.tree.push(TreeNode {
                        label: if count == 0 {
                            name.into_owned()
                        } else {
                            format!("{name} [{count} today]")
                        },
                        path: file,
                        directory: false,
                        depth: 1,
                    });
                }
            }
        }
    }
    fn select_tree_file(&mut self, store: &Store) -> io::Result<()> {
        if let Some(node) = self.selected_tree() {
            if !node.directory {
                let path = node.path.clone();
                self.file_index = self.files.iter().position(|p| *p == path).unwrap_or(0);
                self.revision_index = 0;
                self.refresh(store)?;
            }
        }
        Ok(())
    }
    pub fn toggle_tree(&mut self, expand: bool) {
        let Some(node) = self.selected_tree().cloned() else {
            return;
        };
        let directory = if node.directory {
            node.path
        } else {
            node.path.parent().unwrap_or(Path::new("/")).to_path_buf()
        };
        if expand {
            self.expanded.insert(directory.clone());
        } else {
            self.expanded.remove(&directory);
        }
        self.rebuild_tree();
        self.tree_index = self
            .tree
            .iter()
            .position(|n| n.path == directory)
            .unwrap_or(0);
    }
    pub fn restore(&mut self, store: &Store) -> io::Result<()> {
        if let Some(r) = self.selected_revision().cloned() {
            let label = if self.restore_rendered {
                "goto"
            } else {
                "restore-backup"
            };
            let made = mutation::restore_revision(store, &r, self.restore_rendered, label)?;
            self.status = match &made.mirror_warning {
                Some(warning) => format!(
                    "Restored revision {} as revision {}; warning: {warning}",
                    r.number, made.number
                ),
                None => format!("Restored revision {} as revision {}", r.number, made.number),
            };
            self.mode = Mode::Main;
            self.refresh(store)?;
            self.revision_index = self
                .revisions
                .iter()
                .position(|revision| revision.number == made.number)
                .unwrap_or(0);
            self.update_diff(store)?;
        }
        Ok(())
    }

    pub fn back(&mut self) {
        self.mode = if self.mode == Mode::Inspect {
            self.back_mode.clone()
        } else {
            Mode::Main
        };
    }

    pub fn page_down(&mut self) {
        match self.mode {
            Mode::Inspect => self.inspect_scroll = self.inspect_scroll.saturating_add(10),
            Mode::SearchResults => {
                self.search_index =
                    (self.search_index + 10).min(self.search_results.len().saturating_sub(1));
                self.keep_search_visible();
            }
            Mode::Activity => {
                self.activity_index =
                    (self.activity_index + 10).min(self.activity.len().saturating_sub(1));
                keep_visible(self.activity_index, &mut self.activity_offset, 8);
            }
            _ => self.diff_scroll = self.diff_scroll.saturating_add(10),
        }
    }

    pub fn page_up(&mut self) {
        match self.mode {
            Mode::Inspect => self.inspect_scroll = self.inspect_scroll.saturating_sub(10),
            Mode::SearchResults => {
                self.search_index = self.search_index.saturating_sub(10);
                self.keep_search_visible();
            }
            Mode::Activity => {
                self.activity_index = self.activity_index.saturating_sub(10);
                keep_visible(self.activity_index, &mut self.activity_offset, 8);
            }
            _ => self.diff_scroll = self.diff_scroll.saturating_sub(10),
        }
    }

    pub fn next_hunk(&mut self) {
        if let Some(line) = hunk_lines(&self.diff)
            .into_iter()
            .find(|line| *line > usize::from(self.diff_scroll))
        {
            self.diff_scroll = line.min(u16::MAX as usize) as u16;
        }
    }

    pub fn previous_hunk(&mut self) {
        if let Some(line) = hunk_lines(&self.diff)
            .into_iter()
            .rev()
            .find(|line| *line < usize::from(self.diff_scroll))
        {
            self.diff_scroll = line.min(u16::MAX as usize) as u16;
        }
    }

    pub fn diff_header(&self) -> String {
        let selected = self.selected_revision().map(|revision| revision.number);
        let left = self.marked_revision.or(selected);
        match (left, selected, self.compare_current) {
            (Some(left), _, true) => format!("rev {left} → current"),
            (Some(left), Some(right), false) => format!("rev {left} → rev {right}"),
            _ => "no comparison".into(),
        }
    }
}

impl SearchFilters {
    fn matches(&self, revision: &Revision) -> bool {
        contains_fold(&revision.access.path.display().to_string(), &self.path)
            && contains_fold(revision.tag.as_deref().unwrap_or(""), &self.tag)
            && contains_fold(&revision.actor, &self.actor)
            && contains_fold(&revision.access.editor, &self.editor)
            && (self.date.is_empty()
                || contains_fold(&revision.access.stamp, &self.date)
                || contains_fold(
                    &format_timestamp(revision.access.epoch, now_epoch()),
                    &self.date,
                ))
    }

    fn field_mut(&mut self, field: FilterField) -> &mut String {
        match field {
            FilterField::Path => &mut self.path,
            FilterField::Tag => &mut self.tag,
            FilterField::Actor => &mut self.actor,
            FilterField::Editor => &mut self.editor,
            FilterField::Date => &mut self.date,
        }
    }
}

impl FilterField {
    fn next(self) -> Self {
        match self {
            Self::Path => Self::Tag,
            Self::Tag => Self::Actor,
            Self::Actor => Self::Editor,
            Self::Editor => Self::Date,
            Self::Date => Self::Path,
        }
    }
}

fn contains_fold(value: &str, needle: &str) -> bool {
    needle.is_empty() || value.to_lowercase().contains(&needle.to_lowercase())
}

fn hunk_lines(diff: &str) -> Vec<usize> {
    diff.lines()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("@@").then_some(index))
        .collect()
}

fn keep_visible(index: usize, offset: &mut usize, height: usize) {
    if index < *offset {
        *offset = index;
    } else if index >= offset.saturating_add(height) {
        *offset = index + 1 - height;
    }
}

fn search_result_line(results: &[SearchResult], selected: usize) -> usize {
    let mut line = 1;
    let mut previous: Option<&Path> = None;
    for (index, result) in results.iter().enumerate() {
        if previous != Some(result.path.as_path()) {
            line += 2;
            previous = Some(&result.path);
        }
        if index == selected {
            return line;
        }
        line += 1 + usize::from(!result.preview.is_empty());
    }
    line
}

fn next(index: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (index + 1).min(len - 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutClass {
    Wide,
    Medium,
    Minimum,
    TooSmall,
}

fn layout_class(width: u16, height: u16) -> LayoutClass {
    if width < 38 || height < 10 {
        LayoutClass::TooSmall
    } else if width < 50 || height < 14 {
        LayoutClass::Minimum
    } else if width < 80 {
        LayoutClass::Medium
    } else {
        LayoutClass::Wide
    }
}

fn diff_panel_height(class: LayoutClass, terminal_height: u16) -> u16 {
    if class == LayoutClass::Minimum || terminal_height < 15 {
        3
    } else if terminal_height >= 18 {
        9
    } else {
        6
    }
}

fn header_height(class: LayoutClass) -> u16 {
    match class {
        LayoutClass::TooSmall => 1,
        LayoutClass::Wide | LayoutClass::Medium | LayoutClass::Minimum => 5,
    }
}

fn rainbow_colour(relative: usize) -> ratatui::style::Color {
    use ratatui::style::Color;
    const PALETTE: [Color; 7] = [
        Color::Red,
        Color::LightRed,
        Color::Yellow,
        Color::Green,
        Color::Cyan,
        Color::Blue,
        Color::Magenta,
    ];
    PALETTE[relative % PALETTE.len()]
}

fn rainbow_band_left(width: usize, elapsed: Duration) -> isize {
    let distance = elapsed.as_millis().min(WAVE_DURATION.as_millis()) as usize
        * (width + RAINBOW_BAND_WIDTH)
        / WAVE_DURATION.as_millis() as usize;
    distance as isize - RAINBOW_BAND_WIDTH as isize
}

fn header_colour(
    column: usize,
    width: usize,
    wave_elapsed: Option<Duration>,
    static_colour: ratatui::style::Color,
) -> ratatui::style::Color {
    let Some(elapsed) = wave_elapsed.filter(|elapsed| *elapsed < WAVE_DURATION) else {
        return static_colour;
    };
    let relative = column as isize - rainbow_band_left(width, elapsed);
    if (0..RAINBOW_BAND_WIDTH as isize).contains(&relative) {
        rainbow_colour(relative as usize)
    } else {
        static_colour
    }
}

fn slogan_gap(width: usize, logo_width: usize, slogan_width: usize) -> usize {
    if width >= logo_width + PREFERRED_SLOGAN_GAP + slogan_width {
        PREFERRED_SLOGAN_GAP
    } else {
        width
            .saturating_sub(logo_width + slogan_width)
            .min(PREFERRED_SLOGAN_GAP)
    }
}

#[cfg(test)]
fn rainbow_columns(width: usize, elapsed: Duration) -> Vec<usize> {
    let left = rainbow_band_left(width, elapsed);
    (0..width)
        .filter(|column| {
            let relative = *column as isize - left;
            elapsed < WAVE_DURATION && (0..RAINBOW_BAND_WIDTH as isize).contains(&relative)
        })
        .collect()
}

fn style_diff_line(line: &str) -> ratatui::style::Style {
    use ratatui::style::{Color, Style};
    let foreground = if line.starts_with("@@") {
        Color::Cyan
    } else if line.starts_with("---") || line.starts_with("+++") {
        Color::Yellow
    } else if line.starts_with('+') {
        Color::Green
    } else if line.starts_with('-') {
        Color::Red
    } else {
        Color::White
    };
    Style::default().fg(foreground).bg(Color::Black)
}

fn now_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn utc_parts(epoch: u64) -> (i32, u32, u32, u32, u32, u32, i64) {
    let raw = epoch.min(i64::MAX as u64) as libc::time_t;
    let mut output = std::mem::MaybeUninit::<libc::tm>::uninit();
    let result = unsafe { libc::gmtime_r(&raw, output.as_mut_ptr()) };
    if result.is_null() {
        return (1970, 1, 1, 0, 0, 0, 0);
    }
    let parts = unsafe { output.assume_init() };
    (
        parts.tm_year + 1900,
        (parts.tm_mon + 1) as u32,
        parts.tm_mday as u32,
        parts.tm_hour as u32,
        parts.tm_min as u32,
        parts.tm_sec as u32,
        epoch as i64 / 86_400,
    )
}

fn format_timestamp(epoch: u64, now: u64) -> String {
    if epoch == 0 {
        return "unknown".into();
    }
    let (year, month, day, hour, minute, second, epoch_day) = utc_parts(epoch);
    let (now_year, _, _, _, _, _, now_day) = utc_parts(now);
    if epoch_day == now_day {
        format!("{hour:02}:{minute:02}:{second:02}")
    } else if epoch_day + 1 == now_day {
        format!("Yesterday {hour:02}:{minute:02}")
    } else if year == now_year {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        format!(
            "{} {day:02} {hour:02}:{minute:02}",
            MONTHS[(month.saturating_sub(1) as usize).min(11)]
        )
    } else {
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
    }
}

fn fit(value: String, width: usize) -> String {
    let count = value.chars().count();
    if count <= width {
        return value;
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut result: String = value.chars().take(width - 1).collect();
    result.push('…');
    result
}

fn timeline_row(revision: &Revision, current: bool, width: usize) -> String {
    let marker = if current { "● CURRENT" } else { "●" };
    let time = format_timestamp(revision.access.epoch, now_epoch());
    let tag = revision
        .tag
        .as_deref()
        .map(|tag| format!(" ★ {tag}"))
        .unwrap_or_default();
    let row = if width >= 64 {
        format!(
            "{marker} rev {:<4} {:<16} {:<7} {:<10}{tag}",
            revision.number, time, revision.access.editor, revision.actor
        )
    } else if width >= 42 {
        format!(
            "{marker} r{} {} {} {}{tag}",
            revision.number, time, revision.access.editor, revision.actor
        )
    } else {
        format!("{marker} r{} {}{tag}", revision.number, time)
    };
    fit(row, width)
}

fn activity_row(revision: &Revision, width: usize) -> String {
    let tag = revision
        .tag
        .as_deref()
        .map(|tag| format!(" ★ {tag}"))
        .unwrap_or_default();
    let path = revision.access.path.display();
    let row = if width >= 80 {
        let path = fit(path.to_string(), 30);
        format!(
            "{:<16} {:<30} {:<7} {:<10}{tag}",
            format_timestamp(revision.access.epoch, now_epoch()),
            path,
            revision.access.editor,
            revision.actor
        )
    } else {
        format!(
            "{} {} {} {}{tag}",
            format_timestamp(revision.access.epoch, now_epoch()),
            path,
            revision.access.editor,
            revision.actor
        )
    };
    fit(row, width)
}
fn unified_diff(
    left: &[u8],
    right: &[u8],
    left_label: &str,
    right_label: String,
) -> io::Result<String> {
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT_DIFF: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bedit-tui-diff-{}-{}",
        std::process::id(),
        NEXT_DIFF.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir)?;
    let a = dir.join("a");
    let b = dir.join("b");
    fs::write(&a, left)?;
    fs::write(&b, right)?;
    let out = Command::new("diff")
        .args(["-u", "--"])
        .arg(&a)
        .arg(&b)
        .output();
    let _ = fs::remove_dir_all(&dir);
    let out = out?;
    if out.status.code().is_some_and(|c| c > 1) {
        return Err(io::Error::other("diff failed"));
    }
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    let _ = lines.next();
    let _ = lines.next();
    let rest = lines.collect::<Vec<_>>().join("\n");
    text = format!("--- {left_label}\n+++ {right_label}\n{rest}");
    Ok(text)
}

pub fn main() -> i32 {
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("bedit: {e}");
            255
        }
    }
}

struct TerminalGuard {
    active: bool,
}
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        Ok(Self { active: true })
    }
    fn leave(&mut self) -> io::Result<()> {
        if self.active {
            crossterm::terminal::disable_raw_mode()?;
            crossterm::execute!(
                io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::cursor::Show
            )?;
            self.active = false;
        }
        Ok(())
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

fn run() -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("interactive terminal required"));
    }
    let store = Store::from_environment()?;
    let config = store.config();
    for path in [
        &config.access,
        &config.backups,
        &config.edits,
        &config.dirs,
        &config.tags,
    ] {
        config.create_dir_all(path)?;
    }
    let mut app = App::load(&store)?;
    let mut guard = TerminalGuard::enter()?;
    let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    while !app.quit {
        terminal.draw(|frame| draw(frame, &app))?;
        let timeout = HeaderAnimationState.next_delay(app.header_started.elapsed());
        if !crossterm::event::poll(timeout)? {
            continue;
        }
        if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
            if key.kind == crossterm::event::KeyEventKind::Press {
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
                    && key.code == crossterm::event::KeyCode::Char('c')
                {
                    app.quit = true;
                    continue;
                }
                if let Err(error) = handle_key(&mut app, &store, key.code, &mut guard) {
                    record_error(&mut app, &error);
                }
            }
        }
    }
    guard.leave()
}

fn record_error(app: &mut App, error: &io::Error) {
    app.status = format!("Error: {error}");
    app.mode = Mode::Main;
}

fn handle_key(
    app: &mut App,
    store: &Store,
    key: crossterm::event::KeyCode,
    _guard: &mut TerminalGuard,
) -> io::Result<()> {
    use crossterm::event::KeyCode::*;
    if matches!(app.mode, Mode::SearchInput | Mode::TagInput) {
        match key {
            Tab => app.next_tab(store)?,
            BackTab => app.previous_tab(store)?,
            Esc => {
                app.input.clear();
                app.mode = Mode::Main
            }
            Enter => {
                if app.mode == Mode::SearchInput {
                    app.inspect_selected(store)?
                } else {
                    app.tag(store)?
                }
            }
            Backspace => {
                app.input.pop();
                if app.mode == Mode::SearchInput {
                    app.update_incremental_search(store)?;
                }
            }
            Up if app.mode == Mode::SearchInput => {
                app.search_index = app.search_index.saturating_sub(1);
                app.select_search_result(store)?;
                app.mode = Mode::SearchInput;
            }
            Down if app.mode == Mode::SearchInput => {
                app.search_index = next(app.search_index, app.search_results.len());
                app.select_search_result(store)?;
                app.mode = Mode::SearchInput;
            }
            PageDown if app.mode == Mode::SearchInput => {
                app.search_index = next(app.search_index, app.search_results.len());
                app.select_search_result(store)?;
                app.mode = Mode::SearchInput;
            }
            PageUp if app.mode == Mode::SearchInput => {
                app.search_index = app.search_index.saturating_sub(1);
                app.select_search_result(store)?;
                app.mode = Mode::SearchInput;
            }
            Char(c) => {
                app.input.push(c);
                if app.mode == Mode::SearchInput {
                    app.update_incremental_search(store)?;
                }
            }
            _ => {}
        }
        return Ok(());
    }
    if app.mode == Mode::Filters {
        match key {
            Esc => app.mode = app.filter_origin.clone(),
            Enter => {
                if matches!(app.filter_origin, Mode::SearchInput | Mode::SearchResults) {
                    app.search(store)?;
                } else if app.filter_origin == Mode::Activity {
                    app.filter_activity(store)?;
                } else {
                    app.mode = Mode::Main;
                    app.status = "Filters saved; use / or H to apply".into();
                }
            }
            Tab | Down => app.filter_field = app.filter_field.next(),
            Backspace => {
                app.filters.field_mut(app.filter_field).pop();
            }
            Char(c) => app.filters.field_mut(app.filter_field).push(c),
            _ => {}
        }
        return Ok(());
    }
    if app.mode == Mode::ConfirmRestore {
        match key {
            Enter | Char('y') => app.restore(store)?,
            Esc | Char('n') => {
                app.mode = Mode::Main;
                app.status = "Cancelled".into()
            }
            _ => {}
        }
        return Ok(());
    }
    match key {
        Tab => app.next_tab(store)?,
        BackTab => app.previous_tab(store)?,
        Char('q') => {
            if app.mode == Mode::Main {
                app.quit = true
            } else {
                app.back()
            }
        }
        Esc => app.back(),
        Up | Char('k') => app.move_up(store)?,
        Down | Char('j') => app.move_down(store)?,
        Left | Char('h') => {
            if app.main_tab != MainTab::ReposFiles {
                app.panel = Panel::History;
            } else if app.panel == Panel::Files {
                app.toggle_tree(false)
            } else {
                app.panel = match app.panel {
                    Panel::Diff => Panel::History,
                    _ => Panel::Files,
                }
            }
        }
        Right | Char('l') => {
            if app.main_tab != MainTab::ReposFiles {
                app.panel = Panel::Diff;
            } else if app.panel == Panel::Files && app.selected_tree().is_some_and(|n| n.directory)
            {
                app.toggle_tree(true)
            } else {
                app.panel = match app.panel {
                    Panel::Files => Panel::History,
                    _ => Panel::Diff,
                }
            }
        }
        Enter => match app.mode {
            Mode::SearchResults => app.jump_search(store)?,
            Mode::Activity => app.jump_activity(store)?,
            Mode::Main => {
                if app.main_tab == MainTab::ReposFiles
                    && app.panel == Panel::Files
                    && app.selected_tree().is_some_and(|n| n.directory)
                {
                    let expand = app
                        .selected_tree()
                        .is_some_and(|n| !app.expanded.contains(&n.path));
                    app.toggle_tree(expand)
                } else {
                    app.inspect_selected(store)?
                }
            }
            _ => {}
        },
        Char('d') => app.mark_diff(store)?,
        Char('c') => app.compare_to_current(store)?,
        Char('/') => {
            app.main_tab = MainTab::Search;
            app.input.clear();
            app.mode = Mode::SearchInput
        }
        Char('f') => {
            if app.main_tab == MainTab::Timeline && app.mode == Mode::Main {
                app.timeline_changes_only = !app.timeline_changes_only;
                app.timeline_index = app
                    .timeline_index
                    .min(app.visible_timeline().len().saturating_sub(1));
                app.select_timeline(store)?;
            } else {
                app.filter_origin = app.mode.clone();
                app.mode = Mode::Filters;
            }
        }
        Char('t') => {
            app.input.clear();
            app.mode = Mode::TagInput
        }
        Char('H') => {
            app.main_tab = MainTab::Timeline;
            app.mode = Mode::Main;
            app.select_timeline(store)?;
        }
        Char('?') => app.mode = Mode::Help,
        Char('r') => {
            app.restore_rendered = false;
            app.mode = Mode::ConfirmRestore
        }
        Char('g') => {
            app.restore_rendered = true;
            app.mode = Mode::ConfirmRestore
        }
        Char('n') => app.next_hunk(),
        Char('N') => app.previous_hunk(),
        PageDown => {
            app.page_down();
            if app.mode == Mode::SearchResults {
                app.select_search_result(store)?;
            }
        }
        PageUp => {
            app.page_up();
            if app.mode == Mode::SearchResults {
                app.select_search_result(store)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    draw_at(frame, app, app.header_started.elapsed());
}

fn draw_at(frame: &mut ratatui::Frame, app: &App, elapsed: Duration) {
    use ratatui::{
        layout::{Constraint, Direction, Layout},
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Clear, Paragraph, Wrap},
    };
    let area = frame.area();
    let class = layout_class(area.width, area.height);
    if class == LayoutClass::TooSmall {
        frame.render_widget(
            Paragraph::new("Terminal too small\nResize to 38×10 or press q")
                .block(Block::default().title(" Bedit ").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height(class)),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(diff_panel_height(class, area.height)),
            Constraint::Length(if class == LayoutClass::Wide { 2 } else { 1 }),
        ])
        .split(area);
    render_header(frame, outer[0], class, elapsed);
    let tabs = [
        (MainTab::Timeline, " Timeline "),
        (MainTab::ReposFiles, " Repos/Files "),
        (MainTab::Search, " Search "),
    ];
    let tab_line = Line::from(
        tabs.into_iter()
            .flat_map(|(tab, label)| {
                let style = if app.main_tab == tab {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Black)
                };
                [Span::styled(format!("[{label}]"), style), Span::raw(" ")]
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(tab_line), outer[2]);

    match app.main_tab {
        MainTab::Timeline => render_timeline(frame, app, outer[3]),
        MainTab::ReposFiles => render_repos_files(frame, app, outer[3]),
        MainTab::Search => render_search(frame, app, outer[3]),
    }

    let diff_lines: Vec<Line> = app
        .diff
        .lines()
        .map(|line| Line::from(Span::styled(line.to_owned(), style_diff_line(line))))
        .collect();
    let marked = app
        .marked_revision
        .filter(|_| app.main_tab == MainTab::ReposFiles)
        .map(|revision| format!(" · MARKED rev {revision}"))
        .unwrap_or_default();
    let label = format!(" DIFF — {}{} ", diff_context_label(app), marked);
    frame.render_widget(
        Paragraph::new(diff_lines)
            .style(Style::default().fg(Color::White).bg(Color::Black))
            .scroll((app.diff_scroll, 0))
            .block(
                Block::default()
                    .title(label)
                    .borders(Borders::ALL)
                    .style(Style::default().fg(Color::White).bg(Color::Black))
                    .border_style(active(app, Panel::Diff).bg(Color::Black)),
            ),
        outer[4],
    );
    let keys = if class == LayoutClass::Wide {
        "Tab tabs  ↑↓ navigate  ←→ panels  PgUp/PgDn scroll  n/N hunks  Enter inspect  / search  f filter  ? help  q quit"
    } else {
        "Tab tabs  ↑↓ nav  ←→ panel  PgUp/PgDn  Enter open  q quit"
    };
    frame.render_widget(Paragraph::new(format!("{} | {keys}", app.status)), outer[5]);
    if !matches!(
        app.mode,
        Mode::Main | Mode::SearchInput | Mode::SearchResults | Mode::Activity
    ) {
        let popup = ratatui::layout::Rect {
            x: area.x + 5,
            y: area.y + 3,
            width: area.width - 10,
            height: area.height - 6,
        };
        frame.render_widget(Clear, popup);
        let text = overlay_text_with_width(app, popup.width.saturating_sub(2) as usize);
        let paragraph = if app.mode == Mode::Inspect {
            Paragraph::new(inspect_lines(app)).style(Style::default().bg(Color::Black))
        } else {
            Paragraph::new(text).style(Style::default().bg(Color::Black))
        };
        frame.render_widget(
            paragraph
                .scroll((
                    if app.mode == Mode::Inspect {
                        app.inspect_scroll
                    } else {
                        0
                    },
                    0,
                ))
                .block(
                    Block::default()
                        .title(format!(" {:?} ", app.mode))
                        .borders(Borders::ALL)
                        .style(Style::default().bg(Color::Black)),
                )
                .wrap(Wrap { trim: false }),
            popup,
        );
    }
}

fn render_header(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    class: LayoutClass,
    elapsed: Duration,
) {
    use ratatui::{
        style::{Color, Modifier, Style},
        text::{Line, Span},
        widgets::Paragraph,
    };
    let snapshot = HeaderAnimationState.snapshot(elapsed);
    let rows: &[&str] = match class {
        LayoutClass::Wide => &[
            "██████╗ ███████╗██████╗ ██╗████████╗",
            "██╔══██╗██╔════╝██╔══██╗██║╚══██╔══╝",
            "██████╔╝█████╗  ██║  ██║██║   ██║",
            "██╔══██╗██╔══╝  ██║  ██║██║   ██║",
            "██████╔╝███████╗██████╔╝██║   ██║",
        ],
        LayoutClass::Medium => &["╔╗ ╔═╗╔╦╗╦╔╦╗", "╠╩╗║╣  ║║║ ║ ", "╚═╝╚═╝═╩╝╩ ╩ "],
        LayoutClass::Minimum | LayoutClass::TooSmall => &["BEDIT"],
    };
    let logo_width = rows
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(5);
    let mut lines = Vec::with_capacity(5);
    for row_index in 0..5 {
        let row = rows.get(row_index).copied().unwrap_or("");
        let mut spans = Vec::new();
        for (column, character) in row.chars().enumerate() {
            let colour = header_colour(
                column,
                usize::from(area.width),
                snapshot.wave_elapsed,
                Color::Cyan,
            );
            spans.push(Span::styled(
                character.to_string(),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ));
        }
        if row_index == 2 && !snapshot.slogan.is_empty() {
            let slogan_width = snapshot.slogan.chars().count();
            let gap = slogan_gap(usize::from(area.width), logo_width, slogan_width);
            let padding = logo_width.saturating_sub(row.chars().count()) + gap;
            let available = usize::from(area.width).saturating_sub(logo_width + gap);
            if available > 0 {
                spans.push(Span::raw(" ".repeat(padding)));
                for (index, character) in fit(snapshot.slogan.to_owned(), available)
                    .chars()
                    .enumerate()
                {
                    let colour = header_colour(
                        logo_width + gap + index,
                        usize::from(area.width),
                        snapshot.wave_elapsed,
                        Color::Black,
                    );
                    spans.push(Span::styled(
                        character.to_string(),
                        Style::default().fg(colour),
                    ));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_timeline(frame: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    use ratatui::{
        style::{Color, Modifier, Style},
        widgets::{Block, Borders, List, ListItem, ListState},
    };
    let width = area.width.saturating_sub(2) as usize;
    let items = app
        .visible_timeline()
        .into_iter()
        .map(|event| {
            let revision = &event.revision;
            let time = format_timestamp(revision.access.epoch, now_epoch());
            let row = if width >= 84 {
                format!(
                    "{time:<16} {:<6} {}  r{}  {:<7} {}",
                    event.kind.label(),
                    revision.access.path.display(),
                    revision.number,
                    revision.access.editor,
                    revision.actor
                )
            } else if width >= 48 {
                format!(
                    "{time:<8} {:<6} {}  r{}",
                    event.kind.label(),
                    revision.access.path.display(),
                    revision.number
                )
            } else {
                format!(
                    "{time:<8} {:<6} {}",
                    event.kind.label(),
                    revision.access.path.display()
                )
            };
            ListItem::new(fit(row, width))
        })
        .collect::<Vec<_>>();
    let mut state =
        ListState::default().with_selected((!items.is_empty()).then_some(app.timeline_index));
    let filter = if app.timeline_changes_only {
        "Changes only"
    } else {
        "All events"
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" TIMELINE — {filter} (f) "))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

fn render_search(frame: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    use ratatui::{
        layout::{Constraint, Direction, Layout},
        style::{Color, Modifier, Style},
        widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(2)])
        .split(area);
    let cursor = if app.mode == Mode::SearchInput {
        "_"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(format!("{}{}", app.input, cursor)).block(
            Block::default()
                .title(" SEARCH — / to enter ")
                .borders(Borders::ALL),
        ),
        rows[0],
    );
    let width = rows[1].width.saturating_sub(2) as usize;
    let items = app
        .search_results
        .iter()
        .map(|result| ListItem::new(render_search_row(result, width)))
        .collect::<Vec<_>>();
    let title = if app.search_results.is_empty() {
        if app.input.is_empty() {
            " RESULTS ".into()
        } else {
            " RESULTS — no matches ".into()
        }
    } else {
        format!(" RESULTS — {} ", app.search_results.len())
    };
    let mut state = ListState::default()
        .with_offset(app.search_offset)
        .with_selected((!items.is_empty()).then_some(app.search_index));
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        rows[1],
        &mut state,
    );
}

fn render_search_row(result: &SearchResult, width: usize) -> String {
    let time = format_timestamp(result.epoch, now_epoch());
    let row = if width >= 84 {
        format!(
            "{time:<16} {:<6} {}  r{}  {:<7} {}",
            result.kind.label(),
            result.path.display(),
            result.revision,
            result.editor,
            result.actor
        )
    } else if width >= 48 {
        format!(
            "{time:<8} {:<6} {}  r{}",
            result.kind.label(),
            result.path.display(),
            result.revision
        )
    } else {
        format!(
            "{time:<8} {:<6} {}",
            result.kind.label(),
            result.path.display()
        )
    };
    fit(row, width)
}

fn diff_context_label(app: &App) -> String {
    match &app.diff_context.target {
        SelectedTarget::None => "no selection".into(),
        SelectedTarget::Timeline { path, revision }
        | SelectedTarget::ReposFiles { path, revision }
        | SelectedTarget::Search { path, revision } => {
            format!("{} · r{revision}", path.display())
        }
    }
}

fn render_repos_files(frame: &mut ratatui::Frame, app: &App, area: ratatui::layout::Rect) {
    use ratatui::{
        layout::{Constraint, Direction, Layout},
        style::{Color, Modifier, Style},
        widgets::{Block, Borders, List, ListItem, ListState},
    };
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);
    let files: Vec<_> = app
        .tree
        .iter()
        .map(|node| {
            ListItem::new(format!(
                "{}{}{}",
                "  ".repeat(node.depth),
                if node.directory {
                    if app.expanded.contains(&node.path) {
                        "▾ "
                    } else {
                        "▸ "
                    }
                } else {
                    "  "
                },
                node.label
            ))
        })
        .collect();
    let mut fs_state = ListState::default()
        .with_offset(app.tree_offset)
        .with_selected((!files.is_empty()).then_some(app.tree_index));
    frame.render_stateful_widget(
        List::new(files)
            .block(
                Block::default()
                    .title(" FILES ")
                    .borders(Borders::ALL)
                    .border_style(active(app, Panel::Files)),
            )
            .highlight_style(Style::default().bg(Color::Blue)),
        top[0],
        &mut fs_state,
    );
    let history_width = top[1].width.saturating_sub(2) as usize;
    let history: Vec<_> = app
        .revisions
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut item = ListItem::new(timeline_row(r, i == 0, history_width));
            if r.tag.is_some() {
                item = item.style(
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                );
            } else if i == 0 {
                item = item.style(Style::default().fg(Color::Green));
            }
            item
        })
        .collect();
    let mut hs = ListState::default()
        .with_offset(app.history_offset)
        .with_selected((!history.is_empty()).then_some(app.revision_index));
    frame.render_stateful_widget(
        List::new(history)
            .block(
                Block::default()
                    .title(format!(
                        " HISTORY — {} ",
                        app.selected_file()
                            .and_then(Path::file_name)
                            .unwrap_or_default()
                            .to_string_lossy()
                    ))
                    .borders(Borders::ALL)
                    .border_style(active(app, Panel::History)),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        top[1],
        &mut hs,
    );
}
fn active(app: &App, p: Panel) -> ratatui::style::Style {
    if app.panel == p {
        ratatui::style::Style::default().fg(ratatui::style::Color::Cyan)
    } else {
        ratatui::style::Style::default()
    }
}
#[cfg(test)]
fn overlay_text(app: &App) -> String {
    overlay_text_with_width(app, 110)
}

fn overlay_text_with_width(app: &App, width: usize) -> String {
    match app.mode {
        Mode::Help => "↑/↓ or j/k navigate\n←/→ or h/l panels\nPgUp/PgDn scroll   n/N diff hunks\nEnter inspect/select\nd mark/compare revisions   c marked → current\nt tag   g goto rendered state   r restore backup\n/ historical search   f metadata filters\nH global activity   Esc back   q quit".into(),
        Mode::SearchInput => format!(
            "Search all rendered revisions: {}_\n\nEnter search   Esc cancel (use f before/after search for filters)",
            app.input
        ),
        Mode::TagInput => format!("Tag selected revision: {}_", app.input),
        Mode::ConfirmRestore => {
            let action = if app.restore_rendered { "Goto" } else { "Restore" };
            let detail = if app.restore_rendered {
                "The historical state becomes a NEW revision."
            } else {
                "The historical backup becomes a NEW revision."
            };
            format!(
                "{action} {} revision {}?\n\nCurrent state will be preserved automatically before this revision becomes current.\n{detail}\n\nEnter/y confirm   Esc/n cancel",
                app.selected_file().unwrap_or(Path::new("file")).display(),
                app.selected_revision().map(|r| r.number).unwrap_or(0)
            )
        }
        Mode::Inspect => {
            let metadata = app.selected_context_revision().map(|revision| {
                format!(
                    "{} — revision {}\nTime: {}\nEditor: {}\nActor: {}\nTag: {}",
                    revision.access.path.display(),
                    revision.number,
                    format_timestamp(revision.access.epoch, now_epoch()),
                    revision.access.editor,
                    revision.actor,
                    revision.tag.as_deref().unwrap_or("—")
                )
            }).unwrap_or_else(|| "No revision selected".into());
            format!("{metadata}\n\n{}\n\n-----------------------------\nDIFF\n{}", numbered(&app.inspect), app.diff)
        }
        Mode::SearchResults => search_results_text(app, width),
        Mode::Activity => activity_text(app, width),
        Mode::Filters => format!(
            "METADATA FILTERS — {:?}\n\n{} path: {}_\n{} tag: {}_\n{} actor/user: {}_\n{} editor: {}_\n{} date/time: {}_\n\nTab/↓ next field   Enter apply   Esc back",
            app.filter_field,
            selected_filter(app, FilterField::Path), app.filters.path,
            selected_filter(app, FilterField::Tag), app.filters.tag,
            selected_filter(app, FilterField::Actor), app.filters.actor,
            selected_filter(app, FilterField::Editor), app.filters.editor,
            selected_filter(app, FilterField::Date), app.filters.date,
        ),
        Mode::Main => String::new(),
    }
}

fn inspect_lines(app: &App) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::{
        style::{Color, Style},
        text::{Line, Span},
    };
    let mut in_diff = false;
    overlay_text_with_width(app, 110)
        .lines()
        .map(|line| {
            let style = if line == "DIFF" || line == "-----------------------------" {
                if line == "DIFF" {
                    in_diff = true;
                }
                Style::default().fg(Color::Yellow).bg(Color::Black)
            } else if in_diff {
                style_diff_line(line)
            } else {
                Style::default().fg(Color::White).bg(Color::Black)
            };
            Line::from(Span::styled(line.to_owned(), style))
        })
        .collect()
}

fn selected_filter(app: &App, field: FilterField) -> &'static str {
    if app.filter_field == field {
        "▶"
    } else {
        " "
    }
}

fn search_results_text(app: &App, width: usize) -> String {
    let mut lines = vec![format!(
        "SEARCH: {}  ({} result{})",
        app.input,
        app.search_results.len(),
        if app.search_results.len() == 1 {
            ""
        } else {
            "s"
        }
    )];
    let mut previous: Option<&Path> = None;
    for (index, hit) in app.search_results.iter().enumerate() {
        if previous != Some(hit.path.as_path()) {
            lines.push(String::new());
            lines.push(fit(hit.path.display().to_string(), width));
            previous = Some(&hit.path);
        }
        let tag = hit
            .tag
            .as_deref()
            .map(|tag| format!(" ★ {tag}"))
            .unwrap_or_default();
        lines.push(fit(
            format!(
                "{} rev {}  {}  {}  {}  {} match{}{}",
                if index == app.search_index {
                    "▶"
                } else {
                    " "
                },
                hit.revision,
                format_timestamp(hit.epoch, now_epoch()),
                hit.editor,
                hit.actor,
                hit.matches,
                if hit.matches == 1 { "" } else { "es" },
                tag
            ),
            width,
        ));
        if !hit.preview.is_empty() {
            lines.push(fit(format!("    {}", hit.preview), width));
        }
    }
    lines.join("\n")
}

fn activity_text(app: &App, width: usize) -> String {
    let files = app
        .activity
        .iter()
        .map(|revision| &revision.access.path)
        .collect::<BTreeSet<_>>()
        .len();
    let editors = app
        .activity
        .iter()
        .map(|revision| revision.access.editor.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let mut lines = vec![format!(
        "RECENT ACTIVITY — {} revisions · {} files · {} editors",
        app.activity.len(),
        files,
        editors
    )];
    lines.extend(app.activity.iter().enumerate().map(|(index, revision)| {
        format!(
            "{} {}",
            if index == app.activity_index {
                "▶"
            } else {
                " "
            },
            activity_row(revision, width.saturating_sub(2))
        )
    }));
    lines.join("\n")
}
fn numbered(text: &str) -> String {
    text.lines()
        .enumerate()
        .map(|(i, l)| format!("{:>5}  {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, Store) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "bedit-tui-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let a = base.join("work/a.txt");
        let b = base.join("work/b.txt");
        fs::write(&a, "three\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        let config = Config::load(base.join("store"), &base.join("missing"), &base).unwrap();
        fs::create_dir_all(&config.root).unwrap();
        let store = Store::new(config);
        mutation::create_revision(&store, &a, "vi", b"one\n", b"two\n").unwrap();
        mutation::create_revision(&store, &a, "vi", b"two\n", b"three\n").unwrap();
        mutation::create_revision(&store, &b, "nano", b"alpha\n", b"beta\n").unwrap();
        (base, store)
    }
    #[test]
    fn selecting_file_updates_history() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.main_tab = MainTab::ReposFiles;
        assert_eq!(app.revisions.len(), 2);
        app.move_down(&store).unwrap();
        assert_eq!(app.selected_file().unwrap().file_name().unwrap(), "b.txt");
        assert_eq!(app.revisions.len(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn repository_tree_collapses_and_expands_without_exposing_store_files() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.tree_index = 0;
        let directory = app.selected_tree().unwrap().path.clone();
        app.toggle_tree(false);
        assert_eq!(app.tree.iter().filter(|node| !node.directory).count(), 0);
        assert!(!app
            .tree
            .iter()
            .any(|node| node.path.starts_with(store.config().root.clone())));
        app.toggle_tree(true);
        assert!(app.tree.iter().any(|node| !node.directory));
        assert!(app.expanded.contains(&directory));
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn revision_navigation_and_diff_marking_are_immediate() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.main_tab = MainTab::ReposFiles;
        app.update_diff(&store).unwrap();
        app.panel = Panel::History;
        assert!(app.diff.contains("current"));
        app.mark_diff(&store).unwrap();
        app.move_down(&store).unwrap();
        app.mark_diff(&store).unwrap();
        assert!(!app.compare_current);
        assert!(app.diff.contains("rev 2"));
        fs::remove_dir_all(base).unwrap();
    }
    #[test]
    fn search_activity_inspection_and_tag_use_store() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.input = "two".into();
        app.search(&store).unwrap();
        assert!(!app.search_results.is_empty());
        app.jump_search(&store).unwrap();
        assert_eq!(app.mode, Mode::Inspect);
        app.mode = Mode::Main;
        app.input = "checkpoint".into();
        app.tag(&store).unwrap();
        assert!(store
            .revisions()
            .unwrap()
            .iter()
            .any(|r| r.tag.as_deref() == Some("checkpoint")));
        app.mode = Mode::Activity;
        app.jump_activity(&store).unwrap();
        assert_eq!(app.panel, Panel::History);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn restore_confirmation_cancel_and_success_keep_app_usable() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        let path = app.selected_file().unwrap().to_path_buf();
        let before = fs::read(&path).unwrap();
        app.mode = Mode::ConfirmRestore;
        app.mode = Mode::Main;
        assert_eq!(fs::read(&path).unwrap(), before);
        app.restore_rendered = true;
        app.mode = Mode::ConfirmRestore;
        app.restore(&store).unwrap();
        assert_eq!(app.mode, Mode::Main);
        assert!(app.status.contains("Restored revision"));
        assert!(app.revisions.len() >= 3);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn small_terminal_renders_recovery_message_without_panicking() {
        let (base, store) = fixture();
        let app = App::load(&store).unwrap();
        let backend = ratatui::backend::TestBackend::new(40, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Terminal too small"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn action_failure_returns_to_main_with_status() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.mode = Mode::ConfirmRestore;
        record_error(&mut app, &io::Error::other("permission denied"));
        assert_eq!(app.mode, Mode::Main);
        assert_eq!(app.status, "Error: permission denied");
        assert!(!app.quit);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn search_groups_rendered_content_and_applies_metadata_filters() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.input = "three".into();
        app.filters.editor = "vi".into();
        app.filters.path = "a.txt".into();
        app.search(&store).unwrap();
        assert_eq!(app.search_results.len(), 1);
        let hit = &app.search_results[0];
        assert_eq!(hit.matches, 1);
        assert_eq!(hit.editor, "vi");
        assert!(!hit.actor.is_empty());
        assert!(overlay_text(&app).contains("1 match"));
        assert!(overlay_text(&app).contains("⟦three⟧"));
        app.filters.editor = "nano".into();
        app.search(&store).unwrap();
        assert!(app.search_results.is_empty());
        app.filters.editor.clear();
        app.filters.path.clear();
        app.filters.tag.clear();
        app.filters.date = store.revisions().unwrap()[0]
            .access
            .stamp
            .chars()
            .take(10)
            .collect();
        app.search(&store).unwrap();
        assert!(!app.search_results.is_empty());
        app.clear_search();
        assert!(app.search_results.is_empty());
        assert!(app.input.is_empty());
        assert_eq!(app.search_index, 0);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn search_navigation_jump_and_back_preserve_results_and_viewport() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.input = "t".into();
        app.search(&store).unwrap();
        assert!(app.search_results.len() >= 2);
        app.move_down(&store).unwrap();
        let selected = app.search_results[app.search_index].clone();
        let index = app.search_index;
        app.jump_search(&store).unwrap();
        assert_eq!(app.mode, Mode::Inspect);
        assert_eq!(app.selected_revision().unwrap().number, selected.revision);
        app.back();
        assert_eq!(app.mode, Mode::SearchResults);
        assert_eq!(app.search_index, index);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn timestamps_and_metadata_rows_adapt_to_width() {
        let (base, store) = fixture();
        let revision = store.revisions().unwrap().remove(0);
        let recent = format_timestamp(revision.access.epoch, revision.access.epoch + 1);
        assert_eq!(recent.len(), 8);
        let yesterday = format_timestamp(revision.access.epoch, revision.access.epoch + 86_400);
        assert!(yesterday.starts_with("Yesterday "));
        let wide = timeline_row(&revision, true, 100);
        assert!(wide.contains(&revision.access.editor));
        assert!(wide.contains(&revision.actor));
        let narrow = timeline_row(&revision, true, 32);
        assert!(narrow.len() <= 32);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn tags_are_prominent_searchable_and_refresh_immediately() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.input = "parser working".into();
        app.tag(&store).unwrap();
        assert!(
            timeline_row(app.selected_revision().unwrap(), true, 100).contains("★ parser working")
        );
        app.input = "parser working".into();
        app.search(&store).unwrap();
        assert_eq!(app.search_results.len(), 1);
        assert!(app.search_results[0].preview.contains("⟦parser working⟧"));
        app.input = "three".into();
        app.filters.tag = "parser".into();
        app.search(&store).unwrap();
        assert_eq!(app.search_results.len(), 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn activity_is_newest_first_includes_metadata_and_jumps() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        assert!(app
            .activity
            .windows(2)
            .all(|pair| pair[0].access.epoch >= pair[1].access.epoch));
        let row = activity_row(&app.activity[0], 200);
        assert!(row.contains(&app.activity[0].access.editor));
        assert!(row.contains(&app.activity[0].actor));
        app.jump_activity(&store).unwrap();
        assert_eq!(
            app.selected_revision().unwrap().number,
            app.activity[0].number
        );
        let actor = app.activity[0].actor.clone();
        app.filters.actor = actor;
        app.filter_activity(&store).unwrap();
        assert!(app
            .activity
            .iter()
            .all(|revision| app.filters.matches(revision)));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn stored_sudo_actor_is_used_by_timeline_activity_search_and_inspection() {
        let base = std::env::temp_dir()
            .join("long-path-that-must-not-push-stored-actor-out-of-the-activity-row")
            .join(format!("bedit-tui-sudo-actor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("work")).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let path = base.join("work/a.txt");
        fs::write(&path, "after\n").unwrap();
        let mut config = Config::load(base.join("store"), &base.join("missing"), &base).unwrap();
        config.history_owner = "root".into();
        config.actor = "faf".into();
        let store = Store::new(config);
        mutation::create_revision(&store, &path, "vi", b"before\n", b"after\n").unwrap();

        let mut app = App::load(&store).unwrap();
        let revision = app.selected_revision().unwrap();
        assert_eq!(revision.actor, "faf");
        assert!(timeline_row(revision, true, 100).contains("faf"));
        assert!(activity_row(&app.activity[0], 100).contains("faf"));
        app.input = "after".into();
        app.search(&store).unwrap();
        assert_eq!(app.search_results[0].actor, "faf");
        app.mode = Mode::Inspect;
        assert!(overlay_text(&app).contains("faf"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn diff_hunks_pages_and_comparisons_preserve_state() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.diff = "@@ -1 +1 @@\n-a\n+b\nctx\n@@ -20 +20 @@\n-c\n+d\n".into();
        app.next_hunk();
        assert_eq!(app.diff_scroll, 4);
        app.previous_hunk();
        assert_eq!(app.diff_scroll, 0);
        app.page_down();
        assert!(app.diff_scroll > 0);
        app.mark_diff(&store).unwrap();
        let marked = app.marked_revision;
        app.move_down(&store).unwrap();
        app.mark_diff(&store).unwrap();
        assert_eq!(app.marked_revision, marked);
        assert!(!app.compare_current);
        assert!(app.diff_header().contains('→'));
        app.compare_to_current(&store).unwrap();
        assert!(app.diff_header().ends_with("current"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn diff_lines_use_semantic_foregrounds_on_black() {
        use ratatui::style::{Color, Style};
        assert_eq!(
            style_diff_line(" context"),
            Style::default().fg(Color::White).bg(Color::Black)
        );
        assert_eq!(
            style_diff_line("+added"),
            Style::default().fg(Color::Green).bg(Color::Black)
        );
        assert_eq!(
            style_diff_line("-removed"),
            Style::default().fg(Color::Red).bg(Color::Black)
        );
        assert_eq!(
            style_diff_line("@@ -1 +1 @@"),
            Style::default().fg(Color::Cyan).bg(Color::Black)
        );
        assert_eq!(
            style_diff_line("--- before"),
            Style::default().fg(Color::Yellow).bg(Color::Black)
        );
        assert_eq!(
            style_diff_line("+++ after"),
            Style::default().fg(Color::Yellow).bg(Color::Black)
        );
        assert_eq!(
            style_diff_line("not + an addition"),
            Style::default().fg(Color::White).bg(Color::Black)
        );
    }

    #[test]
    fn normal_diff_panel_is_three_rows_taller_without_squeezing_small_layouts() {
        assert_eq!(diff_panel_height(LayoutClass::Wide, 30), 9);
        assert_eq!(diff_panel_height(LayoutClass::Medium, 18), 9);
        assert_eq!(diff_panel_height(LayoutClass::Medium, 14), 3);
        assert_eq!(diff_panel_height(LayoutClass::Minimum, 12), 3);
    }

    #[test]
    fn inspect_popup_file_body_renders_white() {
        use ratatui::style::Color;
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.mode = Mode::Inspect;
        app.inspect = "popup-white-body".into();
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let cells = terminal.backend().buffer().content();
        let expected: Vec<_> = "popup-white-body"
            .chars()
            .map(|value| value.to_string())
            .collect();
        let start = cells
            .windows(expected.len())
            .position(|window| {
                window
                    .iter()
                    .zip(&expected)
                    .all(|(cell, expected)| cell.symbol() == expected)
            })
            .unwrap();
        assert!(cells[start..start + "popup-white-body".len()]
            .iter()
            .all(|cell| cell.fg == Color::White));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn files_and_history_active_styles_remain_unchanged() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.panel = Panel::Files;
        assert_eq!(
            active(&app, Panel::Files),
            ratatui::style::Style::default().fg(ratatui::style::Color::Cyan)
        );
        app.panel = Panel::History;
        assert_eq!(
            active(&app, Panel::History),
            ratatui::style::Style::default().fg(ratatui::style::Color::Cyan)
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn inspection_has_metadata_and_returns_to_preserved_viewport() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.revision_index = 1;
        app.diff_scroll = 7;
        app.inspect_selected(&store).unwrap();
        assert_eq!(app.inspect_scroll, 0);
        let text = overlay_text(&app);
        assert!(text.contains("Editor:"));
        assert!(text.contains("Actor:"));
        app.inspect_scroll = 9;
        app.back();
        assert_eq!(app.mode, Mode::Main);
        assert_eq!(app.revision_index, 1);
        assert_eq!(app.diff_scroll, 7);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn restore_and_goto_explain_safety_and_select_new_revision() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.restore_rendered = true;
        app.mode = Mode::ConfirmRestore;
        let prompt = overlay_text(&app);
        assert!(prompt.contains("NEW revision"));
        assert!(prompt.contains("preserved automatically"));
        let before = app.revisions.len();
        app.restore(&store).unwrap();
        assert_eq!(app.revisions.len(), before + 1);
        assert_eq!(app.revision_index, 0);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn tree_recent_counts_and_viewports_persist() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        assert!(app
            .tree
            .iter()
            .any(|node| !node.directory && node.label.contains("today")));
        app.tree_offset = 3;
        app.panel = Panel::History;
        app.panel = Panel::Files;
        assert_eq!(app.tree_offset, 3);
        let expanded = app.expanded.clone();
        app.mode = Mode::Help;
        app.back();
        assert_eq!(app.expanded, expanded);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn responsive_layout_has_wide_medium_minimum_and_recovery_classes() {
        assert_eq!(layout_class(100, 30), LayoutClass::Wide);
        assert_eq!(layout_class(60, 18), LayoutClass::Medium);
        assert_eq!(layout_class(42, 12), LayoutClass::Minimum);
        assert_eq!(layout_class(30, 7), LayoutClass::TooSmall);
        let (base, store) = fixture();
        let app = App::load(&store).unwrap();
        for (width, height) in [(100, 30), (60, 18), (42, 12), (30, 7)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn main_tabs_default_to_timeline_and_cycle_without_losing_state() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        assert_eq!(app.main_tab, MainTab::Timeline);
        app.activity_index = 1;
        app.next_tab(&store).unwrap();
        assert_eq!(app.main_tab, MainTab::ReposFiles);
        app.tree_index = 2;
        app.next_tab(&store).unwrap();
        assert_eq!(app.main_tab, MainTab::Search);
        app.input = "three".into();
        app.previous_tab(&store).unwrap();
        app.previous_tab(&store).unwrap();
        assert_eq!(app.main_tab, MainTab::Timeline);
        assert_eq!(app.activity_index, 1);
        app.next_tab(&store).unwrap();
        assert_eq!(app.tree_index, 2);
        app.next_tab(&store).unwrap();
        assert_eq!(app.input, "three");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn header_animation_reveals_once_waves_and_has_bounded_deadlines() {
        let animation = HeaderAnimationState;
        assert_eq!(animation.snapshot(Duration::ZERO).slogan, "");
        assert_eq!(animation.snapshot(Duration::from_secs(2)).slogan, "BACKUPS");
        assert_eq!(
            animation.snapshot(Duration::from_secs(4)).slogan,
            "BACKUPS SAVE"
        );
        assert_eq!(
            animation.snapshot(Duration::from_secs(6)).slogan,
            "BACKUPS SAVE LIVES"
        );
        assert!(animation.snapshot(Duration::from_millis(6_500)).wave_active);
        assert!(!animation.snapshot(Duration::from_secs(8)).wave_active);
        assert!(
            animation
                .snapshot(Duration::from_millis(67_900))
                .wave_active
        );
        assert_eq!(
            animation.snapshot(Duration::from_millis(67_900)).slogan,
            "BACKUPS SAVE LIVES"
        );
        assert_eq!(header_height(LayoutClass::Wide), 5);
        assert_eq!(header_height(LayoutClass::Medium), 5);
        assert_eq!(header_height(LayoutClass::Minimum), 5);
        assert!(animation.next_delay(Duration::from_secs(8)) > Duration::from_secs(50));
        assert!(animation.next_delay(Duration::from_millis(6_500)) <= Duration::from_millis(80));
    }

    #[test]
    fn rainbow_band_progress_is_frame_deterministic() {
        let animation = HeaderAnimationState;
        let first = animation.snapshot(Duration::from_millis(6_060));
        let second = animation.snapshot(Duration::from_millis(6_120));
        assert_eq!(first.wave_elapsed, Some(Duration::from_millis(60)));
        assert_eq!(second.wave_elapsed, Some(Duration::from_millis(120)));
        assert!(
            rainbow_band_left(100, first.wave_elapsed.unwrap())
                < rainbow_band_left(100, second.wave_elapsed.unwrap())
        );
        assert_ne!(rainbow_colour(0), rainbow_colour(1));
    }

    #[test]
    fn travelling_rainbow_is_a_bounded_left_to_right_band() {
        use ratatui::style::Color;
        let width = 100;
        let early = Duration::from_millis(180);
        let middle = Duration::from_millis(900);
        let late = Duration::from_millis(1_620);

        let early_columns = rainbow_columns(width, early);
        let middle_columns = rainbow_columns(width, middle);
        let late_columns = rainbow_columns(width, late);
        assert!(!early_columns.is_empty());
        assert!(early_columns.len() <= RAINBOW_BAND_WIDTH);
        assert!(middle_columns.len() <= RAINBOW_BAND_WIDTH);
        assert!(late_columns.len() <= RAINBOW_BAND_WIDTH);
        assert!(early_columns.iter().max() < middle_columns.iter().min());
        assert!(middle_columns.iter().max() < late_columns.iter().min());

        let middle_left = *middle_columns.iter().min().unwrap();
        let middle_right = *middle_columns.iter().max().unwrap();
        assert_eq!(
            header_colour(0, width, Some(middle), Color::Cyan),
            Color::Cyan
        );
        assert_ne!(
            header_colour(middle_left, width, Some(middle), Color::Cyan),
            Color::Cyan
        );
        assert_eq!(
            header_colour(middle_right + 1, width, Some(middle), Color::Cyan),
            Color::Cyan
        );
        assert_eq!(header_colour(99, width, None, Color::Black), Color::Black);
        assert!(rainbow_columns(width, WAVE_DURATION).is_empty());
    }

    #[test]
    fn wide_slogan_uses_twelve_columns_and_narrow_spacing_never_overflows() {
        assert_eq!(slogan_gap(110, 36, 18), 12);
        for width in [42, 60, 69] {
            let gap = slogan_gap(width, 40, 18);
            assert!(40 + gap <= width);
            assert!(gap <= 12);
        }
    }

    #[test]
    fn periodic_wave_keeps_revealed_slogan_and_uses_same_progress() {
        let animation = HeaderAnimationState;
        let initial = animation.snapshot(Duration::from_millis(6_600));
        let periodic = animation.snapshot(Duration::from_millis(68_400));
        assert_eq!(initial.slogan, "BACKUPS SAVE LIVES");
        assert_eq!(periodic.slogan, "BACKUPS SAVE LIVES");
        assert_eq!(initial.wave_elapsed, periodic.wave_elapsed);
    }

    #[test]
    fn selected_current_revision_falls_back_to_actual_stored_change_body() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.main_tab = MainTab::ReposFiles;
        app.revision_index = 0;
        app.update_diff(&store).unwrap();
        assert!(app.diff.contains("@@"), "{}", app.diff);
        assert!(app.diff.contains("-two"), "{}", app.diff);
        assert!(app.diff.contains("+three"), "{}", app.diff);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn timeline_classifies_and_filters_events_and_drives_shared_diff() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        assert!(app
            .timeline
            .iter()
            .all(|event| event.kind == EventKind::Edit));
        assert!(app
            .timeline
            .windows(2)
            .all(|pair| { pair[0].revision.access.epoch >= pair[1].revision.access.epoch }));
        app.timeline_changes_only = true;
        assert!(app
            .visible_timeline()
            .iter()
            .all(|event| event.kind.is_change()));
        app.select_timeline(&store).unwrap();
        assert!(matches!(
            app.diff_context.target,
            SelectedTarget::Timeline { .. }
        ));
        assert!(app.diff.contains("@@"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn timeline_truthfully_classifies_create_edit_and_access() {
        let (base, store) = fixture();
        let access_path = base.join("work/b.txt");
        mutation::create_revision(&store, &access_path, "nano", b"beta\n", b"beta\n").unwrap();
        let create_path = base.join("work/created.txt");
        fs::write(&create_path, "born\n").unwrap();
        mutation::create_revision(&store, &create_path, "vi", b"", b"born\n").unwrap();
        let mut app = App::load(&store).unwrap();
        assert!(app
            .timeline
            .iter()
            .any(|event| event.kind == EventKind::Create));
        assert!(app
            .timeline
            .iter()
            .any(|event| event.kind == EventKind::Edit));
        let access_index = app
            .visible_timeline()
            .iter()
            .position(|event| event.kind == EventKind::Access)
            .unwrap();
        app.timeline_index = access_index;
        app.select_timeline(&store).unwrap();
        assert_eq!(app.diff, "No content change — file accessed only");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn search_is_a_tab_and_selection_drives_diff_without_popup() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.main_tab = MainTab::Search;
        app.input = "three".into();
        app.search(&store).unwrap();
        assert_eq!(app.main_tab, MainTab::Search);
        assert_eq!(app.mode, Mode::SearchResults);
        assert!(!app.search_results.is_empty());
        app.select_search_result(&store).unwrap();
        assert!(matches!(
            app.diff_context.target,
            SelectedTarget::Search { .. }
        ));
        assert!(app.diff.contains("@@"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn search_matches_filename_metadata_and_handles_no_results() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.input = "b.txt".into();
        app.search(&store).unwrap();
        assert!(!app.search_results.is_empty());
        assert!(app
            .search_results
            .iter()
            .all(|result| result.path.ends_with("b.txt")));
        assert!(app.search_results[0].preview.starts_with("filename:"));
        app.input = "definitely-not-present".into();
        app.search(&store).unwrap();
        assert!(app.search_results.is_empty());
        assert_eq!(app.diff, "No search results");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn tab_row_and_fixed_header_render_without_layout_jump() {
        let (base, store) = fixture();
        let app = App::load(&store).unwrap();
        for elapsed in [
            Duration::ZERO,
            Duration::from_secs(2),
            Duration::from_secs(6),
        ] {
            let backend = ratatui::backend::TestBackend::new(110, 30);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| draw_at(frame, &app, elapsed))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rendered = buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("Timeline"));
            assert!(rendered.contains("Repos/Files"));
            assert!(rendered.contains("Search"));
            assert!(rendered.contains("DIFF"));
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn common_inspect_from_every_tab_contains_content_and_the_shared_real_diff() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        for tab in [MainTab::Timeline, MainTab::ReposFiles] {
            app.main_tab = tab;
            if tab == MainTab::ReposFiles {
                app.file_index = app
                    .files
                    .iter()
                    .position(|path| path.file_name().unwrap() == "a.txt")
                    .unwrap();
                app.revision_index = 0;
                app.refresh(&store).unwrap();
            } else {
                app.timeline_index = app
                    .timeline
                    .iter()
                    .position(|event| event.revision.leaf == "a.txt" && event.revision.number == 2)
                    .unwrap();
                app.select_timeline(&store).unwrap();
            }
            let shared_diff = app.diff.clone();
            app.inspect_selected(&store).unwrap();
            let inspect = overlay_text(&app);
            assert_eq!(app.mode, Mode::Inspect);
            assert!(inspect.contains("Editor:"));
            assert!(inspect.contains("Actor:"));
            assert!(inspect.contains("DIFF"));
            assert!(inspect.contains("@@"));
            assert!(inspect.contains("-two"));
            assert!(inspect.contains("+three"));
            assert!(inspect.ends_with(&shared_diff));
            app.back();
        }
        app.input = "three".into();
        app.search(&store).unwrap();
        let shared_diff = app.diff.clone();
        app.inspect_selected(&store).unwrap();
        assert_eq!(app.mode, Mode::Inspect);
        assert!(overlay_text(&app).ends_with(&shared_diff));
        app.move_down(&store).unwrap();
        assert_eq!(app.inspect_scroll, 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn search_printable_keys_are_incremental_and_enter_is_the_only_activation() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.main_tab = MainTab::Search;
        app.mode = Mode::SearchInput;
        let mut guard = TerminalGuard { active: false };
        for key in ['o', 'n', 'e'] {
            handle_key(
                &mut app,
                &store,
                crossterm::event::KeyCode::Char(key),
                &mut guard,
            )
            .unwrap();
            assert_eq!(app.mode, Mode::SearchInput);
        }
        assert_eq!(app.input, "one");
        assert_ne!(app.mode, Mode::Inspect);
        assert_eq!(app.diff_scroll, 0);

        app.input.clear();
        handle_key(
            &mut app,
            &store,
            crossterm::event::KeyCode::Char('t'),
            &mut guard,
        )
        .unwrap();
        let broad = app.search_results.len();
        handle_key(
            &mut app,
            &store,
            crossterm::event::KeyCode::Char('w'),
            &mut guard,
        )
        .unwrap();
        assert!(app.search_results.len() <= broad);
        assert!(render_search_row(&app.search_results[0], 100).contains("EDIT"));
        handle_key(
            &mut app,
            &store,
            crossterm::event::KeyCode::Enter,
            &mut guard,
        )
        .unwrap();
        assert_eq!(app.mode, Mode::Inspect);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn inspect_has_no_editor_or_live_file_mutation_key() {
        let (base, store) = fixture();
        let mut app = App::load(&store).unwrap();
        app.inspect_selected(&store).unwrap();
        let path = app.selected_context_revision().unwrap().access.path.clone();
        let before = fs::read(&path).unwrap();
        let mut guard = TerminalGuard { active: false };
        for key in ['o', 'e'] {
            handle_key(
                &mut app,
                &store,
                crossterm::event::KeyCode::Char(key),
                &mut guard,
            )
            .unwrap();
        }
        assert_eq!(app.mode, Mode::Inspect);
        assert_eq!(fs::read(path).unwrap(), before);
        assert!(!overlay_text(&app).contains("open protected editor"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn header_slogan_blank_separator_and_black_tabs_render_as_specified() {
        use ratatui::style::Color;
        let (base, store) = fixture();
        let app = App::load(&store).unwrap();
        for (width, height) in [(110, 30), (60, 20), (42, 16)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| draw_at(frame, &app, Duration::from_secs(8)))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert!((0..width).all(|x| buffer[(x, 5)].symbol() == " "));
            assert!(buffer
                .content()
                .iter()
                .filter(|cell| ["T", "i", "m", "e", "l", "n"].contains(&cell.symbol()))
                .any(|cell| cell.fg == Color::Black));
        }
        let backend = ratatui::backend::TestBackend::new(110, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_at(frame, &app, Duration::from_secs(8)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let expected: Vec<_> = "BACKUPS SAVE LIVES"
            .chars()
            .map(|c| c.to_string())
            .collect();
        let slogan_x = (0..110 - expected.len() as u16)
            .find(|start| {
                expected
                    .iter()
                    .enumerate()
                    .all(|(offset, symbol)| buffer[(*start + offset as u16, 2)].symbol() == symbol)
            })
            .unwrap();
        assert_eq!(slogan_x, 36 + PREFERRED_SLOGAN_GAP as u16);
        assert!((slogan_x..slogan_x + 18).all(|x| buffer[(x, 2)].fg == Color::Black));
        terminal
            .draw(|frame| draw_at(frame, &app, Duration::from_millis(6_900)))
            .unwrap();
        assert!((slogan_x..slogan_x + 18)
            .any(|x| terminal.backend().buffer()[(x, 2)].fg != Color::Black));
        for (width, height) in [(110, 30), (60, 20), (42, 16)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut resized = ratatui::Terminal::new(backend).unwrap();
            resized
                .draw(|frame| draw_at(frame, &app, Duration::from_millis(6_900)))
                .unwrap();
            assert!((0..width).all(|x| resized.backend().buffer()[(x, 5)].symbol() == " "));
        }
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn inspect_access_message_and_diff_semantic_colours_are_shared() {
        use ratatui::style::Color;
        let (base, store) = fixture();
        let path = base.join("work/access.txt");
        mutation::create_revision(&store, &path, "vi", b"same\n", b"same\n").unwrap();
        let mut app = App::load(&store).unwrap();
        let index = app
            .visible_timeline()
            .iter()
            .position(|event| event.kind == EventKind::Access)
            .unwrap();
        app.timeline_index = index;
        app.select_timeline(&store).unwrap();
        app.inspect_selected(&store).unwrap();
        assert!(overlay_text(&app).contains("No content change — file accessed only"));
        app.diff = "--- old\n+++ new\n@@ -1 +1 @@\n-removed\n+added".into();
        let lines = inspect_lines(&app);
        assert_eq!(
            lines
                .iter()
                .find(|line| line.to_string() == "--- old")
                .unwrap()
                .spans[0]
                .style
                .fg,
            Some(Color::Yellow)
        );
        assert_eq!(
            lines
                .iter()
                .find(|line| line.to_string() == "@@ -1 +1 @@")
                .unwrap()
                .spans[0]
                .style
                .fg,
            Some(Color::Cyan)
        );
        assert_eq!(
            lines
                .iter()
                .find(|line| line.to_string() == "-removed")
                .unwrap()
                .spans[0]
                .style
                .fg,
            Some(Color::Red)
        );
        assert_eq!(
            lines
                .iter()
                .find(|line| line.to_string() == "+added")
                .unwrap()
                .spans[0]
                .style
                .fg,
            Some(Color::Green)
        );
        fs::remove_dir_all(base).unwrap();
    }
}
