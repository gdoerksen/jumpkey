# pick-bench `--check` cases

Live checks of picking from the outside (roadmap step 6). Each case runs
`--repeat` times; it passes only if every repetition passes. The page
cases use `bench/fixtures/check.html`, which writes its state into its
own title: the last action (`click`, `dbl`, `ctx` and the element id),
an action counter, and counters of keys and Esc presses that reached the
page. The harness reads only that page's title.

| Case | What it does | Passes when |
|---|---|---|
| `pick-click` | Types the label drawn over a button | That button got a click, and no key reached the page |
| `pick-modifiers` | Types a label with Shift, Ctrl or Ctrl+Shift on the last key | Shift right-clicks; Ctrl and Ctrl+Shift double-click |
| `prefix-backspace` | Types the first char of another multi-char label, Backspace, then the right label | The right button is clicked and no key leaked (falls back to Backspace on an empty prefix when every label is one char) |
| `esc-cancel` | Shows labels, presses Esc | Labels close, nothing is clicked, Esc doesn't reach the page |
| `focus-kept` | Shows labels and watches the foreground window for 300 ms | The target stays the foreground window the whole time |
| `toggle` | Presses the hotkey again while labels show | Labels close within 500 ms and no other pick opens |
| `toggle-during-search` | Presses the hotkey twice 10 ms apart | Nothing is open 2 s later |
| `rapid-x3` | Three presses 40 ms apart | Exactly one overlay (open, close, open), and nothing opens after Esc |
| `wheel-cancel` | Turns the wheel while labels show | Labels close |
| `click-cancel` | Clicks an empty area of the page while labels show | Labels close and the page receives the click |
| `foreground-cancel` | Switches to another window while labels show | Labels close |
| `window-pick` | Window picker: types the label of the harness's own window | That window becomes the foreground window |
| `lone-alt` | On the Win32 check window, taps Alt alone while labels show | The window never enters menu-bar mode |
| `menu-win32` | Opens a popup menu on the check window, then shows labels and presses Esc | The menu stays open while labels show and after Esc |

The chords come from `bench/config/check.toml` and contain no Alt: with
`RegisterHotKey`, the chord reaches the app first, and Alt closes Win32
and Chromium menus (review #34).

## Expected results

`expected-<build>.json` maps each case to `pass` or `fail` for a given
build, so known failures are written down and any change, better or
worse, shows up as `CHANGED`:

- `expected-main.json`: today's build (measured 2026-11-22, 10 of 10
  repetitions each way). The overlay takes focus, so `focus-kept`
  fails. Hotkey presses during a pick queue up, so `toggle`,
  `toggle-during-search` and `rapid-x3` fail. `menu-win32` fails worse
  than expected: with a Win32 popup menu open the overlay shows but
  can't take focus, so Esc goes to the menu and the labels stay up
  (only the wheel closes them). In that state `jumpkey --close` doesn't
  work either: the overlay's nested message loop swallows the quit
  message.

Roadmap step 6 flips those as it lands: the toggle cases in PR 2, focus
and menus in PR 4.
