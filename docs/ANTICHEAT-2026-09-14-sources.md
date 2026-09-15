# RICOCHET / Activision exposure research — consolidated report

**Compiled** 2026-09-14. **Subject:** telemouse, a passive Windows mouse-telemetry recorder
(raw-input capture via `RIDEV_INPUTSINK`, JSONL recording, UDP/WebSocket streaming to a browser
dashboard and an OBS browser-source overlay, offline aim metrics). **Question:** could anything in
it trigger RICOCHET anti-cheat, a shadowban, or violate Activision's Security & Enforcement Policy?

**Evidence tiers used throughout:**

- `[OFFICIAL]` — Activision / Call of Duty / Blizzard News / Activision Support / an Activision employee on the record.
- `[JOURNALISM]` — established outlet with an identifiable author.
- `[VENDOR]` — the maintainer or publisher of the software in question (not Activision).
- `[COMMUNITY]` — Reddit, Steam forums, GitHub issues, vendor forums. Uncorroborated unless stated.
- `[NOT FOUND]` — searched, no evidence either way. A bounded negative, not proof.

**Source access caveats.** `www.callofduty.com` and `www.activision.com` refuse automated fetches, so
several official progress-report posts are cited through the `news.blizzard.com` mirror or through
outlets quoting them verbatim. Three things could not be retrieved at all and are flagged in §9:
Activision's **Software Terms of Use** (the binding contract), the `support.activision.com/ricochet-anti-cheat`
page, and the IGN interview URL of 2026-09-10.

**Sources deliberately excluded.** `prismnews.com`, `backgrind.com`, `tier1settings.com`,
`hwidchange.com`, `mitchcactus.co`, `tateware.com`, `disgg.com`, `skyrant.net`, `gamemodifier.com`,
`boosting-ground.com`, `unbanster.com`, `tracexhwidspoofer.com`, `nohboard.org`, `nohboard.net`,
`tech-insider.org`. These are SEO/AI content farms or cheat/HWID-spoofer-adjacent. Two specific
claims originating there are **unverified and should be treated as false**: that RICOCHET has
"documented false-positive 'gameplay enhancement' bans on setups running OBS-class overlay software",
and that "Steam Input is whitelisted for CoD". `nohboard.org`/`.net` are **not** the NohBoard
project's sites — NohBoard exists only on GitHub — and must not be cited as maintainer statements.

---

## 1. Executive summary

**What actually produces enforcement** falls into three buckets, all of which require the software to
interact with the game, or the account to look anomalous:

1. **Touching the game** — reading or writing game memory, injection, modifying game data on disk or
   in memory. This is the only thing the RICOCHET kernel driver is officially described as looking
   for: it monitors processes interacting with a game "to determine if they are **manipulating** the
   game."
2. **Modifying input** — hardware passthroughs (Cronus Zen, XIM, ReaSnow) and software that
   synthesizes, remaps or emulates input. Since February 2026 this is detected by analysing **input
   behaviour** — timing, consistency, response patterns — rather than by fingerprinting the device.
3. **Account-level signals** — improbable statistics, a sudden change in account behaviour, failed
   hardware attestation. These drive Limited Matchmaking, which Activision explicitly frames as an
   *investigation state*, not a cheating verdict.

**The sharpest line the evidence supports is read versus modify.** Every documented adverse outcome in
this audit — reWASD, Cronus/XIM/ReaSnow, QuadStick, Interception under other anti-cheats — involves
software or hardware that **modifies, injects, emulates, filters or hides** input. **No evidence was
found, at any tier, of RICOCHET acting against a tool that merely reads raw input.** That is a gap in
the public record, not an official clearance.

**What is folklore.** That merely having a third-party program installed gets you banned. There is
**no surviving documented case of any software category causing a ban or shadowban by presence
alone.** The 2022 "RGB software" theory — the strongest version of this claim — is contradicted by
contemporaneous journalism attributing the same ban wave to crash-induced false positives (§5).
Activision has also explicitly ruled out mass reporting as a trigger.

**One real qualifier.** For **playability** rather than enforcement, installation alone has been
sufficient at least once: reWASD's vendor states Activision "made a one-sided decision to make their
games unworkable if reWASD is **installed**" (§5). So "only what interacts with the game matters"
holds for bans, not for whether the game launches.

**The important asymmetry.** Activision publishes **no allowlist and no guidance whatsoever** for
legitimate third-party software. The Security & Enforcement Policy and the RICOCHET overview say
nothing about overlays, streaming tools, recording software, VPNs, macros or passive tools. The sole
named approval in Call of Duty's history is **Cephable** (§6) — bespoke, negotiated, confined to
non-competitive modes, and still revocable. Contrast BattlEye, a different vendor, which states
outright that passive programs are never a ban reason. So "safe" can only ever mean *outside every
published prohibition and every described detection surface* — never *endorsed*.

**Documented false-positive causes**, for calibration: an exploitable naive string-signature scan
(Oct 2024), an internal "overlap between separate detections" (Sept 2026), behavioural input analysis
misfiring on an accessibility controller (May 2026), and — community-reported only — a ReFS
filesystem interaction (Dec 2025). **None was caused by legitimate software running on the victim's PC.**

**Bottom line for telemouse:** nothing in the shipped binaries falls in buckets 1, 2 or 3. The single
genuine policy risk in the repository is `tmbench inject`, which is not shipped. The
best-evidenced real-world hazard for this software category is not anti-cheat at all — it is
**antivirus keylogger heuristics** against unsigned input-capture binaries (§8).

---

## 2. Security & Enforcement Policy — exact definitions and penalty ladder

**Source:** <https://support.activision.com/articles/call-of-duty-security-and-enforcement-policy> —
**last updated 07/31/26**. All quotes `[OFFICIAL]`, verified by direct fetch.

### Unauthorized software — the general definition

> "Utilizing any code and/or software not authorized by Activision that can be used in connection with
> the game and/or any component or feature thereof which changes and/or facilitates the gameplay or
> other activity, including to gain an unfair advantage, manipulate stats, and/or manipulate game data."

### The enumerated list (this is where "manipulation of game data" lives)

> "This includes, but is not limited to, aimbots, wallhacks, trainers, stats hacks, texture hacks,
> leaderboard hacks, injectors, **input mapping software**, or any other software used to
> **deliberately modify game data on disk or in memory**."

The operative verb throughout is **modify**. There is no prohibition anywhere on reading, observing,
or recording. There is **no standalone definition of "manipulation of game data"** separate from this
clause.

**`input mapping software`** sits flatly alongside aimbots with no qualification. It is the single
enumerated phrase most likely to be misapplied to a passive mouse tool, and worth pre-empting
explicitly in any compliance documentation.

### Unauthorized peripherals / devices

> "Utilizing an unsupported external hardware device or application to interact with the game and use
> for cheating. Unsupported peripheral devices and applications include, but are not limited to,
> unapproved input modification devices, modded controllers, IP flooders, and lag switches."

**Cronus, XIM and Strike Pack are NOT named in the policy document.** They appear only in RICOCHET
blog posts (§3).

### Other relevant categories

- **Circumventing Security:** "Any attempt to circumvent our security systems."
- **Spoofing:** "Any attempt to hide, disguise, or obfuscate your identity or the identity of your
  hardware devices."
- **Decompiling/Reverse Engineering** is listed under Exploits — this describes reverse-engineering
  *the game*, not owning a debugger.

### What the policy does NOT mention

**Overlays, streaming tools, recording software, VPNs, macros, scripts, rapid fire, and passive tools
appear nowhere on the page.** Verified twice by direct query. The words "macro", "script" and
"rapid fire" do not appear at all.

### Penalty ladder (the policy's own ordering)

1. **Warnings**
2. **Temporary and Permanent Bans**
3. **Limited Matchmaking** — "Accounts in this state may be placed in limited matchmaking lobbies with
   other accounts in a similar state. Members of a player's party may also experience limited
   matchmaking lobbies while partied with a player in this state."
4. **Ranked Play Restrictions** — "Restrictions may last for the remainder of an active Season, carry
   over to multiple upcoming Seasons, or be permanent across future Call of Duty titles." Party
   members can be affected too.
5. **RICOCHET Anti-Cheat™ In-game Mitigations** — "While RICOCHET Anti-Cheat™ is used for data
   collection and machine learning to reduce cheating, the system also includes several in-game
   mitigations to identify and hinder cheaters."
6. **Hardware Bans** — device-level, cross-title, not displayed to the player.

### Appeals — <https://support.activision.com/ban-appeal> `[OFFICIAL]`, no date shown

- "Temporary bans and accounts in a limited matchmaking state **cannot be appealed**."
- "Hardware bans are not displayed or eligible for appeal."
- **Permanent bans are appealable only where the infraction resulted from account compromise.**
- Response "within 8 hours", up to 3 business days where unauthorized activity is detected.

The practical consequence, borne out by the QuadStick case (§6): a wrongly-issued *temporary* ban has
no formal remedy at all. The only route is publicity.

### Not retrieved

**Activision Software Terms of Use** — <https://www.activision.com/legal/software-terms-of-use>.
`[NOT FOUND]` — host refuses automated retrieval (repeated ECONNRESET). This is the legally binding
document and is likely to contain broader reverse-engineering and data-interception clauses than the
support-site policy. **Read manually before relying on this report for a compliance conclusion.**

---

## 3. RICOCHET's publicly described detection surfaces

### The kernel driver — official statements

- "The driver monitors the machine and processes interacting with a game using RICOCHET Anti-Cheat to
  **determine if they are manipulating the game**."
  — <https://support.activision.com/articles/ricochet-overview>, **last updated 11/10/25** `[OFFICIAL]`
