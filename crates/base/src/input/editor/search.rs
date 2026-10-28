use crate::input::InputModeKind;
use gpui::{Context, Window};
use ropey::Rope;
use std::{ops::Range, rc::Rc, time::Duration};
use sum_tree::Bias;

pub use super::search_query::{
    REGEX_REACH, SearchOptions, SearchQuery, expand_replacement, find_in_str,
};
use super::{
    InputBaseState, Replace, RopeExt as _, Search, movement::MoveDirection, state::ScrollPadding,
};

/// Documents larger than this (bytes) are searched off the UI thread: the
/// matches on screen come first, the rest arrive in batches after a short
/// debounce, and the count reads `≥N` until the scan is done. Smaller ones
/// are searched in place, as before.
pub const ASYNC_SEARCH_BYTES: usize = 1 << 20;
/// Typing pauses this long before a big document is scanned.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);
/// Bytes scanned per background batch.
const SEARCH_BATCH_BYTES: usize = 4 << 20;
/// At most this much around the viewport is scanned at once, on the UI
/// thread (a 10 MB single line is all "visible").
const QUICK_SCAN_BYTES: usize = 256 << 10;

/// Stateful, presentation-independent search engine used by text inputs.
#[derive(Debug, Clone)]
pub struct SearchMatcher {
    text: Rope,
    pub query: Option<SearchQuery>,
    /// Why the query has no matcher: an invalid regular expression.
    error: Option<String>,
    matched_ranges: Rc<Vec<Range<usize>>>,
    current_match_ix: usize,
    replacing: bool,
    /// A background scan is running; `matched_ranges` holds what it found
    /// so far plus the on-screen matches beyond it.
    pending: bool,
    /// Bumped by every new scan; batches of an older one are dropped.
    generation: u64,
    /// Matches of the running scan, in order, and how far it has got.
    scanned: Vec<Range<usize>>,
    scanned_to: usize,
    /// On-screen matches found before the scan reaches them.
    quick: Vec<Range<usize>>,
}

/// Matches of `query` that start in `from..to` (clipped to char
/// boundaries), and where the next batch should start: at `to`, or past a
/// match that runs over it, so batches together find what one scan would.
pub fn find_in_range(
    text: &Rope,
    query: &SearchQuery,
    from: usize,
    to: usize,
) -> (Vec<Range<usize>>, usize) {
    let len = text.len();
    let from = text.clip_offset(from.min(len), Bias::Left);
    let to = text.clip_offset(to.min(len), Bias::Right).max(from);
    // The slice around the batch: a little before (for `^`, `\b`, whole
    // words) and as far as the longest match after.
    let start = text.clip_offset(from.saturating_sub(16), Bias::Left);
    let end = text.clip_offset((to + query.max_match_len() + 4).min(len), Bias::Right);
    let haystack = text.slice(start..end).to_string();
    let (found, next) = find_in_str(&haystack, query, from - start, to - start);
    (
        found
            .into_iter()
            .map(|r| start + r.start..start + r.end)
            .collect(),
        start + next,
    )
}

/// One search over an input: the query, how the built-in panel shows it, and
/// its matches. Read it through [`InputBaseState::search_session`]; it is
/// written only through the input state's search methods, and it grows, so
/// build it with `Default` and do not destructure it exhaustively.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SearchSession {
    /// The built-in search panel is showing.
    pub open: bool,
    pub replace_mode: bool,
    pub case_insensitive: bool,
    /// ab: whole words only.
    pub whole_word: bool,
    /// .*: the query is a regular expression.
    pub regex: bool,
    pub query: String,
    pub replacement: String,
    pub anchor_offset: Option<usize>,
    pub matcher: SearchMatcher,
    /// A search is in progress and its matches are highlighted: the panel is
    /// open, or a query was set without it and not closed since.
    active: bool,
}

impl Default for SearchSession {
    fn default() -> Self {
        Self {
            open: false,
            active: false,
            replace_mode: false,
            case_insensitive: true,
            whole_word: false,
            regex: false,
            query: String::new(),
            replacement: String::new(),
            anchor_offset: None,
            matcher: SearchMatcher::new(),
        }
    }
}

