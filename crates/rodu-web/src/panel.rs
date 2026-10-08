//! Side panel to read and edit one item. All user text is rendered as plain text.

use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rodu_api::{CommentView, ITEM_TYPES, ItemView, PRIORITIES, PrincipalView, StateView};
use serde_json::{Value, json};
use wasm_bindgen::JsCast;

use crate::api::{Api, ApiError};

/// `createdAt` in the browser's locale and time zone, like `Date#toLocaleString`.
fn local_time(timestamp: &str) -> String {
    js_sys::Date::new(&timestamp.into())
        .to_locale_string("default", &wasm_bindgen::JsValue::UNDEFINED)
        .into()
}

#[component]
pub fn ItemPanel(
    api: Api,
    item_key: String,
    #[prop(into)] states: Signal<Vec<StateView>>,
    #[prop(into)] principals: Signal<Vec<PrincipalView>>,
    on_close: Callback<()>,
    on_changed: Callback<()>,
    on_error: Callback<ApiError>,
) -> impl IntoView {
    let item = RwSignal::new(None::<ItemView>);
    let comments = RwSignal::new(Vec::<CommentView>::new());
    let title = RwSignal::new(String::new());
    let body = RwSignal::new(String::new());
    let comment = RwSignal::new(String::new());

    let key = StoredValue::new(item_key);
    let load = move || async move {
        match api.item(&key.get_value()).await {
            Ok(detail) => {
                title.set(detail.item.title.clone());
                body.set(detail.item.body.clone());
                comments.set(detail.comments);
                item.set(Some(detail.item));
            }
            Err(e) => on_error.run(e),
        }
    };
    spawn_local(load());

    let listener = window_event_listener(ev::keydown, move |e| {
        if e.key() == "Escape" {
            on_close.run(());
        }
    });
    on_cleanup(move || listener.remove());

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

        let status = current.status.to_lowercase();
        let known_status = states.with_untracked(|s| {
            s.iter().find(|s| s.name.to_lowercase() == status).map(|s| s.name.clone())
        });
        let assignee = current.assignee.clone().unwrap_or_default();
        let estimate = current.estimate;
        let saved_body = current.body.clone();
        let discard_body = saved_body.clone();

        view! {
            <aside class="panel" aria-label=format!("{} details", current.key)>
                <header class="panel-head">
                    <span class="key">{current.key.clone()}</span>
                    <button
                        type="button"
                        class="ghost"
                        aria-label="Close"
                        on:click=move |_| on_close.run(())
                    >
                        "×"
                    </button>
                </header>

                <input
                    class="panel-title"
                    aria-label="Title"
                    prop:value=move || title.get()
                    on:input=move |e| title.set(event_target_value(&e))
                    on:blur=move |_| save_title()
                    on:keydown=move |e| {
                        if e.key() == "Enter"
                            && let Some(input) = e
                                .current_target()
                                .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                        {
                            let _ = input.blur();
                        }
                    }
                />

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
                    <h3>"Description"</h3>
                    <textarea
                        aria-label="Description"
                        rows="8"
                        placeholder="Add details, acceptance criteria, links…"
                        prop:value=move || body.get()
                        on:input=move |e| body.set(event_target_value(&e))
                    ></textarea>
                    {move || {
                        let saved = saved_body.clone();
                        let discard = discard_body.clone();
                        body.with(|b| *b != saved)
                            .then(|| {
                                view! {
                                    <div class="row">
                                        <button
                                            type="button"
                                            on:click=move |_| {
                                                update(json!({ "body": body.get_untracked() }))
                                            }
                                        >
                                            "Save description"
                                        </button>
                                        <button
                                            type="button"
                                            class="ghost"
                                            on:click=move |_| body.set(discard.clone())
                                        >
                                            "Discard"
                                        </button>
                                    </div>
                                }
                            })
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
                                <p class="comment-body">{c.body}</p>
                            </li>
                        </For>
                    </ol>
                    <form on:submit=add_comment>
                        <textarea
                            aria-label="New comment"
                            rows="3"
                            placeholder="Write a comment"
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
