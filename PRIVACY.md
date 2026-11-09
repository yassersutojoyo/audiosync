# Privacy

AudioSync has no accounts, no analytics, no telemetry and no crash reporter. I don't run a server that it talks to, so I never see what you play or which speakers you have.

## What goes over the network

- Your speakers: audio goes only to the AirPlay, Google Cast and local speakers you turn on. AudioSync finds network speakers by asking for their Bonjour (mDNS) announcements on your local network.
- Web Remote: it's off until you turn it on. Then AudioSync serves a small page on your local network. It answers only devices with local or private addresses, and a phone has to enter a 6-digit pairing code before it can control anything. For each paired phone AudioSync keeps a hash of its token, not the token.
- Update checks: one of two things that reach past your network on their own (the other is Spotify cover art, below). The Mac app, the Windows exe and the Linux AppImage download `appcast.xml` from this repository's [GitHub Releases](https://github.com/yassersutojoyo/audiosync/releases) to see whether there's a newer version. An update is downloaded only after you say yes. GitHub sees your IP address, like with any download; see the [GitHub privacy statement](https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement).
  - On Windows and Linux the check runs at launch and every 24 hours while the app is open. Turn it off with File > Check for Updates Automatically.
  - On the Mac, turn it off in Settings > Updates > Check for Updates Automatically.
  - A `.deb` install checks the same way, but only points you to the release page. Builds you make yourself and the Microsoft Store package never check.
- Spotify cover art (Mac only): when Spotify is the app playing, the Mac app downloads the cover image from the address Spotify gives it, to show it next to the song. That request goes to whatever server Spotify's address names, not to me. There's no switch for it yet.

## What stays on your computer

- Settings and pairings: your choices, speaker pairings, Cast speakers and paired phones are saved only on your computer:
  - Mac: the app's preferences and `~/Library/Application Support/AudioSync`
  - Windows: `%LOCALAPPDATA%\AudioSync` (Apple TV pairings are encrypted for your Windows account)
  - Linux: `$XDG_DATA_HOME/audiosync`, usually `~/.local/share/audiosync`

  On Windows and Linux, quit AudioSync and delete that folder to start over.
- Audio capture: on the Mac, AudioSync asks for System Audio Recording to capture the audio you choose. What it captures goes only to the speakers you picked.
- Microphone: AudioSync uses an input device only when you choose one as the source, or the microphone when you calibrate a speaker's delay. Calibration recordings stay in memory and are never saved or sent.
- Debug log: it's off unless you start AudioSync with `AUDIOSYNC_DEBUG=1`. Then it writes to `/tmp/audiosync-debug.log` on the Mac, `%TEMP%\audiosync-debug.log` on Windows or `$XDG_STATE_HOME/audiosync/debug.log` on Linux. On Windows and Linux a crash adds one line to that file even when the log is off. It goes nowhere unless you attach it to an issue yourself.

Questions about this? [Open an issue](https://github.com/yassersutojoyo/audiosync/issues/new/choose).
