# Tray flyout v2: layout and behaviour spec

This is the single normative tray presentation and interaction spec, recording the approved R5 design and its later explicit user decisions. Reference mockups are retained by the design session under `.sandbox/design/tray-v2/`.

This spec supersedes the untracked design mockups and the old tray layout in architecture §10. Delete confirmation and login geometry are recorded here in §18. Behaviour and credit entitlement are owned by contract C5; mockups are reference material only.

All sizes are DIPs at 100 % scale; multiply by the monitor scale. Coordinates are relative to the flyout's top-left (0,0). The width is 360.
- **Mockup URL.** `mockup.html?state=<name>&theme=light|dark` reproduces each PNG. The states are overview, relogin, exhausted, credits, menu, empty, quit, settings and tooltip. Add `&debug=1` to outline the boxes. With no parameters the page shows a gallery.
- **PNG margin.** Each PNG has a 32 DIP desktop margin, so the flyout origin is at (32,32) at 1x and at (64,64) at 2x.
- **Sample clock.** The mockup's "now" is 2026-10-18 (Sun) 21:05 local time.
- **Shadow.** The window shadow in the PNGs stands in for the DWM shadow. Do not draw it.

## 1. Theme

- **Config key.** Add `[tray] theme = "light" | "dark" | "system"` to `config.toml`. The default is `"light"`. The tray reads it at startup and again when the file changes (the same watcher as the other config, or the next flyout open).
  - `system` follows `AppsUseLightTheme`.
  - An unknown value means `light`, with one warn log line.
- **No toggle in the UI** in v2 (see open question Q3).

## 2. Fonts and text

- **Family.** `Segoe UI Variable Text`, then `Segoe UI`. Hangul comes from DirectWrite fallback (Malgun Gothic). Icons use `Segoe Fluent Icons`, then `Segoe MDL2 Assets`. 600 means SEMI_BOLD. Use tabular figures (`tnum`) everywhere.
- **Line boxes.** Each text run is vertically centred in its line box. A single line that does not fit is trimmed with an ellipsis.
- **Wrapping.** Multi-line Korean wraps at spaces only (keep-all), as in the old spec §1. This applies to the empty state, tooltips, two-line menu items and dialogs.

| Role | Size/line | Weight | Colour |
|---|---|---|---|
| header title | 14/20 | 600 | text |
| summary provider name | 13/18 | 600 | text |
| summary verdict | 13/18 | 400 | text |
| summary detail, note, section label (600), plan, usage label, reset, credits line, reason, disclosure, hint, footer stale text | 12/16 | 400 | secondary |
| summary "할 일 N" | 12/16 | 600 | bad-text |
| account name | 13/18 | 600 (off row: 400, secondary) | text |
| usage percent "38% 남음" | 12/16 | 600 | text |
| chip | 12/20 | 600 | chip-*-fg |
| small row button | 12/24 | 600 accent, 400 standard | on-accent / text |
| footer button, menu item, standard and accent button | 13/18 | 400 | text |
| menu info: email | 13/18 | 600 | text |
| menu info: other lines, two-line item subtitle | 12/16 | 400 | secondary (values 600 text) |
| empty-state title | 15/20 | 600 | text |
| empty-state body | 13/20 | 400 | secondary |
| tooltip | 12/16 | 400 (key values 600) | text / secondary |

## 3. Colour tokens

The light theme is the default. The dark theme applies only on opt-in. Contrast is WCAG 2.x against the background the token sits on.

| Token | Light | Dark | Notes |
|---|---|---|---|
| surface | #FFFFFF | #202020 | |
| surface-footer | #F7F7F7 | #1B1B1B | footer |
| popup | #FFFFFF | #2C2C2C | menus, tooltips, dialog |
| border (`DWMWA_BORDER_COLOR`) | #D9D9D9 | #3A3A3A | |
| popup-border, menu separator | #D4D4D4 | #474747 | |
| text | #1A1A1A | #F3F3F3 | 17.4:1 / 14.7:1 |
| secondary | #5F6368 | #A8A8A8 | 6.05:1 on white, 5.65:1 on footer / 6.9:1 |
| text-disabled | #A3A3A3 | #727272 | disabled controls only, never for information |
| divider | #EBEBEB | #313131 | |
| track | #E3E6EA | #3A3A3A | bar background |
| bar-ok / bar-low / bar-crit | #2E7D32 / #B7791F / #C42B1C | #5DBB63 / #E0A84A / #EF6F61 | bar-ok green (user, 2026-10-19: remaining bars go green → amber → red as they drain). Bars are non-text, and the number is always printed next to them. |
| bar-paused | #A3A3A3 | #727272 | |
| dot-ok / dot-warn / dot-bad / dot-off | #2C7A4B / #B7791F / #C42B1C / #8A8F95 | #6CCB8F / #E0A84A / #EF6F61 / #8A8A8A | Each dot always has a verdict word next to it. |
| chip-use bg/fg | #E8F1FB / #005FB8 | #1D3346 / #60CDFF | 5.5:1 / 7.2:1 |
| chip-warn bg/fg | #FBF0DC / #8A560A | #3D2F14 / #E8B864 | 5.4:1 / 7.1:1 |
| chip-bad bg/fg | #FCEBE9 / #B42318 | #432020 / #F08A80 | 5.7:1 / 5.9:1 |
| chip-off bg/fg | #EEF0F2 / #50555B | #2E2E2E / #B5B5B5 | 6.6:1 |
| warn-text / bad-text | #8A560A / #B42318 | #E8B864 / #F08A80 | 6.2:1 / 6.6:1 on white |
| accent / on-accent | #005FB8 / #FFFFFF | #60CDFF / #000000 | 6.3:1 |
| btn-bg / btn-border | #FBFBFB / #D1D1D1 | #2D2D2D / #3D3D3D | |
| check-border | #8A8A8A | #9A9A9A | |
| hover overlay | #000000 @ 4.5 % | #FFFFFF @ 6 % | |
| pressed overlay | #000000 @ 3 % | #FFFFFF @ 4 % | |
| focus-outer / focus-inner | #000000 / #FFFFFF | #FFFFFF / #000000 | |
| popup shadow | black, y 8, σ 8, 14 % | 37 % | as in the old spec §7 |

