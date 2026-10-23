# Live text output while dictating

**Type:** Decision doc
**Date:** 2026-10-23
**Status:** Decided 2026-10-23. Build A, off by default.

## Question

Should FnKey show or paste text while Fn is still held, instead of pasting once at release?

Raised by Eugene after learning that Deepgram already streams results back during the press.

## How it works today

- Audio streams to Deepgram over one WebSocket in 20 ms chunks while Fn is held.
- Deepgram sends drafts about once a second. A segment becomes final after a 0.3 s pause, or every few
  seconds of continuous speech. A final segment never changes.
- FnKey ignores drafts, joins the finals and pastes once at release. "Press Return after paste" fires
  after that single paste.

Measured from air on 2026-10-23, 4 runs, India region: release to last result took 0.16–0.18 s.

## Criteria (fixed before comparing)

1. Wait after release, until the text is in the app.
2. Feedback while speaking, so you know it hears you.
3. Risk of wrong, partial or doubled text landing in the app. Worse with auto-return on.
4. Code added to a small public app.

## Options

### A. Paste each final phrase as it arrives

- Wins criterion 2. Text shows up phrase by phrase in the focused app.
- No gain on criterion 1. The wait after release is already about 0.2 s.
- Strongest counterargument: text is in the app before you finish. If Deepgram drops midway, the Groq
  fallback would paste the whole clip again. So any live paste must switch the fallback off.
- Failure mode: you press Fn+Delete late or switch windows mid-sentence. Half a sentence stays behind,
  or lands in the wrong window.
- Size: about 60–100 lines. The paste and Return steps split up, a "pasted anything" flag skips the
  fallback, and a space goes between phrases. Off by default, through a config file, so other users
  see no change.

### B. Show draft words in the menu bar, paste once at release

- Partly wins criterion 2. No change to criteria 1 and 3.
- Size: about 20 lines. It reuses the status item title.
- Strongest counterargument: you don't look at the menu bar while talking. On the MacBook Air the notch
  can hide status items.
- Failure mode: a feature that is never seen.

### C. Keep paste at release

- Criterion 1 is already good. No risk, no code.
- Strongest counterargument: long dictations feel blind. A mishearing shows up only at the end.
- Failure mode: you keep re-dictating long passages because an early mishearing ruined the whole thing.

## Facts that would change the ranking

- Typical dictation length. Mostly under 10 s favours C. Often over 30 s favours A.
- How often you dictate into chats with auto-return on. Often raises A's risk.
- Whether you still notice a wait after release now that you use the India region.

## Preferred option compared

Eugene leaned to A as "not a big change". Where A is worse: it adds nothing to the wait after release,
and it is the only option that can leave partial or doubled text in the app. It is better only on
feedback while speaking.

## Recommendation

C, unless most dictations run past about 30 s. In that case, build A behind an off-by-default switch,
with the fallback disabled once anything is pasted.

## Arguments log

- 2026-10-23, Claude (latency): release to text is already 0.16–0.18 s from Bangkok on the India
  region, so live output cannot make release noticeably faster.
- 2026-10-23, Claude (risk): the same day, Groq turned silence into "Thank you for watching!" and
  auto-return sent it into a chat. Text that lands early cannot be taken back.
- 2026-10-23, Eugene (effort): "I guess it's not a big change." Agreed on size, about 60–100 lines.
- 2026-10-23, Claude (measurement): Deepgram's usage API refuses this key (403), so length was estimated
  from 2,182 messages Eugene sent to Claude in 30 days, mostly by FnKey. Median 10 words, about 4 s
  of speech at 2.5 words a second. 72% under about 10 s, 14% at 10–30 s, under 5% at 30–60 s. The
  10% over 150 words are probably mostly pasted text. This supports C. Live text would show early in
  about 1 dictation in 5.
- 2026-10-23, Eugene (after first use): "It's pasting. It's very, very good." Asked for a menu toggle like
  "Press Return after paste". Also: "somehow it doesn't always work, or there is a delay."
- 2026-10-23, Claude (measurement, the delay): a 30 s narration clip streamed from air to the India
  region at real time, 2 runs, the same both times. Finals came every 3.3–5.0 s of continuous speech,
  5–12 words each. 6 of 8 arrived within 0.3 s of the phrase's last word, and 2 lagged 1.2–1.8 s. A
  100 ms pause rule finalized no sooner than 300 ms. So live text shows up in chunks of a sentence or
  so. A dictation under about 4 s gets no early final and pastes only at release. That is Deepgram's
  finalization, not a FnKey fault. The log showed no errors, timeouts or fallbacks in the live tests.

## Questions for Eugene

1. Criterion 2: how long are most of your dictations? Under 10 s, 10–30 s, or often over 30 s?
   **Answer:** "You can measure it." Measured above: median about 4 s, 72% under about 10 s.
2. Criteria 1 and 2: which should FnKey do: keep paste at release, paste phrases live, or show a
   menu bar preview?
   **Answer:** Paste phrases live.

## Decision — 2026-10-23

Build A as a menu toggle, "Paste while speaking", next to "Press Return after paste" (Eugene asked for
the menu after first use). It is saved as `~/.config/fnkey/live_paste` and off by default. The recommendation stayed
C on the measured lengths. Eugene chose A, and being off by default keeps the risk his alone. Guard rails:
- Only final phrases paste. Drafts never do, so nothing has to be erased.
- Pastes are spaced at least 200 ms apart, so a burst of finals cannot race the clipboard.
- Once anything has pasted, no Groq fallback runs, so text is never doubled.
- "Press Return after paste" fires once, at release, after the last phrase.
- FnKey tags its own key events so its key listener never treats them as a shortcut.
