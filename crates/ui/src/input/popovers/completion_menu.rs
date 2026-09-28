use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    Action, AnyElement, App, AppContext, Context, DismissEvent, Empty, Entity, EventEmitter,
    Half as _, HighlightStyle, InteractiveElement as _, IntoElement, ParentElement, Pixels, Point,
    Render, RenderOnce, SharedString, Styled, StyledText, Subscription, Window, deferred, div,
    prelude::FluentBuilder, px, relative,
};
use lsp_types::{CompletionItem, CompletionTextEdit};

const MAX_MENU_WIDTH: Pixels = px(320.);
const MAX_MENU_HEIGHT: Pixels = px(240.);
const POPOVER_GAP: Pixels = px(4.);

use crate::{
    ActiveTheme, IndexPath, Selectable, actions, h_flex,
    input::{
        self, InputState, RopeExt,
        popovers::{editor_popover, render_markdown},
    },
    label::Label,
    list::{List, ListDelegate, ListEvent, ListState},
};

struct ContextMenuDelegate {
    query: SharedString,
    menu: Entity<CompletionMenu>,
    items: Vec<Rc<CompletionItem>>,
    selected_ix: usize,
    /// Items already sent to the provider's `resolve_completion`.
    resolved: HashSet<usize>,
}

impl ContextMenuDelegate {
    fn set_items(&mut self, items: Vec<CompletionItem>) {
        self.items = items.into_iter().map(Rc::new).collect();
        self.selected_ix = 0;
        self.resolved.clear();
    }

    fn selected_item(&self) -> Option<&Rc<CompletionItem>> {
        self.items.get(self.selected_ix)
    }
}

#[derive(IntoElement)]
struct CompletionMenuItem {
    ix: usize,
    item: Rc<CompletionItem>,
    children: Vec<AnyElement>,
    selected: bool,
    highlight_prefix: SharedString,
}

impl CompletionMenuItem {
    fn new(ix: usize, item: Rc<CompletionItem>) -> Self {
        Self {
            ix,
            item,
            children: vec![],
            selected: false,
            highlight_prefix: "".into(),
        }
    }

    fn highlight_prefix(mut self, s: impl Into<SharedString>) -> Self {
        self.highlight_prefix = s.into();
        self
    }
}
impl Selectable for CompletionMenuItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl ParentElement for CompletionMenuItem {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}
impl RenderOnce for CompletionMenuItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let item = self.item;

        let deprecated = item.deprecated.unwrap_or(false);
        let matched_len = item
            .filter_text
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(self.highlight_prefix.len())
            .min(item.label.len());

        let highlights = vec![(
            0..matched_len,
            HighlightStyle {
                color: Some(cx.theme().blue),
                ..Default::default()
            },
        )];

        h_flex()
            .id(self.ix)
            .gap_2()
            .p_1()
            .text_xs()
            .line_height(relative(1.))
            .rounded(cx.theme().radius.half())
            .when(item.deprecated.unwrap_or(false), |this| this.line_through())
            .hover(|this| this.bg(cx.theme().accent.opacity(0.8)))
            .when(self.selected, |this| {
                this.bg(cx.theme().tokens.accent)
                    .text_color(cx.theme().accent_foreground)
            })
            .child(div().child(StyledText::new(item.label.clone()).with_highlights(highlights)))
            .when(item.detail.is_some(), |this| {
                this.child(
                    Label::new(item.detail.as_deref().unwrap_or("").to_string())
                        .text_color(cx.theme().muted_foreground)
                        .when(deprecated, |this| this.line_through())
                        .italic(),
                )
            })
            .children(self.children)
    }
}

impl EventEmitter<DismissEvent> for ContextMenuDelegate {}

impl ListDelegate for ContextMenuDelegate {
    type Item = CompletionMenuItem;

    fn items_count(&self, _: usize, _: &gpui::App) -> usize {
        self.items.len()
    }

    fn render_item(
        &mut self,
        ix: crate::IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let item = self.items.get(ix.row)?;
        Some(CompletionMenuItem::new(ix.row, item.clone()).highlight_prefix(self.query.clone()))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<crate::IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_ix = ix.map(|i| i.row).unwrap_or(0);
        // Deferred: this runs inside the editor's own update (its arrow
        // keys), and resolving reads the editor.
        let menu = self.menu.clone();
        cx.defer(move |cx| menu.update(cx, |menu, cx| menu.resolve_selected(cx)));
        cx.notify();
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(item) = self.selected_item() else {
            return;
        };

        self.menu.update(cx, |this, cx| {
            this.select_item(&item, window, cx);
        });
    }
}

