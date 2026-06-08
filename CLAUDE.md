> Global rules apply. Agent Directory routing lives in `~/.claude/CLAUDE.md` — read that first. This project participates in the directory; use `/send-to` to ask peers.

# lightshot-we-deserve — Project Context

MemPalace wing: `lightshot-we-deserve` (protocol lives in `~/.claude/CLAUDE.md`).

## Purpose
An open-source alternative to lightshot - Windows-based desktop app to make screenshots

## Stack
static

## Surface
- URL: (local-only, no public URL)
- Audience: local
- Loopback port: 5041 → container app port
- Container name: `lightshot-we-deserve`
- Compose root: `/home/wolf/projects/lightshot-we-deserve/`

## Onboarding (read me first if you're a new agent here)
You are inside Wolf's Server. The team and routing rules live in `~/.claude/CLAUDE.md` (The Order, QA, Researcher, Voice Agent). Wolf is a Product Manager — communicate in features and outcomes, never in code/file paths/jargon. ~5-line ceiling in chat. Russian = he's tired, reply in English.

## Conventions inherited from the host
- Compose binds `127.0.0.1:5041` only. Never `0.0.0.0`.
- Watchtower opt-out label on every service (Watchtower removed from host; label is defense-in-depth).
- Healthcheck on the long-running service.
- Server-side source of truth. Git is checkpoints + sync, not deploy.
- Commit identity: `manager` / `alexanyanwolf@gmail.com`.

## Build & Release (Windows .exe)
- This repo is wired directly to GitHub: `origin` remote + a write-capable deploy key live on the host. Repo is public.
- ⚠️ Authorization boundary: pushing/tagging requires the host wrapper `/data/bin/host`, which is a host-root control. Global rules (`~/.claude/CLAUDE.md`) forbid non-Order agents (CWD ≠ `/workspace/wolfs-server`) from running host-root wrappers, even though they're reachable. The Order has *verbally* asked this wing to self-publish, but that contradicts the durable written boundary. Until the global rulebook is updated to carve out this exception, route push/tag/release via `SEND TO @TheOrder`. Do NOT run `/data/bin/host` unilaterally.
- Mechanism (for reference, once authorized): push+tag → `/data/bin/host 'cd ~/projects/lightshot-we-deserve && GIT_SSH_COMMAND="ssh -F ~/.ssh/config" git push origin master --tags'`; fetch release link → `/data/bin/host 'curl -sS https://api.github.com/repos/manager/lightshot-we-deserve/releases/latest | grep browser_download_url'`.
- CI: GitHub Actions builds on `windows-latest` on every push to master and on `v*` tags. Tagging `v*` produces a GitHub Release with the NSIS installer `.exe` attached — that Release asset is the download link to give Wolf.
- Dev container is Linux and cannot build/run the Windows app; local verification is `cargo check` only (needs GTK/webkit/xcb/pipewire + `libclang-dev`/`clang`, `LIBCLANG_PATH=/usr/lib/llvm-14/lib`).

## Vendor credits in use
(none — does not consume vendor API credits)

## Sunset checklist
1. Confirm with Wolf.
2. `docker compose down -v` to drop volumes.
3. Revoke CF Access app + drop tunnel hostname + delete DNS record (manifest at /var/lib/new-project-state/lightshot-we-deserve.json holds the IDs).
4. Remove Apex tile + OPS LOG entry.
5. Archive GitHub repo.
6. Seal MemPalace wing.

## MemPalace usage (wing: )
When you find yourself stuck > 10 minutes on a problem and figure it out, write a brief drawer in your wing — chronology + fix. Next-session-you won't waste the same 10 minutes. Same when a deployment/config decision is non-obvious — capture *why* alongside *what*.
