# Windows audio routing

Research notes for the Windows 11 version of AudioSync (`desktop/`). Process loopback capture is built, hardware test 1 has been run (results below) and double audio is detected and warned about ([Detection](#detection)), with a guide on the notice for fixing the routing by hand; the other hardware tests aren't done yet. The question: how to hear a captured app only through AudioSync's delayed, synced outputs, and not also directly from the PC.

## The problem

On macOS, AudioSync captures system or app audio with a Core Audio process tap whose mute behavior silences the source's own output (`ProcessTapCapture.swift`), so only the synced copies are heard.

On Windows, the matching tool is the Application Loopback API: [`ActivateAudioInterfaceAsync`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync) with [`AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params). What Microsoft documents about it:

- It captures the render streams of one process and its children, or of everything except that process tree ([`PROCESS_LOOPBACK_MODE`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode)). It needs Windows 10 build 20348 or later, so every Windows 11 release qualifies.
- The capture isn't tied to an audio endpoint, and a process with no render streams yields silence ([Application Loopback sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)).
- There's no option to mute the source. The app keeps playing on its own endpoint, undelayed, next to AudioSync's delayed copies: double audio.

Two documented facts matter for every option below:

- An app's output follows the default device only if the app opens its streams on the default device; apps that open a specific endpoint by ID handle routing themselves ([Stream routing](https://learn.microsoft.com/en-us/windows/win32/coreaudio/stream-routing)).
- For endpoint loopback, the tap sits before volume and mute by default, and post-volume capture is opt-in ([`AUDCLNT_STREAMOPTIONS_POST_VOLUME_LOOPBACK`](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/ne-audioclient-audclnt_streamoptions)). Microsoft doesn't say whether a per-app session's volume or mute applies before or after the process loopback tap. Users on the OBS forums report that muting or lowering an app in the Windows volume mixer also mutes or lowers what an application capture records ([thread 1](https://obsproject.com/forum/threads/how-do-i-mute-audio-from-a-certain-window-while-still-making-it-so-you-can-hear-in-the-recording.163301/), [thread 2](https://obsproject.com/forum/threads/bypass-windows-volume-level-in-capture.176825/post-709693)). That's community evidence, not documentation, so it became hardware test 1, and the measurement agrees with the forums: session mute and volume apply before the process loopback tap ([results](#test-1-results-session-mute)).

## Options

### 1. Per-app routing through the internal audio-policy interface

Windows keeps a per-app output device, the one Settings shows under System > Sound > Volume mixer. There is no public API for it. Tools such as [EarTrumpet](https://github.com/File-New-Project/EarTrumpet) and SoundSwitch set it through an internal COM interface, usually called `IAudioPolicyConfigFactory`. Its persisted get and set default-endpoint calls arrived in Windows 10 1803 (RS4) and are what the modern Sound settings use ([Hanselman on EarTrumpet 2.0](https://www.hanselman.com/blog/EarTrumpet20MakesWindows10sAudioSubsystemEvenBetterandItsFree.aspx)).

How it would work: AudioSync sets the source app's output (by process ID, render flow and role) to an endpoint nobody listens to, captures it with process loopback, and clears the route when capture stops. The device argument is an endpoint's device-interface path, built from its MMDevice ID and the documented [`DEVINTERFACE_AUDIO_RENDER`](https://learn.microsoft.com/en-us/windows/win32/coreaudio/devinterface-xxx-guids) class.

Risks:

- **Undocumented and unstable.** The interface changed with Windows 10 21H2 and Windows 11. Sound Deck lost per-app switching until it updated its interop ([SoundDeck #52](https://github.com/GeekyEggo/SoundDeck/issues/52)), and EarTrumpet's public source tree keeps separate variants for 21H2-and-later and older builds. Any Windows update could break it again. Microsoft's guidance is not to call undocumented APIs ([Undocumented APIs](https://learn.microsoft.com/en-us/windows/win32/w8cookbook/undocumented-apis)).
- **Store.** Microsoft Store policy 10.2.8 requires supported methods and user consent to change Windows settings, and names undocumented APIs as unsupported ([Store policies](https://learn.microsoft.com/en-us/windows/apps/publish/store-policies)). Expect rejection.
- **Persistence.** Windows persists the route per app. If AudioSync crashes, the app stays on the silent endpoint until AudioSync or the user resets it, so restore-on-launch bookkeeping is mandatory.
- **Partial coverage.** Apps that open a specific endpoint ignore it; Sound Deck users reported Chrome not following even after the fix.
- **Still needs a silent endpoint.** Routing only moves the app. Something inaudible has to exist to route it to (options 2 and 4).

Clean-room path: the method names and their meaning are public in prose (the post above, the [winappaudiorouter docs](https://winappaudiorouter.readthedocs.io/en/stable/)), but the interface IDs, activation class name and method order exist only in other projects' source or in Windows binaries. Our rules forbid copying another project's code, and an interface declaration is code; reverse engineering Windows to recover it is no better. Writing our own requires either the maintainer's approval to use those facts with the source and license recorded (EarTrumpet's license is MIT with an added excluded-entities clause, so it needs a careful read) or dropping the option. Licensing risk: medium. Effort: small to write, ongoing after every Windows feature update.

### 2. A virtual audio driver

A virtual endpoint gives AudioSync a silent place to send audio (the app is routed there, or the whole system's default is). It doesn't route anything by itself.

- **VB-CABLE (VB-Audio).** Closed-source, donationware. The [licensing page](https://vb-audio.com/Services/licensing.htm) allows embedding standard VB-CABLE in an installer (silent install) when its donationware model applies: users can see it's a VB-Audio product and can donate, and the distributor credits the origin and says it's donationware. Company or institutional deployments need paid licenses; the A+B and C+D cables can't be bundled. It's a third-party kernel driver we can't patch or rebuild, so it would be listed in `THIRD_PARTY_NOTICES.md` and need admin rights to install.
- **Virtual Audio Cable (Muzychenko).** Commercial, with Home and Business licenses; a custom build for developers' own apps is available by arrangement ([purchase page](https://vac.muzychenko.net/en/purchase.htm)). Paid, and not open source.
- **Open-source drivers.** [Scream](https://github.com/duncanthrax/scream) (MS-PL) is cross-signed and loads only under the policy exceptions, such as Secure Boot off. [Virtual-Audio-Driver](https://github.com/VirtualDrivers/Virtual-Audio-Driver) (MIT, with MS-PL parts from Microsoft's samples) is in beta and asks for test signing. Neither installs on a stock Secure Boot PC today.
- **Our own driver.** Microsoft's [SimpleAudioSample](https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/simple-audio-sample-device-driver/), sysvad, and the samples for the newer [ACX framework](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview) are a starting point. The [Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples/blob/main/LICENSE) repository is MS-PL, so derived files stay MS-PL: fine as a separate component next to AudioSync's own code, and no third-party GPL code is involved. Signing is the real cost. Since Windows 10 1607, new kernel drivers on Secure Boot PCs load only when signed through Microsoft's Hardware Dev Center, and a Dev Center account needs an EV code-signing certificate ([Driver signing policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)). Microsoft now presents attestation signing as a testing option that can't reach retail Windows Update and isn't Windows Certified; production drivers are meant to pass the HLK ([Driver signing options](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings)). Kernel code also means a bug can blue-screen the PC.
- **Installer impact (any driver).** MSIX doesn't support drivers ([Prepare to package](https://learn.microsoft.com/en-us/windows/msix/desktop/desktop-to-uwp-prepare)), so it needs an elevated MSI or EXE installer. Store policy 10.2.4 generally disallows depending on non-Microsoft drivers, with case-by-case exceptions only for WHCP-certified ones.

### 3. Manual setup by the user

- **Per-app output to an unused endpoint.** In Windows 11 Settings > System > Sound > Volume mixer, the user picks, for the source app, an output they don't listen to: a monitor's HDMI or DisplayPort audio with the monitor muted or without speakers, an unused jack, or a virtual cable. Process loopback isn't tied to an endpoint, so capture should continue (hardware test 2). The choice persists until the user resets it. All documented, no code; the cost is a setup step and a spare endpoint.
- **Mute the app in the volume mixer.** Doesn't work: test 1 measured that a muted session gives process loopback pure silence, and a turned-down one gives it the same lower level ([results](#test-1-results-session-mute)). AudioSync could have done this itself with documented APIs: find the app's session with [`IAudioSessionManager2`](https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nn-audiopolicy-iaudiosessionmanager2) and [`GetProcessId`](https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nf-audiopolicy-iaudiosessioncontrol2-getprocessid), then mute it through [`ISimpleAudioVolume`](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-isimpleaudiovolume). Session volume and mute persist across restarts, so the same restore-on-launch care as option 1 applies.
- **The app's own output picker.** Many players, browsers, chat apps and games let the user choose their output device. Pointing that at an unused endpoint works the same way, documented and per app.

### 4. Other approaches

- **Whole-system mode with exclude.** The user sets the Windows default output to an unused endpoint. AudioSync captures everything except its own process tree (the documented exclude mode), then plays delayed copies to the real speakers by explicit endpoint ID. Everything except the one-time default change is documented, AudioSync never hears itself, and no app needs routing. Downside: any app AudioSync isn't streaming is silent.
- **Detect and warn.** Sessions are enumerated per endpoint through `IAudioSessionManager2`, so AudioSync can tell which endpoint the captured app plays on. If that endpoint is also one of AudioSync's local outputs, or the user's default device, it can warn about double audio and link to the setup steps. [`IMMNotificationClient`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient) reports when the silent endpoint disappears (monitor switched off, cable unplugged) and Windows moves the app back to the default.
- **Use the original as the zero-delay reference.** Delay the other outputs to match the app's direct sound instead of silencing it. This fails as soon as any output needs to be ahead of the PC's speaker, which AirPlay's buffer always does. Not viable.
- **Clock note.** An app routed to another endpoint is paced by that endpoint's clock, so the capture clock differs from every output clock. The per-output drift correction described in [`ENGINE_SPEC.md`](ENGINE_SPEC.md) has to cover that (hardware test 7).

## Comparison

| Option | Documented | Main risk | Licensing | Effort | Store / installer |
|---|---|---|---|---|---|
| 1. Policy-interface routing | No | Breaks between Windows builds; persisted route after a crash | Interface facts only in third-party code or Windows binaries; needs approval | Small, then ongoing | Store 10.2.8 rejection likely |
| 2a. Bundle VB-CABLE | Driver: n/a | Third-party closed kernel driver | Donationware terms allow bundling with conditions | Small | Elevated installer; Store 10.2.4 |
| 2b. Our own driver | Yes (WDK) | Kernel bugs, signing pipeline | MS-PL sample base | Large: EV cert, HLK or attestation, upkeep | Elevated installer; Store 10.2.4 |
| 3. Manual per-app routing | Yes | Setup friction; needs a spare endpoint | None | Docs and a guide screen | No impact |
| 3b. Auto-mute session | Yes | Silences the capture too (measured in test 1): not viable | None | Small | No impact |
| 4. Whole-system exclude mode | Yes, except one manual default change | Other apps silent while active | None | Small | No impact |

## Recommendation

Build first: process loopback capture with only documented APIs, plus detect-and-warn, a guided manual setup (per-app output to an unused endpoint, or whole-system mode with exclude), and restore-on-launch for anything AudioSync changes. Hardware test 1 ruled out the shortcut: a muted session doesn't feed process loopback, so documented auto-mute (3b) is off the table and the guided setup stays the main path.

Defer: the undocumented policy interface (an opt-in experiment at most, only after the maintainer approves the clean-room source and with crash-safe restore), our own driver (EV certificate, signing pipeline, kernel risk), and bundling VB-CABLE (linking to it as an optional download in the setup guide is enough for now).

Update, 16 November 2026: the maintainer approved the undocumented default-endpoint call for whole-system mode, with the interface facts sourced as below. [Moving the default output while streaming](#moving-the-default-output-while-streaming) describes it. Per-app routing (option 1) stays deferred.

## Hardware tests on a Windows 11 PC

1. **Session mute.** Play a test tone in an app, capture it with process loopback (include tree), then mute the app in the volume mixer, set it to 50%, and do both again with `ISimpleAudioVolume`. Measure the captured level each time. Done with `ISimpleAudioVolume`; see the results after this list.
2. **Routed endpoint.** Route the app to a second endpoint (HDMI audio, an unused jack, a virtual cable). Capture continues; nothing is audible on the default device.
3. **Endpoint loss.** Disable or unplug that endpoint mid-capture. Where does the app's audio go, does capture survive, and how quickly does the notification arrive?
4. **Whole-system exclude.** Default output on an unused endpoint; AudioSync renders to the real speakers by ID with its own process excluded. No feedback, no double audio.
5. **Persistence.** Per-app route and session mute survive an app restart and a reboot; Settings' reset clears them; AudioSync restores them after a forced kill.
6. **Multi-process apps.** A browser playing from a child process: include tree captures it, and the route or mute applies to the right executable.
7. **Drift.** Capture an app paced by endpoint A while rendering to B and to AirPlay for 30 minutes; check lag stays within the drift-correction bounds.
8. **Builds.** Repeat 1 to 4 on each supported Windows 11 feature update (23H2, 24H2, 25H2), and on every new one before release.
9. **Reused ancestor ID.** Start AudioSync from a helper process that then exits, start processes until one gets the helper's old process ID, and capture that process with include tree. AudioSync's own output must not come through. AudioSync now accepts such an app because it checks process creation times; if Windows' include-tree walk follows recorded parent IDs without that check, go back to refusing any app whose recorded ancestors include AudioSync's.

### Test 1 results: session mute

Measured on Windows 11 25H2 (OS build 26200.9711) with [`desktop/examples/mute_probe.rs`](../desktop/examples/mute_probe.rs). To rerun it, close AudioSync and run `cargo run --release --example mute_probe` in `desktop/`. The probe starts a second copy of itself that plays a 1 kHz sine (peak -26 dBFS, so RMS -29 dBFS) on the default output in shared mode, finds that process's session through `IAudioSessionManager2` and `GetProcessId`, and sets its mute and volume through `ISimpleAudioVolume`: the same session values the volume mixer shows, though the mixer's own controls weren't clicked. Each setting is captured for 3 seconds with process loopback on the player's tree, and the first two also with a whole-system capture that excludes only a separate capture process. The run is silent: the probe mutes every active output endpoint before the tone starts, checks each one reads back as muted, and puts every endpoint's mute and volume back at the end (`mute_probe restore` does that by hand if a run dies halfway).

| Player's session | Process loopback, include tree (peak / RMS dBFS) | Whole system, exclude (peak / RMS dBFS) |
|---|---|---|
| Unmuted, 100% | -26.0 / -29.1 | -26.0 / -29.1 |
| Muted | silence | silence |
| Unmuted, 50% | -32.0 / -35.1 | not measured |
| Unmuted, 0% | silence | not measured |

Two runs per row, identical to 0.1 dB. "Silence" means every captured sample was zero: packets kept arriving at the normal rate, unflagged, just empty.

What it means:

- The session's mute and volume apply before the process loopback tap, for one app and for the whole system alike: 50% is exactly 6 dB down (a linear gain) and mute is digital silence. Auto-muting the source (3b) would silence AudioSync's copies too, so it's out. Guiding the user to send the app, or the Windows default output, to an output they don't listen to stays the recommendation.
- Endpoint mute sits after the tap: every output endpoint was muted for the whole test and the unmuted session still came through at full level. So muting the endpoint the app plays on, when AudioSync doesn't play there itself, might be a documented alternative to rerouting the app. It silences everything else on that endpoint too, and nothing beyond this run has tested it.

### Test 2 results: an app on another endpoint

Measured on Windows 11 26H2 (OS build 26300.9711) with `mute_probe on <endpoint ID>`, which starts the same tone player on an endpoint chosen by ID instead of the default, as an app moved there by its own output setting would play, and captures the player's tree with process loopback for 3 seconds, twice. The endpoint was Steam Streaming Speakers, an active virtual output nobody listened to. Unlike the plain run, `on` mutes nothing.

| Player renders on | Process loopback, include tree (peak / RMS dBFS) |
|---|---|
| The default output (test 1, row 1) | -26.0 / -29.1 |
| Steam Streaming Speakers, by ID | -26.0 / -29.1 |

The capture doesn't depend on the endpoint the app plays on, so moving one app's output to a silent endpoint keeps AudioSync's copy at full level, while muting its session silences the copy too (test 1, rerun on the same build: the same process loopback numbers). What this run doesn't show: whether Windows' per-app output setting moves a stream that is already playing, which needs the per-app interface itself.

## Detection

While AudioSync streams, it looks at every active output every 3 seconds: its mute and volume (`IAudioEndpointVolume`) and the apps with an active session on it (`IAudioSessionManager2`). It only reads. It never changes a mute, a volume or the default output. On Windows the check shows the notice only when there's no silent endpoint to move the sound to, or the whole system streams without the default moved: otherwise AudioSync moves the default output and holds back copies itself (see [Moving the default output while streaming](#moving-the-default-output-while-streaming)), and the answer only goes to the debug log and to rule 8.

The rule: while AudioSync plays to at least one output, the source is heard twice from any output that isn't muted or at volume 0 where

- the captured app plays (or one of its child processes, since the capture includes them), when one app is captured;
- anything but AudioSync plays, or that is the Windows default output, when the whole system is captured.

An output AudioSync plays to counts too: the original and the delayed copy meet on the same speaker. That's why the notice names the outputs you can simply mute apart from the ones AudioSync plays to as well. For those, the only fix is sending the app, or the default output, somewhere else.

A few choices I made along the way:

- An output keeps counting for about 15 seconds after the last active session there. Sessions go inactive whenever an app pauses or goes quiet for a moment, and without that hold a second app that plays a sound now and then would make the notice come and go in whole-system mode.
- A paused app (one app captured, silent everywhere) keeps counting on the outputs AudioSync doesn't play to until it plays somewhere else or that output gets muted. It'll most likely play there again, and a mute shows right away. On an output AudioSync plays to it drops out after the 15 seconds, because moving the app away only shows once it plays again.
- A virtual cable, or any other software device, counts only while AudioSync plays to it. Otherwise it's taken to be the output nobody listens to that the notice suggests.
- A dismissed notice stays away for the same source and the same outputs, or fewer of them, until that app quits (for the whole system, until AudioSync quits).

The known miss: a software device that forwards its sound to speakers you hear, like a voice changer or a virtual mixer, isn't flagged when one app is captured and plays into it, unless AudioSync plays to that device too. AudioSync can't see where the device sends its sound. Whole-system mode catches it anyway, because the forwarding app has its own session on the real speakers.

## Moving the default output while streaming

The whole-system source plays everything except AudioSync twice when Windows' default output is a speaker you hear: once straight from the app, once as AudioSync's delayed copy. From 0.9.3 AudioSync does option 4 itself while it streams the whole system: it makes an output nobody listens to Windows' default, so apps play there, and puts the default back when it stops. The capture needs no change for this. The whole-system capture is a process loopback that leaves out AudioSync's own tree and isn't tied to an endpoint (see [The problem](#the-problem)), so it takes in the apps wherever they play, and AudioSync's own outputs open their endpoints by ID, so they don't follow the default. ENGINE_SPEC clauses S469 and S470 hold the same rules.

### Interface facts and their sources

Windows has no documented call that sets the default output. The Sound control panel and tools such as EarTrumpet, SoundSwitch and AudioDeviceCmdlets use an undocumented COM class. These facts come from other projects' source and are written here as facts only. No declaration, signature line or code from those files goes into AudioSync, which declares the interface itself from this list. The maintainer approved this use on 16 November 2026.

- The class `CPolicyConfigClient` has CLSID `870af99c-171d-4f9e-af0d-e63df40c2bc9`. Sources: Sunshine (LizardByte), `PolicyConfig.h`, GPL-3.0 ([rendered source](https://docs.lizardbyte.dev/projects/sunshine/v2025.118.151840/PolicyConfig_8h_source.html)); EarTrumpet, `EarTrumpet/Interop/MMDeviceAPI/PolicyConfigClient.cs`, MIT with an added excluded-entities clause ([repository](https://github.com/File-New-Project/EarTrumpet)).
- The interface `IPolicyConfig` that AudioSync asks that class for has IID `f8679f50-850a-41cf-9c72-430f290290c8`. It derives from `IUnknown`, and after `IUnknown`'s three methods its methods are, in order: GetMixFormat, GetDeviceFormat, ResetDeviceFormat, SetDeviceFormat, GetProcessingPeriod, SetProcessingPeriod, GetShareMode, SetShareMode, GetPropertyValue, SetPropertyValue, SetDefaultEndpoint, SetEndpointVisibility. So SetDefaultEndpoint is the eleventh, slot 13 of the vtable counting from 0. Source: Sunshine's `PolicyConfig.h`, as above, which marks it compatible with Windows 7 and later.
- SetDefaultEndpoint takes two arguments, an endpoint ID string (null-terminated UTF-16, the same ID [`IMMDevice::GetId`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immdevice-getid) gives) and an [`ERole`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/ne-mmdeviceapi-erole) value, and returns an HRESULT. It sets one role per call, for the whole PC rather than one app ([Microsoft Q&A](https://learn.microsoft.com/en-za/answers/questions/226483/set-active-audio-endpoint)). Sources: Sunshine's `PolicyConfig.h`; EarTrumpet's `PolicyConfigClient.cs`, which sets one role per call; AudioDeviceCmdlets (Francois Gendron), `SOURCE/AudioDeviceCmdlets.cs`, MIT ([repository](https://github.com/frgnca/AudioDeviceCmdlets)), which sets the default device and the default communication device with separate calls.
- A success code doesn't prove the default moved. After a Windows 10 update in 2016, a script reported S_OK while the default stayed put ([AutoIt forum thread](https://www.autoitscript.com/forum/topic/183977-after-win10-anniversary-update-setdefaultendpoint-change-default-sound-device-is-no-longer-working)); that script asked for `IUnknown`'s IID instead of the one above. So AudioSync reads the default back with the documented [`IMMDeviceEnumerator::GetDefaultAudioEndpoint`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immdeviceenumerator-getdefaultaudioendpoint) after every call.
- Across versions: every project above uses the same CLSID and IID on Windows 10 and 11. The separate per-app interface (`IAudioPolicyConfigFactory`, option 1) is the one that changed with 21H2 and Windows 11; AudioSync doesn't use it. Process loopback already needs build 20348 or later, so older interface variants don't matter here.
- Roles: Windows has three, console, multimedia and communications, and each is assigned to exactly one render device at a time ([Device roles](https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-roles)). Apps and Windows sounds play on console and multimedia; chat and VoIP apps play on communications.
- When a default changes, Windows calls [`IMMNotificationClient::OnDefaultDeviceChanged`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immnotificationclient-ondefaultdevicechanged) once per changed role, with a null ID when no device can take the role. AudioSync already registers such a listener (`desktop/src/audio/wasapi/devices.rs`).
- What existing streams do: since Windows 7, streams opened on the default through Media Foundation, DirectSound or the WAVE functions move to the new default by themselves, and since Windows 10 1607 so do WASAPI streams opened on the default-device interface (`DEVINTERFACE_AUDIO_RENDER`) ([Stream routing](https://learn.microsoft.com/en-us/windows/win32/coreaudio/stream-routing), [Automatic stream routing](https://learn.microsoft.com/en-us/windows/win32/coreaudio/automatic-stream-routing)). An app that opened a specific endpoint by ID, or that has its own output set in Settings > System > Sound > Volume mixer, stays where it is.

### Rules

1. **Whole system only.** The switch happens only while the whole-system source streams to at least one output. One app, several apps and input sources are unchanged (no switch, no restore record): moving the default for one app would silence every other app on it. One app or a mix of apps is handled by rule 8's held copy instead: no copy goes to an output where a captured app already plays. Source: maintainer decisions, 16 and 17 November 2026.
2. **Which output counts as silent.** An active render endpoint qualifies when it is listed with the virtual kind (device enumerator `ROOT`, [S381](ENGINE_SPEC.md)) and isn't one of the outputs AudioSync plays to. A device with a physical jack never qualifies, nor does any other kind (built-in, USB, Bluetooth, HDMI, DisplayPort, other). When several qualify, one whose device no app records from right now goes first (an active capture session on a recording endpoint in the same device container, [`PKEY_Device_ContainerId`](https://learn.microsoft.com/en-us/windows/win32/properties/props-system-devices-containerid), as when an app listens to a loopback cable), then the smallest endpoint ID in byte order, so the choice holds across launches. AudioSync never installs or bundles a driver; with none present, rule 8 applies. Source: S381's kinds; [option 2](#2-a-virtual-audio-driver) for why no driver ships.
3. **Only console and multimedia.** AudioSync sets the console and multimedia roles and never touches communications, so calls stay where the user put them. Both roles move or neither does: when the second call or its read-back fails, the first role is put back at once. Source: [Device roles](https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-roles); maintainer decision, 16 November 2026.
4. **The record comes first.** Before the first call, AudioSync writes a restore record into its settings file, through the same atomic write as every other setting (S385): the previous default's endpoint ID for console and for multimedia, and the silent endpoint's ID. When that write fails, nothing is switched and rule 8 applies. When the default is already the silent endpoint for both roles (the user set it up by hand), nothing is switched and nothing is recorded.
5. **Putting it back.** AudioSync restores when streaming stops, when the source changes away from the whole system, when the app quits, when Windows asks to end the session ([`WM_QUERYENDSESSION`](https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-queryendsession), never refusing it) or ends it ([`WM_ENDSESSION`](https://learn.microsoft.com/en-us/windows/win32/shutdown/wm-endsession)), and at the next launch, before streaming starts, when a record is still on disk (after a crash, a kill or a power cut). Restoring sets each recorded role back to its previous endpoint only when that role's default is still the silent endpoint, then deletes the record. A previous endpoint that's no longer active is replaced by the reference output (S174), or else by the active non-virtual endpoint with the smallest ID; with neither, the role stays where it is, the debug log says so and the record is kept. A role that failed to restore keeps the record, and the next launch tries again. After a session-end restore the stream keeps running without a switch, under rule 8, until it stops.
6. **The user's choice wins.** When a console or multimedia default changes while switched to anything other than the silent endpoint and AudioSync didn't make that change, the user (or Windows) chose it. AudioSync doesn't switch back, doesn't restore over it later (rule 5 only restores a role still on the silent endpoint), and runs rule 8 until streaming starts again. Source: maintainer decision, 16 November 2026.
7. **The silent endpoint goes away.** When it leaves the active list while switched, AudioSync restores both roles to the recorded endpoints (whatever Windows picked in between), deletes the record and runs rule 8 until streaming starts again. The same applies when the user chooses the silent endpoint as one of AudioSync's outputs.
8. **No switch.** With no silent endpoint, a failed or missing interface (the class doesn't create, the interface isn't there, a call or read-back fails), a failed record write, or after rules 5, 6 or 7: Windows' default output that is also a local output gets no copy from AudioSync, since the original already plays there. While one app or a mix of apps is captured, with or without a silent endpoint, every local output the double-audio check finds a captured app playing on gets no copy either (for about 15 seconds after it stops playing there). Its row stays on and looks like any other switched-on row. Its volume, EQ and delay have nothing to act on while no copy plays there, and AudioSync sets its remembered Windows level only when it starts playing a copy: an output held out from the start keeps the level Windows has, and one held out after it played a copy gets its starting level back once it is switched off or streaming stops. On a PC with no virtual output this is the normal case: the original plays undelayed on the default output while the other outputs play AudioSync's copy, so the default output isn't in sync with AirPlay or Cast speakers. Nothing crashes, and the debug log gives the reason once. Source: maintainer decision, 16 November 2026.
9. **Apps that don't move.** An app that keeps playing on a real speaker after the switch (its own output set in the Volume mixer, or a stream opened on a specific endpoint) is found by the existing double-audio check (S382), which counts any app outside AudioSync's tree playing on an audible output, and goes to the debug log. While the switch holds, such an app is still heard twice where AudioSync plays too: a known gap. AudioSync doesn't read or change per-app outputs.
10. **Tested with a fake.** The switch and restore decisions go through one seam: reading the default per role, setting it, and the endpoint list. Tests drive a fake of that seam. No test, and no example in `desktop/examples/`, changes the real default output.

Known gaps: a virtual device whose sound reaches speakers (a voice changer, a virtual mixer, or "Listen to this device" on a cable's recording side) qualifies as silent, and AudioSync can't tell: "Listen to this device" shows no capture session, and Windows documents no property for it. Rule 2's preference only puts a device an app records from last. Apps on the silent endpoint run on that endpoint's clock, so the drift correction has to hold, as hardware test 7 asks.

## Moving one app's output

With one app as the source, moving Windows' default output would silence every other app, so AudioSync moves only that app's own output, the per-app choice Settings > System > Sound > Volume mixer offers ([default endpoint selection](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/default-audio-endpoint-selection)), to the same silent endpoint S469 picks, and puts the app's earlier choice back afterwards. Test 2 measured that process loopback keeps taking the app in at full level there. ENGINE_SPEC clause S471 holds the rules.

### Interface facts

Windows has no documented call for this. These facts come from EarTrumpet ([repository](https://github.com/File-New-Project/EarTrumpet)) and SoundSwitch ([repository](https://github.com/Belphemur/SoundSwitch)), written here as facts only; AudioSync declares the calls itself from this list.

- The runtime class `Windows.Media.Internal.AudioPolicyConfig` gives an activation factory through [`RoGetActivationFactory`](https://learn.microsoft.com/en-us/windows/win32/api/roapi/nf-roapi-rogetactivationfactory). AudioSync asks it for `ab3d4648-e242-459f-b02f-541c70306324` from build 21390 on and for `2a59116d-6c4f-45e0-a74f-707e3fef9258` before that.
- The interface derives from `IInspectable`. Counting from QueryInterface, slot 25 sets an app's output (process ID, data flow, role, endpoint as an HSTRING) and slot 26 reads it (the same keys, the endpoint returned as an HSTRING the caller frees). Slot 27 clears every app's choice at once; AudioSync never calls it.
- The endpoint is the [`IMMDevice::GetId`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immdevice-getid) ID wrapped as `\\?\SWD#MMDEVAPI#` + ID + `#{e6327cad-dcec-4949-ae8a-991e976a79d2}`, the render device-interface class ([DEVINTERFACE_AUDIO_RENDER](https://learn.microsoft.com/en-us/windows/win32/coreaudio/devinterface-xxx-guids)). An empty string puts the app back on the default.
- The call fails for a process with no audio session, so AudioSync moves only processes of the captured tree that have one and run the app's own executable (a shared host such as a web view runtime would move every app using it), looks again every check for helpers that start later, and puts an app back through whichever of its processes answers.
- Windows keeps the choice per executable and across restarts, which is why the earlier choice goes to a restore record first.

Checked on build 26300 (Windows 11 26H2) with an ignored test, `the_per_app_output_reads_back_on_this_pc` in `desktop/src/audio/wasapi/app_output.rs`, on the test's own process, which holds a stream of silence on the default output so it has a session: the read gave an empty string before any change (so empty means the default), the set to the silent endpoint read back as that endpoint in the wrapped form, and setting the empty string read back empty again. This PC runs only the later variant, so the earlier IID and its HSTRING argument are untested.

With no silent endpoint at all, the double-audio notice says the app is also heard on the PC; for the endpoint gone or a stream that didn't move, only the debug log says it, once per stream. An app that picks its own output device ignores the per-app choice.

## AirPlay on Windows

AirPlay isn't part of this routing question. AudioSync has its own AirPlay sender, written in Rust, in [`airplay/`](../airplay); [AIRPLAY_SPEC.md](AIRPLAY_SPEC.md) describes what it does.
