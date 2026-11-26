# Windows recorder spike

From this directory, run `cargo build --release` and `cargo run --release -- --selfcheck`.
Use `--cursor drawn1` or `--cursor drawn2` to select the composited cursor.
The default monitor is the one under the pointer. Explicit monitor indices are zero-based, sorted by desktop origin.
Zoom script centres are in monitor-local physical pixels. For example, `--zoom-script "1.5:2:480:270,3:1:480:270"`.
Ctrl+C or Ctrl+Break requests an early stop. A stop after capture starts can produce a one-frame file.
The self-check always runs for four seconds. It removes its MP4 unless passed `--keep`.

## Pipeline

1. Enumerate monitors and create a capture item for the selected HMONITOR with `CreateForMonitor`. Create the D3D11 device on that monitor's DXGI adapter.
2. Show two click-through layered squares. Set `WDA_EXCLUDEFROMCAPTURE` on the magenta square. The green square stands in for ink and uses default affinity.
3. Pull BGRA frames from a free-threaded capture pool. Hold the newest eligible frame in a private GPU texture. Use the first frame's `SystemRelativeTime` as the QPC origin. At each 60 Hz tick, select capture content at or before the tick. Repeat the last texture on a still desktop so time and zoom keep advancing.
4. Step `RecordingViewport` by 1/60 s. Sample its fractional crop rectangle with a bilinear shader. The same shader can composite the cursor. Fit the output to 3840×2160, preserve aspect ratio to within even-pixel rounding, and convert to BT.709 limited-range NV12 with a D3D11 video processor.
5. Send timestamped D3D11 surfaces to a Media Foundation H.264 Sink Writer. Enable hardware transforms and provide a DXGI manager. Retry with software if hardware initialization or first sample submission fails. Set the average bitrate to six times the output pixel count, with a 2 Mbps floor. That is about 0.10 bit per pixel per 60 Hz frame and is close to the Swift recorder's policy.
6. Release held capture frames, close the session and frame pool, and signal the window thread to destroy its own windows. Finalize and release the writer before releasing D3D11. The self-check opens the MP4 with a Source Reader. It scans decoded timestamps and examines only the marker regions in two RGB frames.

## API choices

