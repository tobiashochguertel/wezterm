---
tags:
  - touch
---
# `touch_long_press_slop`

{{since('nightly')}}

On Wayland touch devices, how far — in surface-local units, roughly
logical pixels — the primary touch point may move before a pending
[long-press](touch_long_press_ms.md) is cancelled and the gesture stays a
regular drag instead of becoming a right button press.

```lua
return {
  touch_long_press_slop = 10.0,
}
```

The default is `10.0`.