impl SearchSession {
    pub(crate) fn open(&mut self, replace_mode: bool, replaceable: bool) {
        self.open = true;
        self.active = true;
        self.replace_mode = replace_mode && replaceable;
    }

    /// Start a search without the built-in panel. A custom search UI drives
    /// the session through [`InputBaseState::set_search_query`], and the
    /// editor highlights the matches the same way it does for the panel.
    pub(crate) fn activate(&mut self) {
        self.active = true;
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.active = false;
    }

    /// Whether a search is in progress: the built-in panel is open, or a
    /// query was set without it and [`InputBaseState::close_search`] has not
    /// run since. Matches are highlighted while this holds.
    pub fn is_active(&self) -> bool {
        self.active
    }

    #[cfg(test)]
    pub(crate) fn update_query(&mut self, query: impl Into<String>, case_insensitive: bool) {
        let options = SearchOptions {
            case_sensitive: !case_insensitive,
            ..self.options()
        };
        self.update_query_with(query, options);
    }

    /// The toggles as one value.
    pub fn options(&self) -> SearchOptions {
        SearchOptions {
            case_sensitive: !self.case_insensitive,
            whole_word: self.whole_word,
            regex: self.regex,
        }
    }

    pub(crate) fn update_query_with(&mut self, query: impl Into<String>, options: SearchOptions) {
        let query = query.into();
        if self.query == query && self.options() == options {
            return;
        }

        self.query = query;
        self.case_insensitive = !options.case_sensitive;
        self.whole_word = options.whole_word;
        self.regex = options.regex;
        self.matcher.update_query_with(&self.query, options);
    }
}

impl<M: InputModeKind> InputBaseState<M> {
    /// Open the search session, or re-invoke it if it is already open.
    ///
    /// This is not idempotent: every call advances
    /// [`InputBaseState::search_activation_revision`], and the presentation
    /// layer answers that by re-focusing the search field and selecting its
    /// contents, the same as pressing the shortcut a second time. Call it from
    /// an action or another user gesture, never from a render pass or an
    /// observer that runs every frame — that would re-select the field under
    /// the user on every frame and make it impossible to type.
    pub fn open_search(&mut self, replace_mode: bool, cx: &mut Context<Self>) {
        if !self.searchable {
            return;
        }
        self.search_activation_revision = self.search_activation_revision.wrapping_add(1);
        self.search_session
            .open(replace_mode, self.is_replaceable());
        let selected = self.selected_text().to_string();
        let query = if selected.is_empty() {
            self.search_session.query.clone()
        } else {
            selected
        };
        let query_changed = query != self.search_session.query;
        // A retained query resumes its previous occurrence. Only a new query
        // is anchored to the current viewport.
        self.search_session.anchor_offset = if query_changed {
            self.last_layout
                .as_ref()
                .map(|layout| layout.visible_range_offset.start)
        } else {
            None
        };
        let options = self.search_session.options();
        self.search_session.update_query_with(query, options);
        if SearchMatcher::is_async_for(&self.text) {
            if query_changed || self.search_session.matcher.text.len() != self.text.len() {
                let anchor = self.search_session.anchor_offset;
                self.schedule_search(anchor, cx);
            }
            cx.notify();
            return;
        }
        self.search_session.matcher.update(&self.text);
        if query_changed && let Some(anchor) = self.search_session.anchor_offset {
            self.search_session.matcher.update_cursor_by_offset(anchor);
        }
        cx.notify();
    }

    pub fn search_session(&self) -> &SearchSession {
        &self.search_session
    }

    /// A counter that advances every time [`InputBaseState::open_search`] runs,
    /// including while the session is already open.
    ///
    /// Re-invoking search leaves the session itself identical, so a presentation
    /// layer that decides what to rebuild by comparing session state cannot see
    /// the second request. Fold this into that comparison to notice it.
    pub fn search_activation_revision(&self) -> u64 {
        self.search_activation_revision
    }