**Colour meaning.** Each colour has exactly one meaning:
- **blue**: serving now
- **red**: the user must act
- **amber**: money is being spent, or a bar is low
- **grey**: waiting or off; nothing to do
- **green**: subscription quota is healthy (the summary dot, and a usage bar with ≥ 30 % remaining; user 2026-10-19)

## 4. Window

- **Shape.** Width 360. Corners `DWMWCP_ROUND`. The DWM border is the border token, with the system shadow. The background is surface, solid.
- **Height.** Height = header + list + 8 (list bottom padding) + 48 (footer). It is capped at 80 % of the work area. Past the cap only the list scrolls, and the header and footer stay fixed. Draw the overlay scroll thumb as in the old spec §3.
- **Anchor.** The bottom edge stays above the taskbar, so a height change moves the top edge.
- **Order freeze.** While the flyout is open, do not reorder rows or collapse an expanded disclosure on data updates. Values update in place. A new account appears at the end of its section, and sorting is applied on the next open. This avoids rows jumping under the pointer.

## 5. Header (fixed, not scrolling)

```
y 0   ┌ title row, 40 high: padding 12 top, 16 left; "quota-baton" line box y 12–32
      ├ [core banner, 28, only when the core is unreachable or unhealthy]
      ├ summary line "Claude", 28
      ├ summary line "Codex", 28
      ├ [note line, 20, only when a provider is "사용 불가"]
      ├ 8 padding
      └ 1 DIP divider, full width (x 0–360), divider token
```
- Overview fixture: 40 + 28 + 28 + 20 + 8 + 1 = 125.
- With no accounts at all (§10), the summary lines and the padding are omitted, so the header is 41.

### 5.1 Summary line (one per provider, always Claude then Codex)

- **Layout.** The line is 28 high, with content at x 16–344 centred vertically.
  - **Dot**: an 8×8 circle at x 16.
  - **Provider name**: 8 after the dot.
  - **Verdict**: 6 after the name.
  - **Detail**: right-aligned to 344, trimmed with an ellipsis, at least 12 from the verdict.
  - **To-do count**: when it exists, " · 할 일 N" is appended to the detail. The " · " is secondary; "할 일 N" is bad-text 600. The count is never trimmed; trim the detail text before it.
- **Not interactive.** The line has no click action. It has a tooltip (§11) and an accessible name equal to its full text.
- **Verdict per provider.** It is derived from `/state.pools[provider]` and the accounts. The first matching row wins.

| # | Condition | Dot | Verdict | Detail |
|---|---|---|---|---|
| 1 | the provider has no accounts | off | 계정 없음 | (empty) |
| 2 | no account has `user_state=active` | off | 꺼짐 | 켜진 계정 없음 |
| 3 | `pools.eligible > 0` and at least one eligible account is **not** serving from credits | ok | 사용 가능 | in-use names (below) |
| 4 | `pools.eligible > 0` and every eligible account is serving from credits (or would, since its subscription windows are exhausted and `allow_credits=true`) | warn | 크레딧으로 사용 중 | `<earliest quota reset> 한도 초기화` |
| 5 | `pools.eligible == 0` | bad | 사용 불가 | `<earliest until>부터 사용 가능`. Use the earliest `health.until` among active accounts with an account-scoped cooling. If no such account exists: `다시 로그인 필요` when any account needs re-login, otherwise `사용할 수 있는 계정 없음`. |

- **In-use names** (row 3).
  - **One account in use**: `<name> 사용 중`, where "in use" means `active_sessions > 0`.
  - **Several in use**: `<name of the one with most active_sessions> 외 N개 사용 중`.
  - **None in use**: `계정 N개 준비됨`, where N is the number of eligible accounts.
- **To-do count N.** The number of the provider's rows in the **needs-action** group (§6.2, red chips). Optional fixes ("켜기", "크레딧으로 계속") are not counted.
- **Note line.** It is shown when at least one provider has verdict 5. The text is "사용 불가인 동안 새 요청은 오류로 끝납니다.", at x 32 (aligned with the provider names), 12/16 secondary.
- **Core banner.** It replaces the two summary lines and the note.
  - **When.** The core is unreachable, or `/health` reports false.
  - **Banner line.** 28 high: "코어에 연결할 수 없습니다" in 13/18 bad-text.
  - **Note line under it.** "다시 시작하는 중 · N초 후 재시도", or "코어 상태를 읽지 못했습니다".
  - **List.** It shows the last known rows at 50 % opacity, with no buttons enabled.
  - **Settings menu.** It keeps working, so the logs stay reachable.

