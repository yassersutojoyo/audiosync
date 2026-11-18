//! A silent test player for the Now Playing card on Windows: it publishes a
//! title, artist, album and cover to the system media session
//! (`SystemMediaTransportControls`, the same `GlobalSystemMediaTransportControls`
//! listing Spotify shows up in), changes song on command or on a timer, and
//! takes the card's buttons. It opens no audio stream, so nothing is heard.
//!
//! From `desktop/`: `cargo run --example fake_player -- [--every <seconds>]
//! [--paused] [--track "Title|Artist|Album"]...`. Without `--track` it plays
//! three built-in test tracks. Each track's cover is a generated picture in its
//! own color. Lines on standard input: `next`, `prev`, `play`, `pause`,
//! `toggle`, `shuffle on|off`, `repeat off|one|all`, `quit`. Every change
//! prints one `state:` line.
//!
//! The session's app ID is the executable's name, so a source of "All System
//! Audio" follows it. It plays no audio, so it is never offered as an app
//! source of its own.

#[derive(Clone, Debug, PartialEq, Eq)]
struct Track {
    title: String,
    artist: String,
    album: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Repeat {
    Off,
    One,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Command {
    Next,
    Previous,
    Play,
    Pause,
    Toggle,
    Shuffle(bool),
    Repeat(Repeat),
    Quit,
}

fn parse_command(line: &str) -> Option<Command> {
    let words: Vec<&str> = line.split_whitespace().collect();
    Some(match words.as_slice() {
        ["next"] => Command::Next,
        ["prev"] => Command::Previous,
        ["play"] => Command::Play,
        ["pause"] => Command::Pause,
        ["toggle"] => Command::Toggle,
        ["shuffle", "on"] => Command::Shuffle(true),
        ["shuffle", "off"] => Command::Shuffle(false),
        ["repeat", "off"] => Command::Repeat(Repeat::Off),
        ["repeat", "one"] => Command::Repeat(Repeat::One),
        ["repeat", "all"] => Command::Repeat(Repeat::All),
        ["quit"] => Command::Quit,
        _ => return None,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    every: Option<std::time::Duration>,
    paused: bool,
    tracks: Vec<Track>,
}

fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        every: None,
        paused: false,
        tracks: Vec::new(),
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--paused" => options.paused = true,
            "--every" => {
                let seconds: f64 = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .filter(|seconds: &f64| *seconds > 0.0)
                    .ok_or("--every needs a number of seconds above 0")?;
                options.every = Some(std::time::Duration::from_secs_f64(seconds));
            }
            "--track" => {
                let value = args.next().ok_or("--track needs \"Title|Artist|Album\"")?;
                let mut parts = value.splitn(3, '|').map(str::to_owned);
                options.tracks.push(Track {
                    title: parts.next().unwrap_or_default(),
                    artist: parts.next().unwrap_or_default(),
                    album: parts.next().unwrap_or_default(),
                });
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.tracks.is_empty() {
        options.tracks = [
            ("Test Song One", "Test Artist A", "Test Album 1"),
            ("Tës Sóng Twö", "Test Artist B", "Test Album 2"),
            (
                "Test Song Three, with a title long enough to show how the card cuts it",
                "Test Artist C",
                "Test Album 3",
            ),
        ]
        .into_iter()
        .map(|(title, artist, album)| Track {
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.to_owned(),
        })
        .collect();
    }
    Ok(options)
}

/// Where the player stands in its list.
#[derive(Debug)]
struct Player {
    count: usize,
    index: usize,
    playing: bool,
    shuffle: bool,
    repeat: Repeat,
    seed: u32,
}

impl Player {
    fn new(count: usize, playing: bool) -> Player {
        Player {
            count,
            index: 0,
            playing,
            shuffle: false,
            repeat: Repeat::Off,
            seed: 0x2545_f491,
        }
    }

    /// The next track. The timer keeps the same one under repeat one; a
    /// press always moves, and shuffle never picks the track that just played.
    fn next(&mut self, by_timer: bool) {
        if by_timer && self.repeat == Repeat::One {
            return;
        }
        if self.shuffle && self.count > 1 {
            self.seed = self.seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let step = 1 + (self.seed >> 16) as usize % (self.count - 1);
            self.index = (self.index + step) % self.count;
        } else {
            self.index = (self.index + 1) % self.count;
        }
    }

    fn previous(&mut self) {
        self.index = (self.index + self.count - 1) % self.count;
    }

    /// Applies a command; false when it asks to quit.
    fn apply(&mut self, command: &Command) -> bool {
        match command {
            Command::Next => self.next(false),
            Command::Previous => self.previous(),
            Command::Play => self.playing = true,
            Command::Pause => self.playing = false,
            Command::Toggle => self.playing = !self.playing,
            Command::Shuffle(on) => self.shuffle = *on,
            Command::Repeat(mode) => self.repeat = *mode,
            Command::Quit => return false,
        }
        true
    }

    fn state(&self, track: &Track) -> String {
        format!(
            "state: {} \"{}\" - {} (track {}/{}) shuffle={} repeat={:?}",
            if self.playing { "playing" } else { "paused" },
            track.title,
            track.artist,
            self.index + 1,
            self.count,
            if self.shuffle { "on" } else { "off" },
            self.repeat,
        )
    }
}

const COVER_SIDE: usize = 300;

/// A 24-bit BMP cover for track `index`: a field in the track's own color
/// with a light diagonal band, so two covers never look alike.
fn cover(index: usize) -> Vec<u8> {
    const COLORS: [[u8; 3]; 6] = [
        [200, 40, 40],
        [40, 150, 60],
        [40, 70, 200],
        [210, 150, 20],
        [140, 50, 170],
        [20, 150, 160],
    ];
    let [red, green, blue] = COLORS[index % COLORS.len()];
    let row = COVER_SIDE * 3;
    let pixels = row * COVER_SIDE;
    let mut bmp = Vec::with_capacity(54 + pixels);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((54 + pixels) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&(COVER_SIDE as i32).to_le_bytes());
    bmp.extend_from_slice(&(COVER_SIDE as i32).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&24u16.to_le_bytes());
    bmp.extend_from_slice(&[0; 24]);
    for y in 0..COVER_SIDE {
        for x in 0..COVER_SIDE {
            let band = (x + y + index * 40) % 150 < 30;
            let pixel = if band {
                [255, 255, 255]
            } else {
                [blue, green, red]
            };
            bmp.extend_from_slice(&pixel);
        }
    }
    bmp
}

#[cfg(windows)]
mod smtc {
    use std::sync::mpsc::Sender;

    use windows::Foundation::TypedEventHandler;
    use windows::Media::{
        AutoRepeatModeChangeRequestedEventArgs, MediaPlaybackAutoRepeatMode, MediaPlaybackStatus,
        MediaPlaybackType, ShuffleEnabledChangeRequestedEventArgs, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    };
    use windows::Storage::Streams::{
        DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::WinRT::{
        ISystemMediaTransportControlsInterop, RO_INIT_MULTITHREADED, RoInitialize,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
    };
    use windows::core::{HSTRING, Ref, Result, w};

    use super::{Command, Player, Repeat, Track};

    pub struct Session {
        controls: SystemMediaTransportControls,
        _window: HWND,
    }

    impl Session {
        /// Opens the session on a hidden window; the buttons and the
        /// shuffle and repeat requests arrive on `commands`.
        pub fn open(commands: Sender<Command>) -> Result<Session> {
            unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
            let window = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("AudioSync fake player"),
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    100,
                    100,
                    None,
                    None,
                    None,
                    None,
                )?
            };
            let interop = windows::core::factory::<
                SystemMediaTransportControls,
                ISystemMediaTransportControlsInterop,
            >()?;
            let controls: SystemMediaTransportControls = unsafe { interop.GetForWindow(window)? };
            controls.SetIsEnabled(true)?;
            controls.SetIsPlayEnabled(true)?;
            controls.SetIsPauseEnabled(true)?;
            controls.SetIsNextEnabled(true)?;
            controls.SetIsPreviousEnabled(true)?;
            let sender = commands.clone();
            controls.ButtonPressed(&TypedEventHandler::new(
                move |_, args: Ref<SystemMediaTransportControlsButtonPressedEventArgs>| {
                    let button = args.ok()?.Button()?;
                    let command = match button {
                        SystemMediaTransportControlsButton::Play => Command::Play,
                        SystemMediaTransportControlsButton::Pause => Command::Pause,
                        SystemMediaTransportControlsButton::Next => Command::Next,
                        SystemMediaTransportControlsButton::Previous => Command::Previous,
                        _ => return Ok(()),
                    };
                    println!("button: {command:?}");
                    let _ = sender.send(command);
                    Ok(())
                },
            ))?;
            let sender = commands.clone();
            controls.ShuffleEnabledChangeRequested(&TypedEventHandler::new(
                move |_, args: Ref<ShuffleEnabledChangeRequestedEventArgs>| {
                    let on = args.ok()?.RequestedShuffleEnabled()?;
                    println!("button: shuffle {on}");
                    let _ = sender.send(Command::Shuffle(on));
                    Ok(())
                },
            ))?;
            controls.AutoRepeatModeChangeRequested(&TypedEventHandler::new(
                move |_, args: Ref<AutoRepeatModeChangeRequestedEventArgs>| {
                    let mode = match args.ok()?.RequestedAutoRepeatMode()? {
                        MediaPlaybackAutoRepeatMode::Track => Repeat::One,
                        MediaPlaybackAutoRepeatMode::List => Repeat::All,
                        _ => Repeat::Off,
                    };
                    println!("button: repeat {mode:?}");
                    let _ = commands.send(Command::Repeat(mode));
                    Ok(())
                },
            ))?;
            Ok(Session {
                controls,
                _window: window,
            })
        }

        pub fn publish(&self, player: &Player, track: &Track) -> Result<()> {
            let controls = &self.controls;
            controls.SetPlaybackStatus(if player.playing {
                MediaPlaybackStatus::Playing
            } else {
                MediaPlaybackStatus::Paused
            })?;
            controls.SetShuffleEnabled(player.shuffle)?;
            controls.SetAutoRepeatMode(match player.repeat {
                Repeat::Off => MediaPlaybackAutoRepeatMode::None,
                Repeat::One => MediaPlaybackAutoRepeatMode::Track,
                Repeat::All => MediaPlaybackAutoRepeatMode::List,
            })?;
            let updater = controls.DisplayUpdater()?;
            updater.SetType(MediaPlaybackType::Music)?;
            let music = updater.MusicProperties()?;
            music.SetTitle(&HSTRING::from(&track.title))?;
            music.SetArtist(&HSTRING::from(&track.artist))?;
            music.SetAlbumTitle(&HSTRING::from(&track.album))?;
            updater.SetThumbnail(&thumbnail(&super::cover(player.index))?)?;
            updater.Update()
        }
    }

    fn thumbnail(bytes: &[u8]) -> Result<RandomAccessStreamReference> {
        let stream = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&stream.GetOutputStreamAt(0)?)?;
        writer.WriteBytes(bytes)?;
        writer.StoreAsync()?.join()?;
        writer.FlushAsync()?.join()?;
        writer.DetachStream()?;
        stream.Seek(0)?;
        RandomAccessStreamReference::CreateFromStream(&stream)
    }

    /// Hands the hidden window its messages so the session stays answered.
    pub fn pump() {
        let mut message = MSG::default();
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::io::BufRead;
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::{Duration, Instant};

    let options = match parse_options(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("fake_player: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    let (sender, commands) = channel();
    let session = match smtc::Session::open(sender.clone()) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("fake_player: the media session couldn't open: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            match parse_command(&line) {
                Some(command) => {
                    if sender.send(command).is_err() {
                        break;
                    }
                }
                None if line.trim().is_empty() => {}
                None => eprintln!("fake_player: unknown command {line:?}"),
            }
        }
    });
    let mut player = Player::new(options.tracks.len(), !options.paused);
    let publish = |player: &Player| {
        let track = &options.tracks[player.index];
        if let Err(error) = session.publish(player, track) {
            eprintln!("fake_player: publishing failed: {error}");
        }
        println!("{}", player.state(track));
    };
    publish(&player);
    let mut due = options.every.map(|every| Instant::now() + every);
    loop {
        smtc::pump();
        match commands.recv_timeout(Duration::from_millis(50)) {
            Ok(command) => {
                if !player.apply(&command) {
                    break;
                }
                if matches!(command, Command::Next | Command::Previous) {
                    due = options.every.map(|every| Instant::now() + every);
                }
                publish(&player);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let (Some(at), Some(every)) = (due, options.every)
                    && player.playing
                    && Instant::now() >= at
                {
                    player.next(true);
                    due = Some(Instant::now() + every);
                    publish(&player);
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(not(windows))]
fn main() {
    eprintln!("fake_player publishes to Windows' media sessions only");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse_command(" next "), Some(Command::Next));
        assert_eq!(parse_command("shuffle on"), Some(Command::Shuffle(true)));
        assert_eq!(
            parse_command("repeat one"),
            Some(Command::Repeat(Repeat::One))
        );
        assert_eq!(parse_command("repeat twice"), None);
        assert_eq!(parse_command("skip"), None);
    }

    #[test]
    fn options_take_tracks_and_timer() {
        let args = ["--every", "2.5", "--paused", "--track", "Song|Band"];
        let options = parse_options(args.map(str::to_owned)).unwrap();
        assert_eq!(options.every, Some(std::time::Duration::from_millis(2500)));
        assert!(options.paused);
        assert_eq!(options.tracks.len(), 1);
        assert_eq!(options.tracks[0].artist, "Band");
        assert_eq!(options.tracks[0].album, "");
        assert_eq!(parse_options(Vec::new()).unwrap().tracks.len(), 3);
        assert!(parse_options(["--every".to_owned(), "0".to_owned()]).is_err());
    }

    #[test]
    fn next_and_previous_wrap() {
        let mut player = Player::new(3, true);
        player.previous();
        assert_eq!(player.index, 2);
        player.next(false);
        assert_eq!(player.index, 0);
    }

    #[test]
    fn repeat_one_holds_the_timer_but_not_a_press() {
        let mut player = Player::new(3, true);
        player.apply(&Command::Repeat(Repeat::One));
        player.next(true);
        assert_eq!(player.index, 0);
        player.next(false);
        assert_eq!(player.index, 1);
    }

    #[test]
    fn shuffle_never_repeats_the_track_that_played() {
        let mut player = Player::new(4, true);
        player.apply(&Command::Shuffle(true));
        for _ in 0..50 {
            let before = player.index;
            player.next(false);
            assert_ne!(player.index, before);
            assert!(player.index < 4);
        }
    }

    #[test]
    fn covers_are_whole_bitmaps_that_differ() {
        let first = cover(0);
        assert_eq!(&first[..2], b"BM");
        assert_eq!(first.len(), 54 + COVER_SIDE * COVER_SIDE * 3);
        assert_eq!(
            u32::from_le_bytes(first[2..6].try_into().unwrap()) as usize,
            first.len()
        );
        assert_ne!(first, cover(1));
    }
}
