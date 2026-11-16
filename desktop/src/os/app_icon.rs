//! A running app's icon for the source menu, as the Mac's menu shows each
//! app's own: on Windows the icon of the process's executable; elsewhere none,
//! and the menu draws its app glyph.

use std::path::PathBuf;

/// The executable a running process was started from.
#[cfg(windows)]
pub fn process_image(pid: u32) -> Option<PathBuf> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    // SAFETY: the handle is closed below and the buffer outlives the call.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut name = [0u16; 1024];
        let mut length = name.len() as u32;
        let read = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(name.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        read.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(
            &name[..length as usize],
        )))
    }
}

#[cfg(not(windows))]
pub fn process_image(_pid: u32) -> Option<PathBuf> {
    None
}

/// The executable behind the app source `id` (`app:<pid>:<executable>`): its
/// process's, else a running process's of the same executable, as for a saved
/// choice restored with pid 0 while its app has no audio session yet.
pub fn source_image(
    id: &str,
    image_of: impl Fn(u32) -> Option<PathBuf>,
    pid_named: impl Fn(&str) -> Option<u32>,
) -> Option<PathBuf> {
    let (pid, executable) = id.strip_prefix("app:")?.split_once(':')?;
    pid.parse()
        .ok()
        .and_then(&image_of)
        .or_else(|| pid_named(executable).and_then(&image_of))
}

/// A running process started from `executable` (a file name, any case).
#[cfg(windows)]
pub fn pid_named(executable: &str) -> Option<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: the snapshot handle is closed below, and `entry`'s size field is
    // set, as both walk calls require.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more && found.is_none() {
            let length = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case(executable)
            {
                found = Some(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

#[cfg(not(windows))]
pub fn pid_named(_executable: &str) -> Option<u32> {
    None
}

/// The first icon in `path`, `size` pixels square, as straight RGBA.
#[cfg(windows)]
pub fn file_icon(path: &std::path::Path, size: u32) -> Option<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
        HBITMAP, HDC, ReleaseDC,
    };
    use windows::Win32::UI::Shell::SHDefExtractIconW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
    use windows::core::PCWSTR;

    fn pixels(dc: HDC, bitmap: HBITMAP, size: u32) -> Option<Vec<u8>> {
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                // Negative: rows top-down.
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut out = vec![0u8; (size * size * 4) as usize];
        // SAFETY: `out` holds `size` rows of 32-bit pixels, as `info` asks for.
        let rows = unsafe {
            GetDIBits(
                dc,
                bitmap,
                0,
                size,
                Some(out.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        (rows == size as i32).then_some(out)
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut icon = HICON::default();
    // SAFETY: `wide` is NUL-terminated; every handle made here is freed here.
    unsafe {
        SHDefExtractIconW(PCWSTR(wide.as_ptr()), 0, 0, Some(&mut icon), None, size)
            .ok()
            .ok()?;
        let mut parts = ICONINFO::default();
        let got = GetIconInfo(icon, &mut parts);
        let _ = DestroyIcon(icon);
        got.ok()?;
        let dc = GetDC(None);
        let color = pixels(dc, parts.hbmColor, size);
        let mask = pixels(dc, parts.hbmMask, size);
        ReleaseDC(None, dc);
        let _ = DeleteObject(parts.hbmColor.into());
        let _ = DeleteObject(parts.hbmMask.into());
        Some(rgba(&color?, &mask?))
    }
}

#[cfg(not(windows))]
pub fn file_icon(_path: &std::path::Path, _size: u32) -> Option<Vec<u8>> {
    None
}

/// An icon's BGRA color pixels as straight RGBA. An icon without alpha takes
/// it from its AND mask, where a set pixel is transparent.
#[cfg_attr(not(windows), allow(dead_code))]
fn rgba(color: &[u8], mask: &[u8]) -> Vec<u8> {
    let has_alpha = color.chunks(4).any(|pixel| pixel[3] != 0);
    color
        .chunks(4)
        .zip(mask.chunks(4))
        .flat_map(|(pixel, masked)| {
            let alpha = if has_alpha {
                pixel[3]
            } else if masked[0] == 0 {
                255
            } else {
                0
            };
            [pixel[2], pixel[1], pixel[0], alpha]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{rgba, source_image};
    use std::path::PathBuf;

    #[test]
    fn a_restored_choice_without_its_process_takes_a_running_one_of_the_same_executable() {
        let path = PathBuf::from("C:/Apps/Spotify.exe");
        let image_of = |pid| (pid == 412).then(|| path.clone());
        let pid_named = |executable: &str| {
            executable
                .eq_ignore_ascii_case("spotify.exe")
                .then_some(412)
        };
        assert_eq!(
            source_image("app:0:Spotify.exe", image_of, pid_named),
            Some(path.clone())
        );
        assert_eq!(
            source_image("app:412:Spotify.exe", image_of, |_| None),
            Some(path.clone())
        );
        assert_eq!(source_image("system", image_of, pid_named), None);
    }

    #[test]
    fn an_icon_with_alpha_keeps_it_and_turns_bgra_into_rgba() {
        let color = [10, 20, 30, 128, 1, 2, 3, 0];
        let mask = [0; 8];
        assert_eq!(rgba(&color, &mask), [30, 20, 10, 128, 3, 2, 1, 0]);
    }

    #[test]
    fn an_icon_without_alpha_takes_it_from_its_mask() {
        let color = [10, 20, 30, 0, 1, 2, 3, 0];
        let mask = [0, 0, 0, 0, 255, 255, 255, 0];
        assert_eq!(rgba(&color, &mask), [30, 20, 10, 255, 3, 2, 1, 0]);
    }
}