## 6. Account list

### 6.1 Sections

- **Section label.** The block is 32 high, with padding 12 top, 16 left and 4 bottom. The text is "Claude" / "Codex", 12/16 600 secondary.
- **Section body.** The rows sorted per §6.2, with a 1 DIP divider (x 16–344) between rows. Then, if the provider has disabled accounts, a divider and the disclosure line (§6.6).
- **Provider with no accounts** (while the other provider has some). The section shows one 44-high line: "Claude 계정이 없습니다" in secondary at x 16, and a standard small button **[계정 추가]** right-aligned to 344. The button starts that provider's login directly.

### 6.2 Row state, chip and sort group

Derive one display state per active or paused account. The first matching row wins.

| # | Condition (AccountView) | Chip | Group | Reason line / button (§6.5) |
|---|---|---|---|---|
| 1 | `health.kind=needs_relogin` | 다시 로그인 필요 (bad) | needs-action | yes, **[다시 로그인]** accent |
| 2 | `cooling` with reason `link_stale` | CPA 갱신 멈춤 (bad) | needs-action | yes, no button |
| 3 | `cooling` with reason `store_error` | 저장 오류 (bad) | needs-action | yes, **[로그 열기]** standard |
| 4 | `cooling` with reason `payment` | 결제 거절 (bad) | needs-action | yes |
| 5 | `cooling` with reason `permission`, scope `account` (organisation denial) | 접근 거부 (bad) | needs-action | yes |
| 6 | serving from credits (`credits_in_use`, §13) | 크레딧 사용 중 (warn) | in-use | the credits line carries the button |
| 7 | `user_state=paused` | 일시정지 (off) | paused | none |
| 8 | `cooling` with reason `quota`, scope `account` | 한도 도달 (off) | waiting | none |
| 9 | `cooling` with reason `auth` (probe pending) | 인증 확인 대기 (off) | waiting | yes |
| 10 | `active_sessions > 0` | 사용 중 (use) | in-use | none, or pacing (§6.5) |
| 11 | `drain=true` | 새 대화 안 받음 (off) | ready | none |
| 12 | otherwise (ready, idle) | **no chip** | ready | none, or pacing |

- **Model-scoped coolings.** They never produce a chip. They appear only in the menu info block.
- **Sort order.** Sort by group: in-use, then needs-action, then ready, then waiting, then paused.
  - **Waiting** rows are ordered by soonest `health.until`.
  - **Other groups** are ordered by name.
- **Disabled accounts** never appear here. They go under the disclosure (§6.6).

### 6.3 Row anatomy (active and paused accounts)

Padding: 10 top, 10 bottom, 16 left. Content runs to x 344; line 1 runs to x 352.

```
H = 10 + 24                                              // line 1
  + (has usage   ? 6 + 16 + 4 + 4 : 0)                    // usage line + bar
  + (has credits ? 6 + (credits button ? 24 : 16) : 0)    // credits line (Codex)
  + (has reason  ? 8 + (reason button ? 24 : 16) : 0)     // reason line
  + 10
```

Overview fixture heights:
- Claude rows: 74 each.
- Codex hello.sample-acct: 10 + 24 + 8 + 24 + 10 = 76. A re-login row has no usage line, see below.
- Codex acct-alpha: 104.

**Line 1** (24 high), laid out right to left:
1. **⋯** box x 324–352, 24 high, radius 4, glyph U+E712 16 DIP secondary.
   - When the menu is open, it shows the hover overlay and a text-coloured glyph.
   - It is a visual affordance only: the whole row is the hit target (§12).
2. **Chip** (if any): right edge 4 left of ⋯ (x 320). 20 high, radius 10, padding 8.
3. **Name** at x 16, 13/18 600.
4. **Plan** 6 after the name, 12/16 secondary. It is hidden when unknown.

- **Rule.** The chip and the plan never shrink. The name is trimmed first and keeps at least 8 to the chip.
- **Name.** Use `label` when it is set. Otherwise use the local part of `email`. If two accounts of the same provider share the local part, show the full email for both. If there is no email, show "이름 없는 계정".
- **Plan labels.**
  - **Claude**: `pro` → Pro, `max_5x` → Max 5x, `max_20x` → Max 20x, `max` (tier unknown) → Max.
  - **Codex**: `free` → Free, `plus` → Plus, `pro` → Pro, `team` → Team, `business` → Business, `enterprise` → Enterprise.
  - Anything else, or null: hidden.

**Usage line** (16 high, 6 below line 1):
- **Left**: the window label ("5시간", "주간", or "N분" / "N일" for `<n>m` windows) in secondary, 4 gap, then **"N% 남음"** in 600 text.
- **Right**: "<time> 초기화" in secondary (time format in §14). If `reset_at` has passed, show "초기화됨".
- **Remaining.** `remaining = clamp(100 − round(used_pct), 0, 100)`. It is always expressed as remaining; **never print a bare used percentage.**
- **Binding window.**
  - **Candidates.** Only account-scoped windows (`5h`, `7d`, Codex primary/secondary `<n>m`) with a known `used_pct`. Sub-buckets such as `7d_oi` or `seven_day_opus` are not candidates.
  - **Choice.** The window with the highest `used_pct`. On a tie, take the later `reset_at`.
  - **Cooling override.** If the account is in `cooling(quota)`, use the limiting window as defined in C5. It normally agrees with the choice above.
