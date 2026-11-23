//! Probe for the call-microphone rule: whether a process that listens to a
//! microphone can be told from Windows' capture session lists without
//! opening anything, and whether moving that process's own output (the
//! per-app policy S471 uses) carries a stream that is already playing.
//!
//! From `desktop/`: `cargo run --release --example call_mic_probe`, or with
//! `--no-mic` to skip the part that opens a capture stream.
//!
//! Nothing it plays is audible: the render helper writes silent buffers. It
//! moves the per-app output of its own helper only, clears it afterwards and
//! compares every default and per-app output before and after. The capture
//! helper discards what it reads and is closed at the end. Endpoints whose
//! name contains "Voicemod" are never used.
//!
//! Roles: no argument is the controller; `tone <default|device>` renders
//! silence on the console default, opened through the default-device
//! interface path (streams that follow a routing change) or through the
//! endpoint itself; `mic <id>` opens a capture stream on endpoint `id`;
//! `list` prints endpoints, defaults and sessions; `tone signal` plays a
//! 3 kHz tone through the default-device path; `route <chrome>` compares
//! two ways to bring a browser call to the recorded output (runner only: it
//! moves the system default).

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    probe::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("call_mic_probe reads Windows audio sessions and runs only on Windows.");
}

#[cfg(windows)]
mod probe {
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, ActivateAudioInterfaceAsync,
        AudioSessionStateActive, DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole,
        IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
        IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
        IAudioRenderClient, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
        IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
        eCapture, eCommunications, eConsole, eMultimedia, eRender,
    };
    use windows::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        STGM_READ,
    };
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_QWORD, RegGetValueW};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::System::WinRT::RoGetActivationFactory;
    use windows::core::{GUID, HRESULT, HSTRING, IInspectable, IUnknown, Interface, PCWSTR, Ref};
    use windows_core::implement;

    type Fallible<T> = Result<T, String>;

    /// The default render device through the interface path, which Windows
    /// re-routes when the app's default changes (DEVINTERFACE_AUDIO_RENDER).
    const DEFAULT_RENDER_PATH: &str = "{E6327CAD-DCEC-4949-AE8A-991E976A79D2}";
    const BUFFER_HNS: i64 = 1_000_000;
    const POLL: Duration = Duration::from_millis(2);
    const WAIT: Duration = Duration::from_secs(5);
    const MIC_CYCLES: usize = 5;
    const Q1_ROUNDS: usize = 50;
    /// The rule's read-back delay after a move (S485).
    const STAYED: Duration = Duration::from_millis(250);
    /// The rule's hold before an app that stopped listening rejoins (S485).
    const HOLD: Duration = Duration::from_secs(3);
    /// A reopen refused while the endpoint switches is tried again this
    /// many times, this far apart.
    const REOPEN_TRIES: u32 = 10;
    const REOPEN_GAP: Duration = Duration::from_millis(50);

    const POLICY_CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";
    const POLICY_IIDS: [GUID; 2] = [
        GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324),
        GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258),
    ];
    const SET_SLOT: usize = 25;
    const GET_SLOT: usize = 26;
    type SetPersisted =
        unsafe extern "system" fn(*mut c_void, u32, i32, ERole, *mut c_void) -> HRESULT;
    type GetPersisted =
        unsafe extern "system" fn(*mut c_void, u32, i32, ERole, *mut *mut c_void) -> HRESULT;

    fn failed(call: &str, error: &windows::core::Error) -> String {
        format!("{call} failed (0x{:08X})", error.code().0)
    }

    pub fn main() -> ExitCode {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let result = init_com().and_then(|()| match args.first().map(String::as_str) {
            None => control(true),
            Some("--no-mic") => control(false),
            Some("tone") => match args.get(1).map(String::as_str) {
                Some("reopen") => tone_reopen(),
                Some("signal") => tone(true, true),
                mode => tone(mode == Some("default"), false),
            },
            Some("chrome") => match args.get(1) {
                Some(path) => chrome(path),
                None => Err("`chrome` needs the browser's path".into()),
            },
            Some("mic") => match args.get(1) {
                Some(id) => mic(id),
                None => Err("`mic` needs an endpoint ID".into()),
            },
            Some("route") => match args.get(1) {
                Some(path) => route(path),
                None => Err("`route` needs the browser's path".into()),
            },
            Some("list") => list(),
            Some(other) => Err(format!("unknown role {other:?}")),
        });
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => {
                eprintln!("call_mic_probe: {message}");
                ExitCode::FAILURE
            }
        }
    }

    // MARK: Controller

    fn control(with_mic: bool) -> Fallible<()> {
        let enumerator = enumerator()?;
        let own = std::process::id();
        println!(
            "os build {}, elevated: {}, own pid {own}",
            os_build(),
            elevated()
        );
        let before = Snapshot::take(&enumerator)?;
        before.print("before");

        let q1 = q1_read_only(&enumerator, own)?;
        let q3 = if with_mic {
            Some(q3_detection(&enumerator, own).unwrap_or_else(|error| format!("error: {error}")))
        } else {
            println!("Q3: skipped (--no-mic)");
            None
        };
        let q2_default = q2_move(&enumerator, "default")?;
        let q2_device = q2_move(&enumerator, "device")?;
        let q2_reopen = q2_move(&enumerator, "reopen")?;

        let after = Snapshot::take(&enumerator)?;
        after.print("after");
        let equal = before.defaults == after.defaults
            && before
                .per_app
                .iter()
                .all(|(pid, outputs)| after.per_app.get(pid).is_none_or(|later| later == outputs));
        println!(
            "READBACK defaults and per-app outputs equal: {}",
            yes_no(equal)
        );
        println!("RESULT Q1 {q1}");
        println!("RESULT Q2 default-path stream: {q2_default}");
        println!("RESULT Q2 endpoint-bound stream: {q2_device}");
        println!(
            "RESULT Q4 endpoint-bound stream reopening on a default-change notice: {q2_reopen}"
        );
        match q3 {
            Some(q3) => println!("RESULT Q3 {q3}"),
            None => println!("RESULT Q3 not run"),
        }
        Ok(())
    }

    /// Q1: reads every capture endpoint's session list many times and checks
    /// that the probe's own pid never shows and the microphone use record
    /// for its exe does not change.
    fn q1_read_only(enumerator: &IMMDeviceEnumerator, own: u32) -> Fallible<String> {
        let used_before = mic_last_used()?;
        let mut own_seen = false;
        let mut sessions = 0;
        let mut active = 0;
        for _ in 0..Q1_ROUNDS {
            let found = sessions_of(enumerator, eCapture)?;
            sessions = found.len();
            active = found.iter().filter(|session| session.active).count();
            own_seen |= found.iter().any(|session| session.pid == own);
            thread::sleep(Duration::from_millis(10));
        }
        let used_after = mic_last_used()?;
        let unchanged = used_before == used_after;
        println!(
            "Q1: {Q1_ROUNDS} reads of the capture session lists, last read {sessions} sessions \
             ({active} active); own pid listed: {}; microphone record of this exe before {} \
             after {}",
            yes_no(own_seen),
            show_used(used_before),
            show_used(used_after)
        );
        Ok(format!(
            "{}: own pid never listed {}, mic record unchanged {} ({}), elevated {}",
            yes_no(!own_seen && unchanged),
            yes_no(!own_seen),
            yes_no(unchanged),
            show_used(used_after),
            elevated()
        ))
    }

    /// Q3: a helper opens and closes a capture stream; the probe times how
    /// long its session takes to read as active and then as not active.
    fn q3_detection(enumerator: &IMMDeviceEnumerator, own: u32) -> Fallible<String> {
        let Some((id, name)) = pick_capture(enumerator)? else {
            println!("Q3: no usable capture endpoint");
            return Ok("untested: no capture endpoint".into());
        };
        println!("Q3: capture endpoint {name:?}");
        let mut on = Vec::new();
        let mut off = Vec::new();
        let mut own_seen = false;
        for cycle in 0..MIC_CYCLES {
            let mut helper = Helper::start(&["mic", &id])?;
            let pid = helper.pid();
            let seen = poll(|| {
                let found = sessions_on(enumerator, &id)?;
                own_seen |= found.iter().any(|session| session.pid == own);
                Ok(found
                    .iter()
                    .any(|session| session.pid == pid && session.active))
            })?;
            let started = helper.wait_for("started")?;
            let up = signed_ms(seen, started);
            helper.close_input();
            let gone = poll(|| {
                let found = sessions_on(enumerator, &id)?;
                Ok(!found
                    .iter()
                    .any(|session| session.pid == pid && session.active))
            })?;
            let stopped = helper.wait_for("stopped")?;
            let down = signed_ms(gone, stopped);
            println!(
                "Q3: cycle {cycle}: active after start {up:?} ms, inactive after stop {down:?} ms"
            );
            on.push(up);
            off.push(down);
            helper.finish();
        }
        let used = mic_last_used()?;
        println!(
            "Q3: microphone record of this exe after the helper: {}",
            show_used(used)
        );
        let detected = on.iter().chain(&off).all(Option::is_some);
        Ok(format!(
            "{}: start seen after {} ms, stop seen after {} ms (min/median/max of {MIC_CYCLES}, \
             ms from the helper's report; 2 ms polling); own pid listed {}; helper raised the \
             mic record {}",
            yes_no(detected),
            spread(&on),
            spread(&off),
            yes_no(own_seen),
            yes_no(used.is_some())
        ))
    }

    /// Q2: a helper renders silence on the console default; the probe moves
    /// the helper's console and multimedia output to another endpoint and
    /// times when its session plays there, then clears the move.
    fn q2_move(enumerator: &IMMDeviceEnumerator, mode: &str) -> Fallible<String> {
        // SAFETY: reads the console default render endpoint.
        let console = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|e| failed("GetDefaultAudioEndpoint", &e))?;
        if usable(&console).is_none() {
            return Ok("untested: the console default is a device the probe never uses".into());
        }
        let mut helper = Helper::start(&["tone", mode])?;
        let pid = helper.pid();
        helper.wait_for("started")?;
        let mut from = None;
        poll(|| {
            from = playing_endpoint(enumerator, pid)?;
            Ok(from.is_some())
        })?;
        let Some(from) = from else {
            helper.finish();
            return Ok("untested: the helper's session never showed".into());
        };
        let Some((to, name)) = pick_render(enumerator, &from)? else {
            helper.finish();
            println!("Q2 ({mode}): no second render endpoint");
            return Ok("untested: no second render endpoint".into());
        };
        println!("Q2 ({mode}): moving the helper to {name:?}");
        let policy = Policy::new()?;
        let target = render_interface(&to);
        let set_at = Instant::now();
        for role in [eConsole, eMultimedia] {
            policy.set(pid, role, &target)?;
        }
        let read_back = policy.get(pid, eConsole)? == target;
        let arrived = poll(|| {
            Ok(sessions_on(enumerator, &to)?
                .iter()
                .any(|session| session.pid == pid && session.active))
        })?;
        let left = !sessions_on(enumerator, &from)?
            .iter()
            .any(|session| session.pid == pid && session.active);
        // The rule's read-back: still active on the old endpoint 250 ms
        // after the move marks the app as stayed.
        let stayed = !left && arrived.is_none_or(|at| at.duration_since(set_at) > STAYED);
        let told: Vec<String> = helper.lines.try_iter().map(|(line, _)| line).collect();
        let notices = told
            .iter()
            .filter(|line| line.starts_with("notice"))
            .count();
        let reopened = told
            .iter()
            .filter(|line| line.starts_with("reopened"))
            .count();
        let refused = told
            .iter()
            .filter(|line| line.starts_with("reopen refused"))
            .count();
        for line in &told {
            println!("helper: {line}");
        }
        let cleared = [eConsole, eMultimedia].into_iter().all(|role| {
            policy.set(pid, role, "").is_ok()
                && policy.get(pid, role).is_ok_and(|now| now.is_empty())
        });
        helper.finish();
        let ms = arrived.map(|at| at.duration_since(set_at).as_millis());
        println!(
            "Q2 ({mode}): read back {}, active on the new endpoint after {} ms, left the old \
             one {}, cleared {}",
            yes_no(read_back),
            ms.map_or("never".into(), |ms| ms.to_string()),
            yes_no(left),
            yes_no(cleared)
        );
        Ok(format!(
            "{}: N = {} ms, left the old endpoint {}, per-app cleared {}, default-change              notices to the app {notices}, reopened {reopened}, reopen refused {refused}, stayed at 250 ms {}",
            yes_no(ms.is_some()),
            ms.map_or("none within 5000".into(), |ms| ms.to_string()),
            yes_no(left),
            yes_no(cleared),
            yes_no(stayed)
        ))
    }

    /// Polls `check` every 2 ms for up to 5 s; the moment it first held.
    fn poll(mut check: impl FnMut() -> Fallible<bool>) -> Fallible<Option<Instant>> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if check()? {
                return Ok(Some(Instant::now()));
            }
            thread::sleep(POLL);
        }
        Ok(None)
    }

    fn signed_ms(seen: Option<Instant>, reported: Instant) -> Option<i64> {
        let seen = seen?;
        Some(if seen >= reported {
            seen.duration_since(reported).as_millis() as i64
        } else {
            -(reported.duration_since(seen).as_millis() as i64)
        })
    }

    fn spread(values: &[Option<i64>]) -> String {
        let mut known: Vec<i64> = values.iter().flatten().copied().collect();
        if known.is_empty() {
            return "never".into();
        }
        known.sort_unstable();
        format!(
            "{}/{}/{}",
            known[0],
            known[known.len() / 2],
            known[known.len() - 1]
        )
    }

    // MARK: Before and after

    #[derive(PartialEq)]
    struct Snapshot {
        defaults: Vec<String>,
        per_app: BTreeMap<u32, [String; 2]>,
    }

    impl Snapshot {
        fn take(enumerator: &IMMDeviceEnumerator) -> Fallible<Self> {
            let mut defaults = Vec::new();
            for flow in [eRender, eCapture] {
                for role in [eConsole, eMultimedia, eCommunications] {
                    // SAFETY: reads a default endpoint; none is an empty entry.
                    let id = unsafe { enumerator.GetDefaultAudioEndpoint(flow, role) }
                        .map(|device| device_id(&device))
                        .unwrap_or_default();
                    defaults.push(id);
                }
            }
            let policy = Policy::new()?;
            let mut per_app = BTreeMap::new();
            for session in sessions_of(enumerator, eRender)? {
                if session.pid == 0 {
                    continue;
                }
                let console = policy.get(session.pid, eConsole).unwrap_or_default();
                let multimedia = policy.get(session.pid, eMultimedia).unwrap_or_default();
                per_app.insert(session.pid, [console, multimedia]);
            }
            Ok(Self { defaults, per_app })
        }

        fn print(&self, when: &str) {
            let set = self
                .per_app
                .values()
                .filter(|outputs| outputs.iter().any(|o| !o.is_empty()));
            println!(
                "{when}: 6 defaults recorded ({} set), {} processes with render sessions, {} with \
                 a per-app output",
                self.defaults.iter().filter(|id| !id.is_empty()).count(),
                self.per_app.len(),
                set.count()
            );
        }
    }

    // MARK: Sessions and endpoints

    struct Session {
        pid: u32,
        active: bool,
    }

    fn active_devices(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
    ) -> Fallible<Vec<IMMDevice>> {
        // SAFETY: lists active endpoints of one flow.
        unsafe {
            let devices = enumerator
                .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
                .map_err(|e| failed("EnumAudioEndpoints", &e))?;
            let count = devices.GetCount().map_err(|e| failed("GetCount", &e))?;
            Ok((0..count)
                .filter_map(|index| devices.Item(index).ok())
                .collect())
        }
    }

    /// The sessions of one endpoint, read through its session manager: no
    /// stream is opened.
    fn sessions_of_device(device: &IMMDevice) -> Vec<Session> {
        let mut found = Vec::new();
        // SAFETY: calls on live COM objects; a session that fails is skipped.
        unsafe {
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                return found;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                return found;
            };
            for number in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(session) = sessions.GetSession(number) else {
                    continue;
                };
                let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let Ok(pid) = session.GetProcessId() else {
                    continue;
                };
                let active = session
                    .GetState()
                    .is_ok_and(|state| state == AudioSessionStateActive);
                found.push(Session { pid, active });
            }
        }
        found
    }

    fn sessions_of(enumerator: &IMMDeviceEnumerator, flow: EDataFlow) -> Fallible<Vec<Session>> {
        Ok(active_devices(enumerator, flow)?
            .iter()
            .flat_map(sessions_of_device)
            .collect())
    }

    fn sessions_on(enumerator: &IMMDeviceEnumerator, id: &str) -> Fallible<Vec<Session>> {
        // SAFETY: opens an endpoint by ID.
        let device = unsafe { enumerator.GetDevice(&HSTRING::from(id)) }
            .map_err(|e| failed("GetDevice", &e))?;
        Ok(sessions_of_device(&device))
    }

    fn playing_endpoint(enumerator: &IMMDeviceEnumerator, pid: u32) -> Fallible<Option<String>> {
        Ok(active_devices(enumerator, eRender)?
            .iter()
            .find_map(|device| {
                sessions_of_device(device)
                    .iter()
                    .any(|session| session.pid == pid && session.active)
                    .then(|| device_id(device))
            }))
    }

    fn usable(device: &IMMDevice) -> Option<(String, String)> {
        let name = friendly_name(device);
        (!name.to_lowercase().contains("voicemod")).then(|| (device_id(device), name))
    }

    /// A render endpoint other than `from`, a cable first.
    fn pick_render(
        enumerator: &IMMDeviceEnumerator,
        from: &str,
    ) -> Fallible<Option<(String, String)>> {
        let mut candidates: Vec<(String, String)> = active_devices(enumerator, eRender)?
            .iter()
            .filter_map(usable)
            .filter(|(id, _)| id != from)
            .collect();
        candidates.sort_by_key(|(_, name)| !name.contains("CABLE"));
        Ok(candidates.into_iter().next())
    }

    /// A cable's capture side first, else the default capture endpoint.
    fn pick_capture(enumerator: &IMMDeviceEnumerator) -> Fallible<Option<(String, String)>> {
        let devices = active_devices(enumerator, eCapture)?;
        if let Some(cable) = devices
            .iter()
            .filter_map(usable)
            .find(|(_, n)| n.contains("CABLE"))
        {
            return Ok(Some(cable));
        }
        // SAFETY: reads the default capture endpoint.
        let default = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) };
        Ok(default.ok().as_ref().and_then(usable))
    }

    fn list() -> Fallible<()> {
        let enumerator = enumerator()?;
        for flow in [eRender, eCapture] {
            for device in active_devices(&enumerator, flow)? {
                let sessions = sessions_of_device(&device);
                let id = device_id(&device);
                let roles: Vec<&str> = [(eConsole, "console"), (eMultimedia, "multimedia")]
                    .into_iter()
                    .chain([(eCommunications, "communications")])
                    .filter(|(role, _)| {
                        // SAFETY: reads a default endpoint.
                        unsafe { enumerator.GetDefaultAudioEndpoint(flow, *role) }
                            .is_ok_and(|default| device_id(&default) == id)
                    })
                    .map(|(_, name)| name)
                    .collect();
                println!(
                    "{} {:?} default for {roles:?}: {} sessions, {} active",
                    if flow == eRender { "render" } else { "capture" },
                    friendly_name(&device),
                    sessions.len(),
                    sessions.iter().filter(|s| s.active).count()
                );
            }
        }
        Ok(())
    }

    // MARK: Per-app output

    struct Policy(IInspectable);

    impl Policy {
        fn new() -> Fallible<Self> {
            // SAFETY: activates the factory and asks it for either variant;
            // `raw` holds one reference on success, which the result takes.
            unsafe {
                let base: IInspectable = RoGetActivationFactory(&HSTRING::from(POLICY_CLASS))
                    .map_err(|e| failed("RoGetActivationFactory", &e))?;
                for iid in POLICY_IIDS {
                    let mut raw = std::ptr::null_mut();
                    if base.query(&iid, &mut raw).is_ok() {
                        return Ok(Self(IInspectable::from_raw(raw)));
                    }
                }
            }
            Err("the per-app interface is missing".into())
        }

        fn method(&self, slot: usize) -> *const c_void {
            // SAFETY: both variants' tables have the set and get slots.
            unsafe {
                let table = *(self.0.as_raw() as *const *const *const c_void);
                *table.add(slot)
            }
        }

        fn get(&self, pid: u32, role: ERole) -> Fallible<String> {
            let mut out: *mut c_void = std::ptr::null_mut();
            // SAFETY: the get slot writes an HSTRING the caller owns, which
            // `HSTRING` takes over.
            unsafe {
                let call: GetPersisted = std::mem::transmute(self.method(GET_SLOT));
                call(self.0.as_raw(), pid, eRender.0, role, &mut out)
                    .ok()
                    .map_err(|e| failed("reading a per-app output", &e))?;
                Ok(std::mem::transmute::<*mut c_void, HSTRING>(out).to_string())
            }
        }

        fn set(&self, pid: u32, role: ERole, device: &str) -> Fallible<()> {
            let device = HSTRING::from(device);
            // SAFETY: the set slot takes the HSTRING by value; it stays owned
            // and alive here through the call.
            unsafe {
                let call: SetPersisted = std::mem::transmute(self.method(SET_SLOT));
                call(
                    self.0.as_raw(),
                    pid,
                    eRender.0,
                    role,
                    std::mem::transmute_copy(&device),
                )
                .ok()
                .map_err(|e| failed("setting a per-app output", &e))
            }
        }
    }

    fn render_interface(endpoint_id: &str) -> String {
        format!(r"\\?\SWD#MMDEVAPI#{endpoint_id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
    }

    // MARK: Helpers (child processes)

    struct Helper {
        child: Child,
        stdin: Option<ChildStdin>,
        lines: Receiver<(String, Instant)>,
    }

    impl Helper {
        fn start(args: &[&str]) -> Fallible<Self> {
            let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            let mut child = Command::new(exe)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("starting the helper failed: {e}"))?;
            let stdout = child.stdout.take().ok_or("no helper output")?;
            let (send, lines) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let _ = send.send((line, Instant::now()));
                }
            });
            let stdin = child.stdin.take();
            Ok(Self {
                child,
                stdin,
                lines,
            })
        }

        fn pid(&self) -> u32 {
            self.child.id()
        }

        fn wait_for(&mut self, word: &str) -> Fallible<Instant> {
            loop {
                match self.lines.recv_timeout(WAIT) {
                    Ok((line, at)) if line.trim() == word => return Ok(at),
                    Ok((line, _)) => println!("helper: {line}"),
                    Err(_) => return Err(format!("the helper never said {word:?}")),
                }
            }
        }

        fn close_input(&mut self) {
            drop(self.stdin.take());
        }

        fn finish(&mut self) {
            self.close_input();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl Drop for Helper {
        fn drop(&mut self) {
            self.finish();
        }
    }

    /// Set when standard input closes.
    fn input_closed() -> Arc<AtomicBool> {
        let closed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&closed);
        thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            flag.store(true, Ordering::Relaxed);
        });
        closed
    }

    fn say(word: &str) {
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{word}");
        let _ = stdout.flush();
    }

    /// Renders silent buffers, or [`SIGNAL_HZ`] when `signal`, until
    /// standard input closes.
    fn tone(default_path: bool, signal: bool) -> Fallible<()> {
        let closed = input_closed();
        let client = if default_path {
            activate_default_render()?
        } else {
            // SAFETY: opens the console default render endpoint.
            unsafe {
                enumerator()?
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .map_err(|e| failed("GetDefaultAudioEndpoint", &e))?
                    .Activate::<IAudioClient>(CLSCTX_ALL, None)
                    .map_err(|e| failed("Activate(IAudioClient)", &e))?
            }
        };
        // SAFETY: calls on the stream this function owns; the mix format is
        // freed after Initialize copied it; every buffer is released silent.
        unsafe {
            let format = client
                .GetMixFormat()
                .map_err(|e| failed("GetMixFormat", &e))?;
            let (channels, rate, bits) = (
                usize::from((*format).nChannels),
                (*format).nSamplesPerSec as f32,
                (*format).wBitsPerSample,
            );
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format, None);
            CoTaskMemFree(Some(format.cast_const().cast()));
            init.map_err(|e| failed("Initialize", &e))?;
            if signal && bits != 32 {
                return Err(format!("the mix format has {bits}-bit samples, not float"));
            }
            let render = client
                .GetService::<IAudioRenderClient>()
                .map_err(|e| failed("GetService(IAudioRenderClient)", &e))?;
            let size = client
                .GetBufferSize()
                .map_err(|e| failed("GetBufferSize", &e))?;
            client.Start().map_err(|e| failed("Start", &e))?;
            say("started");
            let step = std::f32::consts::TAU * SIGNAL_HZ / rate;
            let mut phase = 0f32;
            while !closed.load(Ordering::Relaxed) {
                // A stream that was re-routed reports a failure until it is
                // reopened; the session move is what is measured, not this.
                let padding = client.GetCurrentPadding().unwrap_or(size);
                let frames = size.saturating_sub(padding);
                if frames > 0
                    && let Ok(data) = render.GetBuffer(frames)
                {
                    let flags = if signal {
                        let samples = std::slice::from_raw_parts_mut(
                            data.cast::<f32>(),
                            frames as usize * channels,
                        );
                        for frame in samples.chunks_mut(channels) {
                            frame.fill(SIGNAL_LEVEL * phase.sin());
                            phase = (phase + step) % std::f32::consts::TAU;
                        }
                        0
                    } else {
                        AUDCLNT_BUFFERFLAGS_SILENT.0 as u32
                    };
                    let _ = render.ReleaseBuffer(frames, flags);
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = client.Stop();
        }
        say("stopped");
        Ok(())
    }

    /// Opens a capture stream on `id`, discards what it reads, and closes it
    /// when standard input closes.
    fn mic(id: &str) -> Fallible<()> {
        let closed = input_closed();
        // SAFETY: calls on the stream this function owns; packets are
        // released unread.
        unsafe {
            let client = enumerator()?
                .GetDevice(&HSTRING::from(id))
                .map_err(|e| failed("GetDevice", &e))?
                .Activate::<IAudioClient>(CLSCTX_ALL, None)
                .map_err(|e| failed("Activate(IAudioClient)", &e))?;
            let format = client
                .GetMixFormat()
                .map_err(|e| failed("GetMixFormat", &e))?;
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format, None);
            CoTaskMemFree(Some(format.cast_const().cast()));
            init.map_err(|e| failed("Initialize", &e))?;
            let capture = client
                .GetService::<IAudioCaptureClient>()
                .map_err(|e| failed("GetService(IAudioCaptureClient)", &e))?;
            client.Start().map_err(|e| failed("Start", &e))?;
            say("started");
            while !closed.load(Ordering::Relaxed) {
                while capture.GetNextPacketSize().is_ok_and(|frames| frames > 0) {
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0;
                    let mut flags = 0;
                    if capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .is_err()
                    {
                        break;
                    }
                    let _ = capture.ReleaseBuffer(frames);
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = client.Stop();
            say("stopped");
            // Held open a moment so the stop, not the exit, is what is seen.
            thread::sleep(Duration::from_millis(1500));
            drop(capture);
            drop(client);
        }
        Ok(())
    }

    fn activate_default_render() -> Fallible<IAudioClient> {
        let done = Arc::new(Done(AtomicBool::new(false)));
        let handler: IActivateAudioInterfaceCompletionHandler =
            Completion(Arc::clone(&done)).into();
        // SAFETY: the path and IID are live through the call; the handler is
        // agile, as the call requires.
        let operation = unsafe {
            ActivateAudioInterfaceAsync(
                &HSTRING::from(DEFAULT_RENDER_PATH),
                &IAudioClient::IID,
                None,
                &handler,
            )
        }
        .map_err(|e| failed("ActivateAudioInterfaceAsync", &e))?;
        let deadline = Instant::now() + WAIT;
        while !done.0.load(Ordering::Acquire) {
            if Instant::now() > deadline {
                return Err("ActivateAudioInterfaceAsync timed out".into());
            }
            thread::sleep(Duration::from_millis(5));
        }
        let mut result = HRESULT(0);
        let mut interface: Option<IUnknown> = None;
        // SAFETY: the activation completed, so its result is ready.
        unsafe { operation.GetActivateResult(&mut result, &mut interface) }
            .map_err(|e| failed("GetActivateResult", &e))?;
        result
            .ok()
            .map_err(|e| failed("ActivateAudioInterfaceAsync", &e))?;
        interface
            .ok_or("no interface")?
            .cast::<IAudioClient>()
            .map_err(|e| failed("QueryInterface(IAudioClient)", &e))
    }

    struct Done(AtomicBool);

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Completion(Arc<Done>);

    impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
        fn ActivateCompleted(
            &self,
            _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
        ) -> windows_core::Result<()> {
            self.0.0.store(true, Ordering::Release);
            Ok(())
        }
    }

    // MARK: Rule proof: an app bound by ID that follows default changes

    /// Render default-change notices, as Windows delivered them.
    #[implement(IMMNotificationClient)]
    struct Notices(Arc<Mutex<Vec<String>>>);

    impl IMMNotificationClient_Impl for Notices_Impl {
        fn OnDeviceStateChanged(
            &self,
            _id: &PCWSTR,
            _state: DEVICE_STATE,
        ) -> windows_core::Result<()> {
            Ok(())
        }

        fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows_core::Result<()> {
            Ok(())
        }

        fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows_core::Result<()> {
            Ok(())
        }

        fn OnDefaultDeviceChanged(
            &self,
            flow: EDataFlow,
            role: ERole,
            id: &PCWSTR,
        ) -> windows_core::Result<()> {
            if flow == eRender {
                // SAFETY: Windows passes a NUL-terminated ID, or null, that
                // lives through the call.
                let id = if id.is_null() {
                    String::new()
                } else {
                    unsafe { id.to_string() }.unwrap_or_default()
                };
                if let Ok(mut told) = self.0.lock() {
                    told.push(format!("notice role {} {id}", role.0));
                }
            }
            Ok(())
        }

        fn OnPropertyValueChanged(
            &self,
            _id: &PCWSTR,
            _key: &PROPERTYKEY,
        ) -> windows_core::Result<()> {
            Ok(())
        }
    }

    /// A silent stream on the endpoint `GetDefaultAudioEndpoint` names now.
    fn open_bound(enumerator: &IMMDeviceEnumerator) -> Fallible<Stream> {
        // SAFETY: reads the console default render endpoint.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
            .map_err(|e| failed("GetDefaultAudioEndpoint", &e))?;
        open_on(device)
    }

    /// A silent stream on endpoint `id`.
    fn open_bound_to(id: &str) -> Fallible<Stream> {
        // SAFETY: looks up a render endpoint by the ID it reported.
        let device = unsafe { enumerator()?.GetDevice(&HSTRING::from(id)) }
            .map_err(|e| failed("GetDevice", &e))?;
        open_on(device)
    }

    type Stream = (IAudioClient, IAudioRenderClient, u32, String);

    fn open_on(device: IMMDevice) -> Fallible<Stream> {
        // SAFETY: opens a stream on `device`; the mix format is freed after
        // Initialize copied it.
        unsafe {
            let id = device_id(&device);
            let client = device
                .Activate::<IAudioClient>(CLSCTX_ALL, None)
                .map_err(|e| failed("Activate(IAudioClient)", &e))?;
            let format = client
                .GetMixFormat()
                .map_err(|e| failed("GetMixFormat", &e))?;
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format, None);
            CoTaskMemFree(Some(format.cast_const().cast()));
            init.map_err(|e| failed("Initialize", &e))?;
            let render = client
                .GetService::<IAudioRenderClient>()
                .map_err(|e| failed("GetService(IAudioRenderClient)", &e))?;
            let size = client
                .GetBufferSize()
                .map_err(|e| failed("GetBufferSize", &e))?;
            client.Start().map_err(|e| failed("Start", &e))?;
            Ok((client, render, size, id))
        }
    }

    /// Renders silence on a stream bound to the console default endpoint and,
    /// at each render default-change notice, closes it and opens one on the
    /// endpoint `GetDefaultAudioEndpoint` then names, until standard input
    /// closes.
    fn tone_reopen() -> Fallible<()> {
        let closed = input_closed();
        let enumerator = enumerator()?;
        let told = Arc::new(Mutex::new(Vec::new()));
        let listener: IMMNotificationClient = Notices(Arc::clone(&told)).into();
        // SAFETY: registers a live, agile listener, removed below.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&listener) }
            .map_err(|e| failed("RegisterEndpointNotificationCallback", &e))?;
        let (mut client, mut render, mut size, mut id) = open_bound(&enumerator)?;
        say(&format!("opened {id}"));
        say("started");
        while !closed.load(Ordering::Relaxed) {
            let notices: Vec<String> = told
                .lock()
                .map(|mut told| told.drain(..).collect())
                .unwrap_or_default();
            if !notices.is_empty() {
                for notice in &notices {
                    say(notice);
                }
                // SAFETY: stops the stream this function owns.
                let _ = unsafe { client.Stop() };
                drop((client, render));
                let mut opened = open_bound(&enumerator);
                for _ in 0..REOPEN_TRIES {
                    if opened.is_ok() {
                        break;
                    }
                    thread::sleep(REOPEN_GAP);
                    opened = open_bound(&enumerator);
                }
                match opened {
                    Ok(stream) => {
                        (client, render, size, id) = stream;
                        say(&format!("reopened {id}"));
                    }
                    Err(error) => {
                        say(&format!("reopen refused: {error}"));
                        // Back on the endpoint it held, so the session stays.
                        (client, render, size, id) = open_bound_to(&id)?;
                    }
                }
            }
            // SAFETY: calls on the stream this function owns; every buffer
            // is released silent.
            unsafe {
                let padding = client.GetCurrentPadding().unwrap_or(size);
                let frames = size.saturating_sub(padding);
                if frames > 0 && render.GetBuffer(frames).is_ok() {
                    let _ = render.ReleaseBuffer(frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: stops the stream and removes the listener registered above.
        unsafe {
            let _ = client.Stop();
            let _ = enumerator.UnregisterEndpointNotificationCallback(&listener);
        }
        say("stopped");
        Ok(())
    }

    // MARK: Rule proof: a real browser with a tone and a microphone

    /// A page that plays a quiet tone at once and listens to the default
    /// microphone for 6 s, then lets the microphone go and keeps the tone.
    const PAGE: &str = r#"<!doctype html><title>call</title><script>
const context = new AudioContext();
const tone = context.createOscillator();
const level = context.createGain();
level.gain.value = 0.05;
tone.connect(level).connect(context.destination);
tone.start();
navigator.mediaDevices.getUserMedia({ audio: true }).then((stream) => {
  const source = context.createMediaStreamSource(stream);
  source.connect(context.createAnalyser());
  setTimeout(() => stream.getTracks().forEach((track) => track.stop()), 6000);
});
</script>"#;

    /// Runs Chrome at `path` on [`PAGE`], with the flag that answers the
    /// permission prompt itself: when its listening shows, its audio is
    /// moved to another endpoint, as the rule pins a call app; once it has
    /// stopped listening for [`HOLD`] the move is cleared. Prints when each
    /// happened and where its session plays at the end.
    fn chrome(path: &str) -> Fallible<()> {
        let enumerator = enumerator()?;
        let own = std::process::id();
        let before: Vec<u32> = sessions_of(&enumerator, eRender)?
            .iter()
            .chain(sessions_of(&enumerator, eCapture)?.iter())
            .map(|session| session.pid)
            .collect();
        let dir = std::env::temp_dir().join("call-mic-chrome");
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let page = dir.join("call.html");
        std::fs::write(&page, PAGE).map_err(|e| format!("{}: {e}", page.display()))?;
        let started = Instant::now();
        let mut browser = Command::new(path)
            .arg(format!("--user-data-dir={}", dir.join("profile").display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--use-fake-ui-for-media-stream",
                "--autoplay-policy=no-user-gesture-required",
                "--allow-file-access-from-files",
            ])
            .arg(format!(
                "file:///{}",
                page.display().to_string().replace('\\', "/")
            ))
            .spawn()
            .map_err(|e| format!("starting Chrome failed: {e}"))?;
        let ms = |at: Instant| at.duration_since(started).as_millis();
        let fresh = |session: &Session| {
            session.active && session.pid != own && !before.contains(&session.pid)
        };
        let mut wait =
            |what: &str, check: &mut dyn FnMut() -> Fallible<bool>| -> Fallible<Option<Instant>> {
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline {
                    if check()? {
                        let at = Instant::now();
                        println!("Q5: {what} at {} ms", ms(at));
                        return Ok(Some(at));
                    }
                    thread::sleep(POLL);
                }
                println!("Q5: {what}: not within 30 s");
                Ok(None)
            };
        let mut player = None;
        wait("Chrome's tone plays", &mut || {
            player = sessions_of(&enumerator, eRender)?
                .into_iter()
                .find(|s| fresh(s))
                .map(|s| s.pid);
            Ok(player.is_some())
        })?;
        let mut listener = None;
        let heard = wait("Chrome listens (capture session active)", &mut || {
            listener = sessions_of(&enumerator, eCapture)?
                .into_iter()
                .find(|s| fresh(s))
                .map(|s| s.pid);
            Ok(listener.is_some())
        })?;
        let result = match (player, listener, heard) {
            (Some(pid), Some(listening), Some(_)) => {
                println!("Q5: render pid {pid}, capture pid {listening}");
                chrome_move(&enumerator, pid, listening, &mut wait)
            }
            _ => Ok("untested: Chrome's tone or listening never showed".to_owned()),
        };
        // Chrome's children end with its browser process.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &browser.id().to_string()])
            .output();
        let _ = browser.wait();
        println!(
            "RESULT Q5 Chrome (same engine as Electron and WebView apps): {}",
            result?
        );
        Ok(())
    }

    type Wait<'a> =
        dyn FnMut(&str, &mut dyn FnMut() -> Fallible<bool>) -> Fallible<Option<Instant>> + 'a;

    fn chrome_move(
        enumerator: &IMMDeviceEnumerator,
        pid: u32,
        listening: u32,
        wait: &mut Wait<'_>,
    ) -> Fallible<String> {
        let Some(from) = playing_endpoint(enumerator, pid)? else {
            return Ok("untested: its session left before the move".into());
        };
        let Some((to, name)) = pick_render(enumerator, &from)? else {
            return Ok("untested: no second render endpoint".into());
        };
        println!("Q5: moving Chrome's audio to {name:?}");
        let policy = Policy::new()?;
        let target = render_interface(&to);
        let set_at = Instant::now();
        for role in [eConsole, eMultimedia] {
            policy.set(pid, role, &target)?;
        }
        let on = |id: &str| -> Fallible<bool> {
            Ok(sessions_on(enumerator, id)?
                .iter()
                .any(|s| s.pid == pid && s.active))
        };
        let arrived = wait("Chrome's tone on the pinned endpoint", &mut || on(&to))?;
        thread::sleep(STAYED.saturating_sub(set_at.elapsed()));
        let stayed = on(&from)?;
        let quiet = wait("Chrome stopped listening", &mut || {
            Ok(!sessions_of(enumerator, eCapture)?
                .iter()
                .any(|s| s.pid == listening && s.active))
        })?;
        if let Some(quiet) = quiet {
            thread::sleep(HOLD.saturating_sub(quiet.elapsed()));
        }
        let cleared_at = Instant::now();
        for role in [eConsole, eMultimedia] {
            policy.set(pid, role, "")?;
        }
        let back = wait("Chrome's tone back on the default", &mut || on(&from))?;
        let ends_on = playing_endpoint(enumerator, pid)?;
        let ms = |at: Option<Instant>, since: Instant| {
            at.map_or("never".to_owned(), |at| {
                format!("{} ms", at.duration_since(since).as_millis())
            })
        };
        // A stream that never left the old endpoint is the stayed case: the
        // rule logs it once and keeps it in the stream.
        let outcome = match (arrived, stayed) {
            (_, true) => "stayed, kept in the stream",
            (Some(_), false) => "left out, then rejoined",
            (None, false) => "neither: its session ended",
        };
        Ok(format!(
            "rule outcome {outcome}; moved after {}, still on the old endpoint at 250 ms {}, \
             back after the hold {}, session ends on {}",
            ms(arrived, set_at),
            yes_no(stayed),
            ms(back, cleared_at),
            match ends_on {
                Some(id) if id == from => "the default".to_owned(),
                Some(id) if id == to => "the pinned endpoint".to_owned(),
                Some(id) => id,
                None => "no endpoint".to_owned(),
            }
        ))
    }

    // MARK: Browser call routes: flip the default, or move it for the call

    const SIGNAL_HZ: f32 = 3000.0;
    const SIGNAL_LEVEL: f32 = 0.1;
    /// Detection blocks of 5 ms; a block carries the signal above a tenth
    /// of its level.
    const BLOCK_MS: u64 = 5;
    const RUNS: usize = 10;
    const LEAK_MAX_MS: u64 = 30;
    const GAP_MAX_MS: u64 = 100;
    const LOOK_AT: Duration = Duration::from_millis(500);
    const WINDOW: Duration = Duration::from_secs(3);

    /// `CPolicyConfigClient` and `IPolicyConfig`, as the app sets defaults
    /// (src/audio/wasapi/default_output.rs).
    const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    const POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    const SET_DEFAULT_SLOT: usize = 13;
    type SetDefault = unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT;

    type Heard = Arc<Mutex<Vec<(Instant, bool)>>>;

    struct Defaults(IUnknown);

    impl Defaults {
        fn new() -> Fallible<Self> {
            // SAFETY: creates an in-process object and asks it for the
            // interface; `raw` holds one reference on success, which the
            // result takes.
            unsafe {
                let client: IUnknown = CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
                    .map_err(|e| failed("CoCreateInstance(CPolicyConfigClient)", &e))?;
                let mut raw = std::ptr::null_mut();
                client
                    .query(&POLICY_CONFIG, &mut raw)
                    .ok()
                    .map_err(|e| failed("QueryInterface(IPolicyConfig)", &e))?;
                Ok(Self(IUnknown::from_raw(raw)))
            }
        }

        fn set(&self, id: &str, roles: &[ERole]) -> Fallible<()> {
            let id = HSTRING::from(id);
            for role in roles {
                // SAFETY: slot 13 is SetDefaultEndpoint, which takes the
                // object, a NUL-terminated ID alive through the call and a role.
                unsafe {
                    let table = *(self.0.as_raw() as *const *const *const c_void);
                    let call: SetDefault = std::mem::transmute(*table.add(SET_DEFAULT_SLOT));
                    call(self.0.as_raw(), PCWSTR(id.as_ptr()), *role)
                        .ok()
                        .map_err(|e| failed("SetDefaultEndpoint", &e))?;
                }
            }
            Ok(())
        }
    }

    fn carries_signal(block: &[f32], rate: f32) -> bool {
        let coeff = 2.0 * (std::f32::consts::TAU * SIGNAL_HZ / rate).cos();
        let (mut s1, mut s2) = (0f32, 0f32);
        for x in block {
            let s = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0);
        2.0 * power.sqrt() / block.len() as f32 > SIGNAL_LEVEL / 10.0
    }

    /// Captures endpoint `id` until `stop` and logs, per block, when it
    /// arrived and whether it carried the signal.
    fn listen(id: String, heard: Heard, stop: Arc<AtomicBool>) -> thread::JoinHandle<Fallible<()>> {
        thread::spawn(move || {
            init_com()?;
            // SAFETY: calls on the stream this thread owns; each packet is
            // read as `frames` float frames before it is released.
            unsafe {
                let client = enumerator()?
                    .GetDevice(&HSTRING::from(id.as_str()))
                    .map_err(|e| failed("GetDevice", &e))?
                    .Activate::<IAudioClient>(CLSCTX_ALL, None)
                    .map_err(|e| failed("Activate(IAudioClient)", &e))?;
                let format = client
                    .GetMixFormat()
                    .map_err(|e| failed("GetMixFormat", &e))?;
                let (channels, rate, bits) = (
                    usize::from((*format).nChannels),
                    (*format).nSamplesPerSec as f32,
                    (*format).wBitsPerSample,
                );
                let init =
                    client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, BUFFER_HNS, 0, format, None);
                CoTaskMemFree(Some(format.cast_const().cast()));
                init.map_err(|e| failed("Initialize", &e))?;
                if bits != 32 {
                    return Err(format!("capture has {bits}-bit samples, not float"));
                }
                let capture = client
                    .GetService::<IAudioCaptureClient>()
                    .map_err(|e| failed("GetService(IAudioCaptureClient)", &e))?;
                client.Start().map_err(|e| failed("Start", &e))?;
                let block = (rate as u64 * BLOCK_MS / 1000) as usize;
                let mut pending: Vec<f32> = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    while capture.GetNextPacketSize().is_ok_and(|frames| frames > 0) {
                        let mut data = std::ptr::null_mut();
                        let (mut frames, mut flags) = (0, 0);
                        if capture
                            .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                            .is_err()
                        {
                            break;
                        }
                        let at = Instant::now();
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                            pending.extend(std::iter::repeat_n(0.0, frames as usize));
                        } else {
                            let samples = std::slice::from_raw_parts(
                                data.cast::<f32>(),
                                frames as usize * channels,
                            );
                            pending.extend(samples.iter().step_by(channels));
                        }
                        let _ = capture.ReleaseBuffer(frames);
                        while pending.len() >= block {
                            let has = carries_signal(&pending[..block], rate);
                            pending.drain(..block);
                            if let Ok(mut heard) = heard.lock() {
                                heard.push((at, has));
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                let _ = client.Stop();
            }
            Ok(())
        })
    }

    /// Milliseconds of blocks in `[from, from + WINDOW)` whose signal
    /// equals `signal`.
    fn blocks_ms(heard: &Heard, from: Instant, signal: bool) -> u64 {
        let heard = heard.lock().map(|h| h.clone()).unwrap_or_default();
        let count = heard
            .iter()
            .filter(|(at, has)| *at >= from && *at < from + WINDOW && *has == signal)
            .count();
        count as u64 * BLOCK_MS
    }

    struct Chrome {
        child: Child,
        player: u32,
        listener: u32,
    }

    impl Drop for Chrome {
        fn drop(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &self.child.id().to_string()])
                .output();
            let _ = self.child.wait();
        }
    }

    struct Route<'a> {
        enumerator: IMMDeviceEnumerator,
        chrome_path: &'a str,
        cable: String,
        recorded: String,
        cable_heard: Heard,
        recorded_heard: Heard,
        music: u32,
        defaults: Defaults,
        policy: Policy,
    }

    impl Route<'_> {
        fn on(&self, id: &str, pid: u32) -> Fallible<bool> {
            Ok(sessions_on(&self.enumerator, id)?
                .iter()
                .any(|s| s.pid == pid && s.active))
        }

        fn where_is(&self, pid: u32) -> Fallible<&'static str> {
            Ok(
                match (self.on(&self.cable, pid)?, self.on(&self.recorded, pid)?) {
                    (true, false) => "cable",
                    (false, true) => "recorded",
                    (true, true) => "both",
                    (false, false) => "none",
                },
            )
        }

        /// Chrome on [`PAGE`] in a new profile, once its tone plays and it
        /// listens; `None` when either never shows within 30 s.
        fn chrome(&self, run: &str) -> Fallible<Option<Chrome>> {
            let own = std::process::id();
            let before: Vec<u32> = sessions_of(&self.enumerator, eRender)?
                .iter()
                .chain(sessions_of(&self.enumerator, eCapture)?.iter())
                .map(|session| session.pid)
                .collect();
            let dir = std::env::temp_dir().join("call-mic-chrome");
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let page = dir.join("call.html");
            std::fs::write(&page, PAGE).map_err(|e| format!("{}: {e}", page.display()))?;
            let child = Command::new(self.chrome_path)
                .arg(format!(
                    "--user-data-dir={}",
                    dir.join(format!("profile-{run}")).display()
                ))
                .args([
                    "--no-first-run",
                    "--no-default-browser-check",
                    "--use-fake-ui-for-media-stream",
                    "--autoplay-policy=no-user-gesture-required",
                    "--allow-file-access-from-files",
                ])
                .arg(format!(
                    "file:///{}",
                    page.display().to_string().replace('\\', "/")
                ))
                .spawn()
                .map_err(|e| format!("starting Chrome failed: {e}"))?;
            let mut chrome = Chrome {
                child,
                player: 0,
                listener: 0,
            };
            let fresh = |flow: EDataFlow| -> Fallible<u32> {
                Ok(sessions_of(&self.enumerator, flow)?
                    .iter()
                    .find(|s| s.active && s.pid != own && !before.contains(&s.pid))
                    .map_or(0, |s| s.pid))
            };
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline && (chrome.player == 0 || chrome.listener == 0) {
                if chrome.player == 0 {
                    chrome.player = fresh(eRender)?;
                }
                if chrome.listener == 0 {
                    chrome.listener = fresh(eCapture)?;
                }
                thread::sleep(POLL);
            }
            Ok((chrome.player != 0 && chrome.listener != 0).then_some(chrome))
        }

        fn pin(&self, pid: u32, to: &str) -> Fallible<()> {
            for role in [eConsole, eMultimedia] {
                self.policy.set(pid, role, to)?;
            }
            Ok(())
        }

        /// Pins Chrome's player to the recorded output and reports whether
        /// it still plays on the cable at the rule's 250 ms read-back.
        fn pin_and_read_back(&self, chrome: &Chrome) -> Fallible<bool> {
            let at = Instant::now();
            self.pin(chrome.player, &render_interface(&self.recorded))?;
            thread::sleep(STAYED.saturating_sub(at.elapsed()));
            self.on(&self.cable, chrome.player)
        }

        /// Route (d): with the pin in place, console and multimedia go to the
        /// recorded output and straight back.
        fn flip(&self, run: usize) -> Fallible<bool> {
            let reopen = q2_move(&self.enumerator, "reopen")?;
            let notice = yes_no(!reopen.contains("notices to the app 0"));
            let Some(chrome) = self.chrome(&format!("d{run}"))? else {
                println!("ROUTE d run {run}: untested, Chrome's tone or listening never showed");
                return Ok(false);
            };
            let start = self.where_is(chrome.player)?;
            let stayed = self.pin_and_read_back(&chrome)?;
            // The app writes its S469 restore record here; the probe keeps
            // the cable in `self.cable` and puts it back after every run.
            let flip = Instant::now();
            self.defaults
                .set(&self.recorded, &[eConsole, eMultimedia])?;
            self.defaults.set(&self.cable, &[eConsole, eMultimedia])?;
            let flipped_in = flip.elapsed().as_millis();
            thread::sleep(LOOK_AT.saturating_sub(flip.elapsed()));
            let chrome_at = self.where_is(chrome.player)?;
            let music_at = self.where_is(self.music)?;
            thread::sleep((WINDOW + Duration::from_millis(100)).saturating_sub(flip.elapsed()));
            let leak = blocks_ms(&self.recorded_heard, flip, true);
            let gap = blocks_ms(&self.cable_heard, flip, false);
            let baseline = blocks_ms(&self.cable_heard, flip - WINDOW, true);
            let ends = self.where_is(chrome.player)?;
            self.pin(chrome.player, "")?;
            let pass = chrome_at == "recorded"
                && music_at == "cable"
                && leak <= LEAK_MAX_MS
                && gap <= GAP_MAX_MS;
            println!(
                "ROUTE d run {run}: pin-only notice to the reopening helper {notice}; Chrome \
                 started on {start}, stayed at 250 ms {}; flip took {flipped_in} ms; at 500 ms \
                 Chrome on {chrome_at}, tone on {music_at}; leak {leak} ms, cable gap {gap} ms \
                 (tone on the cable {baseline} ms of the 3 s before); Chrome at 3 s on {ends}; \
                 pass {}",
                yes_no(stayed),
                yes_no(pass)
            );
            Ok(pass)
        }

        /// Route (b): the default moves to the recorded output for the
        /// call and returns 3 s after Chrome stops listening.
        fn move_for_call(&self, run: usize) -> Fallible<bool> {
            let Some(chrome) = self.chrome(&format!("b{run}"))? else {
                println!("ROUTE b run {run}: untested, Chrome's tone or listening never showed");
                return Ok(false);
            };
            let stayed = self.pin_and_read_back(&chrome)?;
            let moved = Instant::now();
            self.defaults
                .set(&self.recorded, &[eConsole, eMultimedia])?;
            let arrived = poll(|| self.on(&self.recorded, chrome.player))?;
            // SAFETY: reads the console default render endpoint.
            let read_back = unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
                .is_ok_and(|device| device_id(&device) == self.recorded);
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut quiet = None;
            while Instant::now() < deadline {
                if !sessions_of(&self.enumerator, eCapture)?
                    .iter()
                    .any(|s| s.pid == chrome.listener && s.active)
                {
                    quiet = Some(Instant::now());
                    break;
                }
                thread::sleep(POLL);
            }
            if let Some(quiet) = quiet {
                thread::sleep(HOLD.saturating_sub(quiet.elapsed()));
            }
            self.pin(chrome.player, "")?;
            let returned = Instant::now();
            self.defaults.set(&self.cable, &[eConsole, eMultimedia])?;
            let chrome_back = poll(|| Ok(self.where_is(chrome.player)? == "cable"))?;
            let music_back = poll(|| Ok(self.where_is(self.music)? == "cable"))?;
            let ms = |at: Option<Instant>, since: Instant| {
                at.map_or("never".to_owned(), |at| {
                    format!("{} ms", at.duration_since(since).as_millis())
                })
            };
            let pass = arrived.is_some() && chrome_back.is_some() && music_back.is_some();
            println!(
                "ROUTE b run {run}: stayed at 250 ms {}; Chrome on the recorded output after {}, \
                 default read back {}; listening stopped {}; after the return Chrome on the \
                 cable after {}, tone after {}; pass {}",
                yes_no(stayed),
                ms(arrived, moved),
                yes_no(read_back),
                yes_no(quiet.is_some()),
                ms(chrome_back, returned),
                ms(music_back, returned),
                yes_no(pass)
            );
            Ok(pass)
        }
    }

    /// Both routes, [`RUNS`] times each, with a default-path tone standing
    /// for the music. The console default at the start stands for the
    /// cable, another render endpoint for the recorded output.
    fn route(chrome_path: &str) -> Fallible<()> {
        let enumerator = enumerator()?;
        // SAFETY: reads the console and multimedia defaults.
        let (console, multimedia) = unsafe {
            (
                enumerator.GetDefaultAudioEndpoint(eRender, eConsole),
                enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia),
            )
        };
        let console = console.map_err(|e| failed("GetDefaultAudioEndpoint", &e))?;
        let multimedia = multimedia
            .map(|device| device_id(&device))
            .unwrap_or_default();
        let Some((cable, cable_name)) = usable(&console) else {
            println!("RESULT ROUTE untested: the console default is a device the probe never uses");
            return Ok(());
        };
        let Some((recorded, recorded_name)) = pick_render(&enumerator, &cable)? else {
            println!("RESULT ROUTE untested: no second render endpoint");
            return Ok(());
        };
        println!("ROUTE: cable {cable_name:?}, recorded output {recorded_name:?}");
        let defaults = Defaults::new()?;
        defaults.set(&cable, &[eConsole, eMultimedia])?;
        let mut music = Helper::start(&["tone", "signal"])?;
        music.wait_for("started")?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut ears = Vec::new();
        for (id, name) in active_devices(&enumerator, eCapture)?
            .iter()
            .filter_map(usable)
        {
            let heard = Heard::default();
            let handle = listen(id, Arc::clone(&heard), Arc::clone(&stop));
            ears.push((name, heard, handle));
        }
        thread::sleep(Duration::from_secs(1));
        let since = Instant::now() - LOOK_AT;
        let toned: Vec<bool> = ears
            .iter()
            .map(|(_, heard, _)| {
                heard
                    .lock()
                    .is_ok_and(|h| h.iter().any(|(at, has)| *at >= since && *has))
            })
            .collect();
        let names: Vec<&String> = ears.iter().map(|(name, _, _)| name).collect();
        println!("ROUTE: capture endpoints {names:?}, carrying the tone {toned:?}");
        let result = if ears.len() == 2 && toned.iter().filter(|t| **t).count() == 1 {
            let cable_at = usize::from(!toned[0]);
            let route = Route {
                enumerator: enumerator.clone(),
                chrome_path,
                cable: cable.clone(),
                recorded,
                cable_heard: Arc::clone(&ears[cable_at].1),
                recorded_heard: Arc::clone(&ears[1 - cable_at].1),
                music: music.pid(),
                defaults: Defaults::new()?,
                policy: Policy::new()?,
            };
            let mut passed = [0; 2];
            for (leg, count) in passed.iter_mut().enumerate() {
                for run in 1..=RUNS {
                    let pass = if leg == 0 {
                        route.flip(run)
                    } else {
                        route.move_for_call(run)
                    };
                    match pass {
                        Ok(true) => *count += 1,
                        Ok(false) => {}
                        Err(error) => println!("ROUTE run {run}: error {error}"),
                    }
                    let _ = route.defaults.set(&cable, &[eConsole, eMultimedia]);
                    thread::sleep(Duration::from_secs(1));
                }
            }
            format!(
                "winner {}: (d) flip passed {}/{RUNS}, (b) move for the call passed {}/{RUNS}",
                if passed[0] == RUNS { "d" } else { "b" },
                passed[0],
                passed[1]
            )
        } else {
            "untested: no capture endpoint pairs with the cable alone".to_owned()
        };
        stop.store(true, Ordering::Relaxed);
        for (name, _, handle) in ears {
            if let Ok(Err(error)) = handle.join() {
                println!("ROUTE: capture of {name:?}: {error}");
            }
        }
        music.finish();
        let _ = defaults.set(&cable, &[eConsole]);
        if !multimedia.is_empty() {
            let _ = defaults.set(&multimedia, &[eMultimedia]);
        }
        println!("RESULT ROUTE {result}");
        Ok(())
    }

    // MARK: Small readers

    fn init_com() -> Fallible<()> {
        // SAFETY: joins this thread to the multithreaded apartment.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| failed("CoInitializeEx", &e))
    }

    fn enumerator() -> Fallible<IMMDeviceEnumerator> {
        // SAFETY: creates the documented MMDevice enumerator.
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| failed("CoCreateInstance(MMDeviceEnumerator)", &e))
    }

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

    fn friendly_name(device: &IMMDevice) -> String {
        // SAFETY: reads one string property; the variant's string is read
        // only when it holds one.
        unsafe {
            let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
                return String::new();
            };
            let Ok(value) = store.GetValue(&PKEY_Device_FriendlyName) else {
                return String::new();
            };
            if value.Anonymous.Anonymous.vt == VT_LPWSTR {
                value
                    .Anonymous
                    .Anonymous
                    .Anonymous
                    .pwszVal
                    .to_string()
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
    }

    /// The microphone privacy record's LastUsedTimeStart for this exe, which
    /// Windows writes when the exe opens a capture stream.
    fn mic_last_used() -> Fallible<Option<u64>> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let key = format!(
            r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged\{}",
            exe.display().to_string().replace('\\', "#")
        );
        let mut data = 0u64;
        let mut bytes = size_of::<u64>() as u32;
        // SAFETY: `data` and its size in bytes describe writable memory.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(key),
                &HSTRING::from("LastUsedTimeStart"),
                RRF_RT_REG_QWORD,
                None,
                Some((&raw mut data).cast()),
                Some(&mut bytes),
            )
        };
        Ok((status == ERROR_SUCCESS).then_some(data))
    }

    fn show_used(value: Option<u64>) -> String {
        value.map_or("unset".into(), |time| format!("set ({time})"))
    }

    fn elevated() -> &'static str {
        let mut token = HANDLE::default();
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        // SAFETY: opens this process's token for query, reads one struct
        // into memory owned here and closes the token.
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
                return "unknown";
            }
            let read = GetTokenInformation(
                token,
                TokenElevation,
                Some((&raw mut elevation).cast()),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            );
            let _ = CloseHandle(token);
            match read {
                Ok(()) => yes_no(elevation.TokenIsElevated != 0),
                Err(_) => "unknown",
            }
        }
    }

    fn os_build() -> String {
        let output = Command::new("cmd").args(["/c", "ver"]).output();
        output
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default()
    }

    fn yes_no(value: bool) -> &'static str {
        if value { "yes" } else { "no" }
    }
}
