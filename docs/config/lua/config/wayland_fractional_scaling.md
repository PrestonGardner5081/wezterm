# `wayland_fractional_scaling`

When running as a native Wayland client on a compositor that supports the
`wp_fractional_scale_v1` and `wp_viewporter` protocols, wezterm renders at
the exact scale of the output (for example 1.25 or 1.5).

Without this, the compositor asks wezterm to render at the next integer
scale and then downsamples the result, which makes text softer and costs
extra GPU work every frame.

The default is `true`. Set it to `false` to fall back to integer scaling:

```lua
config.wayland_fractional_scaling = false
```
