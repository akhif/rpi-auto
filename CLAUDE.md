# rpi-auto

Personal build for one car, a 2025 Toyota RAV4 with wired-only Android Auto. A Raspberry Pi Zero 2 W in the car's USB port is a wireless Android Auto dongle while driving and a Kodi player on the car screen when parked. The owner is Akhif. Design, decisions and milestone exit criteria are in `docs/SPEC.md`; read it before any design or code work.

## Terms

- **Drive mode**: stock aa-proxy-rs behaviour. The Pi relays the phone's wireless Android Auto to the head unit over USB.
- **Pi mode** (in code: **source mode**): the Pi itself is the Android Auto mobile device. It sends Kodi's screen as H.264 and its audio to the head unit, and takes touch, speed and GPS back.
- **Trigger**: a user-initiated Disconnect on the phone starts Pi mode; the phone reconnecting ends it. Drive mode always wins.

## Layout

- Repo root: a GitHub fork of [aa-proxy/aa-proxy-rs](https://github.com/aa-proxy/aa-proxy-rs). Our Rust changes live in `src/` alongside upstream's.
- `buildroot/`: upstream [aa-proxy/buildroot](https://github.com/aa-proxy/buildroot), a br2-external tree (Buildroot itself is a submodule), imported with `git subtree`. It builds the SD image.
- `docs/`: `SPEC.md` plus design notes.

Upstream updates: aa-proxy-rs through GitHub's "Sync fork" or `git fetch upstream && git merge upstream/main` (remote `upstream` = https://github.com/aa-proxy/aa-proxy-rs); Buildroot through `git subtree pull --prefix=buildroot https://github.com/aa-proxy/buildroot main --squash`.

## How work flows

- The **planner** (Akhif with Claude in claude.ai) owns `docs/SPEC.md`, the GitHub milestones and the issues. Each issue labelled `task` is one self-contained brief; issues labelled `manual` are Akhif's own steps.
- An **executor** session takes exactly one `task` issue, works on the branch the session assigns, and opens a PR titled `T<NN>: <summary>` whose description includes `Closes #<issue>`.
- The planner reviews and merges. PRs are merged with a merge commit, never squashed, because `git subtree pull` needs the subtree merge history intact.
- Every PR description ends with two sections:
  - **On-device test plan**: numbered steps Akhif runs on the Pi or in the car, covering what to flash, what to tap, what to expect, and the log lines that confirm it (`/var/log/aa-proxy-rs.log`). Write "None" when nothing on the device changed.
  - **Plan notes**: anything that should change the spec, later tasks or estimates. Executors propose there; the planner edits the spec and the issues.
- Work outside the issue's scope goes into Plan notes as a proposal, and the PR stays within scope.

## Hardware in the loop

The agent cannot reach the Pi or the car; Akhif runs every on-device test. Desk testing goes through Google's Desktop Head Unit (DHU); see the README section "Connecting to Desktop Head Unit (DHU)". SD images build in GitHub Actions (issue T03 sets this up), so a local Buildroot build in a cloud session is a last resort.

## Guardrails

- Certificate and key files (`*.pem`) stay on the device under `/etc/aa-proxy-rs/`. `.gitignore` lists `*.pem`, and key material stays out of every commit, test fixture and log excerpt.
- Source mode never enables `video_in_motion`. It pauses video when the head unit reports a speed above 0 km/h and keeps audio playing.
- Drive mode keeps working at every milestone. Source mode sits behind a config flag that defaults to off.
- Upstream files stay as close to upstream as possible: new behaviour goes in new modules with small hooks into existing ones, and `README.md` stays upstream's, so upstream merges stay clean.
- Run `./setup-dev.sh` once per clone; its pre-commit hook enforces `cargo fmt`.

## Suggested skills

- `grill`: to stress-test a design with Akhif before coding.
- `writing-for-agents`: when editing this file.
- `handoff`: at the end of a session, so the next one starts from the current state.
