---
tags:
  - mouse
---
# `mouse_double_click_ms`

{{since('nightly')}}

The interval, in milliseconds, within which successive clicks of the same
mouse button on the same character cell are counted as a click streak —
that is, the timing window for double click and triple click detection.

```lua
return {
  mouse_double_click_ms = 500,
}
```

The default is `500`.