- Original announcement, identical in substance: "The driver monitors the machine and processes
  interacting with Call of Duty: Warzone to determine if they are manipulating the game."
  — <https://news.blizzard.com/en-us/article/23733251/ricochet-anti-cheat-call-of-dutys-new-anti-cheat-initiative>,
  **Oct 2021** `[OFFICIAL]`
- Lifecycle: "The driver shuts down when you exit the game and turns on when you start a new game."
  It is required to play and cannot be opted out of. `[OFFICIAL]`

**Broader scope statement — important, and it limits how strongly anything can be called "safe".**
The same overview page also describes RICOCHET as working through "client- and server-side detection
systems, **monitoring applications running on a machine during gameplay**, detecting anomalies in
gameplay using behavioral models, validating that hardware has not been tampered with." `[OFFICIAL]`

That phrase reserves the right to observe software that never touches the game process. It does not
say what is done with the observation, and no documented case exists of a non-interacting application
being actioned — but it means the honest ceiling for any third-party tool is "unnamed and
unenforced", not "documented-safe".

**What the overview page does NOT contain:** anything about legitimate third-party software,
overlays, streaming or recording tools, peripherals, what to do if a legitimate program conflicts, or
any explanation of how the driver distinguishes authorized from unauthorized programs. Verified by
direct query.

### Mitigations — Splat, Cloaking, Disarm, Damage Shield, Hallucinations

Named on the RICOCHET overview page `[OFFICIAL]`. Behaviour corroborated by XDA, Coleman Hamstead,
**2025-04-04**, <https://www.xda-developers.com/activision-vs-call-of-duty-cheaters/> `[JOURNALISM]`:

| Mitigation | Effect |
|---|---|
| **Splat** | Disables the cheater's parachute mid-air. |
| **Disarm** | Strips weapons and equipment, including melee and fists. |
| **Damage Shield** | Legitimate players take little or no damage from a flagged cheater. |
| **Cloaking** | Legitimate players become invisible and silent to a flagged cheater. |
| **Hallucinations** | Decoy characters visible only to flagged cheaters; stated not to affect legitimate players' aim, progression or stats. |

**Critical point: all of these activate only after a player is flagged.** They are consequences, not
detectors. A tool that is never flagged never encounters them.

- "Damage Shield, Disarm, Splat, Hallucination, and others will be live" at Black Ops 6 launch.
  — <https://news.blizzard.com/en-us/blizzard/24150099/ricochet-anti-cheat-progress-report-black-ops-6-launch>,
  **Oct 2024** `[OFFICIAL]`

### 2023 — third-party hardware device detection

- RICOCHET Season 03 (MWII / Warzone 2.0), quoted by Engadget, Steve Dent, **2023-04-05**,
  <https://www.engadget.com/call-of-duty-can-detect-and-ban-xim-style-cheat-hardware-100314416.html>
  `[OFFICIAL via JOURNALISM]`: "These devices act as a passthrough for controllers on PC and console
  and, **when used improperly or maliciously**, can provide a player with the ability to gain an
  unfair gameplay advantage, such as reducing or eliminating recoil."
- Targets named: **XIM, Cronus Zen, ReaSnow S1**.
- Enforcement ladder: an "unsupported device warning" → in-game mitigations → permanent bans across
  all Call of Duty titles.

### January 2024 — mouse-and-keyboard aim-assist tools

The most-cited precedent involving commercial consumer software. **Read the attribution carefully.**

- RICOCHET announcement, **2024-01-16** `[OFFICIAL]`: "Our security detection systems now target
  players using tools to activate aim assist while using a mouse and keyboard. **The Call of Duty
  application will close if detected.** Repeated use of these tools may lead to further account action."
- **The official statement names no software.** Attribution to reWASD in press coverage is editorial
  inference.
  — TechSpot, **2024-01-17**, <https://www.techspot.com/news/101549-call-duty-anti-cheat-system-now-close-game.html> `[JOURNALISM]`
  — Dexerto, **2024-01-16**, <https://www.dexerto.com/call-of-duty/cod-devs-finally-clamp-down-on-mouse-and-keyboard-aim-assist-cheats-plaguing-warzone-2480652/> `[JOURNALISM]`
- **Confirmation comes from the vendor instead, on stronger terms** — see §5 (reWASD row).
- Note the ladder: **game close first**, account action only on repetition.

### Machine learning, behavioural models, and the Replay Investigation Tool

- **MWIII progress report, Nov 2023** `[OFFICIAL]` (callofduty.com/blog/2023/11/…, not directly
  fetchable): machine learning activated across RICOCHET-protected titles; a model trained to
  identify wallhacks, "raging" and similar; a single PC can review up to **1,000 clips/day**, and
  flagged clips are routed to a **human reviewer**.
- **Season 01 report, Nov 2024** `[OFFICIAL]`,
  <https://news.blizzard.com/en-us/article/24151773/ricochet-anti-cheattm-progress-report-season-01-and-ranked-play>:
  "a new behavioral model to analyze for anomalous skill has recorded and categorized over
  **4.4 million data points per hour** at its peak"; "RICOCHET Anti-Cheat also uses its *Replay
  Investigation* Tool, with the ability to watch any completed match, to monitor replays of top players."
- **BO6 launch report, Oct 2024** `[OFFICIAL]`: "machine-learning behavioral systems, focused on speed
  of detection"; "machine-learning detection models to analyze gameplay to combat aim bots";
  "Third-party hardware detections and more".

### Signature scanning — a documented weakness

- TechCrunch, Lorenzo Franceschi-Bicchierai, **2024-11-07**,
  <https://techcrunch.com/2024/11/07/hacker-says-they-banned-thousands-of-call-of-duty-gamers-by-abusing-anti-cheat-flaw>
  `[JOURNALISM]`: RICOCHET was scanning for **hardcoded text strings as signatures** in game memory,
  "banning players when these strings appeared regardless of context." Sending the phrase
  "Trigger Bot" as a private message or as part of an online ID in a friend request caused innocent
  players to be banned. The cheat developer claimed "thousands upon thousands"; Activision said
  "a small number".
- Corroborated: Insider Gaming, **2024-10-18**, <https://insider-gaming.com/call-of-duty-anti-cheat/>
  `[JOURNALISM]` — streamer BobbyPoff among those falsely banned and restored.

### February 2026 — the shift from device fingerprint to input behaviour

The most consequential recent change.

- TeamRICOCHET Season 02 update, **2026-02-02** `[OFFICIAL]`,
  <https://news.blizzard.com/en-us/article/24243445/teamricochet-season-02-update>: detections
  "focus on how inputs behave, not which device is plugged in… analyzes input timing, consistency,
  and response patterns".
- Quoted via Dexerto, **2026-02-02**,
  <https://www.dexerto.com/call-of-duty/cod-cracks-down-on-cronus-zen-xim-in-major-anti-cheat-update-for-black-ops-7-season-2-3313252/>
  `[JOURNALISM]`: the goal is to "distinguish between natural human play and machine modified input",
  because these devices "can be endlessly customized" and have no single fingerprint. Activision:
  **"These devices are not permitted in Call of Duty. They are cheating tools, even if they masquerade
  as accessibility devices."** Named: **Cronus Zen, XIM Matrix**. Live from **2026-02-05**.
- **Why it matters here:** the detector reasons about the input stream the *game* receives. A passive
  listener does not alter that stream and cannot influence it. But see the QuadStick case (§6) for its
  error mode.

### 2025–2026 platform changes

- **TPM 2.0 + Secure Boot** introduced with BO6 / Warzone **Season 05, 2025-08-07**; mandatory for the
  Black Ops 7 Beta and launch.
- Current requirement page, <https://support.activision.com/articles/trusted-platform-module-and-secure-boot>,
  **last updated 08/27/26** `[OFFICIAL]`: required for **Black Ops 7, Warzone, and the Modern Warfare 4
  Beta**. Non-compliant on BO7/Warzone → "restricted from accessing certain game modes (including
  Ranked Play) and playlists and may be placed in separate matchmaking pools with other players who do
  not meet these requirements." Non-compliant on **MW4 Beta → "restricted from playing any online game
  mode."** Windows 10 22H2+ or Windows 11. Attestation tool v1.1.3 (2026-08-06). Rationale: "TPM 2.0
  and Secure Boot help protect your system against cheats and unauthorized access by verifying your
  PC's integrity from startup to gameplay." **No mention of virtual machines or emulated TPM anywhere.**
- **Season 03, Apr 2026** `[JOURNALISM]` GameRant, **2026-04-02**,
  <https://gamerant.com/call-of-duty-black-ops-7-season-3-anti-cheat-update/>: "expanded its device
  detections"; **SMS two-factor authentication now required for newly created free-to-play PC
  Activision accounts**, expanding to existing accounts; "updated attestation messaging".
- **Season 04, Jun 2026** `[OFFICIAL]`: players failing **Microsoft Azure Attestation** are placed in a
  separate matchmaking pool, limited to **Nuketown 24/7** in BO7 and **Battle Royale Casual** in
  Warzone, because the non-attested population is small.
- **Sept 2026:** 70,000 hardware bans in one week across BO7/Warzone; 293,000 accounts banned
  year-to-date through June `[JOURNALISM]`. Activision reports roughly two-thirds of players
  temp-banned for scripted input devices return without the scripts.
- **Driver-signing:** Secure Boot verifies the boot chain and prevents test-signed or unsigned kernel
  drivers from loading. It is **not** a per-application allowlist and does not block signed
  third-party drivers. Microsoft's vulnerable-driver blocklist is a separate, Microsoft-maintained
  mechanism. No Activision statement ties either to third-party user-mode software.
- **"Ricochet ban waves affecting legitimate software":** `[NOT FOUND]`. No such event is documented.