- **Windows.Graphics.Capture through the `windows` crate.** This gives direct control of cursor mode, border preference, capture timestamps, and frame lifetimes. A capture wrapper would still need to expose these controls and the D3D11 surface. Desktop Duplication would require more work to match window inclusion and exclusion. A free-threaded pool avoids a dispatcher queue. The recording worker initializes WinRT and COM as MTA. The window thread uses Win32 and GDI without COM. It pumps messages and uses a short, bounded stop wait. Windows capture and MF calls that may block run on the worker under a process watchdog. In a driver hang, the watchdog bounds the process wait. It cannot promise MP4 finalization.
- **Shader crop and composite, then video-processor conversion.** A video-processor-only crop would use integer rectangles and need a separate cursor compositor. The shader keeps fractional positions for smooth motion and uses bilinear filtering at every zoom. This matches the Swift baseline. Fine text can look softer at 2x because the source has no extra pixels. NV12 chroma subsampling can soften coloured edges. There is no staging texture or explicit full-frame CPU readback in recording code.
- **Media Foundation Sink Writer.** It handles H.264 negotiation and MP4 muxing without a separate encoder dependency or custom muxer. The hardware path delivers DXGI buffers to the selected MFT. Media Foundation rejects a hardware-enabled writer with converters disabled. Give it matching NV12 and leave that attribute unset on the hardware path. A software MFT can map or copy the surface internally. An encoder that fails after accepting the first sample ends the recording. The spike does not restart the recording midstream. [Microsoft hardware-transform documentation](https://learn.microsoft.com/en-us/windows/win32/medfound/mf-readwrite-enable-hardware-transforms)
- **CFR from capture time.** Variable-duration samples would play still periods correctly, but would freeze a viewport or manually drawn cursor until the desktop changed. Resampling on a QPC-aligned 60 Hz clock also decouples the file from monitor refresh.
- **Cursor options.** System mode gives WGC's normal pointer with no extra application work. Drawn mode disables WGC's cursor, polls `GetCursorInfo`, gets the native dimensions and hot spot from `GetIconInfo`, and caches two small textures made with `DrawIconEx`. Drawing on black and white preserves usual alpha edges and legacy invert pixels. At 1x desktop zoom, the 32×32 cursor bitmap observed on this desktop covers 32×32 pixels in drawn1 and 64×64 in drawn2. The hot spot stays at the pointer position. Expect a slightly softer outline at 2x due to bilinear filtering. Desktop zoom scales the cursor too, provided it is inside the crop. The cache tracks handle changes and uses the first frame of animated cursors. The spike logs cursor submissions and reads only test-marker pixels in the recordings, so pointer appearance was not inspected.
- **Border.** The spike sets `IsBorderRequired = false`. Windows can ignore it without consent or when another session requires a border. The unpackaged spike does not open a consent prompt. A packaged app needs the borderless capture capability and must ask through `GraphicsCaptureAccess.RequestAccessAsync`. [Microsoft border documentation](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired)

## Measurements

The release build had no warnings. All four self-checks passed on Windows at 2560×1440 using the NVIDIA H.264 Encoder MFT. Each file had exactly 240 decoded frames over 4.000 s. The maximum timestamp gap was 16.667 ms. The green square measured 52×52 at 1x and 103×102 near 2x. There were zero matching magenta pixels near the excluded square.

| Run | Crop and NV12 avg / worst (ms) | WriteSample avg / worst (ms) | Wall / CPU (s) | Capture updates | MP4 bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| System 1 | 0.417 / 5.971 | 0.027 / 0.387 | 4.209 / 1.000 | 194 | 10,504,590 |
| System 2 | 0.444 / 4.763 | 0.037 / 0.866 | 4.034 / 0.859 | 194 | 10,568,043 |
| System 3 | 0.403 / 6.271 | 1.109 / 45.663 | 4.031 / 0.984 | 138 | 9,937,539 |
| Drawn 2x | 0.470 / 4.580 | 0.044 / 1.747 | 4.043 / 0.828 | 206 | 10,715,087 |

Crop and NV12 times measure CPU submission, including allocation. WriteSample measures wall time inside the call, including writer backpressure. Process CPU includes finalization and excludes verification. NVIDIA's per-process monitor returned two usable encoder utilization samples of 26% and 9%. It did not report per-process graphics utilization. Those samples verify encoder activity and do not time the shader or conversion.

The 2x cursor added 0.015 ms average CPU polling and upload time, with a 1.320 ms worst frame. It submitted 240 layers and uploaded the shape once. The fixed-1x zoom and drawn1 run averaged 0.016 ms per frame for the cursor, with a 0.879 ms worst frame. It produced 4,779,473 bytes in two seconds, or **2,389,736 bytes/s** at 1x. File size varies with the desktop content. The 25 ms recording finalized as a 2560×1440 H.264 MP4 with two frames, verified with ffprobe. All measurement files were removed.

## What to carry into M8

Keep the per-monitor D3D11 device, GPU crop and conversion, capture-clock timing, and the separation between capture/encoding and the windows. Keep the source-reader fixture for automated marker checks. Add an encoder-compatible texture pool and recycle surfaces only after downstream release. The spike allocates one output texture per frame. A production pipeline needs bounded queues, explicit error reporting, and a policy for missed deadlines. The spike keeps every output tick and can fall behind under load. Leave the test squares, CLI script, and process watchdog in the spike. Let the app own start, stop, and its real overlays.

To add audio, run WASAPI loopback on the playback endpoint for system audio. Open a capture endpoint separately for microphone audio. Align WASAPI packet QPC positions with the first capture frame, fill discontinuities with silence as appropriate, and feed PCM to an AAC stream configured on the same Sink Writer. [Microsoft loopback documentation](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)

## Edge cases

- **Second monitor with different DPI or refresh rate.** Not tested, expected behaviour is that the manifest's Per-Monitor V2 mode keeps monitor geometry and cursor coordinates in physical pixels, including negative origins. The output stays at 60 fps. Faster displays drop intervening updates. Slower or static displays repeat them.
- **HDR display.** Not tested, expected behaviour is clipped or washed-out highlights when HDR content goes through BGRA8/NV12. Use FP16 WGC surfaces and tone map for SDR, or implement a separate HDR format and encoder. [Microsoft capture documentation](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)
- **DRM content.** Not tested, expected behaviour is for Windows or the protected-media pipeline to hide the protected region, often as black, while the rest of the desktop remains visible.
- **Unplug or resolution change.** Not tested, expected behaviour is to stop and finalize the written samples when WGC closes the item, a capture call errors, or a frame reports a different size. If finalization succeeds, a regular recording keeps the partial file. The spike does not rebuild the pool while recording.
- **No hardware encoder.** Not tested on hardware without an encoder. Expected behaviour is to retry the software writer if the hardware setup or first write fails. The software path was exercised on this desktop during development after a hardware configuration error. It passed the full media and marker check and used 14.750 s of process CPU for a 4 s file. Sustained 4K60 software encoding was not measured.