/// A context menu for code completions and code actions.
pub struct CompletionMenu {
    offset: usize,
    editor: Entity<InputState>,
    list: Entity<ListState<ContextMenuDelegate>>,
    open: bool,

    /// The offset of the first character that triggered the completion.
    pub(crate) trigger_start_offset: Option<usize>,
    query: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl CompletionMenu {
    /// Creates a new `CompletionMenu` with the given offset and completion items.
    ///
    /// NOTE: This element should not call from InputState::new, unless that will stack overflow.
    pub(crate) fn new(
        editor: Entity<InputState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let view = cx.entity();
            let menu = ContextMenuDelegate {
                query: SharedString::default(),
                menu: view,
                items: vec![],
                selected_ix: 0,
                resolved: HashSet::new(),
            };

            let list = cx.new(|cx| ListState::new(menu, window, cx));

            let _subscriptions =
                vec![
                    cx.subscribe(&list, |this: &mut Self, _, ev: &ListEvent, cx| {
                        match ev {
                            ListEvent::Confirm(_) => {
                                this.hide(cx);
                            }
                            _ => {}
                        }
                        cx.notify();
                    }),
                ];

            Self {
                offset: 0,
                editor,
                list,
                open: false,
                trigger_start_offset: None,
                query: SharedString::default(),
                _subscriptions,
            }
        })
    }

    /// Asks the provider to fill in the highlighted item, once, and puts
    /// the answer in its place — which shows its documentation.
    fn resolve_selected(&mut self, cx: &mut Context<Self>) {
        let (ix, item) = {
            let delegate = self.list.read(cx).delegate();
            let ix = delegate.selected_ix;
            let Some(item) = delegate.items.get(ix).cloned() else {
                return;
            };
            if delegate.resolved.contains(&ix) {
                return;
            }
            (ix, item)
        };
        let Some(provider) = self.editor.read(cx).lsp.completion_provider.clone() else {
            return;
        };
        self.list
            .update(cx, |list, _| list.delegate_mut().resolved.insert(ix));
        let text = self.editor.read(cx).text.clone();
        let task = provider.resolve_completion((*item).clone(), &text, cx);
        cx.spawn(async move |this, cx| {
            let Ok(resolved) = task.await else {
                return;
            };
            _ = this.update(cx, |this, cx| {
                this.list.update(cx, |list, cx| {
                    let delegate = list.delegate_mut();
                    // Only into the very item asked about: the list is
                    // rebuilt on every keystroke, and an answer measured
                    // against older text must not land on its successor.
                    if let Some(slot) = delegate.items.get_mut(ix)
                        && Rc::ptr_eq(slot, &item)
                    {
                        *slot = Rc::new(resolved);
                    }
                    cx.notify();
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn select_item(&mut self, item: &CompletionItem, window: &mut Window, cx: &mut Context<Self>) {
        let item = item.clone();
        let mut range = self.trigger_start_offset.unwrap_or(self.offset)..self.offset;

        let editor = self.editor.clone();

        cx.spawn_in(window, async move |_, cx| {
            // An item that has not said which imports it needs may say so
            // once resolved; ask before inserting it.
            let resolve = editor.update(cx, |editor, cx| {
                if item.additional_text_edits.is_some() {
                    return None;
                }
                let provider = editor.lsp.completion_provider.clone()?;
                let text = editor.text.clone();
                Some(provider.resolve_completion(item.clone(), &text, cx))
            });
            let item = match resolve {
                Some(task) => task.await.unwrap_or(item),
                None => item,
            };
            editor.update_in(cx, |editor, window, cx| {
                editor.completion_inserting = true;

                let mut new_text = item.label.clone();
                if let Some(text_edit) = item.text_edit.as_ref() {
                    match text_edit {
                        CompletionTextEdit::Edit(edit) => {
                            new_text = edit.new_text.clone();
                            range.start = editor.text.position_to_offset(&edit.range.start);
                            range.end = editor.text.position_to_offset(&edit.range.end);
                        }
                        CompletionTextEdit::InsertAndReplace(edit) => {
                            new_text = edit.new_text.clone();
                            range.start = editor.text.position_to_offset(&edit.replace.start);
                            range.end = editor.text.position_to_offset(&edit.replace.end);
                        }
                    }
                } else if let Some(insert_text) = item.insert_text.clone() {
                    // Replaces what was typed since the menu opened, as the
                    // label does, rather than going in after it.
                    new_text = insert_text;
                }

                // The completion and its additional edits (an import at the
                // top, say) are all written against the text as it is now:
                // applied from the end backwards, each leaves the others'
                // offsets alone. The caret goes after the completion.
                let additional: Vec<(Range<usize>, String)> = item
                    .additional_text_edits
                    .iter()
                    .flatten()
                    .map(|e| {
                        let start = editor.text.position_to_offset(&e.range.start);
                        let end = editor.text.position_to_offset(&e.range.end).max(start);
                        (start..end, e.new_text.clone())
                    })
                    .collect();
                let has_extra = !additional.is_empty();
                let (edits, caret) = plan_completion_edits(range.clone(), new_text, additional);
                for (r, t) in edits {
                    editor.replace_text_in_range_silent(
                        Some(editor.range_to_utf16(&r)),
                        &t,
                        window,
                        cx,
                    );
                }
                if has_extra {
                    editor.move_to(caret.min(editor.text.len()), None, cx);
                }
                editor.completion_inserting = false;
                // FIXME: Input not get the focus
                editor.focus(window, cx);
            })
        })
        .detach();

        self.hide(cx);
    }

    pub(crate) fn handle_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.open {
            return false;
        }

        cx.propagate();
        if input::Enter::is_primary(&*action) {
            self.on_action_enter(window, cx);
        } else if action.partial_eq(&input::Escape) {
            self.on_action_escape(window, cx);
        } else if action.partial_eq(&input::MoveUp) {
            self.on_action_up(window, cx);
        } else if action.partial_eq(&input::MoveDown) {
            self.on_action_down(window, cx);
        } else {
            return false;
        }

        true
    }

    fn on_action_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.list.read(cx).delegate().selected_item().cloned() else {
            return;
        };
        self.select_item(&item, window, cx);
    }

    fn on_action_escape(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.hide(cx);
    }

    fn on_action_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_prev(&actions::SelectUp, window, cx)
        });
    }

    fn on_action_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |this, cx| {
            this.on_action_select_next(&actions::SelectDown, window, cx)
        });
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Hide the completion menu and reset the trigger start offset.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.trigger_start_offset = None;
        cx.notify();
    }

    /// Sets the trigger start offset if it is not already set.
    pub(crate) fn update_query(&mut self, start_offset: usize, query: impl Into<SharedString>) {
        if self.trigger_start_offset.is_none() {
            self.trigger_start_offset = Some(start_offset);
        }
        self.query = query.into();
    }

    pub(crate) fn show(
        &mut self,
        offset: usize,
        items: impl Into<Vec<CompletionItem>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = items.into();
        self.offset = offset;
        self.open = true;
        self.list.update(cx, |this, cx| {
            let longest_ix = items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| {
                    item.label.len() + item.detail.as_ref().map(|d| d.len()).unwrap_or(0)
                })
                .map(|(ix, _)| ix)
                .unwrap_or(0);

            this.delegate_mut().query = self.query.clone();
            this.delegate_mut().set_items(items);
            this.set_selected_index(Some(IndexPath::new(0)), window, cx);
            this.set_item_to_measure_index(IndexPath::new(longest_ix), window, cx);
        });

        cx.notify();
    }

    fn origin(&self, cx: &App) -> Option<Point<Pixels>> {
        let editor = self.editor.read(cx);
        let Some(last_layout) = editor.last_layout.as_ref() else {
            return None;
        };
        let Some(cursor_origin) = last_layout.cursor_bounds.map(|b| b.origin) else {
            return None;
        };

        let scroll_origin = self.editor.read(cx).scroll_handle.offset();

        Some(
            scroll_origin + cursor_origin - editor.input_bounds.origin
                + Point::new(-px(4.), last_layout.line_height + px(4.)),
        )
    }
}

