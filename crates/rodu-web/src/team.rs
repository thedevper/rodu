//! Side panel for the team this workspace syncs with (ADR 0004, step 3a): who is on it, the
//! machines asking to join, the invites waiting, and inviting, admitting and removing. On a plain
//! board it shares the board with a team instead (step 3b). All text from the server is rendered
//! as plain text.

use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rodu_api::{InviteCreated, PlaceView, TeamView};

use crate::api::{Api, ApiError};

/// What the panel shows: the team, or why there is none.
#[derive(Clone)]
enum Loaded {
    Waiting,
    Team(TeamView),
    NoTeam(ApiError),
}

#[component]
pub fn TeamPanel(api: Api, on_close: Callback<()>, on_error: Callback<ApiError>) -> impl IntoView {
    let loaded = RwSignal::new(Loaded::Waiting);
    let done = RwSignal::new(None::<String>);
    // The invite just made, shown once: it holds the join secret.
    let made = RwSignal::new(None::<InviteCreated>);
    let invite_name = RwSignal::new(String::new());
    let invite_days = RwSignal::new(String::from("7"));
    // The codes typed for machines asking to join, by machine, so a reload keeps them.
    let typed = RwSignal::new(std::collections::HashMap::<String, String>::new());

    // Every signal here is gone once the panel closes, and a reply can still arrive after that:
    // each one is reached with try_, and a closed panel ignores the reply.
    let load = move || async move {
        let next = match api.team().await {
            Ok(team) => Loaded::Team(team),
            Err(e) if e.status == 404 => Loaded::NoTeam(e),
            Err(e) => {
                on_error.run(e);
                return;
            }
        };
        // Only a change rebuilds the panel, so focus and what is being typed are kept.
        let changed = loaded.try_with_untracked(|now| match (now, &next) {
            (Loaded::Team(a), Loaded::Team(b)) => a != b,
            // Still no team: the share part keeps what it shows, such as why a share stopped.
            (Loaded::NoTeam(_), Loaded::NoTeam(_)) => false,
            _ => true,
        });
        if changed == Some(true) {
            loaded.try_set(next);
        }
    };
    // While open, follow the team as machines join and leave. The revision is read before the
    // first load, so a change landing between the two is still picked up by the first check.
    let seen = StoredValue::new(None::<u64>);
    spawn_local(async move {
        if let Ok(now) = api.revision().await {
            seen.try_set_value(Some(now));
        }
        load().await;
    });
    let interval = set_interval_with_handle(
        move || {
            spawn_local(async move {
                let Ok(now) = api.revision().await else { return };
                let Some(before) = seen.try_get_value() else { return };
                seen.try_set_value(Some(now));
                if before != Some(now) {
                    load().await;
                }
            });
        },
        std::time::Duration::from_secs(3),
    )
    .ok();
    on_cleanup(move || {
        if let Some(interval) = interval {
            interval.clear();
        }
    });

    let listener = window_event_listener(ev::keydown, move |e| {
        if e.key() == "Escape" {
            on_close.run(());
        }
    });
    on_cleanup(move || listener.remove());

    // Say what a change did, or why it failed, then show the team as it is now.
    let finish = move |result: Result<String, ApiError>| async move {
        match result {
            Ok(message) => {
                done.try_set(Some(message));
            }
            Err(e) => on_error.run(e),
        }
        load().await;
    };

    let invite = move |event: ev::SubmitEvent| {
        event.prevent_default();
        let name = invite_name.get_untracked().trim().to_string();
        if name.is_empty() {
            return;
        }
        let Ok(days) = invite_days.get_untracked().trim().parse::<u64>() else {
            on_error.run(ApiError::local("Give the number of days the invite lasts, from 1 to 90"));
            return;
        };
        spawn_local(async move {
            match api.invite(&name, Some(days)).await {
                Ok(created) => {
                    invite_name.try_set(String::new());
                    done.try_set(None);
                    made.try_set(Some(created));
                }
                Err(e) => on_error.run(e),
            }
            load().await;
        });
    };

    let revoke = move |name: String| {
        spawn_local(async move {
            made.try_update(|m| {
                if m.as_ref().is_some_and(|m| m.name == name) {
                    *m = None;
                }
            });
            finish(api.revoke_invite(&name).await.map(|d| d.message)).await;
        });
    };

    let admit = move |name: String, machine: String| {
        let code = typed.with_untracked(|t| t.get(&machine).cloned()).unwrap_or_default();
        spawn_local(async move {
            let result = api.admit(&name, &code).await;
            if result.is_ok() {
                typed.try_update(|t| t.remove(&machine));
            }
            finish(result.map(|d| d.message)).await;
        });
    };

    let remove = move |name: String, encrypted: bool| {
        let warning = format!(
            "Remove {name}? Nothing {name}'s machines write from now on is taken in, and a \
             removed machine cannot be admitted again.{} Also take away their access to the \
             shared folder.",
            if encrypted {
                " The team key is changed too: they can still read what the folder holds now, \
                 but not what each machine writes once it has the new key."
            } else {
                ""
            }
        );
        let sure = window().confirm_with_message(&warning).unwrap_or(false);
        if !sure {
            return;
        }
        spawn_local(async move {
            finish(api.remove(&name).await.map(|d| d.message)).await;
        });
    };

    view! {
        <aside class="panel team-panel" aria-label="Team">
            <header class="panel-head">
                <span class="key">"Team"</span>
                <button type="button" class="ghost" aria-label="Close" on:click=move |_| on_close.run(())>
                    "×"
                </button>
            </header>
            {move || done.get().map(|message| view! { <p class="team-done" role="status">{message}</p> })}
            {move || match loaded.get() {
                Loaded::Waiting => view! { <p class="loading">"Loading…"</p> }.into_any(),
                Loaded::NoTeam(e) => view! { <SharePanel api=api why=e on_error=on_error /> }.into_any(),
                Loaded::Team(team) => {
                    let may_admit = team.may_admit;
                    let encrypted = team.encrypted;
                    let summary = format!(
                        "{} · {}",
                        if team.encrypted { "Encrypted" } else { "Not encrypted" },
                        if team.signed { "admits each machine" } else { "anyone with the code" },
                    );
                    view! {
                        <p class="team-folder" title="The shared folder">{team.folder.clone()}</p>
                        <p class="team-summary">{summary}</p>

                        <h3>"People"</h3>
                        <ul class="team-people">
                            {team
                                .people
                                .into_iter()
                                .map(|person| {
                                    let name = person.name.clone();
                                    // The server says who can be removed, by the CLI's rule.
                                    let removable = may_admit && person.removable;
                                    view! {
                                        <li>
                                            <div class="person-head">
                                                <strong>{person.name.clone()}</strong>
                                                {removable
                                                    .then(|| {
                                                        view! {
                                                            <button
                                                                type="button"
                                                                class="ghost danger"
                                                                on:click=move |_| remove(name.clone(), encrypted)
                                                            >
                                                                "Remove…"
                                                            </button>
                                                        }
                                                    })}
                                            </div>
                                            {(!person.agents.is_empty())
                                                .then(|| {
                                                    view! {
                                                        <p class="muted">{format!("agents: {}", person.agents.join(", "))}</p>
                                                    }
                                                })}
                                            {person.machines.is_empty().then(|| view! { <p class="muted">"no machine recorded yet"</p> })}
                                            <ul class="machines">
                                                {person
                                                    .machines
                                                    .into_iter()
                                                    .map(|machine| {
                                                        view! {
                                                            <li>
                                                                <code>{machine.id}</code>
                                                                {machine
                                                                    .marks
                                                                    .into_iter()
                                                                    .map(|mark| view! { <span class="mark">{mark}</span> })
                                                                    .collect_view()}
                                                            </li>
                                                        }
                                                    })
                                                    .collect_view()}
                                            </ul>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>

                        {(!team.unknown.is_empty())
                            .then(|| {
                                view! {
                                    <h3>"Unknown machines"</h3>
                                    <p class="muted">"Writing to the folder, but no person on the team claims them."</p>
                                    <ul class="machines">
                                        {team
                                            .unknown
                                            .into_iter()
                                            .map(|machine| {
                                                view! {
                                                    <li>
                                                        <code>{machine.id}</code>
                                                        {machine.marks.into_iter().map(|mark| view! { <span class="mark">{mark}</span> }).collect_view()}
                                                    </li>
                                                }
                                            })
                                            .collect_view()}
                                    </ul>
                                }
                            })}

                        {(!team.asking.is_empty())
                            .then(|| {
                                view! {
                                    <h3>"Asking to join"</h3>
                                    <p class="muted">
                                        "Ask each person for the code their machine showed when it joined, and type it here: only a matching code admits it."
                                    </p>
                                    <ul class="team-asking">
                                        {team
                                            .asking
                                            .into_iter()
                                            .map(|request| {
                                                let name = request.name.clone();
                                                let machine = request.machine.clone();
                                                let shown = machine.clone();
                                                let edited = machine.clone();
                                                view! {
                                                    <li>
                                                        <div>
                                                            <strong>{request.name.clone()}</strong>
                                                            " · machine "
                                                            <code>{request.machine}</code>
                                                        </div>
                                                        {may_admit
                                                            .then(|| {
                                                                view! {
                                                                    <form
                                                                        class="inline-form"
                                                                        on:submit=move |e: ev::SubmitEvent| {
                                                                            e.prevent_default();
                                                                            admit(name.clone(), machine.clone());
                                                                        }
                                                                    >
                                                                        <input
                                                                            aria-label="The code they read to you"
                                                                            placeholder="The code they read to you"
                                                                            spellcheck="false"
                                                                            prop:value=move || {
                                                                                typed.with(|t| t.get(&shown).cloned()).unwrap_or_default()
                                                                            }
                                                                            on:input=move |e| {
                                                                                let value = event_target_value(&e);
                                                                                typed.update(|t| {
                                                                                    t.insert(edited.clone(), value);
                                                                                });
                                                                            }
                                                                        />
                                                                        <button type="submit">"Admit"</button>
                                                                    </form>
                                                                }
                                                            })}
                                                    </li>
                                                }
                                            })
                                            .collect_view()}
                                    </ul>
                                }
                            })}

                        {(team.signed && may_admit)
                            .then(|| {
                                view! {
                                    <h3>"Invite someone"</h3>
                                    <form class="inline-form" on:submit=invite>
                                        <input
                                            aria-label="Their name"
                                            placeholder="Their name"
                                            spellcheck="false"
                                            prop:value=move || invite_name.get()
                                            on:input=move |e| invite_name.set(event_target_value(&e))
                                        />
                                        <input
                                            class="days"
                                            aria-label="Days it lasts"
                                            type="number"
                                            min="1"
                                            max="90"
                                            prop:value=move || invite_days.get()
                                            on:input=move |e| invite_days.set(event_target_value(&e))
                                        />
                                        <button type="submit">"Invite"</button>
                                    </form>
                                    {move || made.get().map(|created| view! { <MadeInvite created=created encrypted=encrypted /> })}
                                    <ul class="team-invites">
                                        {team
                                            .invites
                                            .into_iter()
                                            .map(|invite| {
                                                let name = invite.name.clone();
                                                let when = if invite.expired {
                                                    format!("expired {}", invite.until)
                                                } else {
                                                    format!("until {}", invite.until)
                                                };
                                                view! {
                                                    <li>
                                                        <span>{invite.name}</span>
                                                        <span class="muted">{when}</span>
                                                        <button
                                                            type="button"
                                                            class="ghost"
                                                            on:click=move |_| revoke(name.clone())
                                                        >
                                                            "Cancel"
                                                        </button>
                                                    </li>
                                                }
                                            })
                                            .collect_view()}
                                    </ul>
                                }
                            })}

                        <p class="muted team-trust">{team.trust}</p>
                    }
                        .into_any()
                }
            }}
        </aside>
    }
}

/// The invite just made, with a button to copy it. It is not shown again.
#[component]
fn MadeInvite(created: InviteCreated, encrypted: bool) -> impl IntoView {
    let copied = RwSignal::new(false);
    let code = StoredValue::new(created.code.clone());
    let copy = move |_| {
        let promise = window().navigator().clipboard().write_text(&code.get_value());
        spawn_local(async move {
            if wasm_bindgen_futures::JsFuture::from(promise).await.is_ok() {
                copied.set(true);
            }
        });
    };
    view! {
        <div class="made-invite" role="status">
            <p>
                {format!(
                    "Invite for {}, until {} (UTC), for one machine. Send it to {} only, not through the team folder{}.",
                    created.name,
                    created.until,
                    created.name,
                    if encrypted { "; it holds the team key" } else { "" },
                )}
            </p>
            <textarea readonly=true rows="3" aria-label="The invite" spellcheck="false">{created.code}</textarea>
            <button type="button" on:click=copy>{move || if copied.get() { "Copied" } else { "Copy" }}</button>
            <p class="muted">
                "They run rodu team join and paste it. This machine admits theirs when it next syncs; it is not shown again."
            </p>
        </div>
    }
}

/// Where sharing stands on a plain board.
#[derive(Clone)]
enum Share {
    Loading,
    /// The Google Drive folders this machine has, to choose from.
    Places(Vec<PlaceView>),
    /// No sharing from here: the reason, as the server gave it.
    Unavailable(ApiError),
    Sharing,
    /// Shared: the message, while the server reopens as a team board.
    Reopening(String),
    /// A share that stopped half way, after which the board reopened: why, and a reload to carry
    /// on from where it stopped.
    Stopped(ApiError),
    /// The board did not come back after a share.
    Gone,
}

/// Shares a plain board with a team, its files in one of this machine's Google Drive folders.
#[component]
fn SharePanel(api: Api, why: ApiError, on_error: Callback<ApiError>) -> impl IntoView {
    let state = RwSignal::new(Share::Loading);
    let chosen = RwSignal::new(None::<PlaceView>);
    let why = StoredValue::new(why);
    spawn_local(async move {
        let next = match api.places().await {
            Ok(places) => {
                chosen.try_set(places.first().cloned());
                Share::Places(places)
            }
            Err(e) if e.status == 404 => Share::Unavailable(why.get_value()),
            Err(e) => Share::Unavailable(e),
        };
        state.try_set(next);
    });

    let share = move |_| {
        let Some(place) = chosen.get_untracked() else { return };
        let sure = window()
            .confirm_with_message(&format!(
                "Share this board with a team? Its files go in a Rodu folder in {}, encrypted. \
                 Everyone you invite gets a copy of the whole board, and your machine admits each \
                 of theirs. The board reopens once it is shared.",
                place.label
            ))
            .unwrap_or(false);
        if !sure {
            return;
        }
        state.set(Share::Sharing);
        spawn_local(async move {
            // A 503 means another tab's share is already reopening the board: wait for it the same way.
            let shared = match api.share(&place.path).await {
                Err(e) if e.status == 503 => Ok(e.message),
                other => other.map(|done| done.message),
            };
            match shared {
                Ok(message) => {
                    state.try_set(Share::Reopening(message));
                    // Shared: the board comes back on the same link, as a team's.
                    match wait_for_reopen(api).await {
                        Reopen::Back | Reopen::Never => {
                            let _ = window().location().reload();
                        }
                        Reopen::Gone => {
                            state.try_set(Share::Gone);
                        }
                    }
                }
                Err(e) => {
                    on_error.run(e.clone());
                    // A share that stopped half way reopens the board all the same: the reason
                    // stays on screen, with a reload to carry on, rather than going with a reload.
                    let next = match wait_for_reopen(api).await {
                        Reopen::Back => Share::Stopped(e),
                        Reopen::Gone => Share::Gone,
                        Reopen::Never => match api.places().await {
                            Ok(places) => Share::Places(places),
                            Err(e) => Share::Unavailable(e),
                        },
                    };
                    state.try_set(next);
                }
            }
        });
    };

    view! {
        <div class="team-share">
            {move || match state.get() {
                Share::Loading => view! { <p class="loading">"Loading…"</p> }.into_any(),
                Share::Unavailable(e) => {
                    view! {
                        <div class="team-none">
                            <p>{e.message}</p>
                            {e.hint.map(|hint| view! { <pre>{hint}</pre> })}
                        </div>
                    }
                        .into_any()
                }
                Share::Places(places) if places.is_empty() => {
                    view! {
                        <div class="team-none">
                            <p>"This board is yours alone. To share it, install Google Drive for desktop and sign in, then open this panel again."</p>
                            <p class="muted">"For another folder app (Dropbox, OneDrive, iCloud Drive, Syncthing), run in a terminal:"</p>
                            <pre>"rodu team create --folder <the shared folder>"</pre>
                        </div>
                    }
                        .into_any()
                }
                Share::Places(places) => {
                    view! {
                        <h3>"Share with a team"</h3>
                        <p class="muted">
                            "This board is yours alone. Choose where the team's files go: a shared drive suits an organisation, since every member sees it at the same place."
                        </p>
                        <ul class="team-places">
                            {places
                                .into_iter()
                                .map(|place| {
                                    let picked = place.path.clone();
                                    let label = place.label.clone();
                                    let place = StoredValue::new(place);
                                    view! {
                                        <li>
                                            <label>
                                                <input
                                                    type="radio"
                                                    name="team-place"
                                                    prop:checked=move || chosen.with(|c| c.as_ref().is_some_and(|c| c.path == picked))
                                                    on:change=move |_| chosen.set(Some(place.get_value()))
                                                />
                                                <span>{label}</span>
                                            </label>
                                        </li>
                                    }
                                })
                                .collect_view()}
                        </ul>
                        <p class="muted">
                            "The board is encrypted and signed: only people you invite can read it, and your machine admits each of theirs. Its files go in a Rodu folder there."
                        </p>
                        <button type="button" on:click=share>"Share this board"</button>
                    }
                        .into_any()
                }
                Share::Sharing => view! { <p class="loading">"Sharing…"</p> }.into_any(),
                Share::Reopening(message) => {
                    view! {
                        <p class="team-done" role="status">{message}</p>
                        <p class="loading">"Reopening the board…"</p>
                    }
                        .into_any()
                }
                Share::Stopped(e) => {
                    view! {
                        <div class="team-none" role="alert">
                            <p>{format!("Sharing stopped: {}", e.message)}</p>
                            {e.hint.map(|hint| view! { <pre>{hint}</pre> })}
                            <p class="muted">"The board reopened as it is now. Fix that, then reload and share again: it carries on from where it stopped."</p>
                        </div>
                        <button type="button" on:click=move |_| {
                            let _ = window().location().reload();
                        }>"Reload"</button>
                    }
                        .into_any()
                }
                Share::Gone => {
                    view! {
                        <div class="team-none" role="alert">
                            <p>"The board did not come back. Check the terminal running rodu web, then start it again if it stopped."</p>
                        </div>
                    }
                        .into_any()
                }
            }}
        </div>
    }
}

/// Where a join stands.
#[derive(Clone)]
enum Join {
    Form,
    Joining,
    /// Joined, with what the join said; `back` once the board reopened on the new workspace.
    Joined {
        message: String,
        back: bool,
    },
    Gone,
}

/// Joins a team with an invite, where `rodu web` found no workspace (ADR 0004, step 3b).
#[component]
pub fn JoinScreen(api: Api) -> impl IntoView {
    let state = RwSignal::new(Join::Form);
    let code = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let again = RwSignal::new(false);
    let failed = RwSignal::new(None::<ApiError>);

    let join = move |e: ev::SubmitEvent| {
        e.prevent_default();
        let typed = code.get_untracked();
        if typed.trim().is_empty() {
            return;
        }
        if again.get_untracked() && name.get_untracked().trim().is_empty() {
            failed.set(Some(ApiError::local(
                "Give your name on the team to join from this computer",
            )));
            return;
        }
        state.set(Join::Joining);
        failed.set(None);
        spawn_local(async move {
            match api.join(&typed, &name.get_untracked(), again.get_untracked()).await {
                Ok(done) => {
                    // The invite is not kept on the page once it did its work.
                    code.try_set(String::new());
                    // What the join said stays on screen: it holds this machine's code to read
                    // out to the team's owner, which a reload would lose.
                    state.try_set(Join::Joined { message: done.message.clone(), back: false });
                    let next = match wait_for_reopen(api).await {
                        Reopen::Back | Reopen::Never => {
                            Join::Joined { message: done.message, back: true }
                        }
                        Reopen::Gone => Join::Gone,
                    };
                    state.try_set(next);
                }
                Err(e) => {
                    failed.try_set(Some(e));
                    state.try_set(Join::Form);
                }
            }
        });
    };

    view! {
        <main class="empty-state join">
            <h1>"Rodu"</h1>
            {move || match state.get() {
                Join::Form | Join::Joining => {
                    let busy = matches!(state.get(), Join::Joining);
                    view! {
                        <p>"There is no board in this folder yet. Join your team's board with the invite a teammate sent you."</p>
                        <form class="join-form" on:submit=join>
                            <label>
                                <span>"Invite"</span>
                                <textarea
                                    rows="3"
                                    autocomplete="off"
                                    spellcheck="false"
                                    placeholder="rodu2-…"
                                    prop:value=move || code.get()
                                    on:input=move |e| code.set(event_target_value(&e))
                                ></textarea>
                            </label>
                            <label>
                                <span>"Your name on the team"</span>
                                <input
                                    type="text"
                                    autocomplete="off"
                                    placeholder="The invite's name, or this computer's user"
                                    prop:value=move || name.get()
                                    on:input=move |e| name.set(event_target_value(&e))
                                />
                            </label>
                            <label class="join-again">
                                <input
                                    type="checkbox"
                                    prop:checked=move || again.get()
                                    on:change=move |e| again.set(event_target_checked(&e))
                                />
                                <span>"I am already on this team, joining from another computer"</span>
                            </label>
                            {move || failed.get().map(|e| view! {
                                <div class="team-none" role="alert">
                                    <p>{e.message}</p>
                                    {e.hint.map(|hint| view! { <pre>{hint}</pre> })}
                                </div>
                            })}
                            <button type="submit" disabled=busy>{if busy { "Joining…" } else { "Join" }}</button>
                        </form>
                        <p class="muted">"The invite holds the team's key: it stays on this computer. To start a board of your own instead, run in a terminal here: rodu init"</p>
                    }
                        .into_any()
                }
                Join::Joined { message, back } => {
                    view! {
                        <p class="team-done join-done" role="status">{message}</p>
                        {if back {
                            view! {
                                <button type="button" on:click=move |_| {
                                    let _ = window().location().reload();
                                }>"Open the board"</button>
                            }
                                .into_any()
                        } else {
                            view! { <p class="loading">"Opening the board…"</p> }.into_any()
                        }}
                    }
                        .into_any()
                }
                Join::Gone => {
                    view! {
                        <div class="team-none" role="alert">
                            <p>"The board did not come back. Check the terminal running rodu web, then start it again if it stopped."</p>
                        </div>
                    }
                        .into_any()
                }
            }}
        </main>
    }
}

/// How the server behaved after a share.
enum Reopen {
    /// It went away and came back: it reopened.
    Back,
    /// It answered as before: it did not reopen.
    Never,
    /// It went away and did not come back within a minute.
    Gone,
}

/// Watches the server after a share, until it answers as itself again.
async fn wait_for_reopen(api: Api) -> Reopen {
    let mut gone = false;
    for _ in 0..120 {
        match api.me().await {
            Err(e) if e.status == 503 || e.status == 0 => gone = true,
            _ if gone => return Reopen::Back,
            _ => return Reopen::Never,
        }
        sleep(500).await;
    }
    Reopen::Gone
}

async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
}