    #[doc(hidden)]
    pub fn set_search_replace_mode(&mut self, replace_mode: bool, cx: &mut Context<Self>) {
        self.search_session.replace_mode = replace_mode && self.is_replaceable();
        cx.notify();
    }

    /// Returns true if the search panel can replace the matches.
    ///
    /// This is false when the input is not `replaceable`, or when it is
    /// `disabled` or `readonly`.
    pub fn is_replaceable(&self) -> bool {
        self.replaceable && self.is_editable()
    }

    /// Set the search query and highlight its matches.
    ///
    /// This is the entry point for a custom search UI: it needs neither
    /// `searchable` nor the built-in panel. Navigate the matches with
    /// [`InputBaseState::next_search_match`] and
    /// [`InputBaseState::previous_search_match`], read the count and the
    /// current index from [`InputBaseState::search_session`], and end the
    /// search with [`InputBaseState::close_search`].
    pub fn set_search_query(
        &mut self,
        query: impl Into<String>,
        case_insensitive: bool,
        cx: &mut Context<Self>,
    ) {
        let options = SearchOptions {
            case_sensitive: !case_insensitive,
            ..self.search_session.options()
        };
        self.set_search_query_with(query, options, cx);
    }

    /// [`InputBaseState::set_search_query`] with every toggle: match case,
    /// whole word and regular expression. An invalid regular expression
    /// finds nothing; [`SearchMatcher::error`] says why.
    pub fn set_search_query_with(
        &mut self,
        query: impl Into<String>,
        options: SearchOptions,
        cx: &mut Context<Self>,
    ) {
        self.search_session.activate();
        let query = query.into();
        let changed =
            query != self.search_session.query || options != self.search_session.options();
        self.search_session.update_query_with(query, options);
        if SearchMatcher::is_async_for(&self.text) {
            if changed || self.search_session.matcher.text.len() != self.text.len() {
                let anchor = self
                    .last_layout
                    .as_ref()
                    .map(|layout| layout.visible_range_offset.start);
                self.schedule_search(anchor, cx);
            }
            cx.notify();
            return;
        }
        self.search_session.matcher.update(&self.text);
        cx.notify();
    }