- **Bar.** 4 below the line: x 16–344 (328 wide), 4 high, radius 2, track colour.
  - **Fill** is the **remaining** share: width `round(328 × remaining / 100)`, at least 4 when remaining > 0, none at 0. Radius 2.
  - **Fill colour.** bar-ok when remaining ≥ 30, bar-low at 10–29, bar-crit below 10. A paused account uses bar-paused.
- **Unknown usage** (no window with a known value yet). The usage line reads "사용량 확인 전" in secondary, and the bar is an empty track. Keep the height.
- **No usage line** for `needs_relogin` rows (their numbers are stale and polls stop, C9). The reason line takes its place.

### 6.4 Credits line (Codex only)

- **When.** Shown when credits are available per contract C5. It sits 6 below the bar (or below line 1 if there is no usage line). Height 16, or 24 when it carries a button. 12/16. The text is trimmed with an ellipsis before the button.
- **Amount.** `floor(balance)` with thousands separators ("19,395"). Unlimited shows "무제한". If the balance is unknown but `has_credits` is true, show "있음". The tooltip shows the exact value. The enable confirmation also covers accounts without a credits line: explicit absence shows "없음", missing evidence shows "확인 전". A contradictory old positive balance never overrides explicit absence.

| Situation | Text | Colour | Button (standard, right-aligned to 344) |
|---|---|---|---|
| serving from credits | 크레딧 19,395 · 지금 사용 중 | warn-text | **[크레딧 사용 끄기]**: `PATCH allow_credits=false` |
| `allow_credits=true`, not serving | 크레딧 19,395 · 한도 도달 시 사용 | secondary | none |
| `allow_credits=false` | 크레딧 19,395 · 한도 도달 시 사용 안 함 | secondary | **[크레딧으로 계속]** only when the account is in account-scoped `cooling(quota)` **and** the provider verdict is 사용 불가, with `quota_credit_releasable=true`. A hard upstream rejection or legacy hold offers the conditional permission setting in the menu, not an immediate-continue action. It opens the confirmation below; only explicit confirmation sends `PATCH allow_credits=true`. |

- An account with no credits (for example hello.sample-acct) shows no credits line.


**Mandatory credits confirmation (R5 user decision).** Every transition from off to on, including the row-menu checkbox, opens an in-flyout confirmation styled like delete: title "유료 크레딧 사용"; body "<name>가 한도에 도달한 뒤, 잔액이 있을 때 유료 크레딧 사용을 허용할까요? (현재 잔액 <amount>)"; buttons [크레딧 사용] [취소]. Confirm alone sends `allow_credits=true`; cancel/Esc/dismiss sends nothing. Consent is never remembered. Turning credits **off** never asks. A checkbox-origin dialog returns to that menu.

### 6.5 Reason line

- **Layout.** 8 below the last line above it. Height 16, or 24 with a button. 12/16 secondary text on the left, trimmed with an ellipsis. An optional small button is right-aligned to 344: 24 high, padding 10, radius 4, 12 left gap. At most one button per row.
- **Accent buttons.** Only fixes the user is expected to take use accent: 다시 로그인, and 켜기 when the provider is unavailable. The others are standard.

| State | Text |
|---|---|
| needs_relogin, provider available | 그동안 `<serving name>`가 처리합니다. (If no account is in use: 그동안 다른 계정이 처리합니다.) |
| needs_relogin, provider unavailable | 토큰이 만료되어 이 계정을 쓸 수 없습니다. |
| link_stale | CPA가 토큰을 갱신하지 않습니다. CPA 실행을 확인하세요. |
| store_error | 토큰을 저장하지 못해 다시 시도하는 중입니다. |
| payment | 결제가 거절되어 `<until>`까지 쉽니다. |
| permission (account) | 조직 접근이 거부되어 `<until>`까지 쉽니다. |
| auth (probe) | 인증을 `<until>`에 다시 확인합니다. |
| pacing: `pacing.backoff_until` in the future, or `waiting > 0` (any group except needs-action) | 요청 속도 조절 중 · N초. This is informational, with no chip and no button. N counts down to `backoff_until`. |

- **Linked accounts** (`source.mode=linked`) never get [다시 로그인] (they cannot need re-login, C5).

### 6.6 Disabled accounts ("꺼진 계정")

- **Disclosure line.** 32 high, with a hover rect x 4–356 at radius 6. Text "꺼진 계정 N개" at x 16, 12/16 secondary, then 6 gap and a 10 DIP chevron: U+E76C collapsed, U+E70D expanded.
- **Default state.** Collapsed.
- **Auto-expand.** On flyout open, auto-expand when the provider verdict is 사용 불가 or 꺼짐. While expanded for that reason, a **hint line** follows the disclosure: 20 high, 12/16 secondary, x 16, "켜면 사용량을 확인한 뒤 바로 이어서 처리합니다."
- **A manual toggle** is remembered until the tray restarts.
- **Disabled row.** 44 high: padding 10, 8 right, 16 left.
  - Name (13/18, 400, secondary) and plan, as in §6.3.
  - Right side: small **[켜기]** button (accent when the provider is 사용 불가 or 꺼짐, standard otherwise), 8 left margin. It sends `POST /accounts/{id}/enable`.
  - ⋯ (row menu).
  - No usage line: disabled accounts are not polled.