---

## 4. Shadowban / Limited Matchmaking — triggers and appeal rules

### Official framing: an investigation state, not a verdict

- Activision developers via Dexerto, **2025-03-28**,
  <https://www.dexerto.com/call-of-duty/black-ops-6-warzone-devs-finally-explain-how-spam-reports-shadowbans-work-3172211/>
  `[OFFICIAL via JOURNALISM]`:
  - "Being placed in Limited Matchmaking doesn't signal someone is a confirmed cheater but **an alarm
    was raised**."
  - Triggers given: "**a major change in an account's behavior** or if **a brand-new account is
    dropping improbable stats**."
  - **Mass reporting is explicitly ruled out:** "Whether it's in-game or if a cheat developer creates
    a hack to submit 10,000 reports, **spam reporting does nothing**." This kills a large piece of
    community folklore.
  - **Under 0.15% of players** are in Limited Matchmaking at any time.
  - **No software of any kind is named as a trigger.**
- David Andrews, Senior Director of Game Security for Call of Duty, IGN interview **2026-09-10**
  (quoted in secondary coverage; IGN itself was not crawlable) `[OFFICIAL statement, JOURNALISM
  channel]`: "we have limited matchmaking that we will put a suspicious account into to isolate them
  while other checks are either running or we're waiting on other data… we're kind of doing this
  review process of looking at that account more holistically and seeing what's really going on here."
- **Party contagion is official** — partying with a flagged account can place you in limited lobbies
  too (§2).

### The central question

**Is any software category on the PC known to cause a shadowban by itself, without interacting with
the game?**

`[NOT FOUND]`. No official statement, no journalism, and no corroborated community case establishes
this. The two uncorroborated claims that exist — Raw Accel (refuted by the project within 22 minutes)
and DS4Windows (two sourceless Steam posts in a thread conflating it with reWASD) — are covered in §5.
This is a bounded negative finding, not proof.

### Failed attestation is a different mechanism

Restriction to a separate matchmaking pool for failing TPM 2.0 / Secure Boot / Azure Attestation is
**not** a shadowban. It is a published hardware-compliance gate with a distinct in-game message
("Failed Attestation Status") and a distinct remedy (enable the features). See §3.

### Appeal rules

Temporary bans and limited-matchmaking states are **formally non-appealable**; hardware bans are not
displayed and not appealable; permanent bans are appealable only where caused by account compromise.
See §2.

### Documented false positives

| Date | Incident | Cause | Outcome |
|---|---|---|---|
| **2024-10-17** | "Trigger Bot" string exploit — cheat developers could ban arbitrary players by sending text | **Naive string-signature scanning in RICOCHET itself**, weaponised externally. Not caused by software on the victim's PC. | Activision "identified and disabled a workaround to a detection system… that impacted a small number of legitimate player accounts"; all accounts restored. `[OFFICIAL via JOURNALISM]` <https://www.dexerto.com/call-of-duty/cod-developers-admit-ricochet-anti-cheat-incorrectly-banned-accounts-2953378/>; <https://www.gamespot.com/articles/call-of-duty-anti-cheat-was-falsely-banning-legitimate-players-but-all-accounts-have-been-restored/1100-6527252/> |
| **2026-05-23** | **QuadStick** sip-and-puff accessibility controller drew a real temporary ban citing "third-party input modification device" | **Behavioural input analysis misfiring** on legitimate but atypical input | Reversed only after public escalation. `[JOURNALISM]` <https://gamerant.com/cod-paralyzed-streamer-banned/> |
| **2026-09-04** | Pro player Matthew "FormaL" Piper permanently banned during the MW4 Beta | Activision: flagged "due to a **unique overlap between separate detections**". No third-party software implicated. | Reverted; underlying issue said to be fixed. `[JOURNALISM]` <https://www.techtroduce.com/call-of-duty-ricochet-false-ban-formal-mw4-beta>, corroborated by CharlieIntel |
| **2025-12-03** | Repeated temporary bans on BO7 for users running **Windows 11 25H2 with a ReFS-formatted system drive**, reportedly while sitting in the main menu without joining a match | Unknown | Limited corroboration, no Activision response. `[COMMUNITY, uncorroborated]` <https://steamcommunity.com/app/1938090/discussions/0/685239361818616794> — included because it is the only report of a pure *environment* allegedly triggering enforcement |

**None of these was caused by legitimate software running on the affected player's PC.**

---

## 5. Software-category table

Worst *documented* outcome. Three outcomes are distinguished throughout, because folklore conflates
them: **(a)** game crashes / refuses to launch / closes; **(b)** shadowban or restricted matchmaking;
**(c)** account ban.

### Input / driver layer

| Software | Worst documented outcome | Evidence | Tier |
|---|---|---|---|
| **Raw Accel** (`rawaccel.sys`, kernel mouse-accel driver) — *closest analogue to a low-level mouse tool* | **Nothing official. One uncorroborated shadowban claim, refuted by the project the same day.** Never a launch block, never a documented ban. | Issue #238, opened and closed **2024-09-13**: *"Dont use rawaccel in Call of Duty I got instantly flagged and shadowbanned"* → closed 22 minutes later by contributor JacobPalecki: *"Raw Accel is anti-cheat safe and so far has not resulted in any confirmed bans… There is no further reason to discuss this until there is either a statistically significant number of players getting banned (which is not happening in our mouse accel community that has many COD players)."* A GitHub issue-search across the whole repo for `"call of duty" OR ricochet OR warzone OR "black ops"` returns **total_count 1** — that issue alone. The project FAQ claims anti-cheat safety for *"FaceIT, Valorant, and Diabotical"* and **never mentions CoD**. <https://github.com/RawAccelOfficial/rawaccel/issues/238>, <https://github.com/RawAccelOfficial/rawaccel/blob/master/doc/FAQ.md> | `[COMMUNITY]` single, refuted |
| **Interception** (oblitum, kernel input filter driver) | **No CoD evidence in either direction.** Blocked by *other* anti-cheats, always as **(a) game-won't-launch**, never a ban. | Issue-search on `oblitum/Interception` for CoD terms = **total_count 0**. FACEIT blocks it — #170, **2023-08-14**: *"FACEIT… popped up an error saying that the mouse.sys driver was forbidden. turns out that interception was the issue"*. EAC/Fortnite — #202, **2025-06-11**: *"Please close Interception before starting the game"*. Last release **2017-05-12**; its own README advertises *"In game applications like BOTs"*, which is why other anti-cheats block it. <https://github.com/oblitum/Interception/issues/170>, <https://github.com/oblitum/Interception/issues/202> | `[COMMUNITY]` for non-CoD; `[NOT FOUND]` for CoD |
| **reWASD** | **(a) game auto-closes**, escalating to **(c) account action on repeat use**. Never a shadowban. **Installation alone reportedly makes the game unworkable.** | Official statement (2024-01-16) **names no software**; attribution is editorial. Vendor confirmation, **2024-02-05**: *"Activision and EA, made a one-sided decision to make their games unworkable if reWASD is **installed**"* and *"we do not recommend buying reWASD for such games as the Call Of Duty series."* <https://www.rewasd.com/blog/post/cancel-culture> | `[OFFICIAL]` + `[VENDOR]` + `[JOURNALISM]` |
| **ViGEmBus / HidHide** | **No evidence found.** Unknown, not cleared. ViGEmBus repo is archived/EOL (last push 2023-11-02); HidHide is active and self-describes as a *"Gaming Input Peripherals Device Firewall for Windows."* | — | `[NOT FOUND]` |
| **AutoHotkey** | **One disputed permanent-ban claim**, script *running*, not merely installed; replies dispute it and nobody corroborates. **The policy contains no "macro", "script" or "rapid fire" language at all** (verified twice). | Steam, **2023-05-19**, <https://steamcommunity.com/app/1938090/discussions/0/3829793451747354019/> | `[COMMUNITY]` contested |
| **Logitech G HUB** | **(a) game-won't-launch** — a Memory Integrity / Core Isolation **driver conflict**, not an anti-cheat verdict. No ban evidence. | Steam thread **2024-09-01**, 2–3 users confirm the fix, <https://steamcommunity.com/app/1938090/discussions/0/4757577823500255211/> | `[COMMUNITY]` corroborated |
| **Razer Synapse** | **None — Activision names it positively.** *"If you are using Razer Synapse, make sure to update to the latest driver."* The same support family separately tells players to *disable* Razer Cortex, NZXT CAM and MSI Afterburner for stability. | <https://support.activision.com/warzone-2/articles/warzone-2-pc-troubleshooting>, **03/11/26** | `[OFFICIAL]` |
| **G HUB / iCUE / Synapse "ban wave" folklore (2022)** | **Downgraded to contradicted folklore.** | Traces to one Razer Insider thread, **2022-12-19**, no staff reply, no primary source. The Tom Henderson survey (500+ banned players; G HUB 70% vs 45% control, Synapse 40%, iCUE 28%) carried its own caveat: *"Such data does not guarantee that false bans are occurring because of RGB programs."* Contemporaneous reporting attributed the same MW2 wave to **crash-induced false positives**, not peripheral software. Activision never confirmed or denied. <https://insider.razer.com/general-discussion-6/anti-cheat-flagging-software-as-unauthorized-modification-or-software-41919>; <https://www.thegamer.com/call-of-duty-modern-warfare-2-banned-using-rgb-software/> (2022-10-31); <https://insider-gaming.com/data-suggests-rgb-software-could-be-getting-you-banned-in-call-of-duty/> (2022-11-04); <https://www.nme.com/news/gaming-news/modern-warfare-2-players-hit-with-perma-bans-due-to-faulty-anti-cheat-software-3368068> (**2022-12-16**) | `[COMMUNITY]` contradicted by `[JOURNALISM]` |
| **Mouse-test utilities** — MouseTester / MouseTester Reloaded, Mouse Rate Checker, Zowie/BenQ tools, Razer HyperPolling, VIA / QMK / QMK Toolbox | **No evidence found in any category** — not one report of a flag, kick, shadowban or ban. Zowie configures driverless (no resident app); QMK/VIA live in firmware. | — | `[NOT FOUND]` |
| **Aim trainers** — Aim Lab, KovaaK's; sensitivity converters | **No evidence found — not even a single unverified post.** Aim Lab's only CoD tie is a CDL marketing sponsorship (**2022-03-14**) with no anti-cheat clause and no in-game integration. mouse-sensitivity.com verified as a pure browser form, no installer. | <https://investor.activision.com/news-releases/news-release-details/call-duty-leaguetm-joins-forces-aim-lab-multi-year-deal-become> | `[OFFICIAL]` sponsorship only; otherwise `[NOT FOUND]` |
| **Steam Input** | **No evidence either way — unknown, not safe.** Mechanism note: Valve's own docs say the overlay *"will hook… XInput, DirectInput, **RawInput**, and Windows.Gaming.Input and inject an emulated Xbox controller device"* — structurally the same class targeted in Jan 2024. | <https://partner.steamgames.com/doc/features/steam_controller/steam_input_gamepad_emulation_bestpractices>. **Strike any "Steam Input is whitelisted for CoD" claim** — traces to an SEO/affiliate page. | `[NOT FOUND]` |
| **DS4Windows** | **Shadowban claimed, uncorroborated and contradicted.** Two Steam users assert it with no source, in a thread that visibly conflates DS4Windows with reWASD; other users report it working fine, including under BO7. | <https://steamcommunity.com/app/1938090/discussions/0/4763208232584874724/> | `[COMMUNITY]` contradicted |
| **x360ce** | **No evidence found.** Only pre-RICOCHET (MW2/MW3-era) discussion; no bearing on 2023–2026. | — | `[NOT FOUND]` |
| **Cronus Zen, XIM (incl. XIM Matrix), ReaSnow S1** | **(c) full ladder** — warning → mitigations → temporary ban → permanent and hardware ban, cross-title. Activision: *"They are cheating tools, even if they masquerade as accessibility devices."* | Engadget **2023-04-05**; Dexerto **2026-02-02**; TeamRICOCHET Season 02 **2026-02-02** | `[OFFICIAL]` |
| **Lag switches / IP flooders** | **(c) ban** — explicitly named infraction. | Security & Enforcement Policy | `[OFFICIAL]` |

