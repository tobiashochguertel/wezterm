---
tags:
  - touch
---
# `touch_scroll_multiplier`

{{since('nightly')}}

On Wayland touch devices, dragging with two fingers produces mouse wheel
scroll events: the content follows your fingers, so dragging down scrolls
back toward older output and dragging up scrolls toward the prompt.

```lua
return {
  touch_scroll_multiplier = 1.0,
}
```

The default is `1.0`. Setting it to `0` disables two-finger scrolling;
larger values scroll faster for the same drag distance.
