//! Desktop process lifetime tracking. The Deck probe adds an independent observer.
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::wait_for_game;
#[cfg(windows)]
pub fn wait_for_game(
    command: std::process::Command,
    started: impl FnOnce(),
) -> std::io::Result<std::process::ExitStatus> {
    use process_wrap::std::{CommandWrap, JobObject};
    let mut wrapper = CommandWrap::from(command);
    let mut child = wrapper.wrap(JobObject).spawn()?;
    started();
    child.wait()
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn wait_for_game(
    mut command: std::process::Command,
    started: impl FnOnce(),
) -> std::io::Result<std::process::ExitStatus> {
    let mut child = command.spawn()?;
    started();
    child.wait()
}