    /// Search a big document without blocking: the matches around the
    /// viewport are found now, the whole document in background batches
    /// after [`SEARCH_DEBOUNCE`]. A newer call (the next keystroke, an
    /// edit) drops this one's task and ignores its late batches.
    fn schedule_search(&mut self, anchor: Option<usize>, cx: &mut Context<Self>) {
        let text = self.text.clone();
        let generation = self.search_session.matcher.begin_async(&text);
        let Some(query) = self.search_session.matcher.query.clone() else {
            self.search_task = None;
            return;
        };
        let visible = self
            .last_layout
            .as_ref()
            .map(|layout| layout.visible_range_offset.clone())
            .unwrap_or(0..0);
        let quick_from = anchor.unwrap_or(visible.start).min(visible.start);
        let quick_to = visible
            .end
            .max(quick_from)
            .min(quick_from + QUICK_SCAN_BYTES);
        let (quick, _) = find_in_range(&text, &query, quick_from, quick_to);
        self.search_session.matcher.set_quick(quick, anchor);
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let len = text.len();
            let mut from = 0;
            loop {
                let to = (from + SEARCH_BATCH_BYTES).min(len);
                let (batch_text, batch_query) = (text.clone(), query.clone());
                let (found, next) = cx
                    .background_executor()
                    .spawn(async move { find_in_range(&batch_text, &batch_query, from, to) })
                    .await;
                let done = next >= len;
                let applied = this
                    .update(cx, |state, cx| {
                        let applied = state
                            .search_session
                            .matcher
                            .apply_batch(generation, found, next, done);
                        if applied {
                            cx.notify();
                        }
                        applied
                    })
                    .unwrap_or(false);
                if !applied || done {
                    break;
                }
                from = next;
            }
        }));
    }

    /// End the search: hide the built-in panel and the match highlights. The
    /// query is kept so the next [`InputBaseState::open_search`] resumes it.
    pub fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search_session.close();
        cx.notify();
    }

    pub fn next_search_match(&mut self, cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.sync_search_matcher();
        let range = self.search_session.matcher.next()?;
        self.select_search_match(&range, cx);
        // Match order does not describe viewport direction after a manual
        // scroll. Always allow search navigation to reveal the active match.
        self.scroll_to_with_padding(range.end, None, ScrollPadding::SurroundingLines, cx);
        Some(range)
    }

    pub fn previous_search_match(&mut self, cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.sync_search_matcher();
        let range = self.search_session.matcher.next_back()?;
        self.select_search_match(&range, cx);
        // Match order does not describe viewport direction after a manual
        // scroll. Always allow search navigation to reveal the active match.
        self.scroll_to_with_padding(range.start, None, ScrollPadding::SurroundingLines, cx);
        Some(range)
    }

    /// Find as you type (VS Code): select the first match at or after the
    /// selection's start and scroll it into view, so the editor follows the
    /// query while it is typed. Returns the match, if any.
    pub fn reveal_search_match_from_selection(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.sync_search_matcher();
        let from = self.selected_range().start;
        let matcher = &mut self.search_session.matcher;
        if matcher.is_empty() {
            return None;
        }
        let ix = matcher.matched_ranges().partition_point(|r| r.start < from);
        if ix >= matcher.len() && matcher.is_pending() {
            // Past what a big document's scan has found so far.
            return None;
        }
        matcher.set_current_match_index(if ix >= matcher.len() { 0 } else { ix });
        let range = matcher.matched_ranges()[matcher.current_match_index()].clone();
        self.select_search_match(&range, cx);
        self.scroll_to_with_padding(range.end, None, ScrollPadding::SurroundingLines, cx);
        Some(range)
    }

    /// Navigating matches selects the current one (as VS Code does): the
    /// caret and status follow it, and closing Find leaves it selected.
    fn select_search_match(&mut self, range: &Range<usize>, cx: &mut Context<Self>) {
        self.set_selected_range(range.clone(), cx);
    }

    /// Replace the current match and move on to the next one. Returns whether
    /// there was a match to replace.
    pub fn replace_current_search_match(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_replaceable() {
            return false;
        }
        self.sync_search_matcher();
        let matcher = &mut self.search_session.matcher;
        let Some(range) = matcher
            .matched_ranges()
            .get(matcher.current_match_index())
            .cloned()
        else {
            return false;
        };
        let next = matcher.peek().unwrap_or_else(|| range.clone());
        let direction = matcher
            .has_next_without_wrap()
            .then_some(MoveDirection::Down);
        if direction.is_none() {
            matcher.set_current_match_index(0);
        }
        let replacement = match matcher.query.as_ref() {
            Some(query) if query.options().regex => {
                let start = self
                    .text
                    .clip_offset(range.start.saturating_sub(16), Bias::Left);
                let end = self
                    .text
                    .clip_offset((range.end + REGEX_REACH).min(self.text.len()), Bias::Right);
                let haystack = self.text.slice(start..end).to_string();
                query.replacement(
                    &haystack,
                    range.start - start..range.end - start,
                    replacement,
                )
            }
            _ => replacement.to_string(),
        };
        let matcher = &mut self.search_session.matcher;
        matcher.begin_replacement();
        let range_utf16 = self.range_to_utf16(&range);
        self.scroll_to(next.end, direction, cx);
        self.replace_text_in_range_silent(Some(range_utf16), &replacement, window, cx);
        true
    }

    /// Replace every match. Returns how many were replaced.
    pub fn replace_all_search_matches(
        &mut self,
        replacement: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if !self.is_replaceable() {
            return 0;
        }
        self.sync_search_matcher();
        if self.search_session.matcher.is_pending() {
            // Replace All needs every match: finish the scan now.
            let generation = self.search_session.matcher.begin_async(&self.text);
            if let Some(query) = self.search_session.matcher.query.clone() {
                let (found, _) = find_in_range(&self.text, &query, 0, self.text.len());
                self.search_session
                    .matcher
                    .apply_batch(generation, found, self.text.len(), true);
            }
            self.search_task = None;
        }
        let ranges = self.search_session.matcher.matched_ranges();
        if ranges.is_empty() {
            return 0;
        }
        let mut text = self.text.clone();
        let query = self.search_session.matcher.query.clone();
        let whole = query
            .as_ref()
            .filter(|query| query.options().regex)
            .map(|_| self.text.to_string());
        for range in ranges.iter().rev() {
            match (&query, &whole) {
                (Some(query), Some(whole)) => {
                    let expanded = query.replacement(whole, range.clone(), replacement);
                    text.replace(range.clone(), &expanded);
                }
                _ => text.replace(range.clone(), replacement),
            }
        }
        self.search_session.matcher.begin_replacement();
        let count = ranges.len();
        self.replace_text_in_range_silent(Some(0..self.text.len()), &text.to_string(), window, cx);
        self.scroll_to(0, Some(MoveDirection::Down), cx);
        count
    }

    /// Keep the matches in step with an edit. A closed search skips the scan:
    /// it copies and searches the whole document, and nothing reads the
    /// matches until the search is resumed or navigated, which sync first.
    pub(super) fn update_search(&mut self, cx: &mut Context<Self>) {
        if !self.search_session.is_active() {
            return;
        }
        if SearchMatcher::is_async_for(&self.text) {
            // The text changed: rescan in the background, keeping the
            // current match near where it was.
            let keep = self
                .search_session
                .matcher
                .matched_ranges()
                .get(self.search_session.matcher.current_match_index())
                .map(|range| range.start);
            self.schedule_search(keep, cx);
            return;
        }
        self.sync_search_matcher();
    }

    /// Recompute the matches if the text changed since the last scan. A big
    /// document is rescanned by [`Self::update_search`] after each edit;
    /// comparing it here would read the whole text on every navigation.
    fn sync_search_matcher(&mut self) {
        if SearchMatcher::is_async_for(&self.text) {
            return;
        }
        self.search_session.matcher.update(&self.text);
    }

    /// An input that is not `searchable` leaves the shortcut to its
    /// ancestors, so a custom search UI can take it.
    pub(super) fn on_action_search(&mut self, _: &Search, _: &mut Window, cx: &mut Context<Self>) {
        if !self.searchable {
            cx.propagate();
            return;
        }
        self.open_search(false, cx);
    }

    pub(super) fn on_action_replace(
        &mut self,
        _: &Replace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.searchable {
            cx.propagate();
            return;
        }
        self.open_search(true, cx);
    }
}

