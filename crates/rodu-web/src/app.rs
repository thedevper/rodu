//! The board: top bar, notices, columns of cards with drag and drop, and the item panel.

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rodu_api::{BoardView, CollectionView, ItemView, PrincipalView, StateView};
use wasm_bindgen::{JsCast, JsValue};

use crate::api::{Api, ApiError, take_token};
use crate::board::{drop_neighbours, group_by_state, initials, insertion_index};
use crate::panel::ItemPanel;

const LAST_COLLECTION: &str = "rodu.collection";
const DRAG_TYPE: &str = "application/x-rodu-item";
/// How often an open board asks whether teammates changed it.
const LIVE_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

thread_local! {
    /// Whether a card is being dragged: the board is not reloaded under the user's hand.
    static DRAGGING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Remembering the last collection is a convenience only; storage errors are ignored.
fn remember(key: &str, value: &str) {
    if let Some(storage) = window().local_storage().ok().flatten() {
        let _ = storage.set_item(key, value);
    }
}

fn recall(key: &str) -> Option<String> {
    window().local_storage().ok().flatten()?.get_item(key).ok().flatten()
}

#[component]
pub fn App() -> impl IntoView {
    match take_token() {
        Some(token) => view! { <Workspace api=Api::new(token) /> }.into_any(),
        None => view! {
            <main class="empty-state">
                <h1>"Rodu"</h1>
                <p>
                    "Open the link printed by " <code>"rodu web"</code>
                    ". It carries a one-time key for this browser tab."
                </p>
            </main>
        }
        .into_any(),
    }
}

#[component]
fn Workspace(api: Api) -> impl IntoView {
    let collections = RwSignal::new(Vec::<CollectionView>::new());
    let collection_key = RwSignal::new(None::<String>);
    let principals = RwSignal::new(Vec::<PrincipalView>::new());
    let me = RwSignal::new(None::<String>);
    let board = RwSignal::new(None::<BoardView>);
    let filter = RwSignal::new(String::new());
    let applied = RwSignal::new(String::new());
    let filter_error = RwSignal::new(None::<ApiError>);
    let notice = RwSignal::new(None::<ApiError>);
    let selected = RwSignal::new(None::<String>);

    spawn_local(async move {
        let loaded = async {
            Ok::<_, ApiError>((api.collections().await?, api.principals().await?, api.me().await?))
        };
        match loaded.await {
            Ok((cols, people, self_view)) => {
                let last = recall(LAST_COLLECTION);
                let key = cols
                    .iter()
                    .find(|c| Some(&c.key) == last.as_ref())
                    .or(cols.first())
                    .map(|c| c.key.clone());
                collections.set(cols);
                principals.set(people);
                me.set(self_view.name);
                collection_key.set(key);
            }
            Err(e) => notice.set(Some(e)),
        }
    });

    // Only the newest board request may update the screen; an older, slower one is dropped.
    let latest_board = StoredValue::new(0_u64);
    // The server's revision as of the board on screen, read before the board itself, so a change
    // that lands in between is reloaded again rather than missed.
    let seen = StoredValue::new(None::<u64>);
    let reload = move || async move {
        let Some(collection) = collection_key.get_untracked() else { return };
        let q = applied.get_untracked();
        latest_board.update_value(|n| *n += 1);
        let request = latest_board.get_value();
        if let Ok(now) = api.revision().await {
            seen.set_value(Some(now));
        }
        let result = api.board(&collection, &q).await;
        if request != latest_board.get_value() {
            return;
        }
        match result {
            Ok(next) => {
                board.set(Some(next));
                filter_error.set(None);
            }
            Err(e) if !q.is_empty() && e.status == 400 => filter_error.set(Some(e)),
            Err(e) => notice.set(Some(e)),
        }
    };
    Effect::new(move |_| {
        collection_key.track();
        applied.track();
        spawn_local(reload());
    });

    // Live: while the page is visible and no card is held, ask whether the board changed (a
    // teammate's change came in) and reload it when it did. The item panel is left alone, so an
    // edit in progress is never overwritten; a save on a stale card is refused by its version.
    set_interval(
        move || {
            let hidden = document().hidden();
            if hidden || DRAGGING.with(std::cell::Cell::get) {
                return;
            }
            spawn_local(async move {
                let Ok(now) = api.revision().await else { return };
                if seen.get_value() != Some(now) {
                    reload().await;
                }
            });
        },
        LIVE_EVERY,
    );

    // Report a failed action, then show the board as the server now has it.
    let finish = move |result: Result<(), ApiError>| async move {
        if let Err(e) = result {
            notice.set(Some(e));
        }
        reload().await;
    };

    let choose_collection = move |key: String| {
        remember(LAST_COLLECTION, &key);
        collection_key.set(Some(key));
        selected.set(None);
    };

    let apply_filter = move |event: ev::SubmitEvent| {
        event.prevent_default();
        let next = filter.get_untracked().trim().to_string();
        if next != applied.get_untracked() {
            applied.set(next);
        }
    };

    let states = Memo::new(move |_| {
        board.with(|b| b.as_ref().map(|b| b.collection.states.clone()).unwrap_or_default())
    });
    let columns = Memo::new(move |_| {
        board.with(|b| b.as_ref().map(|b| group_by_state(&b.collection.states, &b.items)))
    });
    let has_board = Memo::new(move |_| board.with(Option::is_some));

    let drop_card = move |state: String, key: String, index: usize| {
        let Some(current) = board.get_untracked() else { return };
        // Only cards on this board can be dropped; anything else is ignored.
        let Some(dragged) = current.items.iter().find(|i| i.key == key) else { return };
        let to_other = dragged.status.to_lowercase() != state.to_lowercase();
        let column = group_by_state(&current.collection.states, &current.items)
            .remove(&state)
            .unwrap_or_default();
        let target = drop_neighbours(&column, &key, index);
        spawn_local(async move {
            let result = if to_other {
                api.transition(&key, &state, target).await.map(drop)
            } else if let Some(target) = target {
                api.move_to(&key, target).await.map(drop)
            } else {
                Ok(())
            };
            finish(result).await;
        });
    };

    let create_card = move |state: String, title: String| {
        let Some(collection) =
            board.with_untracked(|b| b.as_ref().map(|b| b.collection.key.clone()))
        else {
            return;
        };
        spawn_local(async move {
            finish(api.create(&collection, &title, &state).await.map(drop)).await;
        });
    };

    let on_open = Callback::new(move |key: String| selected.set(Some(key)));
    let on_error = Callback::new(move |e: ApiError| notice.set(Some(e)));
    let on_changed = Callback::new(move |()| spawn_local(reload()));
    let on_close = Callback::new(move |()| selected.set(None));

    view! {
        <div class="app">
            <header class="topbar">
                <div class="brand">
                    <span class="logo" aria-hidden="true"></span>
                    "Rodu"
                </div>
                <select
                    aria-label="Collection"
                    on:change=move |e| choose_collection(event_target_value(&e))
                >
                    {move || {
                        let chosen = collection_key.get();
                        collections
                            .get()
                            .into_iter()
                            .map(|c| {
                                let is_chosen = chosen.as_ref() == Some(&c.key);
                                view! {
                                    <option value=c.key.clone() prop:selected=is_chosen>
                                        {format!("{} · {}", c.key, c.name)}
                                    </option>
                                }
                            })
                            .collect_view()
                    }}
                </select>
                <form class="filter" on:submit=apply_filter>
                    <input
                        aria-label="Filter"
                        placeholder="Filter, e.g. assignee = me() AND priority IN (urgent, high)"
                        spellcheck="false"
                        prop:value=move || filter.get()
                        on:input=move |e| filter.set(event_target_value(&e))
                    />
                    <Show when=move || !applied.with(String::is_empty)>
                        <button
                            type="button"
                            class="ghost"
                            on:click=move |_| {
                                filter.set(String::new());
                                applied.set(String::new());
                            }
                        >
                            "Clear"
                        </button>
                    </Show>
                </form>
                <span class="me">{move || me.get()}</span>
            </header>

            {move || {
                filter_error
                    .get()
                    .map(|e| {
                        view! {
                            <div class="filter-error" role="alert">
                                <strong>{e.message}</strong>
                                {e.hint.map(|hint| view! { <pre>{hint}</pre> })}
                            </div>
                        }
                    })
            }}

            {move || {
                notice
                    .get()
                    .map(|n| {
                        view! {
                            <div class="notice" role="alert">
                                <div>
                                    <strong>{n.message}</strong>
                                    {n.hint.map(|hint| view! { <p>{hint}</p> })}
                                </div>
                                <button
                                    type="button"
                                    class="ghost"
                                    aria-label="Dismiss"
                                    on:click=move |_| notice.set(None)
                                >
                                    "×"
                                </button>
                            </div>
                        }
                    })
            }}

            <main class="board">
                <Show
                    when=move || has_board.get()
                    fallback=move || {
                        view! {
                            <p class="loading">
                                {move || {
                                    if collections.with(Vec::is_empty) && notice.with(Option::is_none)
                                    {
                                        "Loading…"
                                    } else {
                                        ""
                                    }
                                }}
                            </p>
                        }
                    }
                >
                    <For each=move || states.get() key=|s| s.name.clone() let:state>
                        {
                            let name = state.name.clone();
                            let items = Signal::derive({
                                let name = name.clone();
                                move || {
                                    columns
                                        .with(|c| {
                                            c.as_ref().and_then(|c| c.get(&name).cloned())
                                        })
                                        .unwrap_or_default()
                                }
                            });
                            let drop_name = name.clone();
                            let on_drop = Callback::new(move |(key, index): (String, usize)| {
                                drop_card(drop_name.clone(), key, index)
                            });
                            let on_create = Callback::new(move |title: String| {
                                create_card(name.clone(), title)
                            });
                            view! {
                                <Column
                                    state=state
                                    items=items
                                    on_open=on_open
                                    on_drop=on_drop
                                    on_create=on_create
                                />
                            }
                        }
                    </For>
                </Show>
            </main>

            {move || {
                board
                    .with(|b| {
                        b.as_ref()
                            .filter(|b| b.total > b.items.len() as u64)
                            .map(|b| {
                                format!(
                                    "Showing {} of {} items. Narrow the filter to see the rest.",
                                    b.items.len(),
                                    b.total,
                                )
                            })
                    })
                    .map(|text| view! { <p class="truncated">{text}</p> })
            }}

            {move || {
                let key = selected.get()?;
                has_board
                    .get()
                    .then(|| {
                        view! {
                            <ItemPanel
                                api=api
                                item_key=key
                                states=states
                                principals=principals
                                on_close=on_close
                                on_changed=on_changed
                                on_error=on_error
                            />
                        }
                    })
            }}
        </div>
    }
}

/// Midpoints of the cards currently in `list`, top to bottom.
fn card_midpoints(list: &web_sys::Element) -> Vec<f64> {
    let Ok(cards) = list.query_selector_all("[data-card]") else { return Vec::new() };
    (0..cards.length())
        .filter_map(|i| cards.item(i)?.dyn_into::<web_sys::Element>().ok())
        .map(|card| {
            let rect = card.get_bounding_client_rect();
            rect.top() + rect.height() / 2.0
        })
        .collect()
}

fn carries_card(event: &ev::DragEvent) -> bool {
    event.data_transfer().is_some_and(|dt| dt.types().includes(&JsValue::from_str(DRAG_TYPE), 0))
}

#[component]
fn Column(
    state: StateView,
    items: Signal<Vec<ItemView>>,
    on_open: Callback<String>,
    on_drop: Callback<(String, usize)>,
    on_create: Callback<String>,
) -> impl IntoView {
    let list_ref = NodeRef::<html::Ol>::new();
    let input_ref = NodeRef::<html::Input>::new();
    let drop_index = RwSignal::new(None::<usize>);
    let adding = RwSignal::new(false);
    let title = RwSignal::new(String::new());

    // The field appears because the user asked to add a card, so it takes the focus.
    Effect::new(move |_| {
        if let Some(input) = input_ref.get() {
            let _ = input.focus();
        }
    });

    let index_at = move |event: &ev::DragEvent| {
        let mids = list_ref.get_untracked().map(|list| card_midpoints(&list)).unwrap_or_default();
        insertion_index(&mids, f64::from(event.client_y()))
    };

    let handle_drop = move |event: ev::DragEvent| {
        event.prevent_default();
        let raw = event.data_transfer().and_then(|dt| dt.get_data(DRAG_TYPE).ok());
        drop_index.set(None);
        let key = raw
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("key")?.as_str().map(str::to_string));
        let Some(key) = key else { return };
        let mut index = index_at(&event);
        // Within a column the dragged card is still in the list; count positions without it.
        let current = items.with_untracked(|items| items.iter().position(|i| i.key == key));
        if let Some(current) = current
            && current < index
        {
            index -= 1;
        }
        on_drop.run((key, index));
    };

    let submit = move |event: ev::SubmitEvent| {
        event.prevent_default();
        let text = title.get_untracked().trim().to_string();
        if !text.is_empty() {
            on_create.run(text);
        }
        title.set(String::new());
        adding.set(false);
    };

    let add_label = format!("Add item to {}", state.name);

    view! {
        <section
            class=format!("column cat-{}", state.category)
            aria-label=state.name.clone()
            on:dragover=move |e| {
                if !carries_card(&e) {
                    return;
                }
                e.prevent_default();
                if let Some(dt) = e.data_transfer() {
                    dt.set_drop_effect("move");
                }
                drop_index.set(Some(index_at(&e)));
            }
            on:dragleave=move |e| {
                let section = e.current_target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
                let entered = e.related_target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
                if !section.is_some_and(|s| s.contains(entered.as_ref())) {
                    drop_index.set(None);
                }
            }
            on:drop=handle_drop
        >
            <header class="column-head">
                <span class="dot" aria-hidden="true"></span>
                <h2>{state.name.clone()}</h2>
                <span class="count">{move || items.with(Vec::len)}</span>
                <button
                    type="button"
                    class="ghost add"
                    aria-label=add_label
                    on:click=move |_| adding.set(true)
                >
                    "+"
                </button>
            </header>
            <ol node_ref=list_ref class="cards">
                {move || {
                    items
                        .get()
                        .into_iter()
                        .enumerate()
                        .map(|(i, item)| {
                            view! {
                                <li class:drop-before=move || drop_index.get() == Some(i)>
                                    <Card item=item on_open=on_open />
                                </li>
                            }
                        })
                        .collect_view()
                }}
                {move || {
                    let end = drop_index.get().is_some_and(|i| i >= items.with(Vec::len));
                    end.then(|| view! { <li class="drop-end"></li> })
                }}
            </ol>
            <Show when=move || adding.get()>
                <form class="new-card" on:submit=submit>
                    <input
                        node_ref=input_ref
                        aria-label="New item title"
                        placeholder="Title, then Enter"
                        prop:value=move || title.get()
                        on:input=move |e| title.set(event_target_value(&e))
                        on:keydown=move |e| {
                            if e.key() == "Escape" {
                                adding.set(false);
                            }
                        }
                        on:blur=move |_| {
                            if title.with_untracked(|t| t.trim().is_empty()) {
                                adding.set(false);
                            }
                        }
                    />
                </form>
            </Show>
        </section>
    }
}

