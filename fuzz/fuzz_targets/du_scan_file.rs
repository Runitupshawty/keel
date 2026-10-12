#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| keel_vfs::fuzz::du_scan_file(data));