### 6.7 Row transitions

- **Pending admin call** (any button or menu toggle). The button shows its label at 50 % opacity and is not interactive until the next `account_changed` or until the call fails. On failure, the reason line shows the error for 10 s in bad-text ("요청을 처리하지 못했습니다").
- **After [켜기].** The row moves into the main list on the next open (§4, order freeze). Until then it stays in place, its button disappears, and its name changes to text colour.

## 7. Footer (48)

- **Background.** surface-footer, with a 1 DIP divider on the top edge and padding 8 left and right.
- **Buttons.** Each is 32 high, padding 8, radius 4, with no fill at rest. Hover uses the overlay, and an open menu shows the overlay.
  - Content: a 14 DIP icon, a 6 gap, then the label in 13/18.
  - **[+ 계정 추가]** (U+E710) at the left. It opens the add menu: "Claude 계정 로그인" and "Codex 계정 로그인". These start the old §9 login panel.
  - **[⚙ 설정]** (U+E713) and **[⏻ 종료]** (U+E7E8) are right-aligned to 352, 2 apart.
- **Stale notice.** When the last successful `/state` is more than 60 s old, show "N분 전 정보" (12/16 secondary) right-aligned against [설정], 4 gap. Show "1분 전 정보" for anything under 2 min.
- **Login panel open.** [계정 추가] is disabled (text-disabled).

## 8. Menus

The surface, item metrics, separators, check items, keyboard and shadow are as in the old spec §7, using the v2 tokens. Leads are 16×16 glyphs, and text starts 12 after the lead.

### 8.1 Row menu (width 264)

- **Placement.** The right edge equals the ⋯ right edge. The top is ⋯ bottom + 4. If the menu would overlap the footer, flip it so its bottom is ⋯ top − 4; if it then overlaps the flyout top, clamp it to 8.
- **Right-click and keyboard.** Right-click anywhere on a row, Enter, Space, Shift+F10 or the Apps key also open the menu, anchored the same way.

**Info block** (non-interactive). Padding 8 top and bottom, 17 left and right, 12/16 secondary. The lines:
1. Full email, 13/18 600 text.
2. Provider · plan label · token source: "토큰 직접 갱신" (owned or adopted) or "CPA가 토큰 갱신" (linked).
   - If `refresh_attention` is set, add a line in bad-text: "토큰 갱신 확인 필요 · <token_expires_at> 만료".
3. 6 gap.
4. One line per window. The label is in a 44-wide column; the value reads "**N% 남음** · <time> 초기화".
   - Account windows come first (5h, 7d, others).
   - Model-scoped sub-buckets with a known value follow, labelled with their model name (for example "Opus 주간").
   - A model-scoped cooling adds " · 사용 제한" to its line.
5. Codex with credits: "크레딧 **19,395.23**", with the exact balance.
6. 6 gap.
7. "연결된 대화 N개 · HH:MM 기준", from `bound_sessions` and the snapshot's `observed_at`.

**Items, in order.** Hide an item when its condition is false; do not grey it out. Drop separators that would be leading, trailing or doubled.

| Item | Lead | Condition |
|---|---|---|
| 사용량 새로고침 | U+E72C | not disabled, not needs_relogin |
| 다시 로그인 | U+E8D7 | owned or adopted |
| 토큰 지금 갱신 | U+E895 | owned or adopted, `refresh_attention` set or token expires within 1 h, not drain, not needs_relogin |
| — separator — | | |
| ☐ 유료 크레딧 사용 허용 | check | Codex |
| ☐ 새 대화 받지 않기 | check | active or paused |
| — separator — | | |
| 일시정지 / 재개 | U+E769 / U+E768 | active / paused |
| 끄기 / 켜기 | U+E7E8 / U+E8FB | not disabled / disabled |
| 계정 삭제… | U+E74D | always (opens old §8 confirmation) |

A toggle shows "변경 중…" in place of its label while its PATCH is pending (as R1-tray.1 does today).

### 8.2 Settings menu (width 240), from [설정]

- **Placement.** The right edge is min(button right, 352). The bottom is the button top − 4.
- **설정 파일 열기** (U+E8A5): opens `config.toml` with the default handler for `.toml`, falling back to `notepad.exe`. If the file does not exist, create it with the commented defaults first.
- **로그 폴더 열기** (U+E838).
- A separator, then an info block (12/16 secondary): "quota-baton <version> · 코어 실행 중" (or "코어 연결 끊김"), and on the next line "설정 파일: config.toml".

### 8.3 Quit menu (width 300), from [종료]

- **Placement.** As for the settings menu.
- **Items.** Two two-line items, each 52 high. The title is 13/18 text, the subtitle 12/16 secondary with 2 gap, both left at item x + 12.
  - "트레이만 닫기" / "프록시는 계속 실행됩니다. CLI는 그대로 작동합니다."
  - "트레이와 프록시 종료" / "quota-baton을 거치는 CLI 요청이 멈춥니다."
- **Keyboard focus.** It starts on the first item.

## 9. Buttons