#[component]
fn Card(item: ItemView, on_open: Callback<String>) -> impl IntoView {
    let key = item.key.clone();
    let drag_key = item.key.clone();
    let open_key = item.key.clone();
    view! {
        <button
            type="button"
            class="card"
            data-card=key.clone()
            draggable="true"
            on:dragend=move |_| DRAGGING.with(|d| d.set(false))
            on:dragstart=move |e| {
                DRAGGING.with(|d| d.set(true));
                if let Some(dt) = e.data_transfer() {
                    let payload = serde_json::json!({ "key": drag_key }).to_string();
                    let _ = dt.set_data(DRAG_TYPE, &payload);
                    dt.set_effect_allowed("move");
                }
            }
            on:click=move |_| on_open.run(open_key.clone())
        >
            <span class="card-top">
                <span class="key">{key.clone()}</span>
                {(item.item_type != "task")
                    .then(|| {
                        view! {
                            <span class=format!(
                                "type type-{}",
                                item.item_type,
                            )>{item.item_type.clone()}</span>
                        }
                    })}
                {(item.priority != "none")
                    .then(|| {
                        view! {
                            <span class=format!(
                                "priority p-{}",
                                item.priority,
                            )>{item.priority.clone()}</span>
                        }
                    })}
            </span>
            <span class="title">{item.title}</span>
            <span class="card-bottom">
                {item.estimate.map(|n| view! { <span class="estimate">{format!("{n} pt")}</span> })}
                {item
                    .due
                    .filter(|d| !d.is_empty())
                    .map(|d| view! { <span class="due">{format!("due {d}")}</span> })}
                {item
                    .assignee
                    .filter(|a| !a.is_empty())
                    .map(|a| {
                        view! {
                            <span class="avatar" title=a.clone()>
                                {initials(&a)}
                            </span>
                        }
                    })}
            </span>
        </button>
    }
}