impl Default for SearchMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchMatcher {
    pub fn new() -> Self {
        Self {
            text: "".into(),
            query: None,
            error: None,
            matched_ranges: Rc::new(Vec::new()),
            current_match_ix: 0,
            replacing: false,
            pending: false,
            generation: 0,
            scanned: Vec::new(),
            scanned_to: 0,
            quick: Vec::new(),
        }
    }

    /// Whether `text` is searched in the background (see
    /// [`ASYNC_SEARCH_BYTES`]).
    pub fn is_async_for(text: &Rope) -> bool {
        text.len() > ASYNC_SEARCH_BYTES
    }

    /// A background scan is still running: the count is a lower bound.
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// Start a background scan of `text`: drops the old matches (their
    /// offsets are for another text) and returns the scan's generation.
    fn begin_async(&mut self, text: &Rope) -> u64 {
        self.text = text.clone();
        self.generation = self.generation.wrapping_add(1);
        self.pending = self.query.is_some();
        self.scanned.clear();
        self.scanned_to = 0;
        self.quick.clear();
        self.matched_ranges = Rc::new(Vec::new());
        self.current_match_ix = 0;
        self.replacing = false;
        self.generation
    }

    /// The on-screen matches, shown before the scan gets there; the current
    /// match is the first at or after `anchor`.
    fn set_quick(&mut self, quick: Vec<Range<usize>>, anchor: Option<usize>) {
        self.quick = quick;
        self.publish(anchor.or_else(|| self.quick.first().map(|r| r.start)));
    }

