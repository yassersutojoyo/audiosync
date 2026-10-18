//! Measures whether process loopback still captures an app whose audio
//! session is muted or turned down in the volume mixer: hardware test 1 in
//! `docs/WINDOWS_AUDIO_ROUTING.md`.
//!
//! From `windows/`: `cargo run --release --example mute_probe`. Close
//! AudioSync first; anything else that plays during the run shows up in the
//! whole-system rows.
//!
//! The probe stays silent. Before any tone plays it mutes every active render
//! endpoint through `IAudioEndpointVolume`, reads each mute back and stops if
//! one isn't muted. When it ends, on error paths too, it puts back each
//! endpoint's mute and volume and the tone player's session. The tone is a
//! 1 kHz sine at peak 0.05, about -26 dBFS.
//!
//! One binary, three roles:
//! - no argument: the controller. It mutes the endpoints, starts the player,
//!   sets the player's session through `ISimpleAudioVolume` (found with
//!   `IAudioSessionManager2` and `IAudioSessionControl2::GetProcessId`, the
//!   way the volume mixer finds it) and measures each condition;
//! - `play`: plays the tone on the default render endpoint in shared mode
//!   until its standard input closes;
//! - `system <ms>`: captures everything except its own process tree and
//!   prints the levels. The controller starts it, so the player is its
//!   sibling and isn't excluded;
//! - `restore`: puts back the endpoint mute and volume that a probe which
//!   died left behind (the controller keeps them in a file next to the
//!   executable while the endpoints are muted);
//! - `status`: prints each active render endpoint's mute and volume.

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    probe::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("mute_probe measures Windows audio sessions and runs only on Windows.");
}

