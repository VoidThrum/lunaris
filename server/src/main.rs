#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static mut __cpu_model: [u32; 4] = [0; 4];

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
#[no_mangle]
pub extern "C" fn __cpu_indicator_init() -> i32 {
    0
}

fn main() -> std::process::ExitCode {
    // The standalone server says in its log when a newer release exists
    // (the app's `lunaris server` child does not: the app has its updater).
    server::update_notice::enable();
    server::run(std::env::args().skip(1).collect())
}