    /// Add a batch of scan `generation` that reached `scanned_to`. Returns
    /// false when the batch is stale (a newer scan started).
    fn apply_batch(
        &mut self,
        generation: u64,
        mut batch: Vec<Range<usize>>,
        scanned_to: usize,
        done: bool,
    ) -> bool {
        if generation != self.generation || !self.pending {
            return false;
        }
        let keep = self
            .matched_ranges
            .get(self.current_match_ix)
            .map(|r| r.start);
        self.scanned.append(&mut batch);
        self.scanned_to = scanned_to;
        if done {
            self.pending = false;
            self.quick.clear();
        }
        self.publish(keep);
        true
    }

    /// matched_ranges = what the scan found + the on-screen matches past
    /// it; the current match stays on the match at or after `keep`.
    fn publish(&mut self, keep: Option<usize>) {
        let mut ranges = self.scanned.clone();
        let scanned_to = self.scanned_to;
        ranges.extend(self.quick.iter().filter(|r| r.start >= scanned_to).cloned());
        self.matched_ranges = Rc::new(ranges);
        self.current_match_ix = match keep {
            Some(offset) => self
                .matched_ranges
                .partition_point(|r| r.start < offset)
                .min(self.len().saturating_sub(1)),
            None => 0,
        };
    }

    /// Update the source text and recompute matches.
    pub fn update(&mut self, text: &Rope) {
        if self.text.eq(text) {
            self.replacing = false;
            return;
        }
        self.text = text.clone();
        self.update_matches();
    }

    pub fn update_query(&mut self, query: &str, case_insensitive: bool) {
        self.update_query_with(
            query,
            SearchOptions {
                case_sensitive: !case_insensitive,
                ..Default::default()
            },
        );
    }

    /// Why the query finds nothing: the regular expression does not parse.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn update_query_with(&mut self, query: &str, options: SearchOptions) {
        (self.query, self.error) = match SearchQuery::new(query, options) {
            Ok(query) => (query, None),
            Err(err) => (None, Some(err)),
        };
        if Self::is_async_for(&self.text) {
            // The owner schedules the background scan; scanning here would
            // stall the UI thread for the whole document.
            self.begin_async(&self.text.clone());
            return;
        }
        self.update_matches();
    }

    pub fn matched_ranges(&self) -> Rc<Vec<Range<usize>>> {
        self.matched_ranges.clone()
    }

    pub fn current_match_index(&self) -> usize {
        self.current_match_ix
    }

    /// The index of the current match into [`SearchMatcher::matched_ranges`],
    /// `None` while there is no match.
    pub fn current(&self) -> Option<usize> {
        (!self.is_empty()).then_some(self.current_match_ix)
    }

    pub fn len(&self) -> usize {
        self.matched_ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matched_ranges.is_empty()
    }

    /// `2/5`: the current match and the total, `0/0` without matches;
    /// `2/≥5` while a background scan is still counting.
    pub fn label(&self) -> String {
        let more = if self.pending { "≥" } else { "" };
        match self.current() {
            Some(ix) => format!("{}/{more}{}", ix + 1, self.len()),
            None if self.pending => "0/…".into(),
            None => "0/0".into(),
        }
    }

    fn peek(&self) -> Option<Range<usize>> {
        self.next_index()
            .and_then(|ix| self.matched_ranges.get(ix).cloned())
    }

    fn has_next_without_wrap(&self) -> bool {
        self.current_match_ix < self.matched_ranges.len().saturating_sub(1)
    }

    pub fn update_cursor_by_offset(&mut self, offset: usize) {
        if self.is_empty() {
            return;
        }
        // The first match that contains or follows `offset`, else the last.
        let ix = self.matched_ranges.partition_point(|r| r.end < offset);
        self.current_match_ix = ix.min(self.len() - 1);
    }

    /// Preserve the current logical match while a replacement mutates text.
    fn begin_replacement(&mut self) {
        self.replacing = true;
    }

    fn set_current_match_index(&mut self, index: usize) {
        self.current_match_ix = index.min(self.matched_ranges.len().saturating_sub(1));
    }

    fn next_index(&self) -> Option<usize> {
        if self.is_empty() {
            None
        } else if self.has_next_without_wrap() {
            Some(self.current_match_ix + 1)
        } else {
            Some(0)
        }
    }

    fn update_matches(&mut self) {
        let mut ranges = Vec::new();
        if let Some(query) = &self.query {
            let text = self.text.to_string();
            ranges.extend(query.find_iter(&text));
        }
        self.matched_ranges = Rc::new(ranges);
        if !self.replacing || self.is_empty() {
            self.current_match_ix = 0;
        } else {
            self.current_match_ix = self.current_match_ix.min(self.len() - 1);
        }
        self.replacing = false;
    }
}