### Overlay, capture, monitoring and developer tools

| Software | Worst documented outcome | Evidence | Tier |
|---|---|---|---|
| **OBS Studio** (incl. browser sources and overlays) | **Nothing.** The OBS Project forum's entire `call-of-duty` tag — 13 threads spanning Oct 2019 – Sep 2024 — contains **zero** anti-cheat, kick or ban threads; all are audio, lag, stuttering, encoding or black-screen game-capture. | <https://obsproject.com/forum/tags/call-of-duty/> | `[COMMUNITY]` negative evidence |
| **MSI Afterburner / RivaTuner Statistics Server (RTSS)** | **(a) crash at startup** since 2019. Community consensus attributes it to CoD's renderer, not anti-cheat. **No ban evidence.** | Activision names Afterburner only as a *crash* conflict: *"Disable NZXT CAM, MSI Afterburner, and Razer Cortex, as these can conflict with Call of Duty: Modern Warfare"* (**10/27/19**). Modern equivalent is generic: *"Disable overclocking or tuning software"* (BO6, **07/02/25**). Guru3D threads #439817, #457930. **No RTSS-side statement about Activision exists** — Unwinder's documented anti-cheat fights are with EasyAntiCheat and EA's EAAC, **not RICOCHET**. | `[OFFICIAL]` + `[COMMUNITY]` |
| **Discord / Steam / NVIDIA App / GeForce Experience / ShadowPlay overlays** | **(a)** crash troubleshooting only. The standard community fix for BO6 error `0x00001338` is disabling them. ShadowPlay issues are "overlay blinks / fails to record" — broken, not blocked. **No ban evidence.** | Generic Activision guidance; <https://steamcommunity.com/app/1938090/discussions/0/3600093929962230035/> | `[OFFICIAL]` generic + `[COMMUNITY]` |
| **AMD Adrenalin / Radeon overlay, Xbox Game Bar** | **No evidence found.** | — | `[NOT FOUND]` |
| **NVIDIA FrameView, Intel PresentMon, CapFrameX** | **Capture silently fails** under some anti-cheats. CapFrameX documents the failure mode generically: *"an ETW session conflict…, a blacklisted process, missing administrator rights, or **an anti-cheat that blocked PresentMon**."* NVIDIA FrameView release notes name **VAC and Javelin** — **RICOCHET and Call of Duty are absent.** | <https://github.com/CXWorld/CapFrameX>; <https://www.nvidia.com/en-au/geforce/technologies/frameview/release-notes/> | `[VENDOR]` |
| **Special K** | **The one real theoretical risk in this group: it is an injector**, and "injectors" is a named infraction class. SK's own guidance is to stop the global injector before launching anti-cheat games, warning that *"some anti-cheat protections might still flag and choose to take action upon detecting its presence."* **No CoD-specific incident found.** | <https://steamcommunity.com/groups/SpecialK_Mods/comments> | `[VENDOR]` self-guidance |
| **LatencyMon, NVIDIA Reflex Analyzer** | **No evidence found.** | — | `[NOT FOUND]` |
| **Cheat Engine** | **Presence alone: no evidence of ban** — no Activision statement either way. Every traceable CoD ban wave targeted **users of specific cheat products in use** (EngineOwning, ArtificialAiming, Phantom Overlay). Use against a protected title is squarely within the policy's ban scope. | <https://techcrunch.com/2025/07/16/call-of-duty-cheaters-complain-after-activision-launches-new-wave-of-mass-bans/> | `[JOURNALISM]`; `[NOT FOUND]` for mere presence |
| **Process Hacker / System Informer** | **No CoD evidence.** The documented conflicts are elsewhere: `kprocesshacker.sys` is detected by **Easy Anti-Cheat** and Honkai Star Rail (*"Some irregular events are detected in your system"*), and persists after uninstall because the driver stays registered. Risk shape is the **kernel driver**, not the UI. | <https://github.com/winsiderss/systeminformer/issues/2134>, opened **2024-07-15** | `[COMMUNITY]` |
| **Process Explorer, Task Manager** | **No evidence found.** | — | `[NOT FOUND]` |
| **Wireshark** | **(a) game refuses to run / crashes** — even when not capturing. Wireshark developer **Guy Harris**: the game *"refuses to let the game run if it detects a packet sniffer"*, criticising it for crashing silently instead of telling the user. **No ban reports in any CoD title.** | <https://ask.wireshark.org/question/15288/game-crashes-when-opening-wireshark/> — posted **2020-03-20**, BOCW confirmation **2023-11-17** | `[COMMUNITY]`, but from Wireshark devs |
| **Debuggers (x64dbg, WinDbg, Visual Studio), idle** | **No evidence found.** No official Activision statement about developer or RE tools merely being installed. Independent RE writeups say CoD's user-mode TAC detects *attached* debuggers and debugging artifacts — active debugging of the protected process, not an installed toolchain. | — | `[NOT FOUND]` / `[COMMUNITY]` |
| **VMs / Hyper-V / WSL2 / VBS** | **No published stance.** The practical gate is TPM 2.0 + Secure Boot validated remotely by Microsoft Azure Attestation; a VM's emulated TPM is unlikely to satisfy it. Consequence is **(b) restricted playlists**, not a ban. A circulating claim that the RICOCHET driver BSODs with Memory Integrity traces only to an SEO content site — **unverified, do not rely on it.** | TPM/Secure Boot article | `[OFFICIAL]` for the gate; `[NOT FOUND]` for VMs |
| **OCR / computer-vision overlays, second-PC capture** | **No RICOCHET stance.** Activision's documented response to the known capture-card CV cheat was **DMCA takedowns and legal pressure**, with no claim of technical detection. Enforcement in that class runs through input-device detection and behavioural models. | <https://www.kitguru.net/gaming/matthew-wilson/ai-assisted-call-of-duty-cheat-for-pc-and-console-targeted-by-activision/>, **2021-07-12** | `[JOURNALISM]` |

### Input-display overlays (the closest category to a mouse-movement visualizer)

