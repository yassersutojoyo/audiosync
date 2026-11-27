//! The lines `fake_tv` takes on stdin.

#[allow(dead_code)]
#[path = "../examples/fake_tv/stdin.rs"]
mod stdin;

use stdin::{Line, read};

#[test]
fn hide_and_show_are_commands_next_to_vol_and_quit() {
    assert_eq!(read("hide"), Line::Hide);
    assert_eq!(read(" show "), Line::Show);
    assert_eq!(read("quit"), Line::Quit);
    assert_eq!(read("vol -24.5"), Line::Vol(-24.5));
    assert_eq!(read("loss 2.5"), Line::Loss(2.5));
    assert_eq!(read("loss off"), Line::Loss(0.0));
    assert_eq!(read("loss 101"), Line::BadLoss);
}

#[test]
fn sleep_sleep_quiet_and_wake_are_commands() {
    assert_eq!(read("sleep"), Line::Sleep { goodbye: true });
    assert_eq!(read("sleep quiet"), Line::Sleep { goodbye: false });
    assert_eq!(read("wake"), Line::Wake);
    assert_eq!(read("sleep loud"), Line::Unknown);
}

#[test]
fn a_bad_or_unknown_line_is_told_apart_from_an_empty_one() {
    assert_eq!(read("vol loud"), Line::BadVol);
    assert_eq!(read("vol"), Line::Unknown);
    assert_eq!(read("hid"), Line::Unknown);
    assert_eq!(read("   "), Line::Empty);
}
