# Rodu on Windows: join your team's board

[ภาษาไทย](windows.th.md)

About 15 minutes, once. Afterwards your team's board lives on your machine and syncs with the
team through Google Drive by itself: no server, nothing to sign up for.

## Before you start

- A Google account that the team folder was shared with, as **Editor**.
- The **invite** your team owner sent you privately. It works once, within 7 days.
- PowerShell, which comes with Windows: press Start and type `PowerShell`.

## 1. Install Rodu

Open PowerShell (no need to run it as administrator) and paste:

```powershell
irm https://raw.githubusercontent.com/TheDevper/rodu/v0.5.0/packaging/install.ps1 | iex
```

**Close PowerShell and open it again**, then check that it says `rodu 0.5.0`:

```powershell
rodu --version
```

If you already use [Scoop](https://scoop.sh), you can install with it instead:
`scoop bucket add thedevper https://github.com/TheDevper/scoop-bucket`, then `scoop install rodu`.

## 2. Install Google Drive for desktop

- Download it from [google.com/drive/download](https://www.google.com/drive/download/) and install it.
- Sign in with the Google account the team folder was shared with.
- File Explorer then shows a **Google Drive (G:)** drive with `My Drive` inside.

## 3. Put the team folder in My Drive

A folder someone else shared does not come to your machine until it has a shortcut in My Drive.

- Open [drive.google.com → Shared with me](https://drive.google.com/drive/shared-with-me).
- Right-click the team folder → **Organize → Add shortcut** → **My Drive** → **Add**.
- Wait a moment, then check that the folder is in `G:\My Drive\`.

## 4. Join the team

Make a folder for the board on your machine and join. Paste the invite when Rodu asks for it,
then press Enter.

```powershell
mkdir $HOME\rodu-work
cd $HOME\rodu-work
rodu team join
```

Rodu finds the team folder in Google Drive by itself. Tell your team owner you have joined: their
machine admits yours the next time it syncs. Check with:

```powershell
rodu team members
```

If Rodu cannot find Google Drive, give it the folder:
`rodu team join --folder "G:\My Drive\<team folder>"`.

## 5. Open the board

```powershell
cd $HOME\rodu-work
rodu web
```

Your browser opens the board. Drag cards between columns, click a card to edit it or comment.
Leave the PowerShell window open while you use it, and press `Ctrl+C` when you are done.

For now, run every `rodu` command in the `rodu-work` folder (or a folder inside it).

## 6. Use it from Claude Code (optional)

```powershell
cd $HOME\rodu-work
claude mcp add rodu -- rodu mcp
```

Start `claude` in that folder and ask, for example, "show my work in rodu" or "create a bug
card: the login button does nothing". What the agent changes is recorded as you, via agent.

## Everyday commands

| Command | What it does |
|---|---|
| `rodu web` | Open the board in your browser |
| `rodu add Fix the login page --type bug` | Create a card |
| `rodu ls "assignee = me()"` | Your work |
| `rodu mv DEMO-12 "In Progress"` | Move a card |
| `rodu sync` | Sync now (every command already syncs by itself) |
| `rodu team` | This machine's team status |

A card you create before the owner's machine syncs gets a temporary key such as `DEMO-KQMRTZ`, then
its number later. The temporary key keeps working.

## Updating

When there is a new version, run the install command again. It installs the latest release:

```powershell
irm https://raw.githubusercontent.com/TheDevper/rodu/v0.5.0/packaging/install.ps1 | iex
```

- It works while `rodu web` or Claude Code is running, but they use the new version only once
  restarted: press `Ctrl+C` in `rodu web` and run it again, and restart Claude Code.
- Check with `rodu --version`. Your board and team stay as they were; there is no need to join again.
- Installed with Scoop? Use `scoop update rodu` instead. Use one way of installing, not both.

## Good to know

> **The invite is the team's key.** It opens the board. Keep it like a password: never post it in a
> group chat or pass it on.

- Keep Google Drive for desktop running. Without it your changes do not reach the team, though the
  board on your machine keeps working.
- Never edit, delete, move or rename files in the team folder on Drive yourself. They are encrypted
  and Rodu looks after them.
- A teammate's change shows up within a few seconds to about a minute, depending on Google Drive.
- A card you have open does not refresh when someone else edits it yet: close and reopen it
  ([#27](https://github.com/TheDevper/rodu/issues/27)).
- Hit an error? Send your team owner the whole error message and the output of `rodu team`, but
  **never the invite** or a link with `#token=` in it.