| Software | Worst documented outcome | Evidence | Tier |
|---|---|---|---|
| **NohBoard** | **No evidence found.** Issue search for `anticheat OR "anti-cheat" OR EasyAntiCheat OR BattlEye OR Vanguard OR ban` → **"No results."** | <https://api.github.com/repos/ThoNohT/NohBoard> | `[OFFICIAL]` issue tracker |
| **OBS input-overlay plugin** (univrsal) | **No evidence found.** Repo search for the same terms plus FACEIT → **0 results**; README and wiki silent. The closest game-specific issue, **#490 (GTA Online)**, is a keyboard-layout/locale bug (`Could not find keyboard map for locale…`), not anti-cheat, despite GTA Online shipping BattlEye. | <https://github.com/univrsal/input-overlay/issues/490> | `[OFFICIAL]` |
| **Keyviz** | **No anti-cheat evidence. Five closed issues from ANTIVIRUS false positives**, including **Windows Defender classifying it as `Backdoor:Win32/Bladabindi!ml`** (#60, **2022-10-04**; also #9, #17, #59, #119). Pitched at tutorials and presentations, not gaming. | <https://github.com/mulaRahul/keyviz/issues> | `[OFFICIAL]` issue tracker |
| **Gamepad Viewer** | **No evidence found.** Browser-based, W3C Gamepad API polled inside the Chromium sandbox — no hooks, no injection, no contact with the game process. The most inert design in the category. | <https://gamepadviewer.com/> | `[NOT FOUND]` |
| **KeyViewer (square3ang)** | ⚠️ **Categorically different** — an ADOFAI **in-game mod loaded inside the game process.** Inappropriate as a comparison for any anti-cheat-protected title. | — | `[OFFICIAL]` |
| **"XOverlay"** | **No verifiable product by that name in this space.** Not confirmed to exist as described. | — | `[NOT FOUND]` |

**Mechanism finding, verified from source code — and it corrects a common assumption.**
**None of the mainstream stream-overlay tools use Windows Raw Input.** They install global low-level
hooks:

- **NohBoard** — `NohBoard/Hooking/Interop/FunctionImports.cs` P/Invokes exactly four APIs:
  `SetWindowsHookEx`, `CallNextHookEx`, `UnhookWindowsHookEx`, `GetKeyState`.
  **`RegisterRawInputDevices` is not imported.** `HookManager.cs` uses the `WH_KEYBOARD_LL` /
  `WH_MOUSE_LL` variants.
  <https://raw.githubusercontent.com/ThoNohT/NohBoard/master/NohBoard/Hooking/Interop/FunctionImports.cs>
  (fetched 2026-09-14) `[OFFICIAL — source code]`
- **OBS input-overlay** → libuiohook, whose Windows backend calls
  `SetWindowsHookEx(WH_KEYBOARD_LL, …, hInst, 0)` and `SetWindowsHookEx(WH_MOUSE_LL, …, hInst, 0)`
  using **its own module handle — no DLL is injected into other processes.**
  <https://raw.githubusercontent.com/kwhat/libuiohook/1.2/src/windows/input_hook.c> `[OFFICIAL — source code]`
- These are **not** DLL-injecting: Microsoft's documentation notes that "SetWindowsHookEx can be used
  to inject a DLL into another process" applies to the non-`_LL` hook types (`WH_CBT`,
  `WH_GETMESSAGE`); low-level hooks "can be called on the thread that installed the hook."
  <https://learn.microsoft.com/en-us/windows/win32/winmsg/about-hooks> (updated **2025-09-15**) `[OFFICIAL — Microsoft]`

**Consequence:** a raw-input-only capture path never enters user32's system-wide hook chain, and is
therefore **quieter than the popular comparables — but also rarer than them.** Do not claim kinship
with NohBoard; the mechanism differs, and the difference favours raw input.

**The real, well-attested failure mode for this category is Windows UIPI, not detection:** a
non-elevated hook cannot observe input destined for an elevated game, so the overlay works when
alt-tabbed to OBS and goes inert in-game. An OS privilege boundary, not an enforcement event.

### Streamer practice and competitive rules

- `[OFFICIAL]` **CDL Challengers 2026 Official Rules** (v1.0, **2025-10-28**), §4: *"Players are to use
  platform compatible controllers for all Challengers Online and LAN competitions. **Mouse and
  Keyboard controls are strictly prohibited.** Players may not use a turbo controller… Players may not
  use a button macro controller… The Administration reserves the right to inspect and review player
  equipment to ensure compliance."*
  <https://www.callofduty.com/content/dam/atvi/callofduty/esports-new/2026-cdl-programs/CDL_Challengers_2026_Season_Official_Rules.pdf>
- §9 "Software and Hardware": *"Participants are prohibited from installing third party software of any
  kind on any competition hardware or machines at Challengers **LAN** events."* A blanket LAN clause;
  **online play is not covered by it.**
- **Keyword sweep of the extracted rulebook text:** `overlay` → **0 hits**; `RICOCHET` → **0**;
  `anti-cheat` / `anticheat` → **0**; `input display` / `input visualization` → **0**. `peripheral`
  appears once, inside a *merchandising-rights* clause. `macro` appears once, in the turbo/macro
  **controller** rule.
- **No publicly posted 2026 CDL pro-tier rulebook exists.**
- Because CoD's competitive tier has been controller-only since the 2020 move to PC, a
  keyboard-and-mouse input overlay has no institutional home in its rules — which is why there is
  nothing to find.
- **Streamer practice:** CoD's transparency ritual is a **physical handcam**, not a software overlay.
  Handcams appear on the CDL broadcast and drive a large content genre, and are known to be
  defeatable. **Exactly one concrete report** of a CoD player running an input overlay exists:
  r/Warzone, ~Feb 2024, *"I've been accused a few times even while having a hand/monitor cam with an
  input overlay on stream."* `[COMMUNITY]` <https://www.reddit.com/r/Warzone/comments/1akavfj/>
- **No named Call of Duty streamer is confirmed to run a keyboard/mouse input overlay**, and no
  "which overlay does X use" discussion exists for CoD. **There is no CoD-specific track record for
  input overlays in either direction** — absence of evidence, not evidence of absence.

### Has Activision ever commented on tools that only READ input?

`[NOT FOUND]`. No statement in either direction, from Activision or the RICOCHET team, about raw-input
listeners, input viewers, keystroke visualizers or mouse-movement visualizers. The nearest adjacent
official statement is the January 2024 one, which concerns **activating aim assist** — modifying
input, the opposite of reading it.

### The "unsupported software detected" error string does not exist

Searched directly; it appears in no Activision support page, no policy, no journalism and no community
screenshot. The strings that actually exist, which folklore conflates:

| String | Category | Source |
|---|---|---|
| "Your account has been permanently banned for using unauthorized software and manipulation of game data." | **(c) ban email** | `[JOURNALISM]` TheGamer, 2022-10-31 |
| "Failed Attestation Status" → restricted modes/playlists | **(b) restriction** | `[OFFICIAL]` TPM/Secure Boot article |
| "Limited Matchmaking" | **(b) investigation state** | `[OFFICIAL]` policy |
| `0x00001338`, `DEV ERROR 292`, `DEV ERROR 11642`, `0xc0000005`, `0x887A0005` | **(a) generic crash codes** | `[COMMUNITY]` — **none attributes a cause to third-party software** |

**Activision's official remedy guidance** for third-party conflicts, BO6 and MWIII crash pages
`[OFFICIAL]`: *"Third-party applications such as **input remapping software, overlays, and recording
software** may interfere with gameplay or disrupt the game's boot process, **even if they're not
running** when you boot or play the game."* The recommended action is to **uninstall**, not merely
close. This is the only Activision text addressing overlay and recording software as a class, and it
is framed entirely as **stability**, never as enforcement.

### Contrast: a different vendor's explicit position

BattlEye `[OFFICIAL — other vendor]`, <https://www.battleye.com/support/>: *"No one is banned for
using non-hack programs (like Fraps, overlays, etc.), picking up or using hacked in-game items,
weapons or vehicles, being on a server at the same time as a cheater, or other passive non-cheating
activity."* **Activision publishes no equivalent assurance.**

---

## 6. Cephable (the sole approval) and QuadStick (the sharpest false positive)

### Cephable — the only third-party application Activision has ever approved by name

`[OFFICIAL]` <https://support.activision.com/articles/expanding-accessibility-options-in-call-of-duty-using-cephable>
— **last updated 04/09/26**:

> "Yes, Cephable is an approved and supported application for use in Call of Duty: Black Ops 7…
> implemented and tested in collaboration with Treyarch, Beenox, and the RICOCHET Anti-Cheat™ team."

- Scope per the directly-fetched support article: **Black Ops 7 Zombies and Co-Op Campaign only** —
  explicitly not competitive modes. (Third-party coverage of the 2026-04-07 CoD blog post gives a
  slightly wider scope — Campaign, Zombies, Dead Ops Arcade, Firing Range — but the support article is
  the narrower and more authoritative wording.)
- Carries the caveat: *"the use of Cephable may be restricted if suspicious behavior is detected."*

**Read for the audit:** an approval mechanism exists, but it is bespoke, per-application, negotiated
directly with the studios and the RICOCHET team, confined to non-competitive modes, and still
revocable — even for an accessibility tool with a legal tailwind. **There is no self-service allowlist
a telemetry tool could apply to, and the absence of one is deliberate rather than an oversight.**
`support.activision.com/search?q=approved devices` returns zero results; the policy uses "unsupported"
and "unapproved" as operative terms while never enumerating what *is* supported.

### QuadStick — the error mode of behavioural input detection

`[JOURNALISM]` <https://gamerant.com/cod-paralyzed-streamer-banned/>, **2026-05-23**: a **QuadStick**
sip-and-puff accessibility controller, used by a quadriplegic player, drew a **real temporary ban
citing "third-party input modification device."** It was reversed only after public escalation.

This is the direct cost of the February 2026 shift to behavioural input analysis: a detector that
reasons about *how inputs behave* rather than *what device is plugged in* will misclassify input
patterns that are unusual but human. Compounding it — **temporary bans and limited-matchmaking states
are formally non-appealable**, so the only remedy available was publicity.

**Relevance to a passive telemetry tool:** it does not synthesize input and cannot alter the pattern
the game sees, so it is outside this detector's input. But the case confirms the detector's error mode
lands on *legitimate input that looks statistically atypical* — worth knowing for anyone using such a
tool's metrics to deliberately train unusual movement patterns.

---

## 7. Process handles and foreground-process reads

### PROCESS_QUERY_LIMITED_INFORMATION

- **Official statement:** `[NOT FOUND]`. Activision has never addressed process-query access, in the
  policy, the RICOCHET overview, or any progress report.
