#![no_main]
// libfuzzer-sys aborts on every panic, even one caught: the previewers catch docx-rs's
// panics on damaged files (an error preview), so the hook goes back to the default one.
// A panic that reaches the target still aborts.
libfuzzer_sys::fuzz_target!(
    init: {
        let _ = std::panic::take_hook();
    },
    |data: &[u8]| keel_preview::fuzz::preview(data)
);