- **Standard.** btn-bg with a 1 DIP inset btn-border, radius 4.
- **Accent.** Solid accent fill, on-accent label.
- **States.**
  - **Hover**: standard gets the overlay; accent goes to 90 % opacity.
  - **Pressed**: the pressed overlay.
  - **Disabled or pending**: 50 % opacity.
- **Sizes.** Small row buttons are 24 high, padding 10, 12/24. Full buttons are 32 high, 13/18.

## 10. Empty state (no accounts at all)

- **Header.** The title row and the divider only.
- **Body.** Padding 20 top, 16 sides, 24 bottom.
  - Title "아직 계정이 없습니다", 15/20 600.
  - 8 below: "Claude나 Codex 계정을 추가하세요. 한 계정이 한도에 도달하면 다른 계정으로 이어서 처리합니다." 13/20 secondary, keep-all wrap.
  - 16 below: two equal **accent** buttons, 160 × 32, 8 gap: [+ Claude 계정 추가] and [+ Codex 계정 추가]. The + is U+E710 at 14 DIP with an 8 gap. Each opens that provider's login panel directly.
  - 16 below: "CPA 계정을 가져오려면 명령줄에서", 12/16 secondary. On the next line, `quota-baton accounts import cpa` in Cascadia Mono 12, text colour.
- **Footer.** Unchanged. [설정] works here; this answers "설정도 안 눌러지고".
- **Height.** The fixture is 298.

## 11. Tooltips

- **Behaviour.** Standard tooltip delay (`SPI_GETMOUSEHOVERTIME`, then show). Keyboard focus on a row also shows its usage tooltip after the same delay.
- **Look.** Popup fill, 1 DIP popup-border, radius 6, padding 8 × 10, maximum width 288, keep-all wrap. Place it below the hovered element, 14 gap, clamped 8 inside the flyout. If it does not fit below, place it above.

| Hover target | Tooltip text |
|---|---|
| Summary line | `Claude 계정 4개 · 켜짐 2개 · 지금 사용 가능 0개.` + (unavailable) `가장 빠른 초기화: acct-alpha 10/19(월) 14:00. 그때까지 새 요청은 오류로 끝납니다.` / (available) `acct-alpha가 대화 2개를 처리 중입니다.` |
| Usage line or bar | Line 1: `**주간 사용량**: 62% 사용, **38% 남음**`. Line 2 (secondary): `10/22(목) 15:22에 초기화됩니다.` Line 3: the other account window, `5시간: 41% 사용, 59% 남음 · 내일 01:40 초기화`. Last line (secondary): `막대는 남은 양입니다. 남은 양이 가장 적은 창을 보여 줍니다. HH:MM 기준.` |
| Chip 사용 중 | `이 계정에서 최근 5분 안에 요청한 대화 N개 · 연결된 대화 M개` |
| Chip 한도 도달 | `<window> 한도에 도달했습니다. <time>에 초기화되면 다시 사용합니다.` |
| Chip 크레딧 사용 중 | `구독 한도에 도달해 유료 크레딧으로 처리하고 있습니다.` |
| Chip 일시정지 / 새 대화 안 받음 | `새 대화를 이 계정에 배정하지 않습니다. 사용량은 계속 확인합니다.` / `이미 연결된 대화만 계속 처리합니다.` |
| Credits line | `크레딧 잔액 19,395.23 · 유료 크레딧 사용 허용: 꺼짐. 켜면 구독 한도에 도달해도 크레딧으로 계속 처리합니다.` |
| Plan | `요금제: Max 20x` |
| Name trimmed with an ellipsis | full email |

The "(요일)" weekday form appears only in tooltips.

## 12. Keyboard, focus and hit targets

- **Opening.** On open, no focus visual is shown until the first key press. The first Tab or arrow press focuses the first row.
- **Tab order.** For each section: each row, then that row's buttons (credits button, then reason button), then the disclosure line, then each disabled row and its [켜기]. After the sections come the footer buttons. Shift+Tab reverses the order.
- **Arrow keys.**
  - Up and Down move between rows and disclosure lines across sections, skipping buttons.
  - Home and End go to the first and the last row.
  - Left and Right on a disclosure line collapse and expand it.
- **Row activation.** Enter, Space, Shift+F10 or Apps open the row menu. Inside a menu the arrows move the highlight, Enter activates, and Esc closes the menu and returns focus to the row.
- **Esc.** With no menu open, Esc closes the flyout.
- **Focus ring.** As in the old spec §12: an inner 1 DIP focus-inner ring and an outer 2 DIP focus-outer ring, 3 DIP outside the control. A row's ring follows its hover rect (x 4–356, radius 6).
- **Hit targets.**
  - **Row.** The whole row rect, x 4–356 × the full row height including padding. A click or right-click anywhere opens the row menu. Hover draws the overlay on that rect at radius 6.
  - **Buttons inside a row** take precedence. Their hit rect is the visual width × 32 high, centred on the visual.
  - **⋯** has no separate target.
  - **Disclosure line**: x 4–356 × 32.
  - **Disabled row**: the whole 44-high rect opens its menu. [켜기] has its own 32-high hit rect.
  - **Footer buttons** are their visual rects, already 32.
  - **Summary lines and section labels** are not interactive.
- **Accessibility (UIA).**
  - **Row**: a list item named "<name>, <plan>, <chip>, <window> N% 남음, <time> 초기화".
  - **Summary line**: a text element with its full sentence.