- **What the official language implies:** RICOCHET's description is consistently "processes
  interacting with a game… to determine if they are **manipulating** the game." Querying a process's
  image path manipulates nothing. This is inference from official text, not an official statement.
- **Counterweight:** the same overview page also claims RICOCHET works by "monitoring applications
  running on a machine during gameplay" (§3), which does not restrict itself to processes that touch
  the game. Nothing is published about what that observation is used for.
- **Technical context** (community/technical-literature tier, not official): kernel anti-cheats
  commonly register `ObRegisterCallbacks` to strip or downgrade access rights on handles opened
  against the protected process. The practical consequence of an over-broad request is that **the open
  fails or is downgraded** — a functional outcome, not an enforcement one.
  `PROCESS_QUERY_LIMITED_INFORMATION` is the right generally left alone, because stripping it would
  break Task Manager.
- **No public evidence, at any tier, that opening such a handle has ever been a flagging signal.**

### Reading the foreground window's process name

`[NOT FOUND]`. `GetForegroundWindow` + `GetWindowThreadProcessId` opens no handle at all and is
unobservable from the game side. No source addresses it.

### Related, and better evidenced

The **UIPI boundary** (§5) is the one documented, reproducible problem in this area: a non-elevated
process cannot query or observe an elevated game. That is an OS privilege boundary producing an
`ERROR_ACCESS_DENIED`, not a detection event — and it is the correct explanation for access-denied
symptoms, which should never be resolved by advising users to elevate.

---

## 8. Implications for a passive raw-input telemetry tool

Behaviours below are drawn from the companion code audit
(`docs/ANTICHEAT-2026-09-14.md`, whose §3 and §5 this report is intended to fill).

### (a) Outside every published prohibition and every described detection surface

No publisher documents anything as "safe", so this is the strongest available category. The honest
label is **"unnamed and unenforced"**, not "documented-safe" — because RICOCHET officially claims to
monitor applications running on the machine during gameplay, and Activision's own crash guidance tells
users to uninstall overlay and recording software *"even if they're not running."*

- **`RIDEV_INPUTSINK` raw-input registration, mouse usage page (0x01/0x02) only.** Windows delivers a
  *copy*; the game's input stream is untouched. Not named in the policy, which prohibits software that
  *modifies* game data. Registering the mouse page and **not** the keyboard page (0x06) means the tool
  is categorically not a keylogger — this is also the single best defence against the antivirus risk
  below, and should be stated explicitly in user-facing documentation.
- **No hook of any kind.** No `SetWindowsHookEx`, no `SetWinEventHook`. Quieter than NohBoard and the
  OBS input-overlay plugin, which both install global `WH_KEYBOARD_LL` / `WH_MOUSE_LL` hooks (§5).
- **`GetForegroundWindow` + `GetWindowThreadProcessId`** — opens no handle, unobservable.
- **Recording to JSONL, UDP to loopback, WebSocket to a local browser, Kafka to a LAN broker** —
  ordinary I/O, no game interaction.
- **Offline aim metrics** (flick, overshoot, settle, tremor) computed from the tool's own recordings —
  no game data, no game process, after the fact.
- **An OBS browser-source overlay** is rendered in OBS's own CEF and composited into the video stream.
  It never enters the game process — architecturally distinct from injected in-game overlays.
- **Absent from all shipped binaries** (verified by PE scan in the companion audit): `SendInput`,
  `mouse_event`, `keybd_event`, `SetCursorPos`, `ClipCursor`, `BlockInput`, `SetWindowsHookEx`,
  `ReadProcessMemory`, `WriteProcessMemory`, `VirtualAllocEx`, `CreateRemoteThread`,
  `GetAsyncKeyState`, screen-capture APIs, and overlay window styles (`WS_EX_TOPMOST`, `WS_EX_LAYERED`,
  `WS_EX_TRANSPARENT`). Every one of these is something the policy or RICOCHET is documented to act on.

### (b) Undocumented, but standard in legitimate software

