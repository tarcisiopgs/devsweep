//! TUI state as a pure reducer over scan, removal and key events.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::model::{Item, ItemId, SourceId, format_size_long};
use crate::remove::RemoveEvent;
use crate::scan::ScanEvent;

/// The terminal app's name from `$TERM_PROGRAM`, for prompts that point
/// at its settings.
pub fn terminal_name(term_program: Option<&str>) -> String {
    match term_program {
        Some("ghostty") => "Ghostty",
        Some("iTerm.app") => "iTerm",
        Some("Apple_Terminal") => "Terminal",
        Some("vscode") => "Visual Studio Code",
        Some("WezTerm") => "WezTerm",
        Some("WarpTerminal") => "Warp",
        _ => "your terminal",
    }
    .to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    List,
    Review,
    Removing,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Items,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortBy {
    Size,
    Name,
    Age,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceState {
    Scanning,
    Done,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    StartRemoval(Vec<Item>),
    /// Skip the items not started yet, then quit.
    StopRemoval,
    Rescan,
    /// Open System Settings on Privacy & Security › Full Disk Access.
    OpenDiskAccessSettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress {
    Pending,
    Running,
    Ok(u64),
    Err(String),
}

#[derive(Clone, Debug)]
pub struct SourceView {
    pub state: SourceState,
    pub items: Vec<Item>,
    pub notes: Vec<String>,
}

impl SourceView {
    fn new() -> SourceView {
        SourceView {
            state: SourceState::Scanning,
            items: Vec::new(),
            notes: Vec::new(),
        }
    }
}

pub struct App {
    pub target: PathBuf,
    pub screen: Screen,
    pub focus: Focus,
    pub sort: SortBy,
    pub filter: Option<String>,
    pub editing_filter: bool,
    pub sources: BTreeMap<SourceId, SourceView>,
    pub cursor_source: usize,
    pub cursor_item: usize,
    pub selected: HashSet<ItemId>,
    pub progress: BTreeMap<ItemId, Progress>,
    /// Items sent for removal, in the order shown on the review screen.
    pub removal: Vec<Item>,
    pub spinner_tick: u64,
    /// First line shown on the review screen.
    pub review_scroll: usize,
    /// The key legend overlay is open.
    pub show_help: bool,
    /// Free disk space before and after the removal, when `df` answered.
    pub disk_free: (Option<u64>, Option<u64>),
    /// The terminal app, named in the Full Disk Access prompt.
    pub terminal: String,
    /// The user already opened the Full Disk Access settings.
    pub opened_settings: bool,
    quit_after_removal: bool,
}

impl App {
    /// `sources` are the scanners that will run; they show as scanning
    /// until their first events arrive.
    pub fn new(target: PathBuf, sources: Vec<SourceId>) -> App {
        App {
            target,
            screen: Screen::List,
            focus: Focus::Sidebar,
            sort: SortBy::Size,
            filter: None,
            editing_filter: false,
            sources: sources
                .into_iter()
                .map(|s| (s, SourceView::new()))
                .collect(),
            cursor_source: 0,
            cursor_item: 0,
            selected: HashSet::new(),
            progress: BTreeMap::new(),
            removal: Vec::new(),
            spinner_tick: 0,
            review_scroll: 0,
            show_help: false,
            disk_free: (None, None),
            terminal: "your terminal".into(),
            opened_settings: false,
            quit_after_removal: false,
        }
    }

    pub fn on_scan(&mut self, ev: ScanEvent) {
        match ev {
            ScanEvent::Found(item) => {
                // Once the review is open the selection is frozen: nothing
                // may join it without being shown first.
                if item.preselected() && self.screen == Screen::List {
                    self.selected.insert(item.id);
                }
                self.view(item.source).items.push(item);
            }
            ScanEvent::Size(id, bytes) => {
                if let Some(item) = self
                    .sources
                    .values_mut()
                    .flat_map(|v| v.items.iter_mut())
                    .find(|i| i.id == id)
                {
                    item.size = Some(bytes);
                }
            }
            ScanEvent::Note(source, note) => self.view(source).notes.push(note),
            ScanEvent::Failed(source, msg) => self.view(source).state = SourceState::Failed(msg),
            ScanEvent::Done(source) => {
                let view = self.view(source);
                if view.state == SourceState::Scanning {
                    view.state = SourceState::Done;
                }
            }
        }
        self.clamp();
    }

    pub fn on_remove(&mut self, ev: RemoveEvent) -> Action {
        match ev {
            RemoveEvent::Started(id) => {
                self.progress.insert(id, Progress::Running);
            }
            RemoveEvent::Ok(id, bytes) => {
                self.progress.insert(id, Progress::Ok(bytes));
            }
            RemoveEvent::Err(id, msg) => {
                self.progress.insert(id, Progress::Err(msg));
            }
            RemoveEvent::Finished => {
                self.screen = Screen::Done;
                if self.quit_after_removal {
                    return Action::Quit;
                }
            }
        }
        Action::None
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if self.show_help && !ctrl_c {
            self.show_help = false;
            return Action::None;
        }
        let action = match self.screen {
            Screen::List if self.editing_filter => {
                if ctrl_c {
                    return Action::Quit;
                }
                self.filter_key(key);
                Action::None
            }
            Screen::List => {
                if ctrl_c {
                    return Action::Quit;
                }
                self.list_key(key)
            }
            Screen::Review => match key.code {
                KeyCode::Char('y') if !ctrl_c => self.start_removal(),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.review_scroll += 1;
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.review_scroll = self.review_scroll.saturating_sub(1);
                    Action::None
                }
                _ => {
                    self.screen = Screen::List;
                    Action::None
                }
            },
            Screen::Removing => {
                if ctrl_c && self.quit_after_removal {
                    // Asked twice: leave now, the running command included.
                    Action::Quit
                } else if (ctrl_c || key.code == KeyCode::Char('q')) && !self.quit_after_removal {
                    self.quit_after_removal = true;
                    Action::StopRemoval
                } else {
                    Action::None
                }
            }
            Screen::Done => match key.code {
                _ if ctrl_c => Action::Quit,
                KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
                KeyCode::Char('r') => {
                    self.reset();
                    Action::Rescan
                }
                _ => Action::None,
            },
        };
        self.clamp();
        action
    }

    fn list_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('r') => {
                self.reset();
                return Action::Rescan;
            }
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('o')
                if self
                    .focused_source()
                    .is_some_and(|s| self.needs_disk_access(s)) =>
            {
                self.opened_settings = true;
                return Action::OpenDiskAccessSettings;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Items,
                    Focus::Items => Focus::Sidebar,
                }
            }
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Items,
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Sidebar,
            KeyCode::Char(' ') => self.toggle_current(),
            KeyCode::Char('a') => self.toggle_all(),
            KeyCode::Char('s') => {
                self.sort = match self.sort {
                    SortBy::Size => SortBy::Name,
                    SortBy::Name => SortBy::Age,
                    SortBy::Age => SortBy::Size,
                }
            }
            KeyCode::Char('/') => {
                self.editing_filter = true;
                self.filter.get_or_insert_with(String::new);
            }
            KeyCode::Enter if !self.selected.is_empty() => {
                self.review_scroll = 0;
                self.screen = Screen::Review;
            }
            _ => {}
        }
        Action::None
    }

    fn filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter = None;
                self.editing_filter = false;
            }
            KeyCode::Enter => {
                self.editing_filter = false;
                if self.filter.as_deref() == Some("") {
                    self.filter = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(f) = self.filter.as_mut() {
                    f.pop();
                }
            }
            KeyCode::Char(c) => self.filter.get_or_insert_with(String::new).push(c),
            _ => {}
        }
        self.cursor_item = 0;
    }

    fn move_cursor(&mut self, delta: isize) {
        let (cursor, len) = match self.focus {
            Focus::Sidebar => (&mut self.cursor_source, self.sources.len()),
            Focus::Items => {
                let len = self.visible_items().len();
                (&mut self.cursor_item, len)
            }
        };
        if len == 0 {
            return;
        }
        *cursor = (*cursor as isize + delta).clamp(0, len as isize - 1) as usize;
        if self.focus == Focus::Sidebar {
            self.cursor_item = 0;
        }
    }

    fn toggle_current(&mut self) {
        if self.focus != Focus::Items {
            return;
        }
        let Some(item) = self
            .visible_items()
            .get(self.cursor_item)
            .map(|i| (i.id, i.selectable()))
        else {
            return;
        };
        if let (id, true) = item
            && !self.selected.remove(&id)
        {
            self.selected.insert(id);
        }
    }

    fn toggle_all(&mut self) {
        let ids: Vec<ItemId> = self
            .visible_items()
            .iter()
            .filter(|i| i.selectable())
            .map(|i| i.id)
            .collect();
        if ids.iter().all(|id| self.selected.contains(id)) {
            for id in &ids {
                self.selected.remove(id);
            }
        } else {
            self.selected.extend(ids);
        }
    }

    fn start_removal(&mut self) -> Action {
        let items: Vec<Item> = self
            .review_groups()
            .into_iter()
            .flat_map(|(_, items)| items)
            .cloned()
            .collect();
        self.progress = items.iter().map(|i| (i.id, Progress::Pending)).collect();
        self.removal = items.clone();
        self.screen = Screen::Removing;
        Action::StartRemoval(items)
    }

    fn reset(&mut self) {
        for view in self.sources.values_mut() {
            *view = SourceView::new();
        }
        self.selected.clear();
        self.progress.clear();
        self.removal.clear();
        self.screen = Screen::List;
        self.cursor_item = 0;
        self.disk_free = (None, None);
        self.quit_after_removal = false;
    }

    fn view(&mut self, source: SourceId) -> &mut SourceView {
        self.sources.entry(source).or_insert_with(SourceView::new)
    }

    fn clamp(&mut self) {
        self.cursor_source = self.cursor_source.min(self.sources.len().saturating_sub(1));
        self.cursor_item = self
            .cursor_item
            .min(self.visible_items().len().saturating_sub(1));
    }

    /// Sources in display order: folder sources first, then machine ones.
    pub fn visible_sources(&self) -> Vec<SourceId> {
        self.sources.keys().copied().collect()
    }

    /// The source failed, or skipped folders, because macOS refused access:
    /// only Full Disk Access for the terminal fixes that.
    pub fn needs_disk_access(&self, source: SourceId) -> bool {
        let Some(view) = self.sources.get(&source) else {
            return false;
        };
        let denied = |m: &str| m.contains("permission denied") || m.contains("no permission");
        matches!(&view.state, SourceState::Failed(m) if denied(m))
            || view.notes.iter().any(|n| denied(n))
    }

    pub fn focused_source(&self) -> Option<SourceId> {
        self.visible_sources().get(self.cursor_source).copied()
    }

    fn sorted<'a>(&self, mut items: Vec<&'a Item>) -> Vec<&'a Item> {
        match self.sort {
            SortBy::Size => items.sort_by(|a, b| {
                b.size
                    .unwrap_or(0)
                    .cmp(&a.size.unwrap_or(0))
                    .then(b.size.is_some().cmp(&a.size.is_some()))
            }),
            SortBy::Name => items.sort_by_key(|i| i.label.to_lowercase()),
            SortBy::Age => items.sort_by_key(|i| std::cmp::Reverse(i.age_days)),
        }
        items
    }

    /// Items of the focused source, filtered and sorted.
    pub fn visible_items(&self) -> Vec<&Item> {
        let Some(source) = self.focused_source() else {
            return vec![];
        };
        let needle = self.filter.as_deref().unwrap_or("").to_lowercase();
        let items = self.sources[&source]
            .items
            .iter()
            .filter(|i| needle.is_empty() || i.label.to_lowercase().contains(&needle))
            .collect();
        self.sorted(items)
    }

    /// Selected items grouped by source, in display order.
    pub fn review_groups(&self) -> Vec<(SourceId, Vec<&Item>)> {
        self.sources
            .iter()
            .filter_map(|(source, view)| {
                let items: Vec<&Item> = view
                    .items
                    .iter()
                    .filter(|i| self.selected.contains(&i.id) && i.selectable())
                    .collect();
                (!items.is_empty()).then(|| (*source, self.sorted(items)))
            })
            .collect()
    }

    pub fn source_total(&self, source: SourceId) -> u64 {
        self.sources
            .get(&source)
            .map(|v| v.items.iter().filter_map(|i| i.size).sum())
            .unwrap_or(0)
    }

    pub fn selected_total(&self) -> (usize, u64) {
        let items: Vec<&Item> = self
            .review_groups()
            .into_iter()
            .flat_map(|(_, items)| items)
            .collect();
        (items.len(), items.iter().filter_map(|i| i.size).sum())
    }

    /// The user asked to quit while items are being removed.
    pub fn stopping(&self) -> bool {
        self.quit_after_removal && self.screen == Screen::Removing
    }

    /// The removal thread ended without `Finished`: what never completed is
    /// reported as failed instead of spinning forever.
    pub fn removal_aborted(&mut self) {
        for p in self.progress.values_mut() {
            if matches!(p, Progress::Pending | Progress::Running) {
                *p = Progress::Err("removal stopped unexpectedly".into());
            }
        }
        self.screen = Screen::Done;
    }

    pub fn freed_total(&self) -> (usize, u64) {
        self.progress
            .values()
            .fold((0, 0), |(n, total), p| match p {
                Progress::Ok(bytes) => (n + 1, total + bytes),
                _ => (n, total),
            })
    }

    /// Items removed and bytes freed per source, in removal order.
    pub fn freed_by_source(&self) -> Vec<(SourceId, usize, u64)> {
        let mut out: Vec<(SourceId, usize, u64)> = Vec::new();
        for item in &self.removal {
            let Some(Progress::Ok(bytes)) = self.progress.get(&item.id) else {
                continue;
            };
            match out.iter_mut().find(|(s, _, _)| *s == item.source) {
                Some((_, n, total)) => {
                    *n += 1;
                    *total += bytes;
                }
                None => out.push((item.source, 1, *bytes)),
            }
        }
        out
    }

    /// One-line result of the removal, used by the native notification.
    pub fn completion_summary(&self) -> String {
        let (n, bytes) = self.freed_total();
        let noun = if n == 1 { "item" } else { "items" };
        let mut summary = format!("Freed {} in {n} {noun}", format_size_long(bytes));
        let failed = self
            .progress
            .values()
            .filter(|p| matches!(p, Progress::Err(_)))
            .count();
        if failed > 0 {
            summary.push_str(&format!(" · {failed} failed"));
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Removal;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    fn item(
        id: u64,
        source: SourceId,
        label: &str,
        size: Option<u64>,
        safe: bool,
        lock: bool,
    ) -> Item {
        Item {
            id,
            source,
            label: label.into(),
            path: None,
            size,
            status: vec![],
            lock: lock.then(|| "claude · PID 1".to_string()),
            safe,
            removal: Removal::Command {
                argv: vec!["true".into()],
                cwd: None,
            },
            age_days: Some(id as u32),
            recheck: crate::model::Recheck::default(),
        }
    }

    fn app() -> App {
        App::new(
            PathBuf::from("/w"),
            vec![SourceId::Artifacts, SourceId::Worktrees, SourceId::Docker],
        )
    }

    /// App focused on the Worktrees source's items.
    fn with_worktrees(items: Vec<Item>) -> App {
        let mut a = app();
        for it in items {
            a.on_scan(ScanEvent::Found(it));
        }
        a.on_key(key(KeyCode::Down)); // sidebar: Artifacts → Worktrees
        a.on_key(key(KeyCode::Tab)); // focus items
        a
    }

    #[test]
    fn sources_start_scanning_in_order() {
        let a = app();
        assert_eq!(
            a.visible_sources(),
            vec![SourceId::Artifacts, SourceId::Worktrees, SourceId::Docker]
        );
        assert!(matches!(
            a.sources[&SourceId::Docker].state,
            SourceState::Scanning
        ));
    }

    #[test]
    fn found_safe_item_is_preselected() {
        let mut a = app();
        a.on_scan(ScanEvent::Found(item(
            1,
            SourceId::Worktrees,
            "w",
            Some(10),
            true,
            false,
        )));
        assert!(a.selected.contains(&1));
    }

    #[test]
    fn items_found_during_review_are_not_preselected() {
        let mut a = with_worktrees(vec![item(
            1,
            SourceId::Worktrees,
            "w",
            Some(1),
            true,
            false,
        )]);
        a.on_key(key(KeyCode::Enter));
        a.on_scan(ScanEvent::Found(item(
            2,
            SourceId::Docker,
            "late",
            Some(1),
            true,
            false,
        )));
        assert!(!a.selected.contains(&2));
    }

    #[test]
    fn found_locked_item_is_never_selected() {
        let mut a = with_worktrees(vec![item(
            1,
            SourceId::Worktrees,
            "w",
            Some(10),
            true,
            true,
        )]);
        assert!(!a.selected.contains(&1));
        a.on_key(ch(' '));
        assert!(!a.selected.contains(&1));
    }

    #[test]
    fn space_toggles_only_selectable() {
        let mut a = with_worktrees(vec![item(
            1,
            SourceId::Worktrees,
            "w",
            Some(10),
            false,
            false,
        )]);
        a.on_key(ch(' '));
        assert!(a.selected.contains(&1));
        a.on_key(ch(' '));
        assert!(!a.selected.contains(&1));
    }

    #[test]
    fn a_toggles_all_unlocked_in_focused_source() {
        let mut a = with_worktrees(vec![
            item(1, SourceId::Worktrees, "a", Some(1), false, false),
            item(2, SourceId::Worktrees, "b", Some(1), false, true),
            item(3, SourceId::Worktrees, "c", Some(1), false, false),
        ]);
        a.on_scan(ScanEvent::Found(item(
            4,
            SourceId::Docker,
            "d",
            Some(1),
            false,
            false,
        )));
        a.on_key(ch('a'));
        assert_eq!(a.selected, [1, 3].into_iter().collect());
        a.on_key(ch('a'));
        assert!(a.selected.is_empty());
    }

    #[test]
    fn size_event_updates_item_and_totals() {
        let mut a = app();
        a.on_scan(ScanEvent::Found(item(
            1,
            SourceId::Docker,
            "d",
            None,
            true,
            false,
        )));
        a.on_scan(ScanEvent::Size(1, 500));
        assert_eq!(a.source_total(SourceId::Docker), 500);
        assert_eq!(a.selected_total(), (1, 500));
    }

    #[test]
    fn sort_cycles_size_name_age() {
        let mut a = with_worktrees(vec![
            item(1, SourceId::Worktrees, "b", Some(5), false, false),
            item(2, SourceId::Worktrees, "a", Some(9), false, false),
            item(3, SourceId::Worktrees, "c", None, false, false),
        ]);
        let labels = |a: &App| {
            a.visible_items()
                .iter()
                .map(|i| i.label.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&a), ["a", "b", "c"]); // size desc, unknown last
        a.on_key(ch('s'));
        assert_eq!(a.sort, SortBy::Name);
        assert_eq!(labels(&a), ["a", "b", "c"]);
        a.on_key(ch('s'));
        assert_eq!(a.sort, SortBy::Age);
        assert_eq!(labels(&a), ["c", "a", "b"]); // age desc (age_days = id)
        a.on_key(ch('s'));
        assert_eq!(a.sort, SortBy::Size);
    }

    #[test]
    fn filter_matches_label_case_insensitive() {
        let mut a = with_worktrees(vec![
            item(1, SourceId::Worktrees, "codex/Glowz", Some(1), false, false),
            item(2, SourceId::Worktrees, "orca/site", Some(1), false, false),
        ]);
        a.on_key(ch('/'));
        for c in "glo".chars() {
            a.on_key(ch(c));
        }
        assert_eq!(a.visible_items().len(), 1);
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.filter.as_deref(), Some("glo"));
        assert_eq!(a.screen, Screen::List);
        a.on_key(ch('/'));
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.filter, None);
        assert_eq!(a.visible_items().len(), 2);
    }

    #[test]
    fn typing_in_filter_does_not_trigger_shortcuts() {
        let mut a = with_worktrees(vec![item(
            1,
            SourceId::Worktrees,
            "w",
            Some(1),
            false,
            false,
        )]);
        a.on_key(ch('/'));
        assert_eq!(a.on_key(ch('q')), Action::None);
        a.on_key(ch('a'));
        assert!(a.selected.is_empty());
    }

    #[test]
    fn enter_with_nothing_selected_stays_on_list() {
        let mut a = with_worktrees(vec![item(
            1,
            SourceId::Worktrees,
            "w",
            Some(1),
            false,
            false,
        )]);
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.screen, Screen::List);
    }

    #[test]
    fn review_then_y_emits_start_removal_with_selected_items() {
        let mut a = with_worktrees(vec![
            item(1, SourceId::Worktrees, "w", Some(1), true, false),
            item(2, SourceId::Worktrees, "x", Some(1), false, false),
        ]);
        a.on_scan(ScanEvent::Found(item(
            3,
            SourceId::Docker,
            "d",
            Some(1),
            true,
            false,
        )));
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.screen, Screen::Review);
        match a.on_key(ch('y')) {
            Action::StartRemoval(items) => {
                assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1, 3])
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(a.screen, Screen::Removing);
    }

    #[test]
    fn arrows_scroll_the_review_instead_of_leaving() {
        let mut a = with_worktrees(vec![item(
            1,
            SourceId::Worktrees,
            "w",
            Some(1),
            true,
            false,
        )]);
        a.on_key(key(KeyCode::Enter));
        a.on_key(key(KeyCode::Down));
        a.on_key(ch('j'));
        assert_eq!(a.screen, Screen::Review);
        assert_eq!(a.review_scroll, 2);
        a.on_key(key(KeyCode::Up));
        assert_eq!(a.review_scroll, 1);
    }

    #[test]
    fn review_other_key_returns_to_list() {
        let mut a = with_worktrees(vec![item(
            1,
            SourceId::Worktrees,
            "w",
            Some(1),
            true,
            false,
        )]);
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.on_key(ch('n')), Action::None);
        assert_eq!(a.screen, Screen::List);
    }

    fn removing() -> App {
        let mut a = with_worktrees(vec![
            item(1, SourceId::Worktrees, "w", Some(100), true, false),
            item(2, SourceId::Worktrees, "x", Some(50), true, false),
        ]);
        a.on_key(key(KeyCode::Enter));
        a.on_key(ch('y'));
        a
    }

    #[test]
    fn remove_events_lead_to_done_with_freed_total() {
        let mut a = removing();
        a.on_remove(RemoveEvent::Started(1));
        a.on_remove(RemoveEvent::Ok(1, 100));
        a.on_remove(RemoveEvent::Started(2));
        a.on_remove(RemoveEvent::Err(2, "boom".into()));
        a.on_remove(RemoveEvent::Finished);
        assert_eq!(a.screen, Screen::Done);
        assert_eq!(a.freed_total(), (1, 100));
        assert!(matches!(a.progress.get(&2), Some(Progress::Err(m)) if m == "boom"));
    }

    #[test]
    fn completion_summary_counts_freed_and_failed() {
        let mut a = removing();
        a.on_remove(RemoveEvent::Ok(1, 1_900_000_000));
        a.on_remove(RemoveEvent::Finished);
        assert_eq!(a.completion_summary(), "Freed 1.9 GB in 1 item");
        a.on_remove(RemoveEvent::Err(2, "boom".into()));
        assert_eq!(a.completion_summary(), "Freed 1.9 GB in 1 item · 1 failed");
    }

    #[test]
    fn o_opens_disk_access_settings_only_for_a_source_blocked_by_permissions() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Trash, SourceId::Docker]);
        a.on_scan(ScanEvent::Failed(
            SourceId::Trash,
            "permission denied reading ~/.Trash".into(),
        ));
        assert!(a.needs_disk_access(SourceId::Trash));
        assert!(!a.needs_disk_access(SourceId::Docker));
        // Sources follow SourceId order: Docker first, then Trash.
        assert_eq!(a.on_key(ch('o')), Action::None, "Docker is not blocked");
        assert!(!a.opened_settings);
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.on_key(ch('o')), Action::OpenDiskAccessSettings);
        assert!(a.opened_settings);
    }

    #[test]
    fn terminal_name_comes_from_term_program() {
        assert_eq!(terminal_name(Some("ghostty")), "Ghostty");
        assert_eq!(terminal_name(Some("Apple_Terminal")), "Terminal");
        assert_eq!(terminal_name(None), "your terminal");
    }

    #[test]
    fn a_permission_note_also_counts_as_blocked() {
        let mut a = App::new(PathBuf::from("/w"), vec![SourceId::Artifacts]);
        a.on_scan(ScanEvent::Note(
            SourceId::Artifacts,
            "3 folders skipped (no permission)".into(),
        ));
        assert!(a.needs_disk_access(SourceId::Artifacts));
    }

    #[test]
    fn r_on_list_rescans() {
        let mut a = app();
        assert_eq!(a.on_key(ch('r')), Action::Rescan);
    }

    #[test]
    fn question_mark_opens_help_and_the_next_key_only_closes_it() {
        let mut a = app();
        a.on_key(ch('?'));
        assert!(a.show_help);
        assert_eq!(a.on_key(ch('q')), Action::None);
        assert!(!a.show_help);
        assert_eq!(a.on_key(ch('q')), Action::Quit);
    }

    #[test]
    fn freed_by_source_sums_successes_in_review_order() {
        let mut a = removing();
        a.on_remove(RemoveEvent::Ok(1, 100));
        a.on_remove(RemoveEvent::Err(2, "boom".into()));
        a.on_remove(RemoveEvent::Finished);
        assert_eq!(a.freed_by_source(), vec![(SourceId::Worktrees, 1, 100)]);
    }

    #[test]
    fn rescan_forgets_disk_free_readings() {
        let mut a = removing();
        a.disk_free = (Some(10), Some(20));
        a.on_remove(RemoveEvent::Finished);
        a.on_key(ch('r'));
        assert_eq!(a.disk_free, (None, None));
    }

    #[test]
    fn r_on_done_emits_rescan_and_resets_state() {
        let mut a = removing();
        a.on_remove(RemoveEvent::Finished);
        assert_eq!(a.on_key(ch('r')), Action::Rescan);
        assert_eq!(a.screen, Screen::List);
        assert!(a.selected.is_empty() && a.progress.is_empty());
        assert!(
            a.sources
                .values()
                .all(|s| s.items.is_empty() && matches!(s.state, SourceState::Scanning))
        );
    }

    #[test]
    fn q_quits_from_list_and_done_but_not_while_removing() {
        let mut a = app();
        assert_eq!(a.on_key(ch('q')), Action::Quit);
        let mut a = removing();
        assert_eq!(a.on_key(ch('q')), Action::StopRemoval);
        assert!(a.stopping());
        assert_eq!(a.on_remove(RemoveEvent::Finished), Action::Quit);
        let mut a = removing();
        a.on_remove(RemoveEvent::Finished);
        assert_eq!(a.on_key(ch('q')), Action::Quit);
    }

    #[test]
    fn second_ctrl_c_while_removing_quits_at_once() {
        let mut a = removing();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(a.on_key(ctrl_c), Action::StopRemoval);
        assert_eq!(a.on_key(ctrl_c), Action::Quit);
    }

    #[test]
    fn a_removal_that_stops_without_finishing_reports_what_was_left() {
        let mut a = removing();
        a.on_remove(RemoveEvent::Started(1));
        a.on_remove(RemoveEvent::Ok(1, 100));
        a.on_remove(RemoveEvent::Started(2));
        a.removal_aborted();
        assert_eq!(a.screen, Screen::Done);
        assert_eq!(
            a.progress.get(&2),
            Some(&Progress::Err("removal stopped unexpectedly".into()))
        );
    }

    #[test]
    fn ctrl_c_quits() {
        let mut a = app();
        assert_eq!(
            a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn empty_source_stays_visible_after_done() {
        let mut a = app();
        a.on_scan(ScanEvent::Done(SourceId::Docker));
        assert!(a.visible_sources().contains(&SourceId::Docker));
        assert_eq!(a.source_total(SourceId::Docker), 0);
    }

    #[test]
    fn failed_and_note_are_recorded() {
        let mut a = app();
        a.on_scan(ScanEvent::Note(SourceId::Docker, "daemon stopped".into()));
        a.on_scan(ScanEvent::Failed(SourceId::Artifacts, "boom".into()));
        a.on_scan(ScanEvent::Done(SourceId::Artifacts));
        assert_eq!(
            a.sources[&SourceId::Docker].notes,
            vec!["daemon stopped".to_string()]
        );
        assert!(
            matches!(&a.sources[&SourceId::Artifacts].state, SourceState::Failed(m) if m == "boom")
        );
    }

    #[test]
    fn cursor_is_clamped_when_items_shrink() {
        let mut a = with_worktrees(vec![
            item(1, SourceId::Worktrees, "a", Some(3), false, false),
            item(2, SourceId::Worktrees, "b", Some(2), false, false),
            item(3, SourceId::Worktrees, "c", Some(1), false, false),
        ]);
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Down));
        assert_eq!(a.cursor_item, 2);
        a.on_key(ch('/'));
        a.on_key(ch('a'));
        assert_eq!(a.cursor_item, 0);
    }
}