## 13. Data the tray needs from the core

The tray computes nothing that the core does not expose. The following are additions or confirmations needed from the core:

| Field | Use | Status |
|---|---|---|
| `plan` for Claude as `pro` / `max_5x` / `max_20x` (or `max` when the tier is unknown) | §6.3 | **new**: the core derives it from the OAuth profile or usage response. Null when unknown. |
| `credits {has_credits, unlimited, balance}` per Codex account | §6.4 | exists (C9). Confirm the shape in AccountView. |
| `credits_in_use` | §5.1 row 4, §6.2 row 6, §6.4 | **new**, per the R1-tray.2 brief (C9 credit signals plus `x-codex-active-limit`) |
| `active_sessions`, `bound_sessions` | §5.1, §6.2, tooltips | exists (C7.1) |
| `pools[provider].eligible` | §5.1 | exists (C6.4) |
| `windows[].reset_at`, `used_pct`, window scope (account vs model sub-bucket) | §6.3 | exists. The scope must be distinguishable; add `scope` if names alone are not enough. |
| snapshot `observed_at` | menu info, tooltip | exists in the snapshot. Expose it in AccountView if it is missing. |
| `next_account` on quota events | notifications | **new**, per the R1-tray.2 brief |

## 14. Formatting

- **Times** (local):
  - same calendar day → `HH:MM`
  - next calendar day → `내일 HH:MM`
  - otherwise → `M/D HH:MM` (no zero padding on M and D)
  - tooltips add the weekday: `10/22(목) 15:22`
  - past reset times → `초기화됨`
- **Countdowns.** `N초` under 60 s; `M:SS` in the login panel.
- **Percent.** An integer with no decimals, always followed by "남음" in rows and the menu. Tooltips may also show the used share ("62% 사용").
- **Numbers.** Use thousands separators.

## 15. Notifications

**Rules.**
- Use the same vocabulary as the flyout. `<acct>` is the short name as in §6.3.
- Times follow §14.
- Debounce: one notification per account and kind per 30 min. A situation change (for example credits → all exhausted) is a new kind.
- Clicking a notification opens the flyout scrolled to that account's row, with its menu closed. The aggregate notifications open it at the top.
- Tokens, keys and upstream bodies never appear.
- This table supersedes the wording in `.sandbox/briefs/R1-tray.2-notifications.md` only where it differs: "내일" times, "켜기", "오류로 끝납니다".

| Situation | Title | Body |
|---|---|---|
| Quota reached, another account takes over (`next_account`) | `acct-alpha · Codex 주간 한도 도달` | `hello.sample-acct 계정으로 이어서 처리합니다. 10/22 15:22 초기화.` |
| Quota reached, the 5-hour window | `acct-alpha · Codex 5시간 한도 도달` | as above, with that window's reset |
| Quota reached, keeps serving from credits | `acct-alpha · Codex 주간 한도 도달` | `유료 크레딧으로 계속 처리합니다. 10/22 15:22 초기화.` |
| Quota reached, no eligible account, credits available but not allowed | `Codex 계정 모두 한도 도달` | `가장 빠른 초기화: acct-alpha 내일 01:40. acct-alpha의 유료 크레딧을 허용하면 바로 이어서 처리합니다.` |
| Quota reached, no eligible account, disabled accounts exist | `Claude 계정 모두 한도 도달` | `가장 빠른 초기화: acct-alpha 내일 14:00. 꺼진 계정 2개를 켜면 이어서 처리할 수 있습니다.` |
| Quota reached, no eligible account, nothing else | `Claude 계정 모두 한도 도달` | `가장 빠른 초기화: acct-alpha 내일 14:00. 그때까지 새 요청은 오류로 끝납니다.` |
| Reset within the reset hold | none | — |
| Credits being spent, then the user turns `allow_credits` off | none | — |
| Re-login needed, another account serves | `hello.sample-acct · Codex 다시 로그인 필요` | `트레이에서 이 계정의 "다시 로그인"을 누르세요. 그동안 acct-alpha가 처리합니다.` |
| Re-login needed, no other account | `hello.sample-acct · Codex 다시 로그인 필요` | `트레이에서 이 계정의 "다시 로그인"을 누르세요. 처리할 다른 계정이 없습니다.` |
| Linked token stale | `<acct> · <Provider> CPA 갱신 멈춤` | `CPA가 이 계정의 토큰을 갱신하지 않습니다. CPA 실행을 확인하거나 계정 인수를 고려하세요.` |
| Billing rejection | `<acct> · Claude 결제 거절` | `<until>까지 이 계정을 쉬고 다른 계정이 처리합니다.` (no other account: `<until>까지 이 계정을 쉽니다.`) |
| Organisation denial | `<acct> · Claude 조직 접근 거부` | `이 계정은 <until>까지 쉽니다.` |
| Store error (critical) | `<acct> · 토큰 저장 실패` | `토큰을 저장하지 못해 다시 시도하고 있습니다. 로그를 확인하세요.` |
| Burst | `새 대화 급증` | `클라이언트의 반복 실행을 확인하세요. 10분 동안 새 대화가 설정 한도를 넘었습니다.` |
| Core restarted | `코어 다시 시작` | `예기치 않게 종료된 코어를 다시 시작했습니다. 계정 상태를 확인하세요.` |