impl Iterator for SearchMatcher {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let ix = self.next_index()?;
        self.current_match_ix = ix;
        self.matched_ranges.get(ix).cloned()
    }
}

impl DoubleEndedIterator for SearchMatcher {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.is_empty() {
            return None;
        }
        if self.current_match_ix == 0 {
            self.current_match_ix = self.len();
        }
        self.current_match_ix -= 1;
        self.matched_ranges.get(self.current_match_ix).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(q: &str, ci: bool) -> SearchQuery {
        SearchQuery::new(
            q,
            SearchOptions {
                case_sensitive: !ci,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn batches_find_what_one_scan_finds() {
        let text = "aa中aaa🙂aXa aaaa\naa中".repeat(50);
        let rope = Rope::from(text.as_str());
        for q in ["aa", "a", "中a", "🙂", "aaaa", "x"] {
            let query = query(q, true);
            let whole: Vec<_> = query.find_iter(&text).collect();
            for batch in [1, 2, 3, 5, 7, 64, 10_000] {
                let mut found = Vec::new();
                let mut from = 0;
                while from < rope.len() {
                    let (mut part, next) = find_in_range(&rope, &query, from, from + batch);
                    assert!(next > from, "progress at {from} (batch {batch}, {q})");
                    found.append(&mut part);
                    from = next;
                }
                assert_eq!(found, whole, "query {q:?}, batch {batch}");
            }
        }
    }

    #[test]
    fn background_batches_merge_with_the_on_screen_matches() {
        let mut matcher = SearchMatcher::new();
        let rope = Rope::from("foo ".repeat(10).as_str());
        matcher.query = Some(query("foo", false));
        let generation = matcher.begin_async(&rope);
        assert!(matcher.is_pending());
        assert_eq!(matcher.label(), "0/…");
        // On screen: the matches at 20 and 24; the current one at 24.
        matcher.set_quick(vec![20..23, 24..27], Some(24));
        assert_eq!(matcher.label(), "2/≥2");
        // The scan's first batch (up to 12) comes in: the current match stays.
        assert!(matcher.apply_batch(generation, vec![0..3, 4..7, 8..11], 12, false));
        assert_eq!(
            matcher.matched_ranges()[matcher.current_match_index()],
            24..27
        );
        assert_eq!(matcher.label(), "5/≥5");
        // A batch of an older scan is ignored.
        assert!(!matcher.apply_batch(generation - 1, vec![12..15], 16, false));
        let (rest, next) = find_in_range(&rope, matcher.query.as_ref().unwrap(), 12, rope.len());
        assert!(matcher.apply_batch(generation, rest, next, true));
        assert!(!matcher.is_pending());
        assert_eq!(matcher.len(), 10);
        assert_eq!(matcher.label(), "7/10");
    }

    #[test]
    fn a_big_document_does_not_scan_on_the_query_change() {
        let mut matcher = SearchMatcher::new();
        let big = Rope::from("x".repeat(ASYNC_SEARCH_BYTES + 1).as_str());
        matcher.update(&big);
        matcher.update_query("x", false);
        assert!(matcher.is_pending());
        assert!(matcher.is_empty());
    }

    #[test]
    fn finds_navigates_and_preserves_replacement_position() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo FOO foo"));
        matcher.update_query("foo", true);
        assert_eq!(&*matcher.matched_ranges(), &[0..3, 4..7, 8..11]);
        assert_eq!(matcher.next(), Some(4..7));
        assert_eq!(matcher.next_back(), Some(0..3));

        matcher.set_current_match_index(2);
        matcher.begin_replacement();
        matcher.update(&Rope::from("foo FOO bar"));
        assert_eq!(matcher.current_match_index(), 1);
    }

    #[test]
    fn next_wraps_to_start() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from(".....aaaaa.....aaaaa.....aaaaa"));
        matcher.update_query("aaaaa", false);
        matcher.set_current_match_index(2);
        assert_eq!(matcher.next(), Some(5..10));
    }

