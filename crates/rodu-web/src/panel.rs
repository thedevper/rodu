//! Side panel to read and edit one item. Descriptions and comments are rendered from Markdown
//! through [`crate::markdown`], which lets nothing in them run or load; all other user text is
//! rendered as plain text.

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rodu_api::{CommentView, ITEM_TYPES, ItemView, PRIORITIES, PrincipalView, StateView};
use serde_json::{Value, json};
use wasm_bindgen::JsCast;

use crate::api::{Api, ApiError};
use crate::markdown;

/// Where the panel remembers its width in pixels, in this browser only. The width is set by
/// dragging the panel's left edge, or by the button that switches between the two presets.
const WIDTH_KEY: &str = "rodu.panel.width";
const NARROW: f64 = 640.0;
const WIDE: f64 = 1080.0;
/// The narrowest the panel can be dragged; it is never wider than the window.
const MIN_WIDTH: f64 = 360.0;

/// `createdAt` in the browser's locale and time zone, like `Date#toLocaleString`.
fn local_time(timestamp: &str) -> String {
    js_sys::Date::new(&timestamp.into())
        .to_locale_string("default", &wasm_bindgen::JsValue::UNDEFINED)
        .into()
}

fn remembered_width() -> f64 {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(WIDTH_KEY).ok().flatten())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|w| w.is_finite())
        .map_or(NARROW, |w| w.max(MIN_WIDTH))
}

fn remember_width(width: f64) {
    if let Some(storage) = window().local_storage().ok().flatten() {
        let _ = storage.set_item(WIDTH_KEY, &width.round().to_string());
    }
}

/// Whether the person is typing in one of the panel's fields right now.
fn typing_in_panel() -> bool {
    document().active_element().is_some_and(|el| {
        matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT")
            && el.closest(".panel").ok().flatten().is_some()
    })
}