- **`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `QueryFullProcessImageNameW` on the foreground
  PID**, cached to once per foreground change. Identical to Task Manager, Explorer, Discord and Steam.
  No evidence it is a signal (§7) — and no official assurance either. It is the only moment the tool
  holds a handle on the game. The companion audit's **F1** fix (resolve the name from a
  `CreateToolhelp32Snapshot`, which opens no per-process handle) takes this to zero, works against an
  elevated game, and makes the design document's "no handles into the game process" claim true.
- **`RegisterHotKey`** — same as OBS and Discord. A registered chord is consumed before the game sees
  it, which is a UX consideration, not a detection one.
- **A Toolhelp process snapshot filtered by name before any handle is opened** — the control panel's
  scanner never makes the game a candidate. Already correct.
- **`SetThreadPriority(ABOVE_NORMAL)` on one thread and an EcoQoS opt-out** — what audio and capture
  software do. Normal priority class, no timer-resolution change.

### (c) Plausibly risky, and why

1. **Input synthesis in the benchmark harness.** `tmbench inject` calls `SendInput` with relative
   mouse motion and periodic clicks. This is the **only thing in the repository that would actually
   violate the Security & Enforcement Policy** if run while Call of Duty is live: "input mapping
   software" and "unapproved input modification devices" are both enumerated, the January 2024
   enforcement targets exactly this, and the February 2026 detector is purpose-built to separate
   machine-generated from human input. Synthetic input is also trivially distinguishable (a raw-input
   listener sees a null device handle; hooks see the injected flag). It is not shipped and not a
   workspace member, but it is built on the machine and `docs/AGENTIC-2026-09-13.md:74` (item B2)
   proposes handing it to an autonomous agent. **Gate it behind an explicit opt-in before that runner
   is built, and carry the gate into the runner.** Do not add a list of game executable names to it —
   an executable carrying `SendInput` *plus* game process names is the classic macro signature and
   looks materially worse than the current code.
2. **Antivirus keylogger heuristics — the best-evidenced real-world hazard in this whole category.**
   Keyviz has five closed issues from AV false positives, including Windows Defender classifying it as
   `Backdoor:Win32/Bladabindi!ml` (§5). Unsigned binaries with no version resource, reading raw input,
   hiding their own console, running from a user directory, is the textbook heuristic profile. This is
   better evidenced than any anti-cheat interaction for input-capture software and should be treated
   as the primary distribution risk.
3. **String-signature blast radius.** RICOCHET has scanned for hardcoded text strings and banned on
   context-free matches (§3). The exposure is theoretical here — the tool's strings live in its own
   process, and the 2024 incident worked because strings entered the *game's* memory as chat — but
   cheat-adjacent vocabulary in process names, window class names or user-visible strings is a real
   category of accident. Current class names are benign; the analyze crate's vocabulary
   ("trigger discipline", "flick", "recoil") is the kind of thing a naive scanner is built around.
   Low and unquantified, but it is the one mechanism by which a non-interacting program could
   conceivably be caught.
4. **Advising elevation.** The control panel turns an access-denied error into "run the panel as
   administrator." Elevating the panel elevates every child, so capture would then hold its handle on
   the game from an elevated process — a worse posture for no gain. The underlying cause is the UIPI
   boundary (§7); reword the hint and never advise elevation.
5. **The "anticheat-safe by design" claim** in `README.md:8`, `docs/GUIDE.md:53` and
   `mouse-telemetry-plan.md:12,16` is not defensible as written. The evidence supports "passive" and
   "does nothing that anti-cheat systems are documented to act on." It cannot support "safe": no
   vendor allowlists third-party tools, Cephable is the sole named approval in Call of Duty's history
   (§6), and the policy reserves the judgement entirely to Activision.
6. **Recordings are a focus log** — foreground executable name per batch, cursor position outside
   games, device strings, monitor layout. A privacy consideration for sharing recordings, not an
   anti-cheat one.

### Recommended order of action

1. **Sign the release binaries and add a version resource and manifest.** Highest value; addresses the
   best-evidenced hazard (2 above).
2. **Gate `tmbench inject` behind an explicit opt-in** before the agentic experiment runner is built.
3. **Apply the F1 fix** — Toolhelp snapshot instead of `OpenProcess` for the foreground name.
4. **Reword the access-denied hint** so it never advises elevation.
5. **Fix the "anticheat-safe" claim**, and if a `docs/FAIR-PLAY.md` is written, state plainly that the
   tool synthesizes no input at all and registers only the mouse usage page — pre-empting the
   "input mapping software" misreading, which is the single enumerated policy phrase most likely to be
   misapplied.
6. **Do not claim kinship with NohBoard or the OBS input-overlay plugin.** Those install global
   low-level hooks; raw input does not. State the mechanism on its own terms.

### One empirical data point

From the companion audit's Appendix B: 39 recordings on the author's machine tag ~2.96 M batches with
`cod.exe` in the foreground — roughly **20.6 hours of Call of Duty played with the agent running**,
with the Kafka sink and a LAN-bound viz active at times, and **no enforcement action reported against
the account**. Consistent with everything above; not proof, and it says nothing about future detection
changes.

---

## 9. Source list

### Official — Activision / Call of Duty / Blizzard News / Activision Support

1. Call of Duty Security and Enforcement Policy — <https://support.activision.com/articles/call-of-duty-security-and-enforcement-policy> — updated **07/31/26** — `[OFFICIAL]`
2. Appeal a Ban — <https://support.activision.com/ban-appeal> — no date shown — `[OFFICIAL]`
3. RICOCHET Anti-Cheat overview — <https://support.activision.com/articles/ricochet-overview> — updated **11/10/25** — `[OFFICIAL]`
4. TPM 2.0 and Secure Boot for Call of Duty — <https://support.activision.com/articles/trusted-platform-module-and-secure-boot> — updated **08/27/26** — `[OFFICIAL]`
5. Expanding Accessibility Options using Cephable — <https://support.activision.com/articles/expanding-accessibility-options-in-call-of-duty-using-cephable> — updated **04/09/26** — `[OFFICIAL]`
6. Black Ops 6 PC Troubleshooting — <https://support.activision.com/black-ops-6/articles/black-ops-6-pc-troubleshooting> — **07/02/25** — `[OFFICIAL]`
7. Crashes or Game Freezes in Black Ops 6 — <https://support.activision.com/black-ops-6/articles/crashes-or-game-freezes-in-black-ops-6> — `[OFFICIAL]`
8. Crashes and Game Freezes in Modern Warfare III — <https://support.activision.com/modern-warfare-iii/articles/crashes-and-game-freezes-in-modern-warfare-iii> — `[OFFICIAL]`
9. Modern Warfare PC Troubleshooting (names NZXT CAM, MSI Afterburner, Razer Cortex) — <https://support.activision.com/modern-warfare/articles/call-of-duty-modern-warfare-pc-troubleshooting> — **10/27/19** — `[OFFICIAL]`
10. Warzone 2 PC Troubleshooting (endorses updating Razer Synapse) — <https://support.activision.com/warzone-2/articles/warzone-2-pc-troubleshooting> — **03/11/26** — `[OFFICIAL]`
11. RICOCHET Anti-Cheat announcement — <https://news.blizzard.com/en-us/article/23733251/ricochet-anti-cheat-call-of-dutys-new-anti-cheat-initiative> — **Oct 2021** — `[OFFICIAL]`
12. RICOCHET Progress Report, Black Ops 6 launch — <https://news.blizzard.com/en-us/blizzard/24150099/ricochet-anti-cheat-progress-report-black-ops-6-launch> — **Oct 2024** — `[OFFICIAL]`
13. RICOCHET Progress Report, Season 01 and Ranked Play — <https://news.blizzard.com/en-us/article/24151773/ricochet-anti-cheattm-progress-report-season-01-and-ranked-play> — **Nov 2024** — `[OFFICIAL]`
14. TeamRICOCHET Season 02 Update (behavioural input shift) — <https://news.blizzard.com/en-us/article/24243445/teamricochet-season-02-update> — **2026-02-02** — `[OFFICIAL]`
15. RICOCHET Anti-Cheat Update, Season 05 — <https://news.blizzard.com/en-us/article/24224368/ricochet-anti-cheat-update-season-05> — `[OFFICIAL]`
16. A message from TeamRICOCHET — <https://news.blizzard.com/en-us/article/24223315/a-message-from-teamricochet> — `[OFFICIAL]`
17. RICOCHET Progress Report, MWIII (ML, Replay Investigation Tool) — <https://www.callofduty.com/blog/2023/11/call-of-duty-ricochet-anti-cheat-modern-warfare-III-progress-report> — **Nov 2023** — `[OFFICIAL]` — *not directly fetchable; cited via search summaries*
18. CDL Challengers 2026 Season Official Rules (v1.0) — <https://www.callofduty.com/content/dam/atvi/callofduty/esports-new/2026-cdl-programs/CDL_Challengers_2026_Season_Official_Rules.pdf> — **2025-10-28** — `[OFFICIAL]`
19. Call of Duty League × Aim Lab sponsorship — <https://investor.activision.com/news-releases/news-release-details/call-duty-leaguetm-joins-forces-aim-lab-multi-year-deal-become> — **2022-03-14** — `[OFFICIAL]`
20. ESA case study, Activision Blizzard anti-cheat — <https://www.theesa.com/case-studies/activision-blizzard-anti-cheat/> — **Feb 2024** — `[OFFICIAL/INDUSTRY]`

### Journalism

21. Engadget, Steve Dent — XIM-style hardware detection — <https://www.engadget.com/call-of-duty-can-detect-and-ban-xim-style-cheat-hardware-100314416.html> — **2023-04-05**
22. TechSpot — game closes on MnK aim-assist detection — <https://www.techspot.com/news/101549-call-duty-anti-cheat-system-now-close-game.html> — **2024-01-17**
23. Dexerto — same announcement — <https://www.dexerto.com/call-of-duty/cod-devs-finally-clamp-down-on-mouse-and-keyboard-aim-assist-cheats-plaguing-warzone-2480652/> — **2024-01-16**
24. Dexerto — devs explain spam reports and shadowbans — <https://www.dexerto.com/call-of-duty/black-ops-6-warzone-devs-finally-explain-how-spam-reports-shadowbans-work-3172211/> — **2025-03-28**
25. Dexerto — devs admit RICOCHET incorrectly banned accounts — <https://www.dexerto.com/call-of-duty/cod-developers-admit-ricochet-anti-cheat-incorrectly-banned-accounts-2953378/> — **2024-10-17**
26. Dexerto — Cronus Zen / XIM crackdown, BO7 Season 2 — <https://www.dexerto.com/call-of-duty/cod-cracks-down-on-cronus-zen-xim-in-major-anti-cheat-update-for-black-ops-7-season-2-3313252/> — **2026-02-02**
27. TechCrunch, Lorenzo Franceschi-Bicchierai — string-signature ban exploit — <https://techcrunch.com/2024/11/07/hacker-says-they-banned-thousands-of-call-of-duty-gamers-by-abusing-anti-cheat-flaw> — **2024-11-07**
28. Insider Gaming — the same exploit — <https://insider-gaming.com/call-of-duty-anti-cheat/> — **2024-10-18**
29. GameSpot — false bans, accounts restored — <https://www.gamespot.com/articles/call-of-duty-anti-cheat-was-falsely-banning-legitimate-players-but-all-accounts-have-been-restored/1100-6527252/> — **Oct 2024**
30. PC Gamer — Activision fixed a "workaround" that banned innocent players — <https://www.pcgamer.com/games/call-of-duty/activision-says-it-fixed-a-workaround-in-call-of-duty-anti-cheat-that-banned-a-small-number-of-innocent-players-but-a-cheat-maker-claims-they-could-ban-anyone-by-typing-two-words-into-chat/> — **Nov 2024**
31. GameRant — paralyzed streamer banned (QuadStick) — <https://gamerant.com/cod-paralyzed-streamer-banned/> — **2026-05-23**
32. GameRant — BO7 Season 3 anti-cheat update — <https://gamerant.com/call-of-duty-black-ops-7-season-3-anti-cheat-update/> — **2026-04-02**
33. Techtroduce — FormaL false ban in MW4 Beta — <https://www.techtroduce.com/call-of-duty-ricochet-false-ban-formal-mw4-beta> — **2026-09-04** — *low-tier outlet; corroborated by CharlieIntel*
34. GamesBeat, Alexander Lee — Activision on enforcement and perception — <https://gamesbeat.com/how-activision-is-fighting-call-of-duty-cheating-on-two-fronts-enforcement-and-perception/> — **2026-08-14**
35. IGN — David Andrews interview on the ban process — **2026-09-10** — **URL not captured; IGN is not crawlable from this environment.** Quotes reached via secondary coverage.
36. XDA, Coleman Hamstead — how Activision strikes down cheaters (mitigation descriptions) — <https://www.xda-developers.com/activision-vs-call-of-duty-cheaters/> — **2025-04-04**
37. TheGamer, Josh Coulson — MW2 bans and RGB software — <https://www.thegamer.com/call-of-duty-modern-warfare-2-banned-using-rgb-software/> — **2022-10-31**
38. Insider Gaming, Tom Henderson — RGB software survey — <https://insider-gaming.com/data-suggests-rgb-software-could-be-getting-you-banned-in-call-of-duty/> — **2022-11-04**
39. NME — MW2 perma-bans attributed to **faulty anti-cheat / crash-induced false positives** — <https://www.nme.com/news/gaming-news/modern-warfare-2-players-hit-with-perma-bans-due-to-faulty-anti-cheat-software-3368068> — **2022-12-16** — *this is what downgrades the RGB theory*
40. TechCrunch — 2025 mass ban wave targeting cheat vendors' users — <https://techcrunch.com/2025/07/16/call-of-duty-cheaters-complain-after-activision-launches-new-wave-of-mass-bans/> — **2025-07-16**
41. KitGuru, Matthew Wilson — AI-assisted CoD cheat targeted by Activision (DMCA response) — <https://www.kitguru.net/gaming/matthew-wilson/ai-assisted-call-of-duty-cheat-for-pc-and-console-targeted-by-activision/> — **2021-07-12**
42. PC Gamer — CDL moves to PC, mouse and keyboard not allowed — <https://www.pcgamer.com/call-of-duty-league-moves-to-pc-for-2021-but-mouse-and-keyboard-isnt-allowed/> — **2020-09-14**
43. GGRecon — CDL will not allow mouse and keyboard on PC — <https://ggrecon.com/articles/cdl-wont-be-allowing-mouse-and-keyboard-set-ups-on-pc> — **2020-09-20**
44. Forbes, Erik Kain — BO7 Beta TPM 2.0 and Secure Boot how-to — <https://www.forbes.com/sites/erikkain/2025/10/02/black-ops-7-beta-how-to-enable-tpm-20-and-secure-boot-and-what-to-do-if-its-not-working/> — **2025-10-02**
45. TechRadar — BO7 revised anti-cheat and the hardware gate — <https://www.techradar.com/gaming/call-of-duty-black-ops-7s-revised-anti-cheat-means-you-might-not-be-able-to-play>
46. GamesRadar — BO7 expands RICOCHET after 263,000 bans — <https://www.gamesradar.com/games/call-of-duty/call-of-duty-black-ops-7-expands-ricochet-anti-cheat-to-combat-exploits-in-ranked-after-banning-over-263-000-cheaters-already-this-year/>
47. TrueAchievements — 70,000 hardware bans — <https://www.trueachievements.com/news/call-of-duty-bans-70000-cheaters>

### Vendor statements (non-Activision)

48. BattlEye Support — "No one is banned for using non-hack programs" — <https://www.battleye.com/support/> — `[OFFICIAL — other vendor]`
49. reWASD blog, "Cancel culture" — games unworkable if reWASD is **installed** — <https://www.rewasd.com/blog/post/cancel-culture> — **2024-02-05** — `[VENDOR]`
50. Raw Accel FAQ (claims AC-safety for FaceIT, Valorant, Diabotical; CoD not mentioned) — <https://github.com/RawAccelOfficial/rawaccel/blob/master/doc/FAQ.md> — `[VENDOR]`
51. NVIDIA FrameView release notes (names VAC and Javelin; RICOCHET absent) — <https://www.nvidia.com/en-au/geforce/technologies/frameview/release-notes/> — `[VENDOR]`
52. CapFrameX README (generic "an anti-cheat that blocked PresentMon") — <https://github.com/CXWorld/CapFrameX> — `[VENDOR]`
53. Special K community guidance on anti-cheat games — <https://steamcommunity.com/groups/SpecialK_Mods/comments> — `[VENDOR]` self-guidance
54. Valve, Steam Input gamepad-emulation best practices (hooks RawInput, injects emulated controller) — <https://partner.steamgames.com/doc/features/steam_controller/steam_input_gamepad_emulation_bestpractices> — `[VENDOR]`
55. mouse-sensitivity.com, BO6 → Aim Labs converter (browser form, no installer) — <https://www.mouse-sensitivity.com/n/call-of-duty-black-ops-6-to-aimlabs/> — `[VENDOR]`

### Source code and Microsoft documentation

56. NohBoard `FunctionImports.cs` — imports only `SetWindowsHookEx`, `CallNextHookEx`, `UnhookWindowsHookEx`, `GetKeyState`; no `RegisterRawInputDevices` — <https://raw.githubusercontent.com/ThoNohT/NohBoard/master/NohBoard/Hooking/Interop/FunctionImports.cs> — fetched **2026-09-14** — `[OFFICIAL — source]`
57. libuiohook Windows backend — `SetWindowsHookEx(WH_KEYBOARD_LL/WH_MOUSE_LL, …)` with its own module handle — <https://raw.githubusercontent.com/kwhat/libuiohook/1.2/src/windows/input_hook.c> — `[OFFICIAL — source]`
58. Microsoft, About Hooks — DLL injection applies to non-LL hook types — <https://learn.microsoft.com/en-us/windows/win32/winmsg/about-hooks> — updated **2025-09-15** — `[OFFICIAL — Microsoft]`
59. Microsoft, `SetWindowsHookExA` — <https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowshookexa> — updated **2025-07-01** — `[OFFICIAL — Microsoft]`
60. NohBoard repo metadata (last release v1.3.0, 2020-04-11; dormant) — <https://api.github.com/repos/ThoNohT/NohBoard> — `[OFFICIAL]`
61. OBS input-overlay releases (stable 5.0.6, 2024-10-24; 5.1.0 pre-release 2025-04-06) — <https://github.com/univrsal/input-overlay/releases> — `[OFFICIAL]`
62. OBS input-overlay issue #490 (GTA Online, locale bug not anti-cheat) — <https://github.com/univrsal/input-overlay/issues/490> — `[OFFICIAL]`
63. Keyviz repo metadata — <https://api.github.com/repos/mulaRahul/keyviz> — `[OFFICIAL]`
64. Keyviz issues — AV false positives incl. `Backdoor:Win32/Bladabindi!ml` (#60, **2022-10-04**; also #9, #17, #59, #119) — <https://github.com/mulaRahul/keyviz/issues> — `[OFFICIAL]`
65. Gamepad Viewer — <https://gamepadviewer.com/> — `[OFFICIAL]`

### Community (uncorroborated unless stated)

66. Raw Accel issue #238 — shadowban claim, refuted 22 minutes later — <https://github.com/RawAccelOfficial/rawaccel/issues/238> — **2024-09-13** — `[COMMUNITY]`
67. Interception issue #170 — FACEIT forbids `mouse.sys` — <https://github.com/oblitum/Interception/issues/170> — **2023-08-14** — `[COMMUNITY]`
68. Interception issue #202 — EAC/Fortnite: "Please close Interception before starting the game" — <https://github.com/oblitum/Interception/issues/202> — **2025-06-11** — `[COMMUNITY]`
69. Appuals — Interception as launch-blocker for Fortnite/Rust/Apex (CoD absent) — <https://appuals.com/close-interception-before-starting-the-game/> — `[COMMUNITY]` low-tier
70. Steam — AutoHotkey perma-ban claim, disputed — <https://steamcommunity.com/app/1938090/discussions/0/3829793451747354019/> — **2023-05-19** — `[COMMUNITY]`
71. Steam — G HUB driver vs Memory Integrity, launch failure — <https://steamcommunity.com/app/1938090/discussions/0/4757577823500255211/> — **2024-09-01** — `[COMMUNITY]` corroborated
72. Steam — DS4Windows shadowban claim, contradicted — <https://steamcommunity.com/app/1938090/discussions/0/4763208232584874724/> — `[COMMUNITY]`
73. Steam — BO7 temporary bans on Windows 11 25H2 + ReFS system drive — <https://steamcommunity.com/app/1938090/discussions/0/685239361818616794> — **2025-12-03** — `[COMMUNITY]` uncorroborated
74. Steam — crash on startup with Discord overlay — <https://steamcommunity.com/app/1938090/discussions/0/3600093929962230035/> — `[COMMUNITY]`
75. Steam — BO6 troubleshooting megathread (GoodbyeDPI causes silent launch failure) — <https://steamcommunity.com/app/1938090/discussions/0/4757577823498524992/> — `[COMMUNITY]`
76. Razer Insider — anti-cheat flagging software thread (origin of the RGB folklore; no staff reply) — <https://insider.razer.com/general-discussion-6/anti-cheat-flagging-software-as-unauthorized-modification-or-software-41919> — **2022-12-19** — `[COMMUNITY]`
77. OBS Project forum, `call-of-duty` tag — 13 threads, Oct 2019 – Sep 2024, **zero** anti-cheat threads — <https://obsproject.com/forum/tags/call-of-duty/> — `[COMMUNITY]` negative evidence
78. Guru3D — "Problem with RTSS and all Call of Duty since 2019" — <https://forums.guru3d.com/threads/problem-with-rtss-and-all-call-of-duty-since-2019.439817/> — `[COMMUNITY]`
79. Guru3D — "Conflict between Call of Duty & MSI Afterburner 4.6.6/RTSS 7.3.7" — <https://forums.guru3d.com/threads/conflict-between-call-of-duty-msi-afterburner-4-6-6-rtss-7-3-7.457930/> — `[COMMUNITY]`
80. Wireshark Q&A — game crashes when Wireshark is open; Guy Harris on packet-sniffer detection — <https://ask.wireshark.org/question/15288/game-crashes-when-opening-wireshark/> — posted **2020-03-20**, BOCW confirmation **2023-11-17** — `[COMMUNITY]`, from Wireshark developers
81. System Informer issue #2134 — `kprocesshacker.sys` detected by EAC and Honkai Star Rail — <https://github.com/winsiderss/systeminformer/issues/2134> — **2024-07-15** — `[COMMUNITY]`
82. r/Warzone — the single concrete report of a CoD player using an input overlay — <https://www.reddit.com/r/Warzone/comments/1akavfj/> — ~**Feb 2024** — `[COMMUNITY]`
83. OBS forum — "Run as administrator" (the UIPI failure mode for input overlays) — <https://obsproject.com/forum/threads/run-as-administrator.155938/> — `[COMMUNITY]`

### Not retrieved — flagged for manual follow-up

84. **Activision Software Terms of Use** — <https://www.activision.com/legal/software-terms-of-use> — **the binding contract; host refuses automated retrieval.** Read manually.
85. **RICOCHET Anti-Cheat support page** — `support.activision.com/ricochet-anti-cheat` — socket hang-up on every attempt.
86. **IGN, David Andrews interview**, 2026-09-10 — URL not captured; IGN not crawlable. Quotes reached only via secondary coverage.
87. **Primary source for a reported Feb 2026 statement** that RICOCHET is "not intended to target skilled players, creators, or competitors" — could not be located.
88. **Reported false-shadowban lawsuit** — referenced in community discussion, primary source not found.
89. **Third-party CoD tournament rulebooks** (Checkmate Gaming, UMG, Battlefy) — not checked.
90. **Cross-game streamer-practice comparison** (Valorant / CS2 / Apex / osu! named streamers using input overlays) — not checked; would establish whether Call of Duty is unusual in having no input-overlay track record.

### Explicitly rejected sources

`prismnews.com`, `backgrind.com`, `tier1settings.com`, `hwidchange.com`, `mitchcactus.co`,
`tateware.com`, `disgg.com`, `skyrant.net`, `gamemodifier.com`, `boosting-ground.com`,
`unbanster.com`, `tracexhwidspoofer.com`, `nohboard.org`, `nohboard.net`, `tech-insider.org`.
SEO/AI content farms or cheat/spoofer-adjacent. Two claims originating there are **unverified and
should be treated as false**: RICOCHET "documented false-positive 'gameplay enhancement' bans on
setups running OBS-class overlay software", and "Steam Input is whitelisted for CoD". A third — that
the RICOCHET driver BSODs with Windows Memory Integrity enabled — is likewise uncorroborated.

---

*End of report. Web-search budget for the compiling session was exhausted; every "not found" above is
a bounded negative from the searches actually performed, not a proof of absence.*