    #[test]
    fn a_query_set_without_the_panel_keeps_the_session_active_until_closed() {
        let mut session = SearchSession::default();
        assert!(!session.is_active());

        session.open(false, true);
        assert!(session.is_active());
        session.close();
        assert!(!session.is_active());

        // A custom search UI never opens the panel; setting a query is what
        // turns the match highlights on, and closing turns them off again.
        session.activate();
        assert!(session.is_active());
        assert!(!session.open);
        session.close();
        assert!(!session.is_active());
    }

    #[test]
    fn regex_and_whole_word_toggles_and_an_invalid_pattern() {
        let mut session = SearchSession::default();
        session
            .matcher
            .update(&Rope::from("foo(a, b) foo(c,d) food foo"));
        let regex = SearchOptions {
            regex: true,
            ..session.options()
        };
        session.update_query_with(r"foo\((\w+), ?(\w+)\)", regex);
        assert_eq!(session.matcher.label(), "1/2");
        assert!(session.matcher.error().is_none());
        // Typing on into an unfinished pattern finds nothing and says why.
        session.update_query_with("foo(", regex);
        assert_eq!(session.matcher.label(), "0/0");
        assert!(session.matcher.error().is_some());
        session.update_query_with(
            "foo",
            SearchOptions {
                whole_word: true,
                ..Default::default()
            },
        );
        assert!(session.matcher.error().is_none());
        assert_eq!(&*session.matcher.matched_ranges(), &[0..3, 10..13, 24..27]);
        assert!(session.whole_word && !session.regex && session.case_insensitive);
    }

    #[test]
    fn identical_query_keeps_the_current_match() {
        let mut session = SearchSession::default();
        session.update_query("foo", true);
        session.matcher.update(&Rope::from("foo bar foo baz foo"));
        session.matcher.update_cursor_by_offset(12);
        assert_eq!(session.matcher.current_match_index(), 2);

        // Reopening Find and the styled search panel's initial query echo both
        // update the session with the same query. Neither should reset the
        // previously active occurrence.
        session.update_query("foo", true);

        assert_eq!(session.matcher.current_match_index(), 2);
        assert_eq!(session.matcher.label(), "3/3");
    }

    #[test]
    fn replacement_keeps_current_match_index_on_next_match() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo foo foo"));
        matcher.update_query("foo", true);
        assert_eq!(matcher.label(), "1/3");

        assert!(matcher.has_next_without_wrap());
        matcher.begin_replacement();
        matcher.update(&Rope::from("bar foo foo"));
        assert_eq!(matcher.current_match_index(), 0);
        assert_eq!(matcher.matched_ranges()[0], 4..7);
        assert_eq!(matcher.label(), "1/2");

        matcher.set_current_match_index(1);
        assert!(!matcher.has_next_without_wrap());
        matcher.set_current_match_index(0);
        matcher.begin_replacement();
        matcher.update(&Rope::from("bar foo bar"));
        assert_eq!(matcher.current_match_index(), 0);
        assert_eq!(matcher.matched_ranges()[0], 4..7);
        assert_eq!(matcher.label(), "1/1");
    }

    #[test]
    fn update_matches_clamps_current_match_index_while_replacing() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("foo foo foo"));
        matcher.update_query("foo", true);
        matcher.set_current_match_index(2);
        matcher.begin_replacement();

        matcher.update(&Rope::from("foo xoo foo"));

        assert_eq!(matcher.len(), 2);
        assert_eq!(matcher.current_match_index(), 1);
        assert_eq!(matcher.label(), "2/2");
    }
}