#[cfg(windows)]
mod probe {
    use std::f64::consts::TAU;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::mem::ManuallyDrop;
    use std::path::PathBuf;
    use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};
    use std::{ptr, slice};

    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
        AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, DEVICE_STATE_ACTIVE,
        IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
        IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
        IAudioRenderClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
        IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, PROCESS_LOOPBACK_MODE,
        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        WAVE_FORMAT_PCM, WAVEFORMATEX, eConsole, eRender,
    };
    use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    };
    use windows::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::core::{HRESULT, HSTRING, IUnknown, Interface, PCWSTR, Ref, w};
    use windows_core::implement;

    /// The stream rate and channel count of the tone and of every capture.
    const RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;

    /// The test tone: a 1 kHz sine at peak 0.05, about -26 dBFS.
    const TONE_HZ: f64 = 1_000.0;
    const TONE_PEAK: f64 = 0.05;

    /// How long each condition is measured, and how long a session change
    /// gets to take effect first.
    const MEASURE: Duration = Duration::from_secs(3);
    const SETTLE: Duration = Duration::from_millis(600);

    /// How many times every condition is measured.
    const RUNS: usize = 2;

    /// The buffer every stream asks for, 100 ms in 100-ns units.
    const BUFFER_HNS: i64 = 1_000_000;

    /// How long a loopback activation may take, and how long a capture waits
    /// for a packet before it checks again, in ms.
    const ACTIVATION_TIMEOUT_MS: u32 = 5_000;
    const PACKET_WAIT_MS: u32 = 100;

    /// The player stops on its own after this long, in case nobody closes its
    /// input.
    const PLAYER_LIMIT: Duration = Duration::from_secs(300);

    /// A session state the controller measures.
    #[derive(Clone, Copy)]
    struct Condition {
        name: &'static str,
        muted: bool,
        level: f32,
    }

    /// Hardware test 1's conditions; the whole-system capture measures the
    /// first two.
    const CONDITIONS: [Condition; 4] = [
        Condition {
            name: "a. unmuted, 100%",
            muted: false,
            level: 1.0,
        },
        Condition {
            name: "b. muted (SetMute)",
            muted: true,
            level: 1.0,
        },
        Condition {
            name: "c. unmuted, 50%",
            muted: false,
            level: 0.5,
        },
        Condition {
            name: "d. unmuted, 0%",
            muted: false,
            level: 0.0,
        },
    ];

    type Fallible<T> = Result<T, String>;

    fn failed(call: &str, error: &windows::core::Error) -> String {
        format!("{call} failed: {error}")
    }

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let result = match args.first().map(String::as_str) {
            None => control(),
            Some("play") => play(),
            Some("system") => {
                let ms = args.get(1).and_then(|value| value.parse().ok());
                system(ms.map_or(MEASURE, Duration::from_millis))
            }
            Some("restore") => restore_saved(),
            Some("status") => status(),
            Some(other) => Err(format!("unknown role {other:?}; run it without arguments")),
        };
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("mute_probe: {message}");
                ExitCode::FAILURE
            }
        }
    }

    // MARK: Controller

    /// Mutes every endpoint, measures every condition and puts everything
    /// back, in that order.
    fn control() -> Fallible<()> {
        init_com()?;
        println!("Windows {}", windows_build());
        let enumerator = enumerator()?;
        // A run that stopped early (Ctrl+C, a closed console) left the
        // endpoints muted; measuring now would save "muted" as the original.
        let left = saved_state()?;
        if left.exists() {
            return Err(format!(
                "{} is left from a run that stopped early; run `mute_probe restore` first",
                left.display()
            ));
        }
        let mut endpoints = MutedEndpoints::mute_all(&enumerator)?;
        let result = measure_all(&enumerator);
        endpoints.restore();
        result
    }

    /// Starts the player, measures every condition, then puts the player's
    /// session back and stops the player.
    fn measure_all(enumerator: &IMMDeviceEnumerator) -> Fallible<()> {
        let mut player = start_player()?;
        let pid = player.child.id();
        let mut session = PlayerSession::find(enumerator, pid)?;
        let result = measure_conditions(&session, pid);
        session.restore();
        player.stop();
        result
    }

    fn measure_conditions(session: &PlayerSession, pid: u32) -> Fallible<()> {
        println!(
            "\n{:<24}{:<20}{:>4}{:>11}{:>10}{:>12}{:>9}{:>10}",
            "capture",
            "session",
            "run",
            "peak dBFS",
            "RMS dBFS",
            "1 kHz dBFS",
            "audio s",
            "silent s"
        );
        for run in 1..=RUNS {
            for condition in CONDITIONS {
                session.set(condition.muted, condition.level)?;
                thread::sleep(SETTLE);
                let levels = capture(
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                    pid,
                    MEASURE,
                )?;
                levels.print("process (include tree)", condition.name, run);
            }
            for condition in &CONDITIONS[..2] {
                session.set(condition.muted, condition.level)?;
                thread::sleep(SETTLE);
                let levels = system_capture(MEASURE)?;
                levels.print("system (exclude probe)", condition.name, run);
            }
        }
        Ok(())
    }

    /// Every active render endpoint, muted, with the mute and volume each had
    /// before. Dropping it puts them back.
    struct MutedEndpoints {
        endpoints: Vec<Endpoint>,
        restored: bool,
    }

    struct Endpoint {
        id: String,
        volume: IAudioEndpointVolume,
        muted: bool,
        level: f32,
    }

    impl MutedEndpoints {
        fn mute_all(enumerator: &IMMDeviceEnumerator) -> Fallible<Self> {
            let mut guard = Self {
                endpoints: Vec::new(),
                restored: false,
            };
            // SAFETY: calls on live COM objects this function owns.
            unsafe {
                let devices = enumerator
                    .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                    .map_err(|e| failed("IMMDeviceEnumerator::EnumAudioEndpoints", &e))?;
                let count = devices
                    .GetCount()
                    .map_err(|e| failed("IMMDeviceCollection::GetCount", &e))?;
                for index in 0..count {
                    let device = devices
                        .Item(index)
                        .map_err(|e| failed("IMMDeviceCollection::Item", &e))?;
                    let volume = device
                        .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                        .map_err(|e| failed("IMMDevice::Activate(IAudioEndpointVolume)", &e))?;
                    let muted = volume
                        .GetMute()
                        .map_err(|e| failed("IAudioEndpointVolume::GetMute", &e))?
                        .as_bool();
                    let level = volume.GetMasterVolumeLevelScalar().map_err(|e| {
                        failed("IAudioEndpointVolume::GetMasterVolumeLevelScalar", &e)
                    })?;
                    let id = device_id(&device);
                    println!(
                        "endpoint {}: muted {}, volume {level:.2} ({id})",
                        index + 1,
                        yes_no(muted)
                    );
                    guard.endpoints.push(Endpoint {
                        id,
                        volume,
                        muted,
                        level,
                    });
                }
                if guard.endpoints.is_empty() {
                    return Err("no active render endpoint".into());
                }
                guard.save()?;
                for endpoint in &guard.endpoints {
                    endpoint
                        .volume
                        .SetMute(true, ptr::null())
                        .map_err(|e| failed("IAudioEndpointVolume::SetMute", &e))?;
                }
                for (index, endpoint) in guard.endpoints.iter().enumerate() {
                    let muted = endpoint
                        .volume
                        .GetMute()
                        .map_err(|e| failed("IAudioEndpointVolume::GetMute", &e))?;
                    if !muted.as_bool() {
                        return Err(format!(
                            "endpoint {} isn't muted; stopping before any tone plays",
                            index + 1
                        ));
                    }
                }
            }
            println!(
                "all {} active render endpoints muted, read back as muted",
                guard.endpoints.len()
            );
            Ok(guard)
        }

        /// Writes each endpoint's ID, mute and volume to [`saved_state`], so
        /// that `mute_probe restore` can put them back if the probe dies.
        fn save(&self) -> Fallible<()> {
            let text: String = self
                .endpoints
                .iter()
                .map(|endpoint| {
                    format!(
                        "{}\t{}\t{}\n",
                        endpoint.id,
                        u8::from(endpoint.muted),
                        endpoint.level
                    )
                })
                .collect();
            let path = saved_state()?;
            std::fs::write(&path, text)
                .map_err(|error| format!("writing {} failed: {error}", path.display()))
        }

        /// Puts back each endpoint's volume, then its mute, and prints what
        /// it reads back. Runs once; removes [`saved_state`] when every
        /// endpoint reads back as it was.
        fn restore(&mut self) {
            if self.restored {
                return;
            }
            self.restored = true;
            let mut all_back = true;
            for (index, endpoint) in self.endpoints.iter().enumerate() {
                // SAFETY: calls on a live endpoint volume object. The volume
                // is only written when it moved, so an untouched one stays
                // exactly as it was.
                let (level_set, mute_set, muted, level) = unsafe {
                    let level_set = match endpoint.volume.GetMasterVolumeLevelScalar() {
                        Ok(level) if (level - endpoint.level).abs() <= 0.001 => Ok(()),
                        _ => endpoint
                            .volume
                            .SetMasterVolumeLevelScalar(endpoint.level, ptr::null()),
                    };
                    (
                        level_set,
                        endpoint.volume.SetMute(endpoint.muted, ptr::null()),
                        endpoint.volume.GetMute(),
                        endpoint.volume.GetMasterVolumeLevelScalar(),
                    )
                };
                let state = match (muted, level) {
                    (Ok(muted), Ok(level)) => {
                        let same = muted.as_bool() == endpoint.muted
                            && (level - endpoint.level).abs() < 0.001;
                        all_back &= same;
                        format!(
                            "reads muted {}, volume {level:.2}{}",
                            yes_no(muted.as_bool()),
                            if same { "" } else { " (DIFFERS)" }
                        )
                    }
                    _ => {
                        all_back = false;
                        "can't be read back".into()
                    }
                };
                let errors = [level_set.err(), mute_set.err()]
                    .into_iter()
                    .flatten()
                    .map(|error| format!(", error {error}"))
                    .collect::<String>();
                println!(
                    "restored endpoint {} to muted {}, volume {:.2}: {state}{errors} ({})",
                    index + 1,
                    yes_no(endpoint.muted),
                    endpoint.level,
                    endpoint.id
                );
            }
            if all_back && let Ok(path) = saved_state() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Prints each active render endpoint's mute and volume; changes nothing.
    fn status() -> Fallible<()> {
        init_com()?;
        let enumerator = enumerator()?;
        // SAFETY: calls on live COM objects this function owns.
        unsafe {
            let devices = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| failed("IMMDeviceEnumerator::EnumAudioEndpoints", &e))?;
            let count = devices
                .GetCount()
                .map_err(|e| failed("IMMDeviceCollection::GetCount", &e))?;
            for index in 0..count {
                let device = devices
                    .Item(index)
                    .map_err(|e| failed("IMMDeviceCollection::Item", &e))?;
                let volume = device
                    .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
                    .map_err(|e| failed("IMMDevice::Activate(IAudioEndpointVolume)", &e))?;
                let muted = volume
                    .GetMute()
                    .map_err(|e| failed("IAudioEndpointVolume::GetMute", &e))?;
                let level = volume
                    .GetMasterVolumeLevelScalar()
                    .map_err(|e| failed("IAudioEndpointVolume::GetMasterVolumeLevelScalar", &e))?;
                println!(
                    "endpoint {}: muted {}, volume {level:.2} ({})",
                    index + 1,
                    yes_no(muted.as_bool()),
                    device_id(&device)
                );
            }
        }
        Ok(())
    }

    /// Where the endpoints' original state is kept while they are muted:
    /// next to the probe's executable, inside the build folder.
    fn saved_state() -> Fallible<PathBuf> {
        Ok(own_path()?.with_file_name("mute_probe-endpoints.txt"))
    }

    /// Puts back the endpoint state that a probe which died left in
    /// [`saved_state`].
    fn restore_saved() -> Fallible<()> {
        init_com()?;
        let enumerator = enumerator()?;
        let path = saved_state()?;
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("nothing to restore ({}: {error})", path.display()))?;
        let mut guard = MutedEndpoints {
            endpoints: Vec::new(),
            restored: false,
        };
        // Lines that can't be restored now (unreadable, or an endpoint that's
        // unplugged or disabled) are written back for a later restore, so
        // one missing endpoint never costs the others their saved state.
        let mut kept = String::new();
        for line in text.lines().filter(|line| !line.is_empty()) {
            let mut fields = line.split('\t');
            let (Some(id), Some(muted), Some(Ok(level))) = (
                fields.next(),
                fields.next(),
                fields.next().map(str::parse::<f32>),
            ) else {
                println!("unreadable line {line:?}: kept");
                kept.push_str(line);
                kept.push('\n');
                continue;
            };
            // SAFETY: calls on live COM objects this function owns.
            let found = unsafe {
                enumerator
                    .GetDevice(&HSTRING::from(id))
                    .and_then(|device| device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None))
            };
            let volume = match found {
                Ok(volume) => volume,
                Err(error) => {
                    println!("endpoint {id} isn't there ({error}): kept for a later restore");
                    kept.push_str(line);
                    kept.push('\n');
                    continue;
                }
            };
            guard.endpoints.push(Endpoint {
                id: id.to_string(),
                volume,
                muted: muted == "1",
                level,
            });
        }
        guard.restore();
        if kept.is_empty() {
            return Ok(());
        }
        std::fs::write(&path, kept)
            .map_err(|error| format!("writing {} failed: {error}", path.display()))?;
        Err(format!(
            "some endpoints weren't restored; they stay in {} (plug them back in and run `mute_probe restore` again)",
            path.display()
        ))
    }

    impl Drop for MutedEndpoints {
        fn drop(&mut self) {
            self.restore();
        }
    }

    /// The player's audio session, as the volume mixer sees it, with the mute
    /// and volume it had. Dropping it puts them back.
    struct PlayerSession {
        volume: ISimpleAudioVolume,
        muted: bool,
        level: f32,
        restored: bool,
    }

    impl PlayerSession {
        /// Waits up to five seconds for `pid`'s session to show up on an
        /// active render endpoint.
        fn find(enumerator: &IMMDeviceEnumerator, pid: u32) -> Fallible<Self> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(volume) = session_volume(enumerator, pid) {
                    // SAFETY: calls on a live session volume object.
                    let (muted, level) = unsafe { (volume.GetMute(), volume.GetMasterVolume()) };
                    let muted = muted
                        .map_err(|e| failed("ISimpleAudioVolume::GetMute", &e))?
                        .as_bool();
                    let level =
                        level.map_err(|e| failed("ISimpleAudioVolume::GetMasterVolume", &e))?;
                    println!(
                        "player session (pid {pid}): muted {}, volume {level:.2}",
                        yes_no(muted)
                    );
                    return Ok(Self {
                        volume,
                        muted,
                        level,
                        restored: false,
                    });
                }
                if Instant::now() > deadline {
                    return Err(format!("no audio session for the player (pid {pid})"));
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        /// Sets the session's volume and mute, and checks they read back.
        fn set(&self, muted: bool, level: f32) -> Fallible<()> {
            // SAFETY: calls on a live session volume object.
            unsafe {
                self.volume
                    .SetMasterVolume(level, ptr::null())
                    .map_err(|e| failed("ISimpleAudioVolume::SetMasterVolume", &e))?;
                self.volume
                    .SetMute(muted, ptr::null())
                    .map_err(|e| failed("ISimpleAudioVolume::SetMute", &e))?;
                let now_muted = self
                    .volume
                    .GetMute()
                    .map_err(|e| failed("ISimpleAudioVolume::GetMute", &e))?
                    .as_bool();
                let now_level = self
                    .volume
                    .GetMasterVolume()
                    .map_err(|e| failed("ISimpleAudioVolume::GetMasterVolume", &e))?;
                if now_muted != muted || (now_level - level).abs() > 0.001 {
                    return Err(format!(
                        "the session reads muted {}, volume {now_level:.2} after setting muted {}, volume {level:.2}",
                        yes_no(now_muted),
                        yes_no(muted)
                    ));
                }
            }
            Ok(())
        }

        /// Puts back the session's mute and volume. Runs once.
        fn restore(&mut self) {
            if self.restored {
                return;
            }
            self.restored = true;
            match self.set(self.muted, self.level) {
                Ok(()) => println!(
                    "\nrestored the player session to muted {}, volume {:.2}",
                    yes_no(self.muted),
                    self.level
                ),
                Err(message) => println!("\ncouldn't restore the player session: {message}"),
            }
        }
    }

    impl Drop for PlayerSession {
        fn drop(&mut self) {
            self.restore();
        }
    }

    /// The `ISimpleAudioVolume` of `pid`'s session on any active render
    /// endpoint.
    fn session_volume(enumerator: &IMMDeviceEnumerator, pid: u32) -> Option<ISimpleAudioVolume> {
        // SAFETY: calls on live COM objects this function owns; an endpoint
        // or session that fails is skipped.
        unsafe {
            let devices = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .ok()?;
            for index in 0..devices.GetCount().ok()? {
                let Ok(device) = devices.Item(index) else {
                    continue;
                };
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                    continue;
                };
                let Ok(sessions) = manager.GetSessionEnumerator() else {
                    continue;
                };
                let Ok(total) = sessions.GetCount() else {
                    continue;
                };
                for number in 0..total {
                    let Ok(session) = sessions.GetSession(number) else {
                        continue;
                    };
                    let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                        continue;
                    };
                    if session.GetProcessId().is_ok_and(|id| id == pid)
                        && let Ok(volume) = session.cast::<ISimpleAudioVolume>()
                    {
                        return Some(volume);
                    }
                }
            }
        }
        None
    }

    /// A child process of the probe. Dropping it closes its input, gives it a
    /// moment to leave and then ends it, so no tone keeps playing.
    struct Spawned {
        child: Child,
        stdin: Option<ChildStdin>,
        name: &'static str,
    }

    impl Spawned {
        fn stop(&mut self) {
            drop(self.stdin.take());
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    _ => break,
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
            println!("ended the {} process by force", self.name);
        }
    }

    impl Drop for Spawned {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn own_path() -> Fallible<PathBuf> {
        std::env::current_exe().map_err(|error| format!("current_exe failed: {error}"))
    }

    /// Starts the tone player and waits until it plays.
    fn start_player() -> Fallible<Spawned> {
        let mut child = Command::new(own_path()?)
            .arg("play")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("starting the player failed: {error}"))?;
        let stdout = child.stdout.take();
        let stdin = child.stdin.take();
        let player = Spawned {
            child,
            stdin,
            name: "player",
        };
        let mut line = String::new();
        if let Some(stdout) = stdout {
            BufReader::new(stdout)
                .read_line(&mut line)
                .map_err(|error| format!("reading from the player failed: {error}"))?;
        }
        if line.trim() != "ready" {
            return Err("the player didn't start".into());
        }
        Ok(player)
    }

    /// Runs a whole-system capture in a child process and reads its levels.
    fn system_capture(duration: Duration) -> Fallible<Levels> {
        let mut child = Command::new(own_path()?)
            .arg("system")
            .arg(duration.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("starting the system capture failed: {error}"))?;
        let stdout = child.stdout.take();
        let mut spawned = Spawned {
            child,
            stdin: None,
            name: "system capture",
        };
        let mut text = String::new();
        if let Some(mut stdout) = stdout {
            stdout
                .read_to_string(&mut text)
                .map_err(|error| format!("reading from the system capture failed: {error}"))?;
        }
        spawned.stop();
        Levels::decode(&text).ok_or_else(|| "the system capture failed".into())
    }

    // MARK: System capture

    /// Captures everything except this process's tree and prints the levels
    /// for the controller.
    fn system(duration: Duration) -> Fallible<()> {
        init_com()?;
        let levels = capture(
            PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            std::process::id(),
            duration,
        )?;
        println!("{}", levels.encode());
        Ok(())
    }

    // MARK: Player

    /// Plays the tone on the default render endpoint until standard input
    /// closes (the controller ended) or [`PLAYER_LIMIT`] passed.
    fn play() -> Fallible<()> {
        init_com()?;
        let open = Arc::new(AtomicBool::new(true));
        let watch = Arc::clone(&open);
        thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            watch.store(false, Ordering::Relaxed);
        });
        let enumerator = enumerator()?;
        let format = wave_format(Sample::F32);
        let event = Event::new()?;
        // SAFETY: calls on live COM objects this function owns; `format`
        // lives through Initialize, which copies it.
        let (client, render, buffer) = unsafe {
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| failed("IMMDeviceEnumerator::GetDefaultAudioEndpoint", &e))?;
            let client = device
                .Activate::<IAudioClient>(CLSCTX_ALL, None)
                .map_err(|e| failed("IMMDevice::Activate(IAudioClient)", &e))?;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    BUFFER_HNS,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| failed("IAudioClient::Initialize", &e))?;
            client
                .SetEventHandle(event.0)
                .map_err(|e| failed("IAudioClient::SetEventHandle", &e))?;
            let render = client
                .GetService::<IAudioRenderClient>()
                .map_err(|e| failed("IAudioClient::GetService", &e))?;
            let buffer = client
                .GetBufferSize()
                .map_err(|e| failed("IAudioClient::GetBufferSize", &e))?;
            (client, render, buffer)
        };
        let mut tone = Tone::default();
        fill(&client, &render, buffer, &mut tone)?;
        // SAFETY: starts the stream this function owns.
        unsafe { client.Start() }.map_err(|e| failed("IAudioClient::Start", &e))?;
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "ready");
        let _ = stdout.flush();
        let deadline = Instant::now() + PLAYER_LIMIT;
        let result = loop {
            if !open.load(Ordering::Relaxed) || Instant::now() > deadline {
                break Ok(());
            }
            // SAFETY: waits on an event this function owns.
            unsafe { WaitForSingleObject(event.0, 200) };
            if let Err(message) = fill(&client, &render, buffer, &mut tone) {
                break Err(message);
            }
        };
        // SAFETY: stops the stream this function owns.
        let _ = unsafe { client.Stop() };
        result
    }

    /// The test tone's oscillator.
    #[derive(Default)]
    struct Tone {
        phase: f64,
    }

    impl Tone {
        fn next_sample(&mut self) -> f32 {
            let value = TONE_PEAK * self.phase.sin();
            self.phase += TAU * TONE_HZ / f64::from(RATE);
            if self.phase >= TAU {
                self.phase -= TAU;
            }
            value as f32
        }
    }

    /// Fills the free part of the render buffer with the tone.
    fn fill(
        client: &IAudioClient,
        render: &IAudioRenderClient,
        buffer: u32,
        tone: &mut Tone,
    ) -> Fallible<()> {
        // SAFETY: calls on the stream this thread owns; GetBuffer hands out
        // `frames` frames of 32-bit float stereo, writable until
        // ReleaseBuffer.
        unsafe {
            let padding = client
                .GetCurrentPadding()
                .map_err(|e| failed("IAudioClient::GetCurrentPadding", &e))?;
            let frames = buffer.saturating_sub(padding);
            if frames == 0 {
                return Ok(());
            }
            let data = render
                .GetBuffer(frames)
                .map_err(|e| failed("IAudioRenderClient::GetBuffer", &e))?;
            let samples = slice::from_raw_parts_mut(
                data.cast::<f32>(),
                frames as usize * usize::from(CHANNELS),
            );
            for frame in samples.chunks_exact_mut(usize::from(CHANNELS)) {
                frame.fill(tone.next_sample());
            }
            render
                .ReleaseBuffer(frames, 0)
                .map_err(|e| failed("IAudioRenderClient::ReleaseBuffer", &e))
        }
    }

    // MARK: Capture

    /// The sample type of a stream.
    #[derive(Clone, Copy)]
    enum Sample {
        F32,
        I16,
    }

    fn wave_format(sample: Sample) -> WAVEFORMATEX {
        let (tag, bits) = match sample {
            Sample::F32 => (WAVE_FORMAT_IEEE_FLOAT, 32u16),
            Sample::I16 => (WAVE_FORMAT_PCM, 16u16),
        };
        let block = CHANNELS * bits / 8;
        WAVEFORMATEX {
            wFormatTag: tag as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: 0,
        }
    }

    /// Captures `pid`'s process tree (or everything but it) for `duration`
    /// and returns the levels.
    fn capture(mode: PROCESS_LOOPBACK_MODE, pid: u32, duration: Duration) -> Fallible<Levels> {
        let (client, sample) = open_capture(mode, pid)?;
        let event = Event::new()?;
        // SAFETY: calls on the stream this function owns.
        let capture = unsafe {
            client
                .SetEventHandle(event.0)
                .map_err(|e| failed("IAudioClient::SetEventHandle", &e))?;
            let capture = client
                .GetService::<IAudioCaptureClient>()
                .map_err(|e| failed("IAudioClient::GetService", &e))?;
            client
                .Start()
                .map_err(|e| failed("IAudioClient::Start", &e))?;
            capture
        };
        let mut levels = Levels::default();
        let deadline = Instant::now() + duration;
        let mut result = Ok(());
        while result.is_ok() && Instant::now() < deadline {
            // SAFETY: waits on an event this function owns.
            unsafe { WaitForSingleObject(event.0, PACKET_WAIT_MS) };
            result = drain(&capture, sample, &mut levels);
        }
        // SAFETY: stops the stream this function owns.
        let _ = unsafe { client.Stop() };
        result.map(|()| levels)
    }

    /// A process loopback client, initialized with 32-bit float (converted
    /// by Windows) or, failing that, 16-bit PCM.
    fn open_capture(mode: PROCESS_LOOPBACK_MODE, pid: u32) -> Fallible<(IAudioClient, Sample)> {
        let mut first_error = None;
        for (sample, autoconvert) in [(Sample::F32, true), (Sample::I16, false)] {
            let client = activate(mode, pid)?;
            let format = wave_format(sample);
            let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            if autoconvert {
                flags |=
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            }
            // SAFETY: `format` lives through the call, which copies it.
            let initialized = unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_HNS,
                    0,
                    &format,
                    None,
                )
            };
            match initialized {
                Ok(()) => return Ok((client, sample)),
                Err(error) => {
                    first_error.get_or_insert(failed("IAudioClient::Initialize", &error));
                }
            }
        }
        Err(first_error.unwrap_or_default())
    }

    /// Reads every waiting packet into `levels`.
    fn drain(capture: &IAudioCaptureClient, sample: Sample, levels: &mut Levels) -> Fallible<()> {
        loop {
            // SAFETY: calls on the capture client this thread owns.
            let packet = unsafe { capture.GetNextPacketSize() }
                .map_err(|e| failed("IAudioCaptureClient::GetNextPacketSize", &e))?;
            if packet == 0 {
                return Ok(());
            }
            let mut data = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: as above, with writable out parameters.
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                .map_err(|e| failed("IAudioCaptureClient::GetBuffer", &e))?;
            let silent = data.is_null() || flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let count = frames as usize * usize::from(CHANNELS);
            if silent {
                levels.push_silence(frames);
            } else {
                // SAFETY: GetBuffer handed out `frames` frames in the
                // initialized format, readable until ReleaseBuffer.
                match sample {
                    Sample::F32 => {
                        let values = unsafe { slice::from_raw_parts(data.cast::<f32>(), count) };
                        levels.push(values.iter().map(|&value| f64::from(value)));
                    }
                    Sample::I16 => {
                        let values = unsafe { slice::from_raw_parts(data.cast::<i16>(), count) };
                        levels.push(values.iter().map(|&value| f64::from(value) / 32_768.0));
                    }
                }
            }
            // SAFETY: releases the packet read above.
            unsafe { capture.ReleaseBuffer(frames) }
                .map_err(|e| failed("IAudioCaptureClient::ReleaseBuffer", &e))?;
        }
    }

    /// What a capture measured: peak and RMS over both channels, and the
    /// 1 kHz component of the left channel (one DFT bin), with the frames it
    /// saw and how many of them came flagged silent.
    #[derive(Clone, Copy, Default)]
    struct Levels {
        frames: u64,
        silent_frames: u64,
        peak: f64,
        sum_squares: f64,
        samples: u64,
        tone_re: f64,
        tone_im: f64,
    }

    impl Levels {
        fn push(&mut self, values: impl Iterator<Item = f64>) {
            let channels = usize::from(CHANNELS);
            for (index, value) in values.enumerate() {
                let channel = index % channels;
                self.peak = self.peak.max(value.abs());
                self.sum_squares += value * value;
                self.samples += 1;
                if channel == 0 {
                    let phase = TAU * TONE_HZ * self.frames as f64 / f64::from(RATE);
                    self.tone_re += value * phase.cos();
                    self.tone_im -= value * phase.sin();
                }
                if channel == channels - 1 {
                    self.frames += 1;
                }
            }
        }

        fn push_silence(&mut self, frames: u32) {
            self.frames += u64::from(frames);
            self.silent_frames += u64::from(frames);
            self.samples += u64::from(frames) * u64::from(CHANNELS);
        }

        fn rms(&self) -> f64 {
            if self.samples == 0 {
                return 0.0;
            }
            (self.sum_squares / self.samples as f64).sqrt()
        }

        /// The amplitude of the 1 kHz component.
        fn tone(&self) -> f64 {
            if self.frames == 0 {
                return 0.0;
            }
            2.0 * self.tone_re.hypot(self.tone_im) / self.frames as f64
        }

        fn print(&self, capture: &str, session: &str, run: usize) {
            let rate = f64::from(RATE);
            println!(
                "{capture:<24}{session:<20}{run:>4}{:>11.1}{:>10.1}{:>12.1}{:>9.2}{:>10.2}",
                dbfs(self.peak),
                dbfs(self.rms()),
                dbfs(self.tone()),
                self.frames as f64 / rate,
                self.silent_frames as f64 / rate
            );
        }

        /// One line for the controller to read back with [`Levels::decode`].
        fn encode(&self) -> String {
            format!(
                "levels {} {} {} {} {} {} {}",
                self.frames,
                self.silent_frames,
                self.peak,
                self.sum_squares,
                self.samples,
                self.tone_re,
                self.tone_im
            )
        }

        fn decode(text: &str) -> Option<Self> {
            let line = text.lines().find(|line| line.starts_with("levels "))?;
            let mut fields = line.split_whitespace().skip(1);
            let mut next = || fields.next();
            Some(Self {
                frames: next()?.parse().ok()?,
                silent_frames: next()?.parse().ok()?,
                peak: next()?.parse().ok()?,
                sum_squares: next()?.parse().ok()?,
                samples: next()?.parse().ok()?,
                tone_re: next()?.parse().ok()?,
                tone_im: next()?.parse().ok()?,
            })
        }
    }

    /// `amplitude` in dBFS; silence is minus infinity.
    fn dbfs(amplitude: f64) -> f64 {
        if amplitude > 0.0 {
            20.0 * amplitude.log10()
        } else {
            f64::NEG_INFINITY
        }
    }

    /// The parameters of a process loopback activation and the PROPVARIANT
    /// that carries them, kept together on the heap so that they stay put
    /// while Windows reads them.
    ///
    /// The variant is never dropped: the `windows` crate's `PROPVARIANT`
    /// clears itself on drop, and clearing a `VT_BLOB` frees the blob with
    /// `CoTaskMemFree`, here the parameters inside this allocation.
    #[repr(C)]
    struct ActivationArgs {
        params: AUDIOCLIENT_ACTIVATION_PARAMS,
        variant: ManuallyDrop<PROPVARIANT>,
    }

    /// An `IAudioClient` that captures `pid`'s process tree (include mode) or
    /// everything except it (exclude mode).
    fn activate(mode: PROCESS_LOOPBACK_MODE, pid: u32) -> Fallible<IAudioClient> {
        let done = Arc::new(Event::new()?);
        let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone {
            done: Arc::clone(&done),
        }
        .into();
        let mut args = Box::new(ActivationArgs {
            params: AUDIOCLIENT_ACTIVATION_PARAMS {
                ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                    ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                        TargetProcessId: pid,
                        ProcessLoopbackMode: mode,
                    },
                },
            },
            variant: ManuallyDrop::new(PROPVARIANT::default()),
        });
        let blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: (&raw mut args.params).cast(),
        };
        // SAFETY: gives the variant the BLOB arm of its union, pointing at
        // the parameters next to it. It owns nothing and is never dropped.
        unsafe {
            let value = &mut *args.variant.Anonymous.Anonymous;
            value.vt = VT_BLOB;
            value.Anonymous.blob = blob;
        }
        // SAFETY: the device path and interface ID are static; the variant
        // stays allocated until the activation completed (or for good,
        // below); the handler is agile, as the call requires.
        let operation = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                // `ManuallyDrop` is `repr(transparent)`.
                Some((&raw const args.variant).cast::<PROPVARIANT>()),
                &handler,
            )
        }
        .map_err(|e| failed("ActivateAudioInterfaceAsync", &e))?;
        // SAFETY: waits on an event this function owns.
        if unsafe { WaitForSingleObject(done.0, ACTIVATION_TIMEOUT_MS) } != WAIT_OBJECT_0 {
            // Windows may still read the parameters, so they stay allocated.
            std::mem::forget(args);
            return Err("ActivateAudioInterfaceAsync timed out".into());
        }
        drop(args);
        let mut result = HRESULT(0);
        let mut interface: Option<IUnknown> = None;
        // SAFETY: the activation completed, so its result is ready.
        unsafe { operation.GetActivateResult(&mut result, &mut interface) }.map_err(|e| {
            failed(
                "IActivateAudioInterfaceAsyncOperation::GetActivateResult",
                &e,
            )
        })?;
        result
            .ok()
            .map_err(|e| failed("ActivateAudioInterfaceAsync", &e))?;
        interface
            .ok_or_else(|| "ActivateAudioInterfaceAsync returned no interface".to_string())?
            .cast::<IAudioClient>()
            .map_err(|e| failed("IUnknown::QueryInterface(IAudioClient)", &e))
    }

    /// Sets an event when an activation completes, on a Windows thread.
    /// `#[implement]` objects are agile, as `ActivateAudioInterfaceAsync`
    /// requires of its handler.
    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct ActivationDone {
        done: Arc<Event>,
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
        fn ActivateCompleted(
            &self,
            _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
        ) -> windows_core::Result<()> {
            // SAFETY: sets an event this object keeps alive.
            unsafe { SetEvent(self.done.0) }
        }
    }

    // MARK: Helpers

    /// An auto-reset Win32 event, closed on drop.
    struct Event(HANDLE);

    // SAFETY: an event handle can be waited on and set from any thread.
    unsafe impl Send for Event {}
    // SAFETY: as above.
    unsafe impl Sync for Event {}

    impl Event {
        fn new() -> Fallible<Self> {
            // SAFETY: creates an unnamed, unsignaled auto-reset event.
            unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
                .map(Self)
                .map_err(|e| failed("CreateEventW", &e))
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: closes the handle this value owns, once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    fn init_com() -> Fallible<()> {
        // SAFETY: joins this thread to the multithreaded apartment for the
        // rest of the process.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| failed("CoInitializeEx", &e))
    }

    fn enumerator() -> Fallible<IMMDeviceEnumerator> {
        // SAFETY: creates the documented MMDevice enumerator.
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| failed("CoCreateInstance(MMDeviceEnumerator)", &e))
    }

    /// An endpoint's ID string, or an empty one.
    fn device_id(device: &IMMDevice) -> String {
        // SAFETY: GetId hands over a COM string, freed after it is copied.
        unsafe {
            let Ok(id) = device.GetId() else {
                return String::new();
            };
            let text = id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id.0.cast_const().cast()));
            text
        }
    }

    /// The Windows version and build, such as "25H2, OS build 26200.1234".
    fn windows_build() -> String {
        let key = w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
        let version = registry_string(key, w!("DisplayVersion")).unwrap_or_default();
        let build = registry_string(key, w!("CurrentBuild")).unwrap_or_default();
        let revision = registry_dword(key, w!("UBR")).unwrap_or_default();
        format!("{version}, OS build {build}.{revision}")
    }

    fn registry_string(key: PCWSTR, value: PCWSTR) -> Option<String> {
        let mut buffer = [0u16; 64];
        let mut bytes = size_of_val(&buffer) as u32;
        // SAFETY: the buffer and its size in bytes describe writable memory.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                value,
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let written = &buffer[..(bytes as usize / 2).min(buffer.len())];
        let end = written
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(written.len());
        Some(String::from_utf16_lossy(&written[..end]))
    }

    fn registry_dword(key: PCWSTR, value: PCWSTR) -> Option<u32> {
        let mut data = 0u32;
        let mut bytes = size_of::<u32>() as u32;
        // SAFETY: `data` and its size in bytes describe writable memory.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                value,
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut data).cast()),
                Some(&mut bytes),
            )
        };
        (status == ERROR_SUCCESS).then_some(data)
    }

    fn yes_no(value: bool) -> &'static str {
        if value { "yes" } else { "no" }
    }
}
