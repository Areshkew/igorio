# Windows overlay spike

Run `cargo run --release -- --selftest` here for the three-second check. Run
`cargo run --release -- --seconds 5` for a short demo. The demo defaults to
20 seconds and also handles console Ctrl+C.

The self-test passed click-through, foreground tracking, BitBlt capture, and
display affinity. The 240 × 200 logical window measured 300 × 250 physical
pixels at 125% scaling. With exclusion enabled, zero RGB pixels differed.
Turning it off produced 42,510 differing pixels in the same rectangle. The
test allows up to 0.1% of pixels to change between captures on a live desktop.

An earlier three-second run used about 47 ms of process CPU over 3.04 seconds,
roughly 1.5% of one logical CPU while the follower was running. Mouse movement
was uncontrolled, and the measurement excludes work done by the compositor.

The real app can reuse the nonactivating layered window, premultiplied BGRA
upload, and measured-time spring. This spike redraws on DPI changes and
moves the existing surface on other ticks. It sets Per-Monitor V2 in code
before creating any windows. The cursor chooses the monitor's work area and
a hidden window supplies its DPI, since the visible overlay can lag behind
the cursor across a monitor boundary.

The BitBlt check uses `CAPTUREBLT` so the control capture includes layered
windows. It waits for DWM after changing visibility and compares RGB only.
[BitBlt documentation](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/nf-wingdi-bitblt)

Mixed-DPI transitions, edge flips during deliberate cursor movement, the
default demo timeout, console Ctrl+C, and other capture tools were not tested.
