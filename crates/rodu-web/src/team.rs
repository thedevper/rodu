//! Side panel for the team this workspace syncs with (ADR 0004, step 3a): who is on it, the
//! machines asking to join, the invites waiting, and inviting, admitting and removing. All text
//! from the server is rendered as plain text.

use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rodu_api::{InviteCreated, TeamView};

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
                Loaded::NoTeam(e) => {
                    view! {
                        <div class="team-none">
                            <p>{e.message}</p>
                            {e.hint.map(|hint| view! { <pre>{hint}</pre> })}
                        </div>
                    }
                        .into_any()
                }
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