**Tray icon tooltip.** One line, at most 127 characters: `quota-baton · Claude 사용 불가(내일 14:00) · Codex 사용 가능`. The verdict words are those of §5.1. If the core is unreachable: `quota-baton · 코어에 연결할 수 없음`.

The icon badge is unchanged:
- **error**: core unreachable
- **attention**: any provider is 사용 불가, or any row is in the needs-action group
- **normal**: otherwise

## 16. Fixture (`mockup.html`, now = 10/18 21:05)

| Provider | Account | Plan | State | Binding window | Other |
|---|---|---|---|---|---|
| Claude | acct-alpha | Max 20x | cooling(quota) | 7d 100 %, reset 10/19 14:00 | 5h 0 % |
| Claude | hello.sample-acct | Max 5x | cooling(quota) | 7d 100 %, reset 10/20 09:00 | 5h 3 % |
| Claude | sample-acct-c, support | unknown | disabled | — | — |
| Codex | hello.sample-acct | Plus | needs_relogin (last known: 5h 100 %, 7d 64 %) | — | no credits |
| Codex | acct-alpha | Plus | cooling(quota) | 5h 100 %, reset 10/19 01:40 | 7d 62 % (reset 10/22 15:22); credits 19,395.23, `allow_credits=false` |
| Codex | sample-acct-c, support | Free | disabled | — | — |

The other states vary this fixture. Each state's numbers are in `S.<state>` in `mockup.html`.

## 17. Open questions (non-blocking)

- **Q1 (resolved by the R5 user decision).** Every off-to-on attempt requires confirmation; off never does. The mandatory rule in §6.4 supersedes the original mockup assumption.
- **Q2.** Claude Max tier: confirm the core can read `max_5x` / `max_20x` from the OAuth profile or usage response. Until it can, show "Max" or hide the plan.
- **Q3.** A theme choice in the settings menu, which would write `[tray] theme`: deferred, so that `config.toml` stays the single owner.
- **Q4.** Opening `config.toml` directly instead of the folder is a behaviour change from R1. The fallback is notepad.


## 18. Inherited common menu and panel geometry

This appendix makes the unchanged old-spec references self-contained. V2 tokens, item order and dimensions above take precedence.

### 18.1 Common menu mechanics

**Surface**:
- popup fill, 1 DIP popup-border, radius 8, padding 4 top/bottom (including the border).
- Shadow: black, y offset 8, Gaussian σ 8 (CSS blur 16), opacity 14 % light / 37 % dark (`D2D1_SHADOW` effect on the menu shape).

**Item**:
- 32 high, inset 5 from each outer edge, radius 4, padding 12 left.
- Hover or keyboard focus: hover overlay.
- With a lead column: a 16×16 lead, then text 12 after it, so the text starts at item x + 40.

**Separator**: 1 DIP popup-border, full inner width (x 1..W−1), 4 above and 4 below.

**Check item**: the lead is a 16×16 box, 1 DIP check-border, radius 3.
- Unchecked: no fill.
- Checked: accent fill, no stroke, with a U+E73E 12 DIP glyph in on-accent.

### 18.2 Delete confirmation

- **Scrim**: covers the whole flyout, including header and footer.
- **Card**:
  - x 16–344 (328 wide), centred vertically: `top = round((H − h)/2)`.
  - popup fill, 1 DIP popup-border, radius 8, popup shadow.
- **Body**, padding 20 (border included):
  - Title "계정 삭제".
  - 8 below it, the email on its own line, 13/20 600, with an ellipsis.
  - "계정을 quota-baton에서 삭제할까요?", 13/20.
  - 4 below it, "제공자 쪽 로그인은 취소되지 않습니다.", 12/16 secondary.
- **Button bar**:
  - 1 DIP divider on top, surface-footer, padding 16 top / 20 sides / 16 bottom.
  - Two equal buttons, 140 × 32, gap 8: [삭제] danger fill, [취소] standard.
  - Default focus: 취소. Esc = 취소.
- Fixture: card height 193 (body 127 + bar 64 + borders).

### 18.3 Login panel (replaces the list; header and footer stay)

- Padding 16 top, 16 sides, 20 bottom. Total height 176 with a two-line instruction.
- Title "Codex 로그인" / "Claude 로그인", 15/20 600.
- 12 below the title, the status block:
  - Progress ring at x 16, y +2: 16×16, stroke 2, a full track circle plus a 90° accent arc rotating once per second.
  - Text at x 42, 302 wide: "브라우저에서 로그인을 마치면 자동으로 완료됩니다.", 13/20, wrapped at spaces.
  - 4 below, at x 42: "남은 시간 M:SS", 12/16 secondary.
- 16 below the status block, two equal standard buttons, 160 × 32, gap 8:
  - [주소 복사] with icon U+E8C8 (14 DIP, 8 gap)
  - [취소]
- Completed, failed and expired replace the status block text. [다시 시도] replaces [주소 복사].

### 18.4 Focus ring geometry

- Inner ring: 1 DIP focus-inner, centred on the control rect inflated by0.5, radius r+0.5.
- Outer ring: 2 DIP focus-outer, centred on the rect inflated by2, radius r+2.
- V2 rows use their x4–356 hover rectangle; v2 keyboard order is §12.
