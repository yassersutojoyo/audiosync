# Windows audio routing

Research notes for the Windows 11 version of AudioSync (`windows/`). Process loopback capture is built; the detect-and-warn, the guided setup and the hardware tests below aren't done yet. The question: how to hear a captured app only through AudioSync's delayed, synced outputs, and not also directly from the PC.

## The problem

On macOS, AudioSync captures system or app audio with a Core Audio process tap whose mute behavior silences the source's own output (`ProcessTapCapture.swift`), so only the synced copies are heard.

On Windows, the matching tool is the Application Loopback API: [`ActivateAudioInterfaceAsync`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync) with [`AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params). What Microsoft documents about it:

- It captures the render streams of one process and its children, or of everything except that process tree ([`PROCESS_LOOPBACK_MODE`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode)). It needs Windows 10 build 20348 or later, so every Windows 11 release qualifies.
- The capture isn't tied to an audio endpoint, and a process with no render streams yields silence ([Application Loopback sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)).
- There's no option to mute the source. The app keeps playing on its own endpoint, undelayed, next to AudioSync's delayed copies: double audio.

Two documented facts matter for every option below:

- An app's output follows the default device only if the app opens its streams on the default device; apps that open a specific endpoint by ID handle routing themselves ([Stream routing](https://learn.microsoft.com/en-us/windows/win32/coreaudio/stream-routing)).
- For endpoint loopback, the tap sits before volume and mute by default, and post-volume capture is opt-in ([`AUDCLNT_STREAMOPTIONS_POST_VOLUME_LOOPBACK`](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/ne-audioclient-audclnt_streamoptions)). Microsoft doesn't say whether a per-app session's volume or mute applies before or after the process loopback tap. Users on the OBS forums report that muting or lowering an app in the Windows volume mixer also mutes or lowers what an application capture records ([thread 1](https://obsproject.com/forum/threads/how-do-i-mute-audio-from-a-certain-window-while-still-making-it-so-you-can-hear-in-the-recording.163301/), [thread 2](https://obsproject.com/forum/threads/bypass-windows-volume-level-in-capture.176825/post-709693)). That's community evidence, not documentation, so it's hardware test 1 below.

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
- **Our own driver.** Microsoft's [SimpleAudioSample](https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/simple-audio-sample-device-driver/), sysvad, and the samples for the newer [ACX framework](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview) are a starting point. The [Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples/blob/main/LICENSE) repository is MS-PL, so derived files stay MS-PL: fine as a separate component next to MIT code, and no GPL code is involved. Signing is the real cost. Since Windows 10 1607, new kernel drivers on Secure Boot PCs load only when signed through Microsoft's Hardware Dev Center, and a Dev Center account needs an EV code-signing certificate ([Driver signing policy](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later-)). Microsoft now presents attestation signing as a testing option that can't reach retail Windows Update and isn't Windows Certified; production drivers are meant to pass the HLK ([Driver signing options](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings)). Kernel code also means a bug can blue-screen the PC.
- **Installer impact (any driver).** MSIX doesn't support drivers ([Prepare to package](https://learn.microsoft.com/en-us/windows/msix/desktop/desktop-to-uwp-prepare)), so it needs an elevated MSI or EXE installer. Store policy 10.2.4 generally disallows depending on non-Microsoft drivers, with case-by-case exceptions only for WHCP-certified ones.

### 3. Manual setup by the user

- **Per-app output to an unused endpoint.** In Windows 11 Settings > System > Sound > Volume mixer, the user picks, for the source app, an output they don't listen to: a monitor's HDMI or DisplayPort audio with the monitor muted or without speakers, an unused jack, or a virtual cable. Process loopback isn't tied to an endpoint, so capture should continue (hardware test 2). The choice persists until the user resets it. All documented, no code; the cost is a setup step and a spare endpoint.
- **Mute the app in the volume mixer.** Only useful if process loopback still captures a muted session. Community reports say it doesn't (see above). If test 1 proves otherwise, AudioSync can do this itself with documented APIs: find the app's session with [`IAudioSessionManager2`](https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nn-audiopolicy-iaudiosessionmanager2) and [`GetProcessId`](https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nf-audiopolicy-iaudiosessioncontrol2-getprocessid), then mute it through [`ISimpleAudioVolume`](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nn-audioclient-isimpleaudiovolume). Session volume and mute persist across restarts, so the same restore-on-launch care as option 1 applies.
- **The app's own output picker.** Many players, browsers, chat apps and games let the user choose their output device. Pointing that at an unused endpoint works the same way, documented and per app.

### 4. Other approaches

- **Whole-system mode with exclude.** The user sets the Windows default output to an unused endpoint. AudioSync captures everything except its own process tree (the documented exclude mode), then plays delayed copies to the real speakers by explicit endpoint ID. Everything except the one-time default change is documented, AudioSync never hears itself, and no app needs routing. Downside: any app AudioSync isn't streaming is silent.
- **Detect and warn.** Sessions are enumerated per endpoint through `IAudioSessionManager2`, so AudioSync can tell which endpoint the captured app plays on. If that endpoint is also one of AudioSync's local outputs, or the user's default device, it can warn about double audio and link to the setup steps. [`IMMNotificationClient`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient) reports when the silent endpoint disappears (monitor switched off, cable unplugged) and Windows moves the app back to the default.
- **Use the original as the zero-delay reference.** Delay the other outputs to match the app's direct sound instead of silencing it. This fails as soon as any output needs to be ahead of the PC's speaker, which AirPlay's roughly two-second buffer always does. Not viable.
- **Clock note.** An app routed to another endpoint is paced by that endpoint's clock, so the capture clock differs from every output clock. The per-output drift correction described in [`ENGINE_SPEC.md`](ENGINE_SPEC.md) has to cover that (hardware test 7).

## Comparison

| Option | Documented | Main risk | Licensing | Effort | Store / installer |
|---|---|---|---|---|---|
| 1. Policy-interface routing | No | Breaks between Windows builds; persisted route after a crash | Interface facts only in third-party code or Windows binaries; needs approval | Small, then ongoing | Store 10.2.8 rejection likely |
| 2a. Bundle VB-CABLE | Driver: n/a | Third-party closed kernel driver | Donationware terms allow bundling with conditions | Small | Elevated installer; Store 10.2.4 |
| 2b. Our own driver | Yes (WDK) | Kernel bugs, signing pipeline | MS-PL sample base | Large: EV cert, HLK or attestation, upkeep | Elevated installer; Store 10.2.4 |
| 3. Manual per-app routing | Yes | Setup friction; needs a spare endpoint | None | Docs and a guide screen | No impact |
| 3b. Auto-mute session | Yes | May silence the capture too (test 1) | None | Small | No impact |
| 4. Whole-system exclude mode | Yes, except one manual default change | Other apps silent while active | None | Small | No impact |

## Recommendation

Build first: process loopback capture with only documented APIs, plus detect-and-warn, a guided manual setup (per-app output to an unused endpoint, or whole-system mode with exclude), and restore-on-launch for anything AudioSync changes. Run hardware test 1 before anything else: if a muted session still feeds process loopback, documented auto-mute (3b) becomes the default and most of the setup goes away.

Defer: the undocumented policy interface (an opt-in experiment at most, only after the maintainer approves the clean-room source and with crash-safe restore), our own driver (EV certificate, signing pipeline, kernel risk), and bundling VB-CABLE (linking to it as an optional download in the setup guide is enough for now).

## Hardware tests on a Windows 11 PC

1. **Session mute.** Play a test tone in an app, capture it with process loopback (include tree), then mute the app in the volume mixer, set it to 50%, and do both again with `ISimpleAudioVolume`. Measure the captured level each time.
2. **Routed endpoint.** Route the app to a second endpoint (HDMI audio, an unused jack, a virtual cable). Capture continues; nothing is audible on the default device.
3. **Endpoint loss.** Disable or unplug that endpoint mid-capture. Where does the app's audio go, does capture survive, and how quickly does the notification arrive?
4. **Whole-system exclude.** Default output on an unused endpoint; AudioSync renders to the real speakers by ID with its own process excluded. No feedback, no double audio.
5. **Persistence.** Per-app route and session mute survive an app restart and a reboot; Settings' reset clears them; AudioSync restores them after a forced kill.
6. **Multi-process apps.** A browser playing from a child process: include tree captures it, and the route or mute applies to the right executable.
7. **Drift.** Capture an app paced by endpoint A while rendering to B and to AirPlay for 30 minutes; check lag stays within the drift-correction bounds.
8. **Builds.** Repeat 1 to 4 on each supported Windows 11 feature update (23H2, 24H2, 25H2), and on every new one before release.
9. **Reused ancestor ID.** Start AudioSync from a helper process that then exits, start processes until one gets the helper's old process ID, and capture that process with include tree. AudioSync's own output must not come through. AudioSync now accepts such an app because it checks process creation times; if Windows' include-tree walk follows recorded parent IDs without that check, go back to refusing any app whose recorded ancestors include AudioSync's.

## AirPlay on Windows (out of scope)

Not planned now; recorded so the options aren't researched twice.
[`WINDOWS_AIRPLAY.md`](WINDOWS_AIRPLAY.md) now covers these options in depth, with a recommendation.

- **OwnTone.** Supports Linux and BSD, not Windows ([installation](https://owntone.github.io/owntone-server/installation/)). It could run under WSL2 or a container as the same separate GPL process. WSL's mirrored networking (Windows 11 22H2 and later) is documented to add multicast support ([WSL networking](https://learn.microsoft.com/en-us/windows/wsl/networking)), which mDNS discovery needs; untested for AirPlay.
- **libraop.** [RAOP (AirPlay 1) player and library](https://github.com/philippe44/libraop) with timing sync and a Windows build, MIT-licensed (it bundles third-party parts under their own terms). Linking it would make AudioSync speak AirPlay 1 itself, with no AirPlay 2.
- **pyatv.** [Python client library](https://github.com/postlund/pyatv), MIT. RAOP streaming of files and URLs, one stream at a time, and only legacy pairing because HAP-based encryption isn't implemented ([supported features](https://pyatv.dev/documentation/supported_features/)). Not a multi-room sender.
- **AirPlay 2 senders.** AirPlay 2 needs HomeKit-style pairing (SRP and Curve25519), encrypted RTSP and PTP timing for buffered multi-room audio. Apple's spec isn't public, so every open-source sender builds on reverse-engineered behavior; OwnTone is the mature one. A native Windows AirPlay 2 sender would be a large separate project.
- **Discovery.** Windows 10 and 11 have a native DNS-SD browse API ([`DnsServiceBrowse`](https://learn.microsoft.com/en-us/windows/win32/api/windns/nf-windns-dnsservicebrowse)), so finding speakers wouldn't need a bundled mDNS stack.