impl Render for CompletionMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        if self.list.read(cx).delegate().items.is_empty() {
            self.open = false;
            return Empty.into_any_element();
        }

        let Some(pos) = self.origin(cx) else {
            return Empty.into_any_element();
        };

        let selected_documentation = self
            .list
            .read(cx)
            .delegate()
            .selected_item()
            .and_then(|item| item.documentation.clone());

        let max_width = MAX_MENU_WIDTH.min(window.bounds().size.width - pos.x);
        let abs_pos = self.editor.read(cx).input_bounds.origin + pos;
        let vertical_layout =
            abs_pos.x + MAX_MENU_WIDTH + POPOVER_GAP + MAX_MENU_WIDTH + POPOVER_GAP
                > window.bounds().size.width;

        deferred(
            div()
                .absolute()
                .left(pos.x)
                .top(pos.y)
                .flex()
                .flex_row()
                .gap(POPOVER_GAP)
                .items_start()
                .when(vertical_layout, |this| this.flex_col())
                .child(
                    editor_popover("completion-menu", cx)
                        .max_w(max_width)
                        .min_w(px(120.))
                        .child(List::new(&self.list).max_h(MAX_MENU_HEIGHT)),
                )
                .when_some(selected_documentation, |this, documentation| {
                    let mut doc = match documentation {
                        lsp_types::Documentation::String(s) => s.clone(),
                        lsp_types::Documentation::MarkupContent(mc) => mc.value.clone(),
                    };
                    if vertical_layout {
                        doc = doc.split("\n").next().unwrap_or_default().to_string();
                    }

                    this.child(
                        div().child(
                            editor_popover("completion-menu", cx)
                                .w(MAX_MENU_WIDTH)
                                .px_2()
                                .child(render_markdown("doc", doc, window, cx)),
                        ),
                    )
                })
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.hide(cx);
                })),
        )
        .into_any_element()
    }
}