#[component]
pub fn ItemPanel(
    api: Api,
    item_key: String,
    #[prop(into)] states: Signal<Vec<StateView>>,
    #[prop(into)] principals: Signal<Vec<PrincipalView>>,
    /// The board's revision: when it moves, the card is read again (a teammate, an agent or
    /// another tab may have changed it).
    #[prop(into)]
    revision: Signal<Option<u64>>,
    /// Set while the description has changes that are not saved, so the board can ask before it
    /// replaces or closes this panel.
    unsaved: RwSignal<bool>,
    on_close: Callback<()>,
    on_changed: Callback<()>,
    on_error: Callback<ApiError>,
) -> impl IntoView {
    let item = RwSignal::new(None::<ItemView>);
    let comments = RwSignal::new(Vec::<CommentView>::new());
    let title = RwSignal::new(String::new());
    let comment = RwSignal::new(String::new());
    let width = RwSignal::new(remembered_width());
    let wide = move || width.get() >= (NARROW + WIDE) / 2.0;
    // Dragging the left edge: the panel follows the pointer until it is let go.
    let resizing = RwSignal::new(false);
    let drag = window_event_listener(ev::pointermove, move |e| {
        if resizing.try_get_untracked() == Some(true) {
            let inner = window().inner_width().ok().and_then(|w| w.as_f64()).unwrap_or(WIDE);
            width.set((inner - f64::from(e.client_x())).clamp(MIN_WIDTH, inner.max(MIN_WIDTH)));
        }
    });
    let drop_edge = window_event_listener(ev::pointerup, move |_| {
        if resizing.try_get_untracked() == Some(true) {
            resizing.set(false);
            remember_width(width.get_untracked());
        }
    });
    on_cleanup(move || {
        drag.remove();
        drop_edge.remove();
    });

    // The description is read until "Edit", and changes only with "Save": a stray click or key
    // never rewrites it. The version it was opened at goes with the save, so a description someone
    // else changed meanwhile is refused rather than overwritten.
    let editing = RwSignal::new(false);
    let preview = RwSignal::new(false);
    let draft = RwSignal::new(String::new());
    let draft_base = StoredValue::new((String::new(), 0_i64));
    let saving = RwSignal::new(false);
    let leaving = RwSignal::new(false);
    // Someone else saved the description after this edit began: their text stays until the
    // person decides, and the draft stays too.
    let conflict = RwSignal::new(false);
    let editor = NodeRef::<html::Textarea>::new();
    let title_box = NodeRef::<html::Textarea>::new();
    // The title box grows with its text (CSS `field-sizing` does this where it is supported).
    let fit_title = move || {
        request_animation_frame(move || {
            // The panel may be gone by the next frame.
            if let Some(Some(area)) = title_box.try_get_untracked() {
                let style = web_sys::HtmlElement::style(&area);
                let _ = style.set_property("height", "auto");
                let _ = style.set_property("height", &format!("{}px", area.scroll_height() + 2));
            }
        });
    };
    Effect::new(move |_| {
        title.track();
        item.track();
        fit_title();
    });
    let dirty =
        move || editing.get() && draft.with(|d| *d != draft_base.with_value(|b| b.0.clone()));
    Effect::new(move |_| unsaved.set(dirty()));
    on_cleanup(move || unsaved.set(false));

    let key = StoredValue::new(item_key);
    // Comments carry no version, so each read takes a number and shows its comments only when no
    // later read has shown its own yet: a slow answer never replaces a fresher list, and a later
    // read that fails does not cost the list an earlier one brought.
    let asked = StoredValue::new(0u64);
    let applied = StoredValue::new(0u64);
    let ask = move || {
        let n = asked.try_get_value().unwrap_or(0) + 1;
        asked.try_set_value(n);
        n
    };
    let fresher = move |n: u64| {
        let newer = applied.try_get_value().is_some_and(|a| n > a);
        if newer {
            applied.try_set_value(n);
        }
        newer
    };
    // Never steps back: an answer older than the card on screen is dropped.
    let show = move |next: ItemView| {
        let older = item
            .try_with_untracked(|i| i.as_ref().is_some_and(|i| i.version > next.version))
            .unwrap_or(true);
        if !older {
            title.set(next.title.clone());
            item.set(Some(next));
        }
    };
    // Also runs after a write, when the panel may have closed: every read is a try.
    let load = move || async move {
        let Some(key) = key.try_get_value() else { return };
        let n = ask();
        match api.item(&key).await {
            Ok(detail) => {
                if fresher(n) {
                    comments.try_set(detail.comments);
                }
                show(detail.item);
            }
            Err(e) => on_error.run(e),
        }
    };
    spawn_local(load());

    // A newer revision: the newest comments always show; the card's fields wait while the person is
    // editing or typing, so nothing they are writing is replaced under them, and show once they
    // leave the field. Only a newer version replaces the card, so a slow answer never undoes a
    // fresher one. The panel may be closed while a request is out: every read after it is a try.
    let pending = StoredValue::new(false);
    let refresh = move || async move {
        let Some(key) = key.try_get_value() else { return };
        let n = ask();
        let Ok(detail) = api.item(&key).await else { return };
        if fresher(n) {
            comments.try_set(detail.comments);
        }
        let Some(shown) = item.try_with_untracked(|i| i.as_ref().map(|i| i.version)) else {
            return;
        };
        if shown.is_some_and(|v| detail.item.version <= v) {
            return;
        }
        if editing.try_get_untracked() == Some(false) && !typing_in_panel() {
            pending.try_set_value(false);
            show(detail.item);
        } else {
            pending.try_set_value(true);
        }
    };
    let first = StoredValue::new(true);
    Effect::new(move |_| {
        revision.track();
        if first.get_value() {
            first.set_value(false);
            return;
        }
        spawn_local(refresh());
    });
    let catch_up = move || {
        if pending.try_get_value() == Some(true) {
            // After the focus has moved, so a move within the panel still counts as typing.
            request_animation_frame(move || spawn_local(refresh()));
        }
    };

    let close = move || {
        if dirty() {
            leaving.set(true);
        } else {
            on_close.run(());
        }
    };
    let listener = window_event_listener(ev::keydown, move |e| {
        // While the description is being edited Escape does nothing: Cancel or Save end it.
        // Nor does it close the card over a comment typed but not sent.
        if e.key() == "Escape"
            && editing.try_get_untracked() == Some(false)
            && comment.try_with_untracked(|c| c.trim().is_empty()) == Some(true)
        {
            on_close.run(());
        }
    });
    on_cleanup(move || listener.remove());
    // Leaving the page with an unsaved description: the browser asks.
    let unload = window_event_listener(ev::beforeunload, move |e| {
        if unsaved.try_get_untracked() == Some(true) {
            e.prevent_default();
            e.set_return_value("unsaved");
        }
    });
    on_cleanup(move || unload.remove());

    // Report a failure, then show the item and the board as the server now has them.
    let finish = move |result: Result<(), ApiError>| async move {
        if let Err(e) = result {
            on_error.run(e);
        }
        load().await;
        on_changed.run(());
    };

    let update = move |patch: Value| {
        let Some(current) = item.get_untracked() else { return };
        spawn_local(async move {
            finish(api.update(&current.key, patch, current.version).await.map(drop)).await;
        });
    };

    let transition = move |to: String| {
        spawn_local(async move {
            finish(api.transition(&key.get_value(), &to, None).await.map(drop)).await;
        });
    };

    let add_comment = move |event: ev::SubmitEvent| {
        event.prevent_default();
        let text = comment.get_untracked().trim().to_string();
        if text.is_empty() {
            return;
        }
        comment.set(String::new());
        spawn_local(async move {
            finish(api.comment(&key.get_value(), &text).await.map(drop)).await;
        });
    };

    let start_editing = move || {
        let Some(current) = item.get_untracked() else { return };
        draft.set(current.body.clone());
        draft_base.set_value((current.body, current.version));
        preview.set(false);
        leaving.set(false);
        conflict.set(false);
        editing.set(true);
        request_animation_frame(move || {
            if let Some(Some(area)) = editor.try_get_untracked() {
                let _ = area.focus();
            }
        });
    };
    let cancel_editing = move || {
        editing.set(false);
        leaving.set(false);
        conflict.set(false);
        // What came in while editing shows now.
        spawn_local(load());
    };
    let save_description = move || {
        if saving.get_untracked() {
            return;
        }
        let (saved, version) = draft_base.get_value();
        let text = draft.get_untracked();
        if text == saved {
            cancel_editing();
            return;
        }
        saving.set(true);
        let key = key.get_value();
        spawn_local(async move {
            let mut result = api.update(&key, json!({ "body": text }), version).await;
            // The card's version also moves for its status, rank or any other field. When the
            // description itself is still what this edit began from, nobody else changed it, so
            // the edit is saved over the new version; the retry is still version-checked. A
            // description someone else changed is refused and shown as a conflict.
            if matches!(&result, Err(e) if e.status == 409)
                && let Ok(now) = api.item(&key).await
                && now.item.body == saved
            {
                result = api.update(&key, json!({ "body": text }), now.item.version).await;
            }
            match result {
                Ok(next) => {
                    editing.try_set(false);
                    leaving.try_set(false);
                    if item.try_get_untracked().is_some() {
                        show(next);
                    }
                    on_changed.run(());
                }
                // Still editing, with the draft kept: the person decides what to do with it.
                Err(e) if e.status == 409 => {
                    conflict.try_set(true);
                }
                Err(e) => on_error.run(e),
            }
            saving.try_set(false);
        });
    };

    move || {
        let Some(current) = item.get() else {
            return view! { <aside class="panel" aria-label="Item details"></aside> }.into_any();
        };

        let saved_title = current.title.clone();
        let save_title = move || {
            let text = title.get_untracked().trim().to_string();
            if !text.is_empty() && text != saved_title {
                update(json!({ "title": text }));
            } else {
                title.set(saved_title.clone());
            }
        };
        let reverted_title = current.title.clone();

        let status = current.status.to_lowercase();
        let known_status = states.with_untracked(|s| {
            s.iter().find(|s| s.name.to_lowercase() == status).map(|s| s.name.clone())
        });
        let assignee = current.assignee.clone().unwrap_or_default();
        let estimate = current.estimate;
        let body = current.body.clone();

        view! {
            <aside
                class="panel"
                class:resizing=move || resizing.get()
                style:width=move || format!("{}px", width.get())
                on:focusout=move |_| catch_up()
                aria-label=format!("{} details", current.key)
            >
                <div
                    class="panel-edge"
                    role="separator"
                    aria-orientation="vertical"
                    aria-label="Drag to resize the panel"
                    on:pointerdown=move |e| {
                        e.prevent_default();
                        resizing.set(true);
                    }
                ></div>
                <header class="panel-head">
                    <span class="key">{current.key.clone()}</span>
                    <div class="panel-actions">
                        <button
                            type="button"
                            class="ghost"
                            aria-pressed=move || wide().to_string()
                            title=move || if wide() { "Narrower panel" } else { "Wider panel" }
                            aria-label=move || if wide() { "Narrower panel" } else { "Wider panel" }
                            on:click=move |_| {
                                let next = if wide() { NARROW } else { WIDE };
                                width.set(next);
                                remember_width(next);
                            }
                        >
                            {move || if wide() { "⇥" } else { "⇤" }}
                        </button>
                        <button type="button" class="ghost" aria-label="Close" on:click=move |_| close()>
                            "×"
                        </button>
                    </div>
                </header>

                {move || {
                    leaving
                        .get()
                        .then(|| {
                            view! {
                                <div class="leave-bar" role="alert">
                                    <span>"The description has changes that are not saved."</span>
                                    <div class="row">
                                        <button type="button" on:click=move |_| leaving.set(false)>
                                            "Keep editing"
                                        </button>
                                        <button
                                            type="button"
                                            class="ghost danger"
                                            on:click=move |_| {
                                                editing.set(false);
                                                on_close.run(());
                                            }
                                        >
                                            "Discard and close"
                                        </button>
                                    </div>
                                </div>
                            }
                        })
                }}

                <textarea
                    node_ref=title_box
                    class="panel-title"
                    aria-label="Title"
                    rows="1"
                    prop:value=move || title.get()
                    on:input=move |e| {
                        // A title is one line: a pasted line break becomes a space.
                        title.set(event_target_value(&e).replace(['\r', '\n'], " "))
                    }
                    on:blur=move |_| save_title()
                    on:keydown=move |e| {
                        let blur = |e: &ev::KeyboardEvent| {
                            if let Some(input) = e
                                .current_target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                            {
                                let _ = input.blur();
                            }
                        };
                        if e.key() == "Enter" {
                            e.prevent_default();
                            blur(&e);
                        } else if e.key() == "Escape" {
                            e.stop_propagation();
                            title.set(reverted_title.clone());
                            blur(&e);
                        }
                    }
                ></textarea>

                <dl class="fields">
                    <dt>"Status"</dt>
                    <dd>
                        <select aria-label="Status" on:change=move |e| transition(event_target_value(&e))>
                            {known_status
                                .is_none()
                                .then(|| {
                                    view! {
                                        <option value="" disabled=true prop:selected=true>
                                            {current.status.clone()}
                                        </option>
                                    }
                                })}
                            {states
                                .get_untracked()
                                .into_iter()
                                .map(|s| {
                                    let chosen = known_status.as_ref() == Some(&s.name);
                                    view! {
                                        <option value=s.name.clone() prop:selected=chosen>
                                            {s.name.clone()}
                                        </option>
                                    }
                                })
                                .collect_view()}
                        </select>
                    </dd>
                    <dt>"Assignee"</dt>
                    <dd>
                        <select
                            aria-label="Assignee"
                            on:change=move |e| {
                                let value = event_target_value(&e);
                                let value = if value.is_empty() { Value::Null } else { Value::String(value) };
                                update(json!({ "assignee": value }));
                            }
                        >
                            <option value="" prop:selected=assignee.is_empty()>
                                "Unassigned"
                            </option>
                            {principals
                                .get_untracked()
                                .into_iter()
                                .map(|p| {
                                    let chosen = p.name == assignee;
                                    let label = if p.kind == "agent" {
                                        format!("{} (agent)", p.name)
                                    } else {
                                        p.name.clone()
                                    };
                                    view! {
                                        <option value=p.name prop:selected=chosen>
                                            {label}
                                        </option>
                                    }
                                })
                                .collect_view()}
                        </select>
                    </dd>
                    <dt>"Priority"</dt>
                    <dd>
                        <select
                            aria-label="Priority"
                            on:change=move |e| update(json!({ "priority": event_target_value(&e) }))
                        >
                            {PRIORITIES
                                .iter()
                                .map(|&p| {
                                    view! {
                                        <option value=p prop:selected=p == current.priority>
                                            {p}
                                        </option>
                                    }
                                })
                                .collect_view()}
                        </select>
                    </dd>
                    <dt>"Type"</dt>
                    <dd>
                        <select
                            aria-label="Type"
                            on:change=move |e| update(json!({ "type": event_target_value(&e) }))
                        >
                            {ITEM_TYPES
                                .iter()
                                .map(|&t| {
                                    view! {
                                        <option value=t prop:selected=t == current.item_type>
                                            {t}
                                        </option>
                                    }
                                })
                                .collect_view()}
                        </select>
                    </dd>
                    <dt>"Estimate"</dt>
                    <dd>
                        <input
                            type="number"
                            min="0"
                            aria-label="Estimate"
                            value=estimate.map(|n| n.to_string()).unwrap_or_default()
                            on:blur=move |e| {
                                let raw = event_target_value(&e);
                                let value = if raw.is_empty() {
                                    None
                                } else {
                                    match raw.parse::<f64>() {
                                        Ok(n) => Some(n),
                                        Err(_) => return,
                                    }
                                };
                                if value != estimate {
                                    update(json!({ "estimate": value }));
                                }
                            }
                        />
                    </dd>
                </dl>

                <section class="description">
                    <div class="section-head">
                        <h3>"Description"</h3>
                        {move || {
                            (!editing.get())
                                .then(|| {
                                    view! {
                                        <button type="button" class="ghost" on:click=move |_| start_editing()>
                                            "Edit"
                                        </button>
                                    }
                                })
                        }}
                    </div>
                    {move || {
                        if editing.get() {
                            view! {
                                <div class="editor">
                                    <div class="tabs" role="tablist" aria-label="Description editor">
                                        <button
                                            type="button"
                                            role="tab"
                                            class="ghost"
                                            aria-selected=move || (!preview.get()).to_string()
                                            on:click=move |_| preview.set(false)
                                        >
                                            "Write"
                                        </button>
                                        <button
                                            type="button"
                                            role="tab"
                                            class="ghost"
                                            aria-selected=move || preview.get().to_string()
                                            on:click=move |_| preview.set(true)
                                        >
                                            "Preview"
                                        </button>
                                        <span class="hint">"Markdown: **bold**, _italic_, - lists, ## headings, [links](https://…)"</span>
                                    </div>
                                    <textarea
                                        node_ref=editor
                                        aria-label="Description"
                                        rows="14"
                                        hidden=move || preview.get()
                                        placeholder="Add details, acceptance criteria, links…"
                                        prop:value=move || draft.get()
                                        on:input=move |e| draft.set(event_target_value(&e))
                                        on:keydown=move |e| {
                                            if e.key() == "Enter" && (e.meta_key() || e.ctrl_key()) {
                                                e.prevent_default();
                                                save_description();
                                            }
                                        }
                                    ></textarea>
                                    {move || {
                                        preview
                                            .get()
                                            .then(|| {
                                                let html = draft.with(|d| markdown::to_html(d));
                                                view! { <div class="markdown preview" inner_html=html></div> }
                                            })
                                    }}
                                    {move || {
                                        conflict
                                            .get()
                                            .then(|| {
                                                view! {
                                                    <p class="conflict" role="alert">
                                                        "Someone else changed this description while you were editing, so yours was not saved. "
                                                        "Copy your text, press Cancel to read theirs, then edit again."
                                                    </p>
                                                }
                                            })
                                    }}
                                    <div class="row">
                                        <button
                                            type="button"
                                            disabled=move || saving.get() || !dirty()
                                            on:click=move |_| save_description()
                                        >
                                            {move || if saving.get() { "Saving…" } else { "Save" }}
                                        </button>
                                        <button type="button" class="ghost" on:click=move |_| cancel_editing()>
                                            "Cancel"
                                        </button>
                                        <span class="hint">"⌘/Ctrl + Enter saves"</span>
                                    </div>
                                </div>
                            }
                                .into_any()
                        } else if body.trim().is_empty() {
                            view! {
                                <button type="button" class="ghost add-description" on:click=move |_| start_editing()>
                                    "Add a description"
                                </button>
                            }
                                .into_any()
                        } else {
                            view! { <div class="markdown" inner_html=markdown::to_html(&body)></div> }.into_any()
                        }
                    }}
                </section>

                <section class="comments">
                    <h3>"Comments"</h3>
                    {move || {
                        comments
                            .with(Vec::is_empty)
                            .then(|| view! { <p class="muted">"No comments yet."</p> })
                    }}
                    <ol>
                        <For each=move || comments.get() key=|c| c.id.clone() let:c>
                            <li>
                                <div class="comment-meta">
                                    <strong>{c.author}</strong>
                                    {c.via.map(|via| view! { <span class="via">{format!("via {via}")}</span> })}
                                    <time datetime=c.created_at.clone()>{local_time(&c.created_at)}</time>
                                </div>
                                <div class="markdown comment-body" inner_html=markdown::to_html(&c.body)></div>
                            </li>
                        </For>
                    </ol>
                    <form on:submit=add_comment>
                        <textarea
                            aria-label="New comment"
                            rows="3"
                            placeholder="Write a comment (Markdown works)"
                            prop:value=move || comment.get()
                            on:input=move |e| comment.set(event_target_value(&e))
                        ></textarea>
                        <button type="submit" disabled=move || comment.with(|c| c.trim().is_empty())>
                            "Comment"
                        </button>
                    </form>
                </section>
            </aside>
        }
        .into_any()
    }
}
