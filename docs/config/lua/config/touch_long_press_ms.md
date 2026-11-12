---
tags:
  - touch
---
# `touch_long_press_ms`

{{since('nightly')}}

On Wayland touch devices, how long in milliseconds the primary touch
point must be held down in roughly the same place before it is converted
into a right button press — the usual press-and-hold -> secondary click
gesture that other platforms provide.

```lua
return {
  touch_long_press_ms = 500,
}
```

The default is `500`. Setting it to `0` disables the long-press
conversion entirely.

See also [touch_long_press_slop](touch_long_press_slop.md).