/// The completion and its additional edits (an import at the top, say), all
/// written against the text as it is now, in the order to apply them: from
/// the end backwards, so each leaves the others' offsets alone. An edit that
/// overlaps the completion is dropped. Also returns where the caret goes:
/// after the completion.
fn plan_completion_edits(
    range: Range<usize>,
    new_text: String,
    additional: Vec<(Range<usize>, String)>,
) -> (Vec<(Range<usize>, String)>, usize) {
    let mut edits: Vec<(Range<usize>, String)> = additional
        .into_iter()
        .filter(|(r, _)| r.end <= range.start || r.start >= range.end)
        .collect();
    // Everything that lands before the completion moves it. An insert at the
    // completion's own start goes in front of it — unless the completion is
    // itself an insert there, which is applied first and ends up in front.
    let shift: isize = edits
        .iter()
        .filter(|(r, _)| {
            r.end <= range.start && !(r.start == range.start && r.is_empty() && range.is_empty())
        })
        .map(|(r, t)| t.len() as isize - r.len() as isize)
        .sum();
    let caret = (range.start as isize + new_text.len() as isize + shift).max(0) as usize;
    edits.push((range, new_text));
    // By start and then by end, both descending: an insert at the
    // completion's start must go in after the completion replaced its range,
    // or the completion would replace the inserted text instead.
    edits.sort_by_key(|(r, _)| std::cmp::Reverse((r.start, r.end)));
    (edits, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut text = text.to_string();
        for (r, t) in edits {
            text.replace_range(r.clone(), t);
        }
        text
    }

    #[test]
    fn an_import_at_the_completions_own_start_does_not_eat_it() {
        let (edits, caret) =
            plan_completion_edits(0..3, "HashMap".into(), vec![(0..0, "use X;\n".into())]);
        let out = apply("Has", &edits);
        assert_eq!(out, "use X;\nHashMap");
        assert_eq!(caret, out.len());
    }

    #[test]
    fn an_import_above_moves_the_caret() {
        let text = "fn a() {}\nHas";
        let (edits, caret) =
            plan_completion_edits(10..13, "HashMap".into(), vec![(0..0, "use X;\n".into())]);
        let out = apply(text, &edits);
        assert_eq!(out, "use X;\nfn a() {}\nHashMap");
        assert_eq!(caret, out.len());
    }
}
